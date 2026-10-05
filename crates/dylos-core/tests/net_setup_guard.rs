use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn run_guard(net_setup_body: &str) -> Result<(std::process::Output, bool), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let fake = dir.path().join("net-setup");
    std::fs::write(&fake, format!("#!/bin/sh\n{net_setup_body}\n"))?;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))?;
    let marker = dir.path().join("run/dylos-net-failed");
    let guard = std::fs::canonicalize(format!(
        "{}/../../xtask/images/net-setup-guard",
        env!("CARGO_MANIFEST_DIR")
    ))?;
    let out = Command::new("sh")
        .arg(guard)
        .env("DYLOS_NET_SETUP", &fake)
        .env("DYLOS_NET_MARKER", &marker)
        .output()?;
    Ok((out, marker.exists()))
}

#[test]
fn failed_net_setup_is_reported_and_marked() -> Result<(), Box<dyn Error>> {
    let (out, marked) = run_guard("exit 3")?;
    assert!(out.status.success(), "init must still reach its shell");
    assert!(String::from_utf8_lossy(&out.stderr).contains("dylos: network setup failed"));
    assert!(marked, "failure marker must exist");
    Ok(())
}

#[test]
fn successful_net_setup_leaves_no_marker() -> Result<(), Box<dyn Error>> {
    let (out, marked) = run_guard("exit 0")?;
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("network setup failed"));
    assert!(!marked);
    Ok(())
}
