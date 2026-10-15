//! Supervision of one or more labs (for example forked clones) in one process, each with its
//! own control socket.
//!
//! A lab's resources are owned by its supervisor task, then by the teardown that dropping the
//! [`Lab`] spawns, and are guarded by the lab directory lock. Recovery by name only runs once
//! the task has been joined and that lock has been released, so it is never refused by, or
//! raced with, the previous owner.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use tokio::task::{AbortHandle, Id, JoinError, JoinSet};

use crate::{Lab, LabConfig, LabError};

/// Everything that went wrong for one lab: operation errors first, then the recovery error.
#[derive(Debug)]
pub struct LabFailure {
    pub lab: String,
    pub errors: Vec<LabError>,
}

impl fmt::Display for LabFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lab {}: ", self.lab)?;
        for (i, error) in self.errors.iter().enumerate() {
            if i > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

/// Every lab failure of a fleet, in the order they were settled.
#[derive(Debug, thiserror::Error)]
#[error("{} lab(s) failed: {}", .0.len(), .0.iter().map(ToString::to_string).collect::<Vec<_>>().join(" | "))]
pub struct FleetError(pub Vec<LabFailure>);

struct Member {
    id: String,
    socket: PathBuf,
    abort: AbortHandle,
    errors: Vec<LabError>,
}

/// Supervises labs until each is torn down on request or a shutdown is asked.
pub struct Fleet {
    config: LabConfig,
    timeout: Duration,
    tasks: JoinSet<Result<(), LabError>>,
    members: HashMap<Id, Member>,
    failures: Vec<LabFailure>,
}

impl Fleet {
    /// `timeout` bounds each teardown request, the wait for supervisors to stop, and the wait
    /// for a released lab lock before recovery.
    #[must_use]
    pub fn new(config: &LabConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
            tasks: JoinSet::new(),
            members: HashMap::new(),
            failures: Vec::new(),
        }
    }

    pub fn spawn(&mut self, lab: Lab) {
        let (id, socket) = (lab.id().to_owned(), lab.control_socket());
        self.spawn_supervisor(id, socket, lab.supervise());
    }

    /// Supervises `id` with an arbitrary future, to inject failing, stalled or panicking
    /// supervisors in tests.
    #[cfg(feature = "test-hooks")]
    pub fn spawn_with(
        &mut self,
        id: String,
        socket: PathBuf,
        supervisor: impl Future<Output = Result<(), LabError>> + Send + 'static,
    ) {
        self.spawn_supervisor(id, socket, supervisor);
    }

    fn spawn_supervisor(
        &mut self,
        id: String,
        socket: PathBuf,
        supervisor: impl Future<Output = Result<(), LabError>> + Send + 'static,
    ) {
        let abort = self.tasks.spawn(supervisor);
        self.members.insert(
            abort.id(),
            Member {
                id,
                socket,
                abort,
                errors: Vec::new(),
            },
        );
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Runs until every lab has stopped or `shutdown` completes, then tears down the rest.
    /// A failed lab is recovered by name as soon as its supervisor has been joined; the other
    /// labs keep running.
    ///
    /// # Errors
    /// [`FleetError`] with every lab that failed, including those that failed before shutdown.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> Result<(), FleetError> {
        tokio::pin!(shutdown);
        while !self.members.is_empty() {
            // Only the join is raced with shutdown: settling (recovery) is never cancelled.
            let joined = tokio::select! {
                biased;
                () = &mut shutdown => break,
                joined = self.tasks.join_next_with_id() => joined,
            };
            match joined {
                Some(joined) => self.settle(joined).await,
                None => break,
            }
        }
        self.shutdown().await;
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(FleetError(self.failures))
        }
    }

    async fn shutdown(&mut self) {
        if self.members.is_empty() {
            return;
        }
        tracing::info!(labs = self.members.len(), "tearing down all labs");
        let timeout = self.timeout;
        let requests = join_all(self.members.iter().map(|(task, member)| async move {
            (*task, Lab::request_teardown(&member.socket, timeout).await)
        }))
        .await;
        for (task, result) in requests {
            if let (Err(error), Some(member)) = (result, self.members.get_mut(&task)) {
                // The supervisor may still own the lab: stop it before anyone else touches it.
                tracing::error!(lab = %member.id, %error, "teardown request failed, aborting supervisor");
                member.abort.abort();
                member.errors.push(error);
            }
        }
        // Join everything before settling: a slow recovery must not eat the others' deadline.
        let deadline = Instant::now() + timeout;
        let mut joined = Vec::new();
        while joined.len() < self.members.len() {
            match tokio::time::timeout_at(deadline.into(), self.tasks.join_next_with_id()).await {
                Ok(Some(result)) => joined.push(result),
                Ok(None) => break,
                Err(_) => {
                    for member in self.members.values_mut() {
                        if !member.abort.is_finished() {
                            tracing::error!(lab = %member.id, "supervisor did not stop, aborting");
                            member.abort.abort();
                            member.errors.push(LabError::Invalid {
                                lab: member.id.clone(),
                                message: format!("supervisor did not stop within {timeout:?}"),
                            });
                        }
                    }
                    // Aborted tasks finish at their next await point.
                    while let Some(result) = self.tasks.join_next_with_id().await {
                        joined.push(result);
                    }
                }
            }
        }
        for result in joined {
            self.settle(result).await;
        }
    }

    async fn settle(&mut self, joined: Result<(Id, Result<(), LabError>), JoinError>) {
        let (task, outcome) = match joined {
            Ok((task, outcome)) => (task, outcome.err()),
            // Cancellation is ours and already recorded as the reason for the abort.
            Err(error) if error.is_cancelled() => (error.id(), None),
            Err(error) => {
                let task = error.id();
                let lab = self.members.get(&task).map(|m| m.id.clone());
                let lab = lab.unwrap_or_default();
                (task, Some(LabError::Task { lab, source: error }))
            }
        };
        let Some(mut member) = self.members.remove(&task) else {
            return;
        };
        member.errors.extend(outcome);
        if member.errors.is_empty() {
            tracing::info!(lab = %member.id, "lab is down");
            return;
        }
        for error in &member.errors {
            tracing::error!(lab = %member.id, %error, "lab failed, recovering by name");
        }
        if let Err(error) = Lab::recover_by_name(&self.config, &member.id, self.timeout).await {
            tracing::error!(lab = %member.id, %error, "recovery by name failed");
            member.errors.push(error);
        }
        self.failures.push(LabFailure {
            lab: member.id,
            errors: member.errors,
        });
    }
}
