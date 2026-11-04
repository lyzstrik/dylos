use dylos_fc::{Error, FcClient};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::prelude::*;

struct WaitingForResponse(Arc<tokio::sync::Notify>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WaitingForResponse {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Waiting<'a>(&'a tokio::sync::Notify);
        impl tracing::field::Visit for Waiting<'_> {
            fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                if field.name() == "waiting_for_early_response" && value {
                    self.0.notify_one();
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        event.record(&mut Waiting(&self.0));
    }
}
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
async fn complete_early_success_is_rejected() -> TestResult {
    assert!(matches!(
        early_response(include_bytes!("fixtures/early_ok.http")).await?,
        Err(Error::IncompleteRequest { source, .. }) if source.kind() == std::io::ErrorKind::BrokenPipe
    ));
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
async fn early_invalid_json_does_not_become_success() -> TestResult {
    let error = early_response(include_bytes!("fixtures/early_invalid_json.http")).await?;
    assert!(matches!(error, Err(Error::IncompleteRequest { .. })));
    Ok(())
}

#[tokio::test]
async fn close_without_response_remains_request_error() -> TestResult {
    let error = early_response(b"").await?;
    assert!(matches!(
        error,
        Err(Error::IncompleteRequest { .. } | Error::Request { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn mutating_early_responses_are_not_false_successes() -> TestResult {
    use std::net::Shutdown;
    use tokio::io::AsyncReadExt;
    for patch in [false, true] {
        for status in [200, 302, 400, 500] {
            for delayed in [false, true] {
                let waiting = Arc::new(tokio::sync::Notify::new());
                let subscriber =
                    tracing_subscriber::registry().with(WaitingForResponse(waiting.clone()));
                let _guard = tracing::subscriber::set_default(subscriber);
                let dir = tempfile::tempdir()?;
                let socket = dir.path().join("mutating.socket");
                let listener = tokio::net::UnixListener::bind(&socket)?;
                let server = tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await?;
                    // Observe request bytes, then refuse the rest of the 8 MiB body.
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await?;
                    // Empty the receive buffer to wake a blocked writer before
                    // shutting down; otherwise readiness can wait for the response.
                    let mut buffered = [0; 8192];
                    loop {
                        match stream.try_read(&mut buffered) {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) => return Err(e),
                        }
                    }
                    let stream = stream.into_std()?;
                    stream.shutdown(Shutdown::Read)?;
                    let mut stream = tokio::net::UnixStream::from_std(stream)?;
                    if delayed {
                        tokio::time::timeout(Duration::from_secs(2), waiting.notified())
                            .await
                            .map_err(std::io::Error::other)?;
                    }
                    let response = format!(
                        "HTTP/1.1 {status} Reply\r\nContent-Length: 28\r\nConnection: close\r\n\r\n{{\"fault_message\":\"rejected\"}}"
                    );
                    for chunk in response.as_bytes().chunks(7) {
                        stream.write_all(chunk).await?;
                        tokio::task::yield_now().await;
                    }
                    Ok::<(), std::io::Error>(())
                });
                let client = FcClient::new(&socket).with_timeout(Duration::from_secs(5));
                let body = "x".repeat(8 * 1024 * 1024);
                let result: Result<Option<Value>, Error> = if patch {
                    client.patch("/vm", &body).await
                } else {
                    client.put("/actions", &body).await
                };
                server.await??;
                if status >= 400 {
                    assert!(
                        matches!(result, Err(Error::Api { status: actual, fault_message, .. }) if actual == status && fault_message == "rejected")
                    );
                } else {
                    assert!(
                        matches!(&result, Err(Error::IncompleteRequest { source, .. }) if matches!(source.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::WriteZero)),
                        "{result:?}, status={status}, delayed={delayed}, patch={patch}"
                    );
                }
            }
        }
    }
    Ok(())
}
