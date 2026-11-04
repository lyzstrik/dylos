use dylos_fc::{Error, FcClient};
use serde_json::Value;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

async fn early_response(
    response: &'static [u8],
) -> Result<Result<Option<Value>, Error>, Box<dyn std::error::Error + Send + Sync>> {
    let dir = tempfile::tempdir()?;
    let socket = dir.path().join("early.socket");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        // Deliberately close without reading: response validation must survive EPIPE.
        stream.write_all(response).await?;
        Ok::<(), std::io::Error>(())
    });
    let result = FcClient::new(&socket)
        .with_timeout(Duration::from_secs(2))
        .get("/vm")
        .await;
    server.await??;
    Ok(result)
}

#[tokio::test]
async fn complete_early_response_is_parsed() -> TestResult {
    assert_eq!(
        early_response(include_bytes!("fixtures/early_ok.http")).await??,
        Some(serde_json::json!({}))
    );
    Ok(())
}

#[tokio::test]
async fn early_api_fault_keeps_status_and_message() -> TestResult {
    let error = early_response(include_bytes!("fixtures/early_fault.http")).await?;
    assert!(
        matches!(error, Err(Error::Api { status: 400, fault_message, .. }) if fault_message == "rejected")
    );
    Ok(())
}

#[tokio::test]
async fn early_response_still_validates_json() -> TestResult {
    let error = early_response(include_bytes!("fixtures/early_invalid_json.http")).await?;
    assert!(matches!(error, Err(Error::Deserialize { status: 200, .. })));
    Ok(())
}

#[tokio::test]
async fn close_without_response_remains_request_error() -> TestResult {
    let error = early_response(b"").await?;
    assert!(matches!(error, Err(Error::Request { .. })));
    Ok(())
}
