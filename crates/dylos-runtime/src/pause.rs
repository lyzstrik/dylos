use std::time::{Duration, Instant};

use futures_util::future::join_all;
use tracing::Instrument;

use crate::{LabError, Vm};

/// Pauses all VMs concurrently, returning the first-to-last confirmation skew.
/// The caller must freeze the fabric first when taking a consistent snapshot.
/// # Errors
/// Waits for all responses, then attempts to resume every VM on failure.
/// Transport failures leave the failed VM's state unknown; rollback errors are preserved.
/// The caller must run this future to completion; dropping it does not perform rollback.
pub async fn pause_all(lab_id: &str, vms: &[Vm]) -> Result<Duration, LabError> {
    async {
        let started = Instant::now();
        let results = join_all(
            vms.iter()
                .map(|vm| async { change(lab_id, vm, true).await.map(|()| Instant::now()) }),
        )
        .await;
        let mut first = None;
        let mut last = None;
        let mut failure = None;
        for result in results {
            match result {
                Ok(at) => {
                    first = Some(first.map_or(at, |old: Instant| old.min(at)));
                    last = Some(last.map_or(at, |old: Instant| old.max(at)));
                }
                Err(error) => record(&mut failure, error),
            }
        }
        if let Some(operation) = failure {
            let mut cleanup = None;
            for result in join_all(vms.iter().map(|vm| change(lab_id, vm, false))).await {
                if let Err(error) = result {
                    record(&mut cleanup, error);
                }
            }
            return Err(match cleanup {
                None => operation,
                Some(cleanup) => LabError::Rollback {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                },
            });
        }
        let skew = first.zip(last).map_or(Duration::ZERO, |(a, b)| b - a);
        tracing::info!(
            skew_us = skew.as_micros(),
            confirmed = vms.len(),
            duration_us = started.elapsed().as_micros(),
            "lab pause confirmations"
        );
        Ok(skew)
    }
    .instrument(tracing::info_span!("pause_all", lab = lab_id))
    .await
}

/// Resumes all VMs concurrently and waits for every response.
/// # Errors
/// Reports failures without re-pausing VMs that resumed successfully.
pub async fn resume_all(lab_id: &str, vms: &[Vm]) -> Result<(), LabError> {
    async {
        let started = Instant::now();
        let mut failure = None;
        for result in join_all(vms.iter().map(|vm| change(lab_id, vm, false))).await {
            if let Err(error) = result {
                record(&mut failure, error);
            }
        }
        tracing::info!(
            duration_us = started.elapsed().as_micros(),
            "lab resume completed"
        );
        failure.map_or(Ok(()), Err)
    }
    .instrument(tracing::info_span!("resume_all", lab = lab_id))
    .await
}

async fn change(lab: &str, vm: &Vm, pause: bool) -> Result<(), LabError> {
    async {
        let started = Instant::now();
        let api = vm.client();
        let result = if pause {
            api.pause().await
        } else {
            api.resume().await
        };
        tracing::info!(
            pause,
            duration_us = started.elapsed().as_micros(),
            success = result.is_ok(),
            "VM state change completed"
        );
        result.map_err(|source| LabError::Api {
            lab: lab.into(),
            vm: vm.paths().id.clone(),
            source: Box::new(source),
        })
    }
    .instrument(tracing::info_span!("vm_state", vm = %vm.paths().id))
    .await
}

fn record(failure: &mut Option<LabError>, error: LabError) {
    tracing::error!(%error, "VM state change failed");
    failure.get_or_insert(error);
}
