//! Pre-warmed restore pool for instant microVM lab forks.
#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::task::JoinSet;

use crate::{Lab, LabConfig, LabError};

fn invalid(lab: &str, message: &str) -> LabError {
    LabError::Invalid {
        lab: lab.into(),
        message: message.into(),
    }
}

/// Manages a pool of pre-warmed, paused clone labs restored from a snapshot.
pub struct RestorePool {
    config: LabConfig,
    snapshot: PathBuf,
    prefix: String,
    capacity: usize,
    sequence: usize,
    ready: VecDeque<Lab>,
    refilling: JoinSet<Result<Lab, LabError>>,
}

impl RestorePool {
    /// Maximum allowed pool capacity to bound host memory consumption.
    pub const MAX_CAPACITY: usize = 64;

    /// Initializes a restore pool and pre-warms `capacity` paused clones.
    ///
    /// # Errors
    /// Returns an error if `capacity` is invalid, prefix is malformed, or bring-up fails.
    pub async fn new(
        config: &LabConfig,
        snapshot: &Path,
        prefix: &str,
        capacity: usize,
    ) -> Result<Self, LabError> {
        if capacity == 0 || capacity > Self::MAX_CAPACITY {
            return Err(invalid(
                prefix,
                &format!("pool capacity must be between 1 and {}", Self::MAX_CAPACITY),
            ));
        }
        dylos_core::validate_name(prefix, 26).map_err(|reason| invalid(prefix, reason))?;

        let mut pool = Self {
            config: config.clone(),
            snapshot: snapshot.to_path_buf(),
            prefix: prefix.to_string(),
            capacity,
            sequence: 0,
            ready: VecDeque::with_capacity(capacity),
            refilling: JoinSet::new(),
        };

        for _ in 0..capacity {
            pool.spawn_refill()?;
        }
        while pool.ready.len() < pool.capacity {
            if let Some(res) = pool.refilling.join_next().await {
                let lab = res.map_err(|source| LabError::Task {
                    lab: "pool_refill".into(),
                    source,
                })??;
                pool.ready.push_back(lab);
            }
        }
        tracing::info!(
            capacity = pool.capacity,
            ready = pool.ready.len(),
            "restore pool ready"
        );
        Ok(pool)
    }

    fn spawn_refill(&mut self) -> Result<(), LabError> {
        self.sequence += 1;
        let id = format!("{}-p{}", self.prefix, self.sequence);
        dylos_core::validate_name(&id, 32).map_err(|reason| invalid(&id, reason))?;
        let (config, snapshot) = (self.config.clone(), self.snapshot.clone());
        self.refilling
            .spawn(async move { Lab::restore_paused_with(&config, &snapshot, &id).await });
        Ok(())
    }

    fn poll_completed_refills(&mut self) -> Result<(), LabError> {
        while let Some(res) = self.refilling.try_join_next() {
            match res {
                Ok(Ok(lab)) => self.ready.push_back(lab),
                Ok(Err(err)) => return Err(err),
                Err(source) => {
                    return Err(LabError::Task {
                        lab: "pool_refill".into(),
                        source,
                    });
                }
            }
        }
        Ok(())
    }

    /// Hands out one clone from the pool, activating it immediately.
    ///
    /// # Errors
    /// Returns an error if activation or background refill fails.
    pub async fn take(&mut self) -> Result<Lab, LabError> {
        self.poll_completed_refills()?;
        let mut lab = if let Some(lab) = self.ready.pop_front() {
            lab
        } else if !self.refilling.is_empty() {
            let res = self.refilling.join_next().await.ok_or_else(|| {
                invalid(
                    &self.prefix,
                    "no pre-warmed clones available and no refill in progress",
                )
            })?;
            res.map_err(|source| LabError::Task {
                lab: "pool_refill".into(),
                source,
            })??
        } else {
            self.sequence += 1;
            let id = format!("{}-p{}", self.prefix, self.sequence);
            Lab::restore_paused_with(&self.config, &self.snapshot, &id).await?
        };

        let started = Instant::now();
        if let Err(err) = lab.activate(&self.config).await {
            let _ = lab.teardown().await;
            return Err(err);
        }
        tracing::info!(lab = %lab.id(), handout_ms = started.elapsed().as_millis(), "clone handed out");
        if self.ready.len() + self.refilling.len() < self.capacity {
            self.spawn_refill()?;
        }
        Ok(lab)
    }

    /// Number of pre-restored paused clones currently ready in the pool.
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }

    /// Target capacity of the pool.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Gracefully tears down all ready paused clones and cancels in-flight refills.
    ///
    /// # Errors
    /// Returns the first error encountered during teardown, if any.
    pub async fn teardown(&mut self) -> Result<(), LabError> {
        self.refilling.abort_all();
        while let Some(res) = self.refilling.join_next().await {
            if let Ok(Ok(mut lab)) = res {
                let _ = lab.teardown().await;
            }
        }
        let mut first_error = None;
        while let Some(mut lab) = self.ready.pop_front() {
            if let Err(e) = lab.teardown().await {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}
