use futures_util::{TryStreamExt, future::join_all};
use rtnetlink::packet_route::link::LinkFlags;
use rtnetlink::{Handle, LinkUnspec};

use crate::{Error, FabricPlan};

#[derive(Debug)]
struct SavedTap {
    name: String,
    index: u32,
    up: bool,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    saved: Option<Vec<SavedTap>>,
    frozen: bool,
}

pub(crate) async fn transition(
    plan: &FabricPlan,
    handle: &Handle,
    state: &mut State,
    freeze: bool,
) -> Result<(), Error> {
    let step = if freeze { "freeze" } else { "thaw" };
    let error = |failures, rollback_failures| Error::FabricTransition {
        lab: plan.lab_id().to_owned(),
        step,
        failures,
        rollback_failures,
    };
    if freeze && state.frozen || !freeze && state.saved.is_none() {
        return Ok(());
    }
    if freeze && state.saved.is_some() {
        return Err(error(
            vec!["incomplete rollback: retry thaw first".into()],
            vec![],
        ));
    }
    if freeze {
        // Preflight every interface before mutating any; join_all waits for all outcomes.
        let snapshots = join_all(plan.taps().iter().map(|tap| async {
            let msg = handle
                .link()
                .get()
                .match_name(tap.name.clone())
                .execute()
                .try_next()
                .await
                .map_err(|e| format!("{}: inspect: {e}", tap.name))?
                .ok_or_else(|| format!("{}: interface missing", tap.name))?;
            Ok(SavedTap {
                name: tap.name.clone(),
                index: msg.header.index,
                up: msg.header.flags.contains(LinkFlags::Up),
            })
        }))
        .await;
        let mut saved = Vec::new();
        let mut failures = Vec::new();
        for snapshot in snapshots {
            match snapshot {
                Ok(tap) => saved.push(tap),
                Err(e) => failures.push(e),
            }
        }
        if !failures.is_empty() {
            return Err(error(failures, vec![]));
        }
        state.saved = Some(saved);
    }
    complete(state, freeze, async |saved, frozen| {
        apply(handle, saved, frozen).await
    })
    .await
    .map_err(|(failures, rollback_failures)| error(failures, rollback_failures))
}

async fn complete(
    state: &mut State,
    freeze: bool,
    operation: impl AsyncFn(&[SavedTap], bool) -> Vec<String>,
) -> Result<(), (Vec<String>, Vec<String>)> {
    let Some(saved) = state.saved.as_ref() else {
        return Ok(());
    };
    let failures = operation(saved, freeze).await;
    if failures.is_empty() {
        state.frozen = freeze;
        if !freeze {
            state.saved = None;
        }
        return Ok(());
    }
    // Failed freeze restores the original state; failed thaw returns to a frozen fabric.
    // Retain the original settings if recovery is incomplete so a later thaw can retry.
    let rollback_failures = operation(saved, !freeze).await;
    state.frozen = !freeze && rollback_failures.is_empty();
    if freeze && rollback_failures.is_empty() {
        state.saved = None;
    }
    Err((failures, rollback_failures))
}

async fn apply(handle: &Handle, taps: &[SavedTap], freeze: bool) -> Vec<String> {
    join_all(taps.iter().map(|tap| async move {
        let builder = LinkUnspec::new_with_index(tap.index);
        let builder = if !freeze && tap.up {
            builder.up()
        } else {
            builder.down()
        };
        // tun_get_user rejects writes without IFF_UP with EIO, before netif_receive_skb.
        // tun_net_close stops transmission but leaves previously queued reader frames intact.
        // Linux v6.12 drivers/net/tun.c; the sandbox checks both properties on the host kernel.
        handle
            .link()
            .change(builder.build())
            .execute()
            .await
            .map_err(|e| {
                format!(
                    "{}: set {}: {e}",
                    tap.name,
                    if freeze { "down" } else { "original state" }
                )
            })
    }))
    .await
    .into_iter()
    .filter_map(Result::err)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{SavedTap, State, complete};
    use std::cell::RefCell;

    #[tokio::test]
    async fn partial_freeze_restores_or_retains_precise_recovery_state() {
        for rollback_fails in [false, true] {
            let mut state = State {
                saved: Some(vec![SavedTap {
                    name: "tap-A-eth0".into(),
                    index: 1,
                    up: true,
                }]),
                frozen: false,
            };
            let calls = RefCell::new(Vec::new());
            let outcome = complete(&mut state, true, async |saved, freeze| {
                calls.borrow_mut().push(freeze);
                assert_eq!(saved[0].name, "tap-A-eth0");
                if freeze || rollback_fails {
                    vec!["tap-A-eth0: injected netlink failure".into()]
                } else {
                    vec![]
                }
            })
            .await;
            assert_eq!(*calls.borrow(), [true, false]);
            assert!(matches!(outcome, Err((failures, rollback))
                if failures.len() == 1 && rollback.len() == usize::from(rollback_fails)));
            assert_eq!(state.saved.is_some(), rollback_fails);
            assert!(!state.frozen);
            if rollback_fails {
                let retry = complete(&mut state, false, async |_, _| vec![]).await;
                assert!(retry.is_ok());
                assert!(state.saved.is_none());
            }
        }
    }
}
