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

#[test]
fn test_lab_down_rejects_traversal_and_symlinks_without_outside_request() {
    let dylos = env!("CARGO_BIN_EXE_dylos");
    let outside_dir = tempfile::tempdir().expect("failed to create outside tempdir");
    let store_dir = tempfile::tempdir().expect("failed to create store tempdir");

    let outside_socket_path = outside_dir.path().join("outside.sock");
    let listener = std::os::unix::net::UnixListener::bind(&outside_socket_path)
        .expect("failed to bind outside listener");
    listener
        .set_nonblocking(true)
        .expect("failed to set nonblocking");

    let assert_no_outside_request = || {
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "outside socket should not receive connections"
        );
    };

    // 1. Absolute path as lab ID
    let output = Command::new(dylos)
        .args(["lab", "down", outside_socket_path.to_str().unwrap()])
        .env("DYLOS_LAB_DIR", store_dir.path())
        .output()
        .expect("failed to run dylos lab down with absolute path");
    assert!(!output.status.success());
    assert_no_outside_request();

    // 2. Traversal path as lab ID
    let output = Command::new(dylos)
        .args(["lab", "down", "../outside-lab"])
        .env("DYLOS_LAB_DIR", store_dir.path())
        .output()
        .expect("failed to run dylos lab down with traversal path");
    assert!(!output.status.success());
    assert_no_outside_request();

    // 3. Lab directory is a symlink pointing outside the store
    let symlink_lab = store_dir.path().join("symlink-lab");
    std::os::unix::fs::symlink(outside_dir.path(), &symlink_lab)
        .expect("failed to create symlink lab dir");
    std::os::unix::fs::symlink(
        &outside_socket_path,
        outside_dir.path().join("control.sock"),
    )
    .expect("failed to create symlink control.sock in outside dir");

    let output = Command::new(dylos)
        .args(["lab", "down", "symlink-lab"])
        .env("DYLOS_LAB_DIR", store_dir.path())
        .output()
        .expect("failed to run dylos lab down with symlink lab dir");
    assert!(!output.status.success());
    assert_no_outside_request();

    // 4. Lab directory is real, but control.sock is a symlink pointing outside
    let real_lab_dir = store_dir.path().join("real-lab");
    std::fs::create_dir(&real_lab_dir).expect("failed to create real lab dir");
    std::os::unix::fs::symlink(&outside_socket_path, real_lab_dir.join("control.sock"))
        .expect("failed to create symlink control.sock in real lab dir");

    let output = Command::new(dylos)
        .args(["lab", "down", "real-lab"])
        .env("DYLOS_LAB_DIR", store_dir.path())
        .output()
        .expect("failed to run dylos lab down with symlink control.sock");
    assert!(!output.status.success());
    assert_no_outside_request();

    // 5. Control socket is a regular file, not a socket
    let file_lab_dir = store_dir.path().join("file-lab");
    std::fs::create_dir(&file_lab_dir).expect("failed to create file lab dir");
    std::fs::write(file_lab_dir.join("control.sock"), b"regular file")
        .expect("failed to write regular control file");

    let output = Command::new(dylos)
        .args(["lab", "down", "file-lab"])
        .env("DYLOS_LAB_DIR", store_dir.path())
        .output()
        .expect("failed to run dylos lab down with regular control file");
    assert!(!output.status.success());
    assert_no_outside_request();
}

#[test]
fn test_lab_down_with_fake_live_supervisor_success() {
    use std::io::{Read, Write};

    let dylos = env!("CARGO_BIN_EXE_dylos");
    let store_dir = tempfile::tempdir().expect("failed to create store tempdir");
    let lab_dir = store_dir.path().join("live-lab");
    std::fs::create_dir(&lab_dir).expect("failed to create lab dir");
    let socket_path = lab_dir.join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .expect("failed to bind control socket");

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("failed to accept connection");
        let mut buf = [0u8; 9];
        stream.read_exact(&mut buf).expect("failed to read request");
        assert_eq!(&buf, b"teardown\n");
        stream.write_all(b"ok\n").expect("failed to write response");
    });

    let output = Command::new(dylos)
        .args(["lab", "down", "live-lab"])
        .env("DYLOS_LAB_DIR", store_dir.path())
        .output()
        .expect("failed to execute dylos lab down");
    server.join().expect("server thread panicked");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lab live-lab is down"));
}

#[test]
fn test_lab_down_with_fake_live_supervisor_timeout() {
    use std::io::Read;

    let dylos = env!("CARGO_BIN_EXE_dylos");
    let store_dir = tempfile::tempdir().expect("failed to create store tempdir");
    let lab_dir = store_dir.path().join("hanging-lab");
    std::fs::create_dir(&lab_dir).expect("failed to create lab dir");
    let socket_path = lab_dir.join("control.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .expect("failed to bind control socket");

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("failed to accept connection");
        let mut buf = [0u8; 9];
        let _ = stream.read_exact(&mut buf);
        std::thread::sleep(std::time::Duration::from_millis(500));
    });

    let output = Command::new(dylos)
        .args(["lab", "down", "hanging-lab"])
        .env("DYLOS_LAB_DIR", store_dir.path())
        .env("DYLOS_TEARDOWN_TIMEOUT_MS", "100")
        .output()
        .expect("failed to execute dylos lab down");
    server.join().expect("server thread panicked");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed to request teardown for lab 'hanging-lab'"));
}

#[test]
fn test_lab_up_handles_signal_during_bring_up() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::process::ExitStatusExt;

    let dylos = env!("CARGO_BIN_EXE_dylos");
    let temp_dir = tempfile::tempdir().expect("failed to create tempdir");
    let spec_path = temp_dir.path().join("spec.yaml");
    std::fs::write(
        &spec_path,
        r"nodes:
  - name: node-1
    image: rootfs
    vcpus: 1
    memory: 128
    interfaces: []
segments: []
",
    )
    .expect("failed to write spec");

    // Test SIGINT
    let mut child = Command::new(dylos)
        .args([
            "lab",
            "up",
            spec_path.to_str().unwrap(),
            "--id",
            "sigint-lab",
        ])
        .env("DYLOS_LAB_DIR", temp_dir.path())
        .env("RUST_LOG", "info")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn dylos lab up");

    let pid = Pid::from_raw(i32::try_from(child.id()).expect("child pid fits in i32"));
    let mut stderr_reader = BufReader::new(child.stderr.take().expect("failed to take stderr"));
    let mut initial_line = String::new();
    while let Ok(n) = stderr_reader.read_line(&mut initial_line) {
        if n == 0 || initial_line.contains("registered termination signal handlers") {
            break;
        }
        initial_line.clear();
    }

    let _ = kill(pid, Signal::SIGINT);
    let status = child.wait().expect("failed to wait for child");
    let mut remaining = Vec::new();
    let _ = stderr_reader.read_to_end(&mut remaining);
    let stderr = format!("{initial_line}{}", String::from_utf8_lossy(&remaining));

    assert!(
        status.signal().is_none(),
        "process was killed by unhandled signal instead of handling SIGINT cleanly"
    );
    assert!(
        stderr.contains("SIGINT") || stderr.contains("Ctrl+C"),
        "stderr should log SIGINT handling: {stderr}"
    );

    // Test SIGTERM
    let mut child = Command::new(dylos)
        .args([
            "lab",
            "up",
            spec_path.to_str().unwrap(),
            "--id",
            "sigterm-lab",
        ])
        .env("DYLOS_LAB_DIR", temp_dir.path())
        .env("RUST_LOG", "info")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn dylos lab up");

    let pid = Pid::from_raw(i32::try_from(child.id()).expect("child pid fits in i32"));
    let mut stderr_reader = BufReader::new(child.stderr.take().expect("failed to take stderr"));
    let mut initial_line = String::new();
    while let Ok(n) = stderr_reader.read_line(&mut initial_line) {
        if n == 0 || initial_line.contains("registered termination signal handlers") {
            break;
        }
        initial_line.clear();
    }

    let _ = kill(pid, Signal::SIGTERM);
    let status = child.wait().expect("failed to wait for child");
    let mut remaining = Vec::new();
    let _ = stderr_reader.read_to_end(&mut remaining);
    let stderr = format!("{initial_line}{}", String::from_utf8_lossy(&remaining));

    assert!(
        status.signal().is_none(),
        "process was killed by unhandled signal instead of handling SIGTERM cleanly"
    );
    assert!(
        stderr.contains("SIGTERM"),
        "stderr should log SIGTERM handling: {stderr}"
    );
}
