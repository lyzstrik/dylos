#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn test_help_and_subcommand_help() {
    let dylos = env!("CARGO_BIN_EXE_dylos");

    // Top-level help
    let output = Command::new(dylos)
        .arg("--help")
        .output()
        .expect("failed to execute dylos --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lab"));
    assert!(stdout.contains("DYLOS_LAB_DIR"));
    assert!(stdout.contains("DYLOS_FC_BIN_DIR"));

    // lab help
    let output = Command::new(dylos)
        .args(["lab", "--help"])
        .output()
        .expect("failed to execute dylos lab --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("up"));
    assert!(stdout.contains("down"));

    // lab up help
    let output = Command::new(dylos)
        .args(["lab", "up", "--help"])
        .output()
        .expect("failed to execute dylos lab up --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("<SPEC>"));
    assert!(stdout.contains("--id"));

    // lab down help
    let output = Command::new(dylos)
        .args(["lab", "down", "--help"])
        .output()
        .expect("failed to execute dylos lab down --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("<ID>"));
}

#[test]
fn test_missing_args_and_unknown_subcommands() {
    let dylos = env!("CARGO_BIN_EXE_dylos");

    // Missing subcommand
    let output = Command::new(dylos)
        .output()
        .expect("failed to execute dylos");
    assert!(!output.status.success());

    // Missing spec argument for lab up
    let output = Command::new(dylos)
        .args(["lab", "up"])
        .output()
        .expect("failed to execute dylos lab up");
    assert!(!output.status.success());

    // Missing id argument for lab down
    let output = Command::new(dylos)
        .args(["lab", "down"])
        .output()
        .expect("failed to execute dylos lab down");
    assert!(!output.status.success());

    // Unknown top-level subcommand
    let output = Command::new(dylos)
        .arg("unknown")
        .output()
        .expect("failed to execute dylos unknown");
    assert!(!output.status.success());

    // Unknown lab subcommand
    let output = Command::new(dylos)
        .args(["lab", "unknown"])
        .output()
        .expect("failed to execute dylos lab unknown");
    assert!(!output.status.success());
}

#[test]
fn test_lab_down_nonexistent_is_idempotent_success() {
    let dylos = env!("CARGO_BIN_EXE_dylos");
    let temp_dir = tempfile::tempdir().expect("failed to create tempdir");

    // First down call on a nonexistent lab
    let output = Command::new(dylos)
        .args(["lab", "down", "nonexistent-lab"])
        .env("DYLOS_LAB_DIR", temp_dir.path())
        .output()
        .expect("failed to execute dylos lab down");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lab nonexistent-lab is down"));

    // Second down call (idempotent, exit 0)
    let output = Command::new(dylos)
        .args(["lab", "down", "nonexistent-lab"])
        .env("DYLOS_LAB_DIR", temp_dir.path())
        .output()
        .expect("failed to execute dylos lab down second time");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lab nonexistent-lab is down"));
}

#[test]
fn test_lab_down_invalid_id_fails() {
    let dylos = env!("CARGO_BIN_EXE_dylos");
    let temp_dir = tempfile::tempdir().expect("failed to create tempdir");

    let output = Command::new(dylos)
        .args(["lab", "down", "invalid/id"])
        .env("DYLOS_LAB_DIR", temp_dir.path())
        .output()
        .expect("failed to execute dylos lab down");
    assert!(!output.status.success());
}

#[test]
fn test_lab_up_missing_spec_file_fails() {
    let dylos = env!("CARGO_BIN_EXE_dylos");

    let output = Command::new(dylos)
        .args(["lab", "up", "nonexistent-spec-file.yaml"])
        .output()
        .expect("failed to execute dylos lab up");
    assert!(!output.status.success());
}
