//! Implementer unit-level tests of the transport.
//! These tests verify the low-level HTTP client behavior directly against a mock server.

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
