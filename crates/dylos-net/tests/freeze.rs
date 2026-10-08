#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use dylos_core::LabSpec;
use dylos_net::{Error, FabricPlan, LabNetwork};

const TAPS: [&str; 4] = ["tap-A-eth0", "tap-B-eth0", "tap-B-eth1", "tap-C-eth0"];

fn sandboxed(test: &str) -> bool {
    if std::env::var_os("DYLOS_FREEZE_SANDBOX").is_some() {
        return true;
    }
    if !Command::new("unshare")
        .args(["-Urmn", "true"])
        .status()
        .is_ok_and(|s| s.success())
    {
        eprintln!("SKIPPED {test}: unprivileged user namespaces unavailable");
        return false;
    }
    let status = Command::new("unshare")
        .arg("-Urmn")
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env("DYLOS_FREEZE_SANDBOX", "1")
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed in sandbox");
    false
}

fn plan() -> FabricPlan {
    FabricPlan::new(
        "freeze-test",
        &LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap(),
    )
    .unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn ip(path: &Path, args: &[&str]) -> String {
    let out = Command::new("nsenter")
        .arg(format!("--net={}", path.display()))
        .arg("ip")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

struct Endpoints {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl Endpoints {
    fn new(path: &Path) -> Self {
        let mut child = Command::new("nsenter")
            .arg(format!("--net={}", path.display()))
            .args([
                "python3",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/freeze-endpoints.py"
                ),
            ])
            .args(TAPS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut endpoints = Self {
            child,
            input,
            output,
        };
        assert_eq!(endpoints.response(), "ready");
        endpoints
    }

    fn response(&mut self) -> String {
        let mut line = String::new();
        assert!(
            self.output.read_line(&mut line).unwrap() > 0,
            "endpoint exited"
        );
        line.trim().to_owned()
    }

    fn command(&mut self, command: &str) -> String {
        writeln!(self.input.as_mut().unwrap(), "{command}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
        self.response()
    }

    fn ok(&mut self, command: &str) {
        assert_eq!(self.command(command), "ok", "{command}");
    }
}

impl Drop for Endpoints {
    fn drop(&mut self) {
        self.input.take();
        let status = self.child.wait().unwrap();
        assert!(
            status.success() || std::thread::panicking(),
            "endpoint helper failed"
        );
    }
}

#[test]
fn post_freeze_frames_are_discarded_and_traffic_recovers() {
    if !sandboxed("post_freeze_frames_are_discarded_and_traffic_recovers") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    runtime().block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        let path = net.netns_path().to_owned();
        let mut peers = Endpoints::new(&path);
        net.thaw().await.unwrap();
        for version in ["6", "4"] {
            // Cover both bridges, not just one pair of peers.
            for (source, target) in [(TAPS[0], TAPS[1]), (TAPS[2], TAPS[3])] {
                ip(&path, &["link", "set", source, "txqueuelen", "37"]);
                peers.ok(&format!("send {source} {target} {version} before"));
                peers.ok(&format!("read {target} {version} before"));
                // Deliberately leave a frame unread in the live reader queue before freezing.
                peers.ok(&format!("send {source} {target} {version} queued"));
            }
            net.freeze().await.unwrap();
            net.freeze().await.unwrap();
            for tap in TAPS {
                let info = ip(&path, &["-o", "link", "show", tap]);
                assert!(!info.contains(",UP") && !info.contains("<UP,"), "{info}");
            }
            // The revised invariant allows queued-before-T frames to be read while frozen.
            for target in [TAPS[1], TAPS[3]] {
                peers.ok(&format!("read {target} {version} queued"));
            }
            let before = peers.command("stats");
            peers.ok("empty");
            for tap in TAPS {
                peers.ok(&format!("flood {tap} {version}"));
            }
            peers.ok("empty");
            assert_eq!(
                before,
                peers.command("stats"),
                "frozen RX/TX counters moved"
            );
            net.thaw().await.unwrap();
            net.thaw().await.unwrap();
            peers.ok("empty");
            for (source, target) in [(TAPS[0], TAPS[1]), (TAPS[2], TAPS[3])] {
                assert!(ip(&path, &["-o", "link", "show", source]).contains("qlen 37"));
                peers.ok(&format!("send {source} {target} {version} after"));
                peers.ok(&format!("read {target} {version} after"));
            }
        }
        drop(peers);
        net.teardown().await.unwrap();
        net.teardown().await.unwrap();
        assert!(!path.exists());
        assert!(matches!(
            net.freeze().await,
            Err(Error::NetworkRemoved { .. })
        ));
    });
}

#[test]
fn missing_interface_errors_preserve_recovery_and_other_interfaces() {
    if !sandboxed("missing_interface_errors_preserve_recovery_and_other_interfaces") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    runtime().block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        let path = net.netns_path().to_owned();
        net.freeze().await.unwrap();
        ip(&path, &["link", "del", TAPS[0]]);
        let error = net.thaw().await.unwrap_err();
        assert!(matches!(&error, Error::FabricTransition { failures, rollback_failures, .. }
            if failures.iter().any(|e| e.contains(TAPS[0]))
            && rollback_failures.iter().any(|e| e.contains(TAPS[0]))), "{error}");
        for tap in &TAPS[1..] {
            let info = ip(&path, &["-o", "link", "show", tap]);
            assert!(!info.contains(",UP") && !info.contains("<UP,"), "{info}");
        }
        assert!(net.thaw().await.is_err(), "recovery state was forgotten");
        assert!(net.freeze().await.is_err(), "partial state reported as frozen");
        net.teardown().await.unwrap();
        assert!(!path.exists());

        let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        ip(net.netns_path(), &["link", "del", TAPS[0]]);
        assert!(matches!(net.freeze().await, Err(Error::FabricTransition { rollback_failures, .. }) if rollback_failures.is_empty()));
        for tap in &TAPS[1..] {
            let info = ip(net.netns_path(), &["-o", "link", "show", tap]);
            assert!(info.contains(",UP") || info.contains("<UP,"), "{info}");
        }
        net.thaw().await.unwrap();
        net.teardown().await.unwrap();
        assert!(!path.exists());
    });
}

#[test]
fn cancelled_freeze_cannot_overtake_thaw_and_original_down_state_is_preserved() {
    if !sandboxed("cancelled_freeze_cannot_overtake_thaw_and_original_down_state_is_preserved") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    runtime().block_on(async {
        let mut net = LabNetwork::create(plan(), dir.path(), 0, 0).await.unwrap();
        let path = net.netns_path().to_owned();
        ip(&path, &["link", "set", TAPS[0], "down"]);
        let mut freezing = Box::pin(net.freeze());
        assert!(futures_util::poll!(&mut freezing).is_pending());
        drop(freezing);
        net.thaw().await.unwrap();
        let info = ip(&path, &["-o", "link", "show", TAPS[0]]);
        assert!(!info.contains(",UP") && !info.contains("<UP,"), "{info}");
        for tap in &TAPS[1..] {
            let info = ip(&path, &["-o", "link", "show", tap]);
            assert!(info.contains(",UP") || info.contains("<UP,"), "{info}");
        }
        net.teardown().await.unwrap();
        assert!(!path.exists());
    });
}
