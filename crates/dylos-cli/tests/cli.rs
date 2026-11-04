#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const DEADLINE: Duration = Duration::from_secs(10);

fn until<T>(mut condition: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(value) = condition() {
            return value;
        }
        assert!(Instant::now() < deadline, "condition timed out");
        std::thread::yield_now();
    }
}

struct Process {
    child: Child,
    logs: tempfile::TempDir,
}

impl Process {
    fn spawn(command: &mut Command) -> Self {
        let logs = tempfile::tempdir().unwrap();
        let child = command
            .stdout(fs::File::create(logs.path().join("stdout")).unwrap())
            .stderr(fs::File::create(logs.path().join("stderr")).unwrap())
            .spawn()
            .unwrap();
        Self { child, logs }
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.logs.path().join("stderr")).unwrap()
    }

    fn finish(&mut self) -> Output {
        let status = until(|| self.child.try_wait().unwrap());
        Output {
            status,
            stdout: fs::read(self.logs.path().join("stdout")).unwrap(),
            stderr: fs::read(self.logs.path().join("stderr")).unwrap(),
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
}

fn command(store: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dylos"));
    command
        .args(args)
        .current_dir(store)
        .env("DYLOS_LAB_DIR", store)
        .env("RUST_LOG", "info");
    command
}

fn run(store: &Path, args: &[&str]) -> Output {
    Process::spawn(&mut command(store, args)).finish()
}

fn assert_status(output: &Output, success: bool, text: &str) {
    assert_eq!(output.status.success(), success, "{output:?}");
    let bytes = if success {
        &output.stdout
    } else {
        &output.stderr
    };
    assert!(String::from_utf8_lossy(bytes).contains(text), "{output:?}");
}

fn listener(path: &Path) -> UnixListener {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(path).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn accept(listener: &UnixListener) -> UnixStream {
    let stream = until(|| match listener.accept() {
        Ok(pair) => Some(Ok(pair)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
        Err(error) => Some(Err(error)),
    })
    .unwrap()
    .0;
    stream.set_read_timeout(Some(DEADLINE)).unwrap();
    stream.set_write_timeout(Some(DEADLINE)).unwrap();
    stream
}

#[test]
fn argument_parsing() {
    let store = tempfile::tempdir().unwrap();
    for (args, success, expected) in [
        (
            vec!["--help"],
            true,
            vec!["lab", "DYLOS_LAB_DIR", "DYLOS_FC_BIN_DIR"],
        ),
        (vec!["lab", "--help"], true, vec!["up", "down"]),
        (vec!["lab", "up", "--help"], true, vec!["<SPEC>", "--id"]),
        (vec!["lab", "down", "--help"], true, vec!["<ID>"]),
        (vec![], false, vec!["Usage:"]),
        (vec!["lab", "up"], false, vec!["<SPEC>"]),
        (vec!["lab", "down"], false, vec!["<ID>"]),
        (vec!["unknown"], false, vec!["unrecognized subcommand"]),
        (
            vec!["lab", "unknown"],
            false,
            vec!["unrecognized subcommand"],
        ),
        (
            vec!["lab", "down", "invalid/id"],
            false,
            vec!["invalid lab id"],
        ),
        (
            vec!["lab", "up", "missing.yaml"],
            false,
            vec!["failed to read lab spec"],
        ),
    ] {
        let output = run(store.path(), &args);
        for text in expected {
            assert_status(&output, success, text);
        }
    }
}

#[test]
fn down_nonexistent_is_idempotent() {
    let store = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        assert_status(
            &run(store.path(), &["lab", "down", "missing"]),
            true,
            "lab missing is down",
        );
    }
    assert_eq!(fs::read_dir(store.path()).unwrap().count(), 0);
}

#[test]
fn down_rejects_escaped_sockets() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    fs::create_dir(&store).unwrap();
    let outside = temp.path().join("outside-lab");
    let socket = outside.join("control.sock");
    let outside_listener = listener(&socket);
    symlink(&outside, store.join("symlink-lab")).unwrap();
    fs::create_dir(store.join("real-lab")).unwrap();
    symlink(&socket, store.join("real-lab/control.sock")).unwrap();
    fs::create_dir(store.join("file-lab")).unwrap();
    fs::write(store.join("file-lab/control.sock"), b"regular file").unwrap();
    let alias = temp.path().join("alias");
    symlink(temp.path(), &alias).unwrap();
    for (base, id) in [
        (store.clone(), outside.to_str().unwrap()),
        (store.clone(), "../outside-lab"),
        (store.clone(), "symlink-lab"),
        (store.clone(), "real-lab"),
        (store.clone(), "file-lab"),
        (alias, "outside-lab"),
    ] {
        assert!(!run(&base, &["lab", "down", id]).status.success());
        assert!(
            matches!(outside_listener.accept(), Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock),
            "escaped socket contacted for {id}"
        );
        assert!(socket.exists());
    }
}

#[test]
fn down_live_supervisor_protocol_and_timeout() {
    for reply in [true, false] {
        let store = tempfile::tempdir().unwrap();
        let listener = listener(&store.path().join("live-lab/control.sock"));
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut stream = accept(&listener);
            let mut request = [0; 9];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"teardown\n");
            if reply {
                stream.write_all(b"ok\n").unwrap();
            } else {
                released.recv_timeout(DEADLINE).unwrap();
            }
        });
        let output = Process::spawn(
            command(store.path(), &["lab", "down", "live-lab"])
                .env("DYLOS_TEARDOWN_TIMEOUT_MS", "100"),
        )
        .finish();
        if !reply {
            release.send(()).unwrap();
        }
        server.join().unwrap();
        assert_status(
            &output,
            reply,
            if reply {
                "lab live-lab is down"
            } else {
                "failed to request teardown for lab 'live-lab'"
            },
        );
    }
}

#[test]
fn signals_before_resource_creation_roll_back_failed_up() {
    check_signal_rollback(false);
}

#[test]
fn signals_are_observed_while_spec_loading_is_blocked() {
    check_signal_rollback(true);
}

fn check_signal_rollback(wait_for_signal: bool) {
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        fs::create_dir(&store).unwrap();
        let spec = temp.path().join("spec.yaml");
        nix::unistd::mkfifo(
            &spec,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        // Hold spec loading at a FIFO so the registered handlers precede any lab resources.
        let mut fifo = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&spec)
            .unwrap();
        let mut process = Process::spawn(&mut command(
            &store,
            &["lab", "up", spec.to_str().unwrap(), "--id", "signal-lab"],
        ));
        until(|| {
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "exited before checkpoint"
            );
            process
                .stderr()
                .contains("registered termination signal handlers")
                .then_some(())
        });
        until(|| {
            fs::read_dir(format!("/proc/{}/fd", process.child.id()))
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| fs::read_link(entry.path()).is_ok_and(|path| path == spec))
                .then_some(())
        });
        assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
        kill(
            Pid::from_raw(i32::try_from(process.child.id()).unwrap()),
            signal,
        )
        .unwrap();
        if wait_for_signal {
            // A registered handler must be serviced even while spec loading is blocked.
            until(|| process.stderr().contains(&signal.to_string()).then_some(()));
            assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
        }
        fifo.write_all(include_bytes!("../../../labs/abc.yaml"))
            .unwrap();
        drop(fifo);
        let output = process.finish();
        assert_eq!(output.status.signal(), None, "{output:?}");
        assert_eq!(
            output.status.code(),
            Some(1),
            "missing image must fail: {output:?}"
        );
        // Immediate release can fail bring-up before the queued signal is observed;
        // signals_are_observed_while_spec_loading_is_blocked proves signal observation.
        if wait_for_signal {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(&signal.to_string()),
                "{output:?}"
            );
        }
        assert_eq!(
            fs::read_dir(&store).unwrap().count(),
            0,
            "failed up leaked storage"
        );
        assert!(!store.join("signal-lab/control.sock").exists());
    }
}

#[test]
fn restore_command_requires_snapshot_and_rejects_invalid_id() {
    let dir = tempfile::tempdir().unwrap();
    assert_status(&run(dir.path(), &["lab", "restore"]), false, "<SNAPSHOT>");
    assert_status(
        &run(
            dir.path(),
            &["lab", "restore", "missing", "--id", "../escape"],
        ),
        false,
        "invalid lab id",
    );
    assert_status(
        &run(dir.path(), &["lab", "restore", "--help"]),
        true,
        "--id",
    );
}
