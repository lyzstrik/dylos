use std::path::{Path, PathBuf};
use std::time::Instant;

use futures_util::TryStreamExt;
use rtnetlink::{Handle, LinkBridge, LinkUnspec};
use tracing::{Instrument, info, info_span, warn};

use crate::netns::{self, io_err, on_fresh_thread, on_fresh_thread_blocking};
use crate::{Error, FabricPlan, tap};

/// A lab's netns with its bridges and TAPs, torn down on drop if [`LabNetwork::teardown`] was
/// not called.
#[derive(Debug)]
pub struct LabNetwork {
    plan: FabricPlan,
    netns_path: PathBuf,
    live: bool,
}

impl LabNetwork {
    /// Creates the netns `<netns_dir>/dylos-<lab id>`, then one bridge per segment and one
    /// persistent TAP per VM interface inside it, all up.
    ///
    /// On failure everything created so far is removed before returning, except when the netns
    /// already existed: it may belong to a running lab, so it is left untouched.
    ///
    /// # Errors
    ///
    /// [`Error::NetnsExists`], or the system or netlink call that failed.
    pub async fn create(plan: FabricPlan, netns_dir: &Path) -> Result<Self, Error> {
        let span = info_span!("lab_network", lab = plan.lab_id());
        let netns_path = netns_dir.join(plan.netns_name());
        let started = Instant::now();
        let (p, path) = (plan.clone(), netns_path.clone());
        let created = on_fresh_thread(plan.lab_id(), move || create_blocking(&p, &path))
            .instrument(span.clone())
            .await;
        match created {
            Ok(()) => {
                let elapsed_ms = started.elapsed().as_millis();
                span.in_scope(|| info!(elapsed_ms, "lab network created"));
                Ok(Self {
                    plan,
                    netns_path,
                    live: true,
                })
            }
            Err(e @ Error::NetnsExists { .. }) => Err(e),
            Err(e) => {
                if let Err(rollback) = teardown(&plan, netns_dir).instrument(span.clone()).await {
                    span.in_scope(|| warn!(error = %rollback, "rollback after failed create"));
                }
                Err(e)
            }
        }
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

    /// Same as [`teardown`]; safe to call again.
    ///
    /// # Errors
    ///
    /// The system or netlink call that failed; the network is then still considered live.
    pub async fn teardown(&mut self) -> Result<(), Error> {
        teardown_at(&self.plan, &self.netns_path).await?;
        self.live = false;
        Ok(())
    }
}

impl Drop for LabNetwork {
    // Blocks for the few milliseconds teardown takes: a deterministic cleanup is worth more here
    // than not blocking, and this path only runs when the owner forgot `teardown` or unwound.
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let (plan, path) = (self.plan.clone(), self.netns_path.clone());
        let lab = plan.lab_id().to_owned();
        if let Err(error) = on_fresh_thread_blocking(&lab, move || teardown_blocking(&plan, &path))
        {
            warn!(lab, %error, "lab network teardown on drop failed");
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
    teardown_at(plan, &netns_dir.join(plan.netns_name())).await
}

async fn teardown_at(plan: &FabricPlan, netns_path: &Path) -> Result<(), Error> {
    let (p, path) = (plan.clone(), netns_path.to_owned());
    let started = Instant::now();
    on_fresh_thread(plan.lab_id(), move || teardown_blocking(&p, &path)).await?;
    info!(
        lab = plan.lab_id(),
        elapsed_ms = started.elapsed().as_millis(),
        "lab network removed"
    );
    Ok(())
}

fn create_blocking(plan: &FabricPlan, path: &Path) -> Result<(), Error> {
    let lab = plan.lab_id();
    netns::create_and_enter(lab, path)?;
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
            tap::create_persistent(&tap.name).map_err(io_err(
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
    })
}

fn teardown_blocking(plan: &FabricPlan, path: &Path) -> Result<(), Error> {
    let lab = plan.lab_id();
    if netns::enter(lab, path)? {
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
    netns::remove(lab, path)
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
