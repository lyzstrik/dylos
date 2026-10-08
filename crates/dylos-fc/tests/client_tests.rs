//! Implementer tests of the Firecracker transport (LYZ-30).
//!
//! Written together with the implementation, they check the basic contract of
//! `FcClient` against the fake server in `tests/support`: a `204` success, a
//! Firecracker error with its `fault_message`, a missing socket and a non-JSON
//! error body. The broader, criteria-driven suite lives in `acceptance.rs`.

mod support;

use dylos_fc::{Error, FcClient};
use serde_json::json;
use std::error::Error as StdError;
use support::FakeServer;
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn StdError>>;

/// Tests that a successful PUT request with a 204 No Content response works correctly.
/// Catches bugs where the client fails to handle empty success responses or sends malformed requests.
#[tokio::test]
async fn test_client_204_success() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    server.set_reply(hyper::StatusCode::NO_CONTENT, vec![]);

    let client = FcClient::new(&sock);
    let body = json!({"id": "test"});

    let res: Option<serde_json::Value> = client.put("/machine-config", &body).await?;
    assert!(res.is_none());

    let history = server.history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].method, "PUT");
    assert_eq!(history[0].path, "/machine-config");

    let expected_body = serde_json::to_vec(&body).map_err(|e| e.to_string())?;
    assert_eq!(history[0].body, expected_body);

    server.shutdown().await;
    Ok(())
}

/// Tests that a 400 Bad Request with a JSON fault message is correctly parsed.
/// Catches bugs where the client fails to extract the `fault_message` field from API errors.
#[tokio::test]
async fn test_client_400_fault_message() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    server.set_reply(
        hyper::StatusCode::BAD_REQUEST,
        br#"{"fault_message": "Invalid request"}"#.to_vec(),
    );

    let client = FcClient::new(&sock);
    let body = json!({"id": "test"});

    let err_res = client
        .put::<_, serde_json::Value>("/machine-config", &body)
        .await;

    if let Err(Error::Api {
        status,
        fault_message,
        ..
    }) = err_res
    {
        assert_eq!(status, 400);
        assert_eq!(fault_message, "Invalid request");
    } else {
        return Err(format!("Expected Api error, got {err_res:?}").into());
    }

    server.shutdown().await;
    Ok(())
}

/// Tests that connecting to a non-existent socket yields a connect error.
/// Catches bugs where the client fails to map socket connection errors appropriately.
#[tokio::test]
async fn test_client_missing_socket() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("missing.socket");

    let client = FcClient::new(&sock);
    let body = json!({"id": "test"});

    let err_res = client
        .put::<_, serde_json::Value>("/machine-config", &body)
        .await;

    if let Err(Error::Connect { .. }) = err_res {
        // expected
    } else {
        return Err(format!("Expected Connect error, got {err_res:?}").into());
    }

    Ok(())
}

/// Tests that a 500 error with a plain text body is still captured in the error.
/// Catches bugs where the client panics or drops the error context when the body isn't JSON.
#[tokio::test]
async fn test_client_non_json_error_body() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    server.set_reply(
        hyper::StatusCode::INTERNAL_SERVER_ERROR,
        b"Plain text error".to_vec(),
    );

    let client = FcClient::new(&sock);

    let err_res = client.get::<serde_json::Value>("/machine-config").await;

    if let Err(Error::Api {
        status,
        fault_message,
        ..
    }) = err_res
    {
        assert_eq!(status, 500);
        assert_eq!(fault_message, "Plain text error");
    } else {
        return Err(format!("Expected Api error, got {err_res:?}").into());
    }

    server.shutdown().await;
    Ok(())
}

/// Tests oversized chunked responses, exactly-at-limit acceptance, and all fields.
#[tokio::test]
async fn test_client_oversized_body() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;
    let client = FcClient::new(&sock);
    let limit = 1024 * 1024;

    for status in [
        hyper::StatusCode::OK,
        hyper::StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        // Exactly at limit accepted
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        server.set_reply_stream(status, rx);
        tx.send(vec![b'A'; limit / 2]).unwrap();
        tx.send(vec![b'A'; limit / 2]).unwrap();
        drop(tx); // Close stream

        let res = client.get::<serde_json::Value>("/machine-config").await;
        if let Err(Error::ResponseTooLarge { .. }) = res {
            return Err(format!("Expected acceptance exactly at limit, got {res:?}").into());
        }

        // Beyond limit
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        server.set_reply_stream(status, rx);
        tx.send(vec![b'A'; limit]).unwrap();
        tx.send(vec![b'A'; 1]).unwrap(); // one byte over limit
        drop(tx);

        let err_res = client.get::<serde_json::Value>("/machine-config").await;
        if let Err(Error::ResponseTooLarge {
            path,
            method,
            route,
            limit: l,
        }) = err_res
        {
            assert_eq!(path, sock);
            assert_eq!(method, "GET");
            assert_eq!(route, "/machine-config");
            assert_eq!(l, limit);
        } else {
            return Err(format!("Expected ResponseTooLarge error, got {err_res:?}").into());
        }
    }

    server.shutdown().await;
    Ok(())
}

/// Tests that a slow response yields `Error::Timeout` (even if headers were received).
#[tokio::test]
async fn test_client_timeout() -> TestResult {
    use std::time::Duration;

    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    server.set_reply_stream(hyper::StatusCode::OK, rx);

    let client = FcClient::new(&sock).with_timeout(Duration::from_millis(50));

    tx.send(b"{\"partial\":".to_vec()).unwrap();

    let res = tokio::time::timeout(
        Duration::from_secs(2),
        client.get::<serde_json::Value>("/machine-config"),
    )
    .await;

    let err_res = res.unwrap();
    if let Err(Error::Timeout {
        path,
        method,
        route,
    }) = err_res
    {
        assert_eq!(path, sock);
        assert_eq!(method, "GET");
        assert_eq!(route, "/machine-config");
    } else {
        return Err(format!("Expected Timeout error, got {err_res:?}").into());
    }

    server.shutdown().await;
    Ok(())
}

/// Tests that an oversized fault message is truncated cleanly at a character boundary,
/// handling multi-byte JSON and invalid UTF-8 without panicking.
#[tokio::test]
async fn test_client_oversized_fault_message() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    let client = FcClient::new(&sock);

    // 1. Multibyte JSON fault message. '🚀' is 4 bytes.
    // 1024 is divisible by 4. 1 byte padding means the 1024 byte limit falls mid-emoji.
    let mut big_fault = String::new();
    big_fault.push('A');
    for _ in 0..300 {
        big_fault.push('🚀');
    }
    let body = format!(r#"{{"fault_message": "{big_fault}"}}"#);
    server.set_reply(hyper::StatusCode::BAD_REQUEST, body.into_bytes());

    let err_res = client.get::<serde_json::Value>("/machine-config").await;
    if let Err(Error::Api { fault_message, .. }) = err_res {
        assert!(fault_message.len() <= 1024 + 3); // 1024 limit + "..."
        assert!(fault_message.ends_with("..."));
    } else {
        return Err(format!("Expected Api error, got {err_res:?}").into());
    }

    // 2. Non-JSON, invalid UTF-8 body.
    let mut invalid_utf8_body = vec![b'A'; 1023];
    invalid_utf8_body.push(0xFF); // Invalid UTF-8 byte
    invalid_utf8_body.push(b'B');
    invalid_utf8_body.extend(vec![b'C'; 100]); // Beyond limit
    server.set_reply(hyper::StatusCode::BAD_REQUEST, invalid_utf8_body);

    let err_res = client.get::<serde_json::Value>("/machine-config").await;
    if let Err(Error::Api { fault_message, .. }) = err_res {
        assert!(fault_message.len() <= 1024 + 3);
        assert!(fault_message.ends_with("..."));
    } else {
        return Err(format!("Expected Api error for invalid UTF-8, got {err_res:?}").into());
    }

    server.shutdown().await;
    Ok(())
}

/// Tests snapshot timeout configuration
#[tokio::test]
async fn test_snapshot_timeout() -> TestResult {
    use dylos_fc::snapshot::SnapshotCreateParams;
    use std::time::Duration;
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    server.set_reply_with_delay(hyper::StatusCode::NO_CONTENT, vec![], notify.clone());

    let client = FcClient::new(&sock)
        .with_timeout(Duration::from_millis(10))
        .with_snapshot_timeout(Duration::from_millis(200));

    let params = SnapshotCreateParams::new("/tmp/snap", "/tmp/mem");

    // Should succeed if it completes within snapshot timeout (200ms) but beyond default timeout (10ms)
    let notify_clone = notify.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        notify_clone.notify_one();
    });

    client.create_snapshot(&params).await?;
    handle.await?;

    // Now test it times out if it takes longer than snapshot timeout
    server.set_reply_with_delay(hyper::StatusCode::NO_CONTENT, vec![], notify.clone());

    let err_res = client.create_snapshot(&params).await;
    if let Err(Error::Timeout {
        path,
        method,
        route,
    }) = err_res
    {
        assert_eq!(path, sock);
        assert_eq!(method, "PUT");
        assert_eq!(route, "/snapshot/create");
    } else {
        return Err(format!("Expected Timeout error, got {err_res:?}").into());
    }

    notify.notify_one(); // unblock server
    server.shutdown().await;
    Ok(())
}

/// Tests that connection driver tasks and sockets settle after a timeout or oversized response.
#[tokio::test]
async fn test_client_connection_cleanup() -> TestResult {
    use std::time::Duration;
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    // We can't clone FcClient, so we create instances
    let mut tasks = vec![];
    for _ in 0..10 {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        server.set_reply_stream(hyper::StatusCode::OK, rx);
        let client = FcClient::new(&sock).with_timeout(Duration::from_millis(10));

        tasks.push(tokio::spawn(async move {
            tx.send(b"{\"partial\":".to_vec()).unwrap();
            let _ = client.get::<serde_json::Value>("/machine-config").await;
        }));
    }

    for task in tasks.drain(..) {
        let _ = task.await;
    }

    // Verify oversized body cleanup
    for _ in 0..10 {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        server.set_reply_stream(hyper::StatusCode::OK, rx);
        let client = FcClient::new(&sock).with_timeout(Duration::from_millis(50));

        tasks.push(tokio::spawn(async move {
            let limit = 1024 * 1024;
            tx.send(vec![b'A'; limit + 1]).unwrap();
            let _ = client.get::<serde_json::Value>("/machine-config").await;
        }));
    }

    for task in tasks.drain(..) {
        let _ = task.await;
    }

    // Give the server connection tasks a moment to settle
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify we can still make a successful request (no fd exhaustion, server still healthy)
    server.set_reply(hyper::StatusCode::NO_CONTENT, vec![]);
    let client = FcClient::new(&sock).with_timeout(Duration::from_millis(100));
    client.get::<serde_json::Value>("/machine-config").await?;

    server.shutdown().await;
    Ok(())
}
