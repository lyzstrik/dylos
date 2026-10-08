use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures_util::TryStreamExt;
use rtnetlink::{Handle, LinkBridge, LinkUnspec};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tracing::{Instrument, info, info_span, warn};

use crate::netns::{self, blocking, io_err, on_fresh_thread, on_fresh_thread_blocking};
use crate::{Error, FabricPlan, tap};

/// A lab's netns with its bridges and TAPs, torn down on drop if [`LabNetwork::teardown`] was
/// not called.
#[derive(Debug)]
pub struct LabNetwork {
    plan: FabricPlan,
    netns_path: PathBuf,
    /// The netns this handle created, held open until torn down. `None` once torn down.
    ns: Option<Arc<File>>,
    fabric_state: Arc<Mutex<crate::freeze::State>>,
}

impl LabNetwork {
    /// Creates the netns `<netns_dir>/dylos-<lab id>`, then one bridge per segment and one
    /// persistent TAP per VM interface inside it, all up and owned by the jailer uid/gid.
    ///
    /// On failure everything created so far is removed before returning, except when the netns
    /// already existed: it may belong to a running lab, so it is left untouched. Cancel-safe:
    /// the worker owns both the rollback and the result, so if this future is dropped the
    /// abandoned network is torn down as if it had been dropped.
    ///
    /// # Errors
    ///
    /// [`Error::NetnsExists`], or the system or netlink call that failed.
    pub async fn create(
        plan: FabricPlan,
        netns_dir: &Path,
        user_id: u32,
        group_id: u32,
    ) -> Result<Self, Error> {
        let span = info_span!("lab_network", lab = plan.lab_id());
        let netns_path = netns_dir.join(plan.netns_name());
        let started = Instant::now();
        let lab = plan.lab_id().to_owned();
        let net = blocking(&lab, move || {
            let ns = create_blocking(&plan, &netns_path, user_id, group_id)?;
            Ok(Self {
                plan,
                netns_path,
                ns: Some(Arc::new(ns)),
                fabric_state: Arc::default(),
            })
        })
        .instrument(span.clone())
        .await?;
        let elapsed_ms = started.elapsed().as_millis();
        span.in_scope(|| info!(elapsed_ms, "lab network created"));
        Ok(net)
    }

    #[must_use]
    pub fn plan(&self) -> &FabricPlan {
        &self.plan
    }

    /// Path of the bind-mounted netns, to hand to `setns` or to the jailer's `--netns`.
    #[must_use]
    pub fn netns_path(&self) -> &Path {
        &self.netns_path
    }

    /// Rejects frames sent after completion; pre-existing TAP reader queues remain readable.
    /// See the crate's freeze limitations.
    /// Idempotent. A cancelled call still completes on its namespace worker.
    ///
    /// # Errors
    /// Reports every failed interface and any failed rollback. Retry `thaw` to recover.
    pub async fn freeze(&mut self) -> Result<(), Error> {
        self.transition(true).await
    }

    /// Restores pre-freeze TAP administrative states; also safe before freeze.
    ///
    /// # Errors
    /// Reports failed interfaces and rollback failures; retains recovery state for a retry.
    pub async fn thaw(&mut self) -> Result<(), Error> {
        self.transition(false).await
    }

    async fn transition(&self, frozen: bool) -> Result<(), Error> {
        let ns = self.ns.clone().ok_or_else(|| Error::NetworkRemoved {
            lab: self.plan.lab_id().to_owned(),
        })?;
        let (plan, path, state) = (
            self.plan.clone(),
            self.netns_path.clone(),
            self.fabric_state.clone(),
        );
        let span = info_span!(
            "fabric_step",
            lab = plan.lab_id(),
            step = if frozen { "freeze" } else { "thaw" }
        );
        // Acquire before dispatch: cancellation cannot let a subsequent thaw overtake freeze.
        let mut state = state.lock_owned().await;
        on_fresh_thread(self.plan.lab_id(), move || {
            let started = Instant::now();
            let result = span.in_scope(|| {
                if !netns::enter_file(plan.lab_id(), &ns, &path)? {
                    return Err(Error::NetworkRemoved {
                        lab: plan.lab_id().to_owned(),
                    });
                }
                with_netlink(plan.lab_id(), async |handle| {
                    crate::freeze::transition(&plan, handle, &mut state, frozen).await
                })
            });
            span.in_scope(|| {
                info!(
                    elapsed_us = started.elapsed().as_micros(),
                    success = result.is_ok(),
                    "fabric step completed"
                );
            });
            result
        })
        .await
    }

    /// Same as [`teardown`], restricted to the netns this handle created: if the path was
    /// already torn down and now pins another netns (a new lab with the same id), that one is
    /// left alone. Safe to call again.
    ///
    /// # Errors
    ///
    /// The system or netlink call that failed; the network is then still considered live.
    pub async fn teardown(&mut self) -> Result<(), Error> {
        let Some(ns) = self.ns.clone() else {
            return Ok(());
        };
        let state = self.fabric_state.clone().lock_owned().await;
        teardown_at(&self.plan, &self.netns_path, Some(ns), Some(state)).await?;
        self.ns = None;
        Ok(())
    }
}

impl Drop for LabNetwork {
    // Never joins: a drop can happen on a tokio worker (a forgotten handle, a cancelled
    // `create`), where waiting for netlink would block the executor. The cleanup thread is
    // detached, so it is lost if the process exits first; explicit `teardown` is the normal path.
    fn drop(&mut self) {
        let Some(ns) = self.ns.take() else {
            return;
        };
        let (plan, path) = (self.plan.clone(), self.netns_path.clone());
        let state = self.fabric_state.clone();
        let spawned = std::thread::Builder::new()
            .name("dylos-netns".into())
            .spawn(move || {
                let _state = state.blocking_lock();
                if let Err(error) = teardown_blocking(&plan, &path, Some(&ns)) {
                    warn!(lab = plan.lab_id(), %error, "lab network teardown on drop failed");
                }
            });
        if let Err(error) = spawned {
            warn!(lab = self.plan.lab_id(), %error, "lab network teardown on drop not started");
        }
    }
}

/// Removes every TAP and bridge of `plan`, then the netns itself.
///
/// Idempotent: succeeds when nothing exists, when only part of the fabric was created, and when
/// called twice. Devices are deleted explicitly rather than left to netns destruction, because a
/// leaked process (a Firecracker that did not exit) would otherwise keep them alive.
///
/// # Errors
///
/// The system or netlink call that failed.
pub async fn teardown(plan: &FabricPlan, netns_dir: &Path) -> Result<(), Error> {
    teardown_at(plan, &netns_dir.join(plan.netns_name()), None, None).await
}

async fn teardown_at(
    plan: &FabricPlan,
    netns_path: &Path,
    owned: Option<Arc<File>>,
    state: Option<OwnedMutexGuard<crate::freeze::State>>,
) -> Result<(), Error> {
    let (p, path) = (plan.clone(), netns_path.to_owned());
    let started = Instant::now();
    on_fresh_thread(plan.lab_id(), move || {
        let _state = state;
        teardown_blocking(&p, &path, owned.as_deref())
    })
    .await?;
    info!(
        lab = plan.lab_id(),
        elapsed_ms = started.elapsed().as_millis(),
        "lab network removed"
    );
    Ok(())
}

/// Builds the fabric on a fresh thread, which moves into the new netns; on failure, rolls back
/// before returning.
fn create_blocking(
    plan: &FabricPlan,
    path: &Path,
    user_id: u32,
    group_id: u32,
) -> Result<File, Error> {
    let lab = plan.lab_id();
    let created = on_fresh_thread_blocking(lab, {
        let (plan, path) = (plan.clone(), path.to_owned());
        move || create_fabric(&plan, &path, user_id, group_id)
    });
    match created {
        Err(e) if !matches!(e, Error::NetnsExists { .. }) => {
            let rollback = {
                let (plan, path) = (plan.clone(), path.to_owned());
                move || teardown_blocking(&plan, &path, None)
            };
            if let Err(error) = on_fresh_thread_blocking(lab, rollback) {
                warn!(lab, %error, "rollback after failed create");
            }
            Err(e)
        }
        other => other,
    }
}

fn create_fabric(
    plan: &FabricPlan,
    path: &Path,
    user_id: u32,
    group_id: u32,
) -> Result<File, Error> {
    let lab = plan.lab_id();
    let ns = netns::create_and_enter(lab, path)?;
    with_netlink(lab, async |handle| {
        for bridge in plan.bridges() {
            // ADR-0002: a restored bridge has forgotten the guests' multicast memberships, so
            // snooping would starve IPv6 neighbor discovery; flood multicast instead.
            let msg = LinkBridge::new(&bridge.name)
                .mcast_snooping(false)
                .up()
                .build();
            let res = handle.link().add(msg).execute().await;
            res.map_err(nl_err(lab, "create bridge", &bridge.name))?;
        }
        for tap in plan.taps() {
            tap::create_persistent(&tap.name, user_id, group_id).map_err(io_err(
                lab,
                "create tap",
                Path::new(&tap.name),
            ))?;
            let index = require_index(lab, handle, &tap.name).await?;
            let bridge = require_index(lab, handle, &tap.bridge).await?;
            let msg = LinkUnspec::new_with_index(index)
                .controller(bridge)
                .up()
                .build();
            let res = handle.link().change(msg).execute().await;
            res.map_err(nl_err(lab, "attach tap", &tap.name))?;
        }
        Ok(())
    })?;
    Ok(ns)
}

/// With `owned`, deletes the devices in that netns, and unpins `path` only if it still pins it.
fn teardown_blocking(plan: &FabricPlan, path: &Path, owned: Option<&File>) -> Result<(), Error> {
    let lab = plan.lab_id();
    let entered = match owned {
        Some(ns) => netns::enter_file(lab, ns, path)?,
        None => netns::enter(lab, path)?,
    };
    if entered {
        with_netlink(lab, async |handle| {
            let taps = plan.taps().iter().map(|t| &t.name);
            for name in taps.chain(plan.bridges().iter().map(|b| &b.name)) {
                if let Some(index) = link_index(handle, name)
                    .await
                    .map_err(nl_err(lab, "find", name))?
                {
                    let res = handle.link().del(index).execute().await;
                    res.map_err(nl_err(lab, "delete", name))?;
                }
            }
            Ok(())
        })?;
    }
    match owned {
        Some(ns) if !netns::pins(path, ns).map_err(io_err(lab, "stat netns", path))? => Ok(()),
        _ => netns::remove(lab, path),
    }
}

/// Runs `f` with a netlink socket opened in the calling thread's netns, on a runtime private to
/// this thread: the socket stays bound to the netns it was opened in.
fn with_netlink<T>(
    lab: &str,
    f: impl AsyncFnOnce(&Handle) -> Result<T, Error>,
) -> Result<T, Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(io_err(lab, "build netlink runtime", Path::new("")))?;
    runtime.block_on(async {
        let (connection, handle, _) = rtnetlink::new_connection().map_err(io_err(
            lab,
            "open netlink socket",
            Path::new(""),
        ))?;
        tokio::spawn(connection);
        f(&handle).await
    })
}

async fn link_index(handle: &Handle, name: &str) -> Result<Option<u32>, rtnetlink::Error> {
    match handle
        .link()
        .get()
        .match_name(name.to_owned())
        .execute()
        .try_next()
        .await
    {
        Ok(msg) => Ok(msg.map(|m| m.header.index)),
        Err(rtnetlink::Error::NetlinkError(e)) if e.raw_code() == -libc::ENODEV => Ok(None),
        Err(e) => Err(e),
    }
}

async fn require_index(lab: &str, handle: &Handle, name: &str) -> Result<u32, Error> {
    link_index(handle, name)
        .await
        .map_err(nl_err(lab, "find", name))?
        .ok_or_else(|| Error::Netlink {
            lab: lab.to_owned(),
            op: "find",
            link: name.to_owned(),
            source: Box::new(rtnetlink::Error::RequestFailed),
        })
}

fn nl_err(lab: &str, op: &'static str, link: &str) -> impl FnOnce(rtnetlink::Error) -> Error {
    let (lab, link) = (lab.to_owned(), link.to_owned());
    move |source| Error::Netlink {
        lab,
        op,
        link,
        source: Box::new(source),
    }
}
