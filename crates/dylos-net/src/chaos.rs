use std::path::Path;

use crate::{Error, LabNetwork, netns};

/// Netem settings applied to every TAP egress in one restored clone.
/// A partition is represented by 100 percent loss.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChaosProfile {
    pub delay_ms: u32,
    pub jitter_ms: u32,
    pub loss_percent: u8,
}

impl ChaosProfile {
    /// Parses `delay_ms:jitter_ms:loss_percent`.
    ///
    /// # Errors
    /// Rejects malformed values, loss above 100, and jitter without delay.
    pub fn parse(value: &str) -> Result<Self, String> {
        let values: Vec<_> = value.split(':').collect();
        if values.len() != 3 {
            return Err("expected delay_ms:jitter_ms:loss_percent".into());
        }
        let profile = Self {
            delay_ms: values[0].parse().map_err(|_| "invalid delay")?,
            jitter_ms: values[1].parse().map_err(|_| "invalid jitter")?,
            loss_percent: values[2].parse().map_err(|_| "invalid loss")?,
        };
        profile.validate()?;
        Ok(profile)
    }

    fn validate(self) -> Result<(), String> {
        if self.loss_percent > 100 || (self.delay_ms == 0 && self.jitter_ms != 0) {
            return Err("loss must be <= 100 and jitter requires delay".into());
        }
        Ok(())
    }
}

impl LabNetwork {
    /// Adds netem to fresh TAPs while the fabric is frozen. On partial failure the caller
    /// must tear down this clone before returning it to users; thaw must not follow failure.
    ///
    /// # Errors
    /// Invalid settings, unfrozen/removed network, namespace entry or tc failure.
    pub async fn apply_chaos(&mut self, profile: ChaosProfile) -> Result<(), Error> {
        let lab = self.plan().lab_id().to_owned();
        let failure = |message| Error::FabricTransition {
            lab: lab.clone(),
            step: "chaos",
            failures: vec![message],
            rollback_failures: vec![],
        };
        profile.validate().map_err(failure)?;
        let state = self.fabric_state.clone().lock_owned().await;
        if !state.frozen {
            return Err(failure("chaos requires a frozen fabric".into()));
        }
        let ns = self
            .ns
            .clone()
            .ok_or_else(|| Error::NetworkRemoved { lab: lab.clone() })?;
        let plan = self.plan().clone();
        let path = self.netns_path().to_owned();
        netns::on_fresh_thread(&lab, move || {
            let _state = state;
            if !netns::enter_file(plan.lab_id(), &ns, &path)? {
                return Err(Error::NetworkRemoved {
                    lab: plan.lab_id().into(),
                });
            }
            for tap in plan.taps() {
                let mut command = std::process::Command::new("tc");
                command.args(["qdisc", "add", "dev", &tap.name, "root", "netem"]);
                if profile.delay_ms > 0 {
                    command.args(["delay", &format!("{}ms", profile.delay_ms)]);
                    if profile.jitter_ms > 0 {
                        command.arg(format!("{}ms", profile.jitter_ms));
                    }
                }
                command.args(["loss", &format!("{}%", profile.loss_percent)]);
                let output = command.output().map_err(netns::io_err(
                    plan.lab_id(),
                    "execute tc netem",
                    Path::new(&tap.name),
                ))?;
                if !output.status.success() {
                    return Err(Error::FabricTransition {
                        lab: plan.lab_id().into(),
                        step: "chaos",
                        failures: vec![format!(
                            "{}: tc: {}",
                            tap.name,
                            String::from_utf8_lossy(&output.stderr)
                        )],
                        rollback_failures: vec![],
                    });
                }
            }
            Ok(())
        })
        .await
    }
}
