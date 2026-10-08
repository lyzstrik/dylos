use std::error::Error;
use std::process::Command;

fn run_report_readiness(failed: bool) -> Result<std::process::Output, Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let marker = dir.path().join("run/dylos-net-failed");
    std::fs::create_dir_all(marker.parent().ok_or("no parent")?)?;
    if failed {
        std::fs::write(&marker, "")?;
    }

    let script = std::fs::canonicalize(format!(
        "{}/../../xtask/images/report-readiness",
        env!("CARGO_MANIFEST_DIR")
    ))?;

    let out = Command::new("sh")
        .arg(script)
        .env("DYLOS_NET_MARKER", &marker)
        .output()?;
    Ok(out)
}

#[test]
fn successful_net_setup_reports_ready() -> Result<(), Box<dyn Error>> {
    let out = run_report_readiness(false)?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dylos: ready"));
    assert!(!stdout.contains("dylos: network setup failed"));
    Ok(())
}

#[test]
fn failed_net_setup_reports_failure() -> Result<(), Box<dyn Error>> {
    let out = run_report_readiness(true)?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dylos: network setup failed"));
    assert!(!stdout.contains("dylos: ready"));
    Ok(())
}
