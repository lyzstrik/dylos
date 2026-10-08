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
        let expected = "A".repeat(limit - 2);
        let body = serde_json::to_vec(&expected)?;
        assert_eq!(body.len(), limit);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        server.set_reply_stream(status, rx);
        tx.send(body[..limit / 2].to_vec())?;
        tx.send(body[limit / 2..].to_vec())?;
        drop(tx);

        let res = client.get::<serde_json::Value>("/machine-config").await;
        if status == hyper::StatusCode::OK {
            assert_eq!(res?, Some(json!(expected)));
        } else {
            match res {
                Err(Error::Api {
                    path,
                    method,
                    route,
                    status,
                    fault_message,
                }) => {
                    assert_eq!(path, sock);
                    assert_eq!(method, "GET");
                    assert_eq!(route, "/machine-config");
                    assert_eq!(status, 500);
                    assert_eq!(fault_message, format!("\"{}...", "A".repeat(1023)));
                }
                other => return Err(format!("Expected Api error at limit, got {other:?}").into()),
            }
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

/// Both typed snapshot routes use the snapshot budget, including stalled calls.
#[tokio::test]
async fn test_snapshot_timeout() -> TestResult {
    use dylos_fc::snapshot::{SnapshotCreateParams, SnapshotLoadParams};
    use std::time::Duration;
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;

    for load in [false, true] {
        for stalled in [false, true] {
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            server.set_reply_with_delay(hyper::StatusCode::NO_CONTENT, vec![], release.clone());
            let client = FcClient::new(&sock)
                .with_timeout(Duration::from_secs(10))
                .with_snapshot_timeout(Duration::from_secs(200));
            let request = tokio::spawn(async move {
                if load {
                    client
                        .load_snapshot(&SnapshotLoadParams::with_file_backend(
                            "/tmp/snap",
                            "/tmp/mem",
                        ))
                        .await
                } else {
                    client
                        .create_snapshot(&SnapshotCreateParams::new("/tmp/snap", "/tmp/mem"))
                        .await
                }
            });
            server.wait_for_request().await?;
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(if stalled { 201 } else { 50 })).await;
            tokio::time::resume();
            if !stalled {
                assert!(
                    !request.is_finished(),
                    "ordinary deadline used for snapshot"
                );
                release.notify_one();
            }
            let result = tokio::time::timeout(Duration::from_secs(2), request).await??;
            if stalled {
                match result {
                    Err(Error::Timeout {
                        path,
                        method,
                        route,
                    }) => {
                        assert_eq!(path, sock);
                        assert_eq!(method, "PUT");
                        assert_eq!(
                            route,
                            if load {
                                "/snapshot/load"
                            } else {
                                "/snapshot/create"
                            }
                        );
                    }
                    other => return Err(format!("Expected snapshot timeout, got {other:?}").into()),
                }
                release.notify_one();
            } else {
                result?;
            }
            server.wait_for_connections().await?;
        }
    }
    server.shutdown().await;
    Ok(())
}

/// Open response streams must lose their peer after timeout or size rejection.
#[tokio::test]
async fn test_client_connection_cleanup() -> TestResult {
    use std::time::Duration;
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;
    let limit = 1024 * 1024;

    for oversized in [false, true] {
        for _ in 0..10 {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            server.set_reply_stream(hyper::StatusCode::OK, rx);
            tx.send(if oversized {
                vec![b'A'; limit + 1]
            } else {
                b"{\"partial\":".to_vec()
            })?;
            let client = FcClient::new(&sock).with_timeout(Duration::from_millis(50));
            let request =
                tokio::spawn(
                    async move { client.get::<serde_json::Value>("/machine-config").await },
                );
            server.wait_for_request().await?;
            let result = tokio::time::timeout(Duration::from_secs(2), request).await??;
            match result {
                Err(Error::ResponseTooLarge {
                    path,
                    method,
                    route,
                    limit: actual,
                }) if oversized => {
                    assert_eq!(actual, limit);
                    assert_eq!(path, sock);
                    assert_eq!(method, "GET");
                    assert_eq!(route, "/machine-config");
                }
                Err(Error::Timeout {
                    path,
                    method,
                    route,
                }) if !oversized => {
                    assert_eq!(path, sock);
                    assert_eq!(method, "GET");
                    assert_eq!(route, "/machine-config");
                }
                other => return Err(format!("Unexpected cleanup request result: {other:?}").into()),
            }
            // Keep the sender open: completion must come from client disconnect,
            // rather than the server reaching the end of the response body.
            server.wait_for_connections().await?;
            assert!(
                tx.is_closed(),
                "response body retained by leaked connection"
            );
        }
    }
    server.shutdown().await;
    Ok(())
}
