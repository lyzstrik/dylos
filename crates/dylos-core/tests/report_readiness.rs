use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn run_report_readiness_basic(failed: bool) -> Result<std::process::Output, Box<dyn Error>> {
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

fn setup_fake_ping(bin_dir: &Path, log_path: &Path) -> Result<(), Box<dyn Error>> {
    let ping_path = bin_dir.join("ping");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> \"{}\"\nif [ \"$1\" = \"$FAKE_FAIL_FAMILY\" ]; then\n    exit 1\nfi\nexit 0\n",
        log_path.display()
    );
    std::fs::write(&ping_path, script)?;
    std::fs::set_permissions(&ping_path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn run_report_readiness_with_cmdline(
    cmdline_content: &str,
    fail_family: Option<&str>,
) -> Result<(std::process::Output, String), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let marker = dir.path().join("run/dylos-net-failed");
    let cmdline = dir.path().join("cmdline");
    std::fs::write(&cmdline, cmdline_content)?;

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let log_path = dir.path().join("ping.log");
    setup_fake_ping(&bin_dir, &log_path)?;

    let script = std::fs::canonicalize(format!(
        "{}/../../xtask/images/report-readiness",
        env!("CARGO_MANIFEST_DIR")
    ))?;

    let path_var = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut cmd = Command::new("sh");
    cmd.arg(script)
        .env("PATH", path_var)
        .env("DYLOS_NET_MARKER", &marker)
        .env("DYLOS_CMDLINE_FILE", &cmdline)
        .env("DYLOS_GATEWAY_TRIES", "1");

    if let Some(ff) = fail_family {
        cmd.env("FAKE_FAIL_FAMILY", ff);
    }

    let out = cmd.output()?;
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    Ok((out, log))
}

#[test]
fn successful_net_setup_reports_ready() -> Result<(), Box<dyn Error>> {
    let out = run_report_readiness_basic(false)?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dylos: ready"));
    assert!(!stdout.contains("dylos: network setup failed"));
    Ok(())
}

#[test]
fn failed_net_setup_reports_failure() -> Result<(), Box<dyn Error>> {
    let out = run_report_readiness_basic(true)?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dylos: network setup failed"));
    assert!(!stdout.contains("dylos: ready"));
    Ok(())
}

#[test]
fn reachable_gateways_report_ready_and_assert_ipv6_first() -> Result<(), Box<dyn Error>> {
    // Deliberately place IPv4 first on the cmdline to test that report-readiness still probes IPv6 first.
    let cmdline = "dylos.rt=0.0.0.0/0,10.0.1.1 dylos.rt=::/0,fd64::1";
    let (out, log) = run_report_readiness_with_cmdline(cmdline, None)?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dylos: ready"));
    assert!(!stdout.contains("dylos: network setup failed"));

    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "Expected 2 ping calls, got: {log}");
    assert_eq!(lines[0], "-6 -c 1 -W 1 fd64::1");
    assert_eq!(lines[1], "-4 -c 1 -W 1 10.0.1.1");
    Ok(())
}

#[test]
fn unreachable_ipv6_gateway_reports_failure() -> Result<(), Box<dyn Error>> {
    let cmdline = "dylos.rt=::/0,fd64::1 dylos.rt=0.0.0.0/0,10.0.1.1";
    let (out, log) = run_report_readiness_with_cmdline(cmdline, Some("-6"))?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dylos: network setup failed"));
    assert!(!stdout.contains("dylos: ready"));

    let lines: Vec<&str> = log.lines().collect();
    // IPv6 failed, so IPv4 was not probed.
    assert_eq!(lines.len(), 1, "Expected 1 ping call, got: {log}");
    assert_eq!(lines[0], "-6 -c 1 -W 1 fd64::1");
    Ok(())
}

#[test]
fn unreachable_ipv4_gateway_reports_failure() -> Result<(), Box<dyn Error>> {
    let cmdline = "dylos.rt=::/0,fd64::1 dylos.rt=0.0.0.0/0,10.0.1.1";
    let (out, log) = run_report_readiness_with_cmdline(cmdline, Some("-4"))?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("dylos: network setup failed"));
    assert!(!stdout.contains("dylos: ready"));

    let lines: Vec<&str> = log.lines().collect();
    // IPv6 succeeded first, then IPv4 failed.
    assert_eq!(lines.len(), 2, "Expected 2 ping calls, got: {log}");
    assert_eq!(lines[0], "-6 -c 1 -W 1 fd64::1");
    assert_eq!(lines[1], "-4 -c 1 -W 1 10.0.1.1");
    Ok(())
}
