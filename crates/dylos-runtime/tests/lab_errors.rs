#![allow(clippy::unwrap_used, clippy::panic)]

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use dylos_runtime::{Lab, LabError};

#[test]
fn io_message_includes_lab_operation_path_and_source() {
    let error = LabError::Io {
        lab: "example-lab".into(),
        operation: "open lab directory",
        path: PathBuf::from("/labs/example-lab"),
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied"),
    };
    assert_eq!(
        error.to_string(),
        "lab example-lab: open lab directory \"/labs/example-lab\": access denied"
    );
    assert_eq!(error.source().unwrap().to_string(), "access denied");
}

#[tokio::test]
async fn control_connection_errors_report_lab_id_and_socket_path() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("11");
    let errors = [
        Lab::request_teardown("example-lab", &socket, Duration::from_secs(1))
            .await
            .unwrap_err(),
        Lab::request_snapshot(
            "example-lab",
            &socket,
            Some("snapshot"),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err(),
    ];
    for error in errors {
        let LabError::Io {
            lab,
            operation,
            path,
            source,
        } = &error
        else {
            panic!("expected I/O error, got {error:?}");
        };
        assert_eq!(lab, "example-lab");
        assert_eq!(*operation, "connect control socket");
        assert_eq!(path, &socket);
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            format!("lab example-lab: connect control socket {socket:?}: {source}")
        );
    }
    assert!(!socket.exists());
}
