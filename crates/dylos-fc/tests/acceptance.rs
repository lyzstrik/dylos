//! Acceptance tests for the Firecracker transport (LYZ-30), derived from the
//! issue criteria rather than from the implementation.

mod support;

use dylos_fc::{Error, FcClient};
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error as StdError;
use std::sync::{Arc, Mutex};
use support::FakeServer;
use tempfile::tempdir;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

type TestResult = Result<(), Box<dyn StdError>>;

fn server_and_client(
    dir: &tempfile::TempDir,
) -> Result<(FakeServer, FcClient, std::path::PathBuf), Box<dyn StdError>> {
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;
    let client = FcClient::new(&sock);
    Ok((server, client, sock))
}

// ---- Criterion 1: method, path, Content-Type, JSON body ----

#[tokio::test]
async fn put_sends_method_path_content_type_and_body() -> TestResult {
    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;
    let body = json!({"vcpu_count": 2, "mem_size_mib": 256});

    let res: Option<Value> = client.put("/machine-config", &body).await?;
    assert!(res.is_none());

    let h = server.history();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].method, "PUT");
    assert_eq!(h[0].path, "/machine-config");
    assert_eq!(h[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(serde_json::from_slice::<Value>(&h[0].body)?, body);
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn patch_sends_method_path_content_type_and_body() -> TestResult {
    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;
    let body = json!({"state": "Paused"});

    let res: Option<Value> = client.patch("/vm", &body).await?;
    assert!(res.is_none());

    let h = server.history();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].method, "PATCH");
    assert_eq!(h[0].path, "/vm");
    assert_eq!(h[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(serde_json::from_slice::<Value>(&h[0].body)?, body);
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn get_sends_method_path_and_no_body() -> TestResult {
    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;
    server.set_reply(hyper::StatusCode::OK, br#"{"state":"Running"}"#.to_vec());

    let res: Option<Value> = client.get("/vm").await?;
    assert_eq!(res, Some(json!({"state": "Running"})));

    let h = server.history();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].method, "GET");
    assert_eq!(h[0].path, "/vm");
    assert_eq!(h[0].body, Vec::<u8>::new());
    server.shutdown().await;
    Ok(())
}

// ---- Criterion 2: error context ----

fn assert_api_error(
    err: &Error,
    sock: &std::path::Path,
    method: &str,
    route: &str,
    status: u16,
    exact_fault: Option<&str>,
    fallback_contains: Option<&str>,
) -> TestResult {
    let Error::Api {
        path,
        method: m,
        route: r,
        status: s,
        fault_message,
    } = err
    else {
        return Err(format!("expected Error::Api, got {err:?}").into());
    };
    assert_eq!(path, sock);
    assert_eq!(m, method);
    assert_eq!(r, route);
    assert_eq!(*s, status);

    if let Some(f) = exact_fault {
        assert_eq!(fault_message, f, "exact fault_message mismatch");
    }
    if let Some(f) = fallback_contains {
        assert!(
            fault_message.contains(f),
            "fault_message: {fault_message} did not contain {f}"
        );
    }

    let shown = err.to_string();
    let mut needles = vec![
        sock.to_string_lossy().into_owned(),
        method.to_string(),
        route.to_string(),
        status.to_string(),
    ];
    if let Some(f) = exact_fault {
        needles.push(f.to_string());
    }
    if let Some(f) = fallback_contains {
        needles.push(f.to_string());
    }

    for needle in needles {
        assert!(shown.contains(&needle), "{needle:?} missing from {shown:?}");
    }
    Ok(())
}

#[tokio::test]
async fn error_4xx_carries_full_context() -> TestResult {
    let dir = tempdir()?;
    let (server, client, sock) = server_and_client(&dir)?;
    server.set_reply(
        hyper::StatusCode::BAD_REQUEST,
        br#"{"fault_message":"The kernel file cannot be opened"}"#.to_vec(),
    );

    let err = client
        .put::<_, Value>("/boot-source", &json!({"kernel_image_path": "/nope"}))
        .await
        .err()
        .ok_or("expected error")?;
    assert_api_error(
        &err,
        &sock,
        "PUT",
        "/boot-source",
        400,
        Some("The kernel file cannot be opened"),
        None,
    )?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn error_5xx_carries_full_context() -> TestResult {
    let dir = tempdir()?;
    let (server, client, sock) = server_and_client(&dir)?;
    server.set_reply(
        hyper::StatusCode::INTERNAL_SERVER_ERROR,
        br#"{"fault_message":"internal failure"}"#.to_vec(),
    );

    let err = client
        .patch::<_, Value>("/vm", &json!({"state": "Paused"}))
        .await
        .err()
        .ok_or("expected error")?;
    assert_api_error(
        &err,
        &sock,
        "PATCH",
        "/vm",
        500,
        Some("internal failure"),
        None,
    )?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn error_on_get_carries_context() -> TestResult {
    let dir = tempdir()?;
    let (server, client, sock) = server_and_client(&dir)?;
    server.set_reply(
        hyper::StatusCode::NOT_FOUND,
        br#"{"fault_message":"not found"}"#.to_vec(),
    );

    let err = client
        .get::<Value>("/nothing")
        .await
        .err()
        .ok_or("expected error")?;
    assert_api_error(&err, &sock, "GET", "/nothing", 404, Some("not found"), None)?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn non_json_error_body_is_still_useful() -> TestResult {
    let dir = tempdir()?;
    let (server, client, sock) = server_and_client(&dir)?;
    server.set_reply(
        hyper::StatusCode::BAD_GATEWAY,
        b"upstream exploded: <html>".to_vec(),
    );

    let err = client
        .get::<Value>("/vm")
        .await
        .err()
        .ok_or("expected error")?;
    assert_api_error(
        &err,
        &sock,
        "GET",
        "/vm",
        502,
        None,
        Some("upstream exploded"),
    )?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn empty_error_body_still_reports_status_and_route() -> TestResult {
    let dir = tempdir()?;
    let (server, client, sock) = server_and_client(&dir)?;
    server.set_reply(hyper::StatusCode::INTERNAL_SERVER_ERROR, vec![]);

    let err = client
        .get::<Value>("/vm")
        .await
        .err()
        .ok_or("expected error")?;
    assert!(matches!(err, Error::Api { status: 500, .. }), "{err:?}");
    let shown = err.to_string();
    assert!(shown.contains("500") && shown.contains("/vm"), "{shown}");
    assert!(shown.contains(sock.to_string_lossy().as_ref()), "{shown}");
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn missing_socket_yields_connect_error_with_path() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("does-not-exist.socket");
    let client = FcClient::new(&sock);

    let err = client
        .get::<Value>("/vm")
        .await
        .err()
        .ok_or("expected error")?;
    let Error::Connect { path, .. } = &err else {
        return Err(format!("expected Error::Connect, got {err:?}").into());
    };
    assert_eq!(path, &sock);
    assert!(err.to_string().contains(sock.to_string_lossy().as_ref()));
    Ok(())
}

// ---- Criterion 3: tracing span with method, route and duration ----

type FieldList = Vec<(String, String)>;

#[derive(Default, Debug, Clone)]
struct Captured {
    fields: FieldList,
    closed: bool,
}

#[derive(Clone, Default)]
struct CaptureLayer {
    spans: Arc<Mutex<Vec<Captured>>>,
    /// Fields of every event emitted during the test.
    events: Arc<Mutex<Vec<FieldList>>>,
}

struct FieldVisitor<'a>(&'a mut Vec<(String, String)>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for CaptureLayer {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = vec![];
        attrs.record(&mut FieldVisitor(&mut fields));
        let Ok(mut spans) = self.spans.lock() else {
            return;
        };
        spans.push(Captured {
            fields,
            closed: false,
        });
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(spans.len() - 1);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let Some(idx) = span.extensions().get::<usize>().copied() else {
            return;
        };
        let mut fields = vec![];
        values.record(&mut FieldVisitor(&mut fields));
        if let Ok(mut spans) = self.spans.lock() {
            spans[idx].fields.extend(fields);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = vec![];
        event.record(&mut FieldVisitor(&mut fields));
        if let Ok(mut ev) = self.events.lock() {
            ev.push(fields);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(idx) = span.extensions().get::<usize>().copied() else {
            return;
        };
        if let Ok(mut spans) = self.spans.lock() {
            spans[idx].closed = true;
        }
    }
}

fn has_field(c: &Captured, name: &str, value: &str) -> bool {
    c.fields
        .iter()
        .any(|(k, v)| k == name && v.trim_matches('"') == value)
}

fn is_duration_field(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.contains("duration") || k.contains("elapsed") || k.contains("latency")
}

#[tokio::test]
async fn each_call_has_a_span_with_method_and_route() -> TestResult {
    let layer = CaptureLayer::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(layer.clone()));

    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;
    server.set_reply(hyper::StatusCode::OK, b"{}".to_vec());
    let _: Option<Value> = client.get("/vm").await?;
    let _: Option<Value> = client.put("/machine-config", &json!({})).await?;
    let _: Option<Value> = client.patch("/vm", &json!({})).await?;
    server.shutdown().await;

    let spans = layer.spans.lock().map_err(|e| e.to_string())?.clone();
    for (m, r) in [("GET", "/vm"), ("PUT", "/machine-config"), ("PATCH", "/vm")] {
        let found = spans
            .iter()
            .find(|c| has_field(c, "method", m) && has_field(c, "route", r));
        let c = found.ok_or_else(|| format!("no span with method={m} route={r}: {spans:?}"))?;
        assert!(c.closed, "span for {m} {r} never closed");
    }
    Ok(())
}

#[tokio::test]
async fn span_records_duration() -> TestResult {
    let layer = CaptureLayer::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(layer.clone()));

    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;

    // Success call
    server.set_reply(hyper::StatusCode::OK, b"{}".to_vec());
    let _: Option<Value> = client.get("/vm").await?;

    // Failed call
    server.set_reply(hyper::StatusCode::NOT_FOUND, b"{}".to_vec());
    let _ = client.get::<Value>("/nothing").await;

    // We can't strictly control 'pending' in the simplistic FakeServer without modifications,
    // but we have two calls (one success, one fail).
    server.shutdown().await;

    let spans = layer.spans.lock().map_err(|e| e.to_string())?.clone();

    for (m, r) in [("GET", "/vm"), ("GET", "/nothing")] {
        let span = spans
            .iter()
            .find(|c| has_field(c, "method", m) && has_field(c, "route", r))
            .ok_or_else(|| format!("no span found for {m} {r}"))?;

        let dur_field = span
            .fields
            .iter()
            .find(|(k, _)| is_duration_field(k))
            .ok_or_else(|| format!("no duration field on span {m} {r}"))?;

        let _dur_val: u64 = dur_field
            .1
            .parse()
            .map_err(|e| format!("duration not numeric: {e}"))?;
        assert_eq!(dur_field.0, "duration_ms");
    }

    Ok(())
}

// ---- Criterion 4: reusable fake server ----

#[derive(Deserialize, Debug, PartialEq)]
struct VmInfo {
    id: String,
    state: String,
}

#[tokio::test]
async fn one_server_handles_sequential_requests_in_order() -> TestResult {
    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;

    let _: Option<Value> = client.put("/a", &json!({"n": 1})).await?;
    let _: Option<Value> = client.patch("/b", &json!({"n": 2})).await?;
    let _: Option<Value> = client.get("/c").await?;
    let _: Option<Value> = client.put("/d", &json!({"n": 4})).await?;

    let seen: Vec<(String, String)> = server
        .history()
        .into_iter()
        .map(|r| (r.method, r.path))
        .collect();
    let expected: Vec<(String, String)> =
        [("PUT", "/a"), ("PATCH", "/b"), ("GET", "/c"), ("PUT", "/d")]
            .into_iter()
            .map(|(m, p)| (m.to_string(), p.to_string()))
            .collect();
    assert_eq!(seen, expected);
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn scripted_2xx_json_body_is_deserialized() -> TestResult {
    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;
    server.set_reply(
        hyper::StatusCode::OK,
        br#"{"id":"vm-1","state":"Running"}"#.to_vec(),
    );

    let info: Option<VmInfo> = client.get("/").await?;
    assert_eq!(
        info,
        Some(VmInfo {
            id: "vm-1".into(),
            state: "Running".into()
        })
    );

    // The scripted reply can be changed between requests on the same server.
    server.set_reply(hyper::StatusCode::NO_CONTENT, vec![]);
    let none: Option<VmInfo> = client.get("/").await?;
    assert!(none.is_none());
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn error_deserialize_malformed_json() -> TestResult {
    let dir = tempdir()?;
    let (server, client, sock) = server_and_client(&dir)?;
    server.set_reply(hyper::StatusCode::OK, b"{malformed}".to_vec());

    let err = client
        .get::<Value>("/vm")
        .await
        .err()
        .ok_or("expected error")?;
    let Error::Deserialize {
        path,
        method,
        route,
        status,
        ..
    } = &err
    else {
        return Err(format!("expected Deserialize, got {err:?}").into());
    };
    assert_eq!(path, &sock);
    assert_eq!(method, "GET");
    assert_eq!(route, "/vm");
    assert_eq!(*status, 200);
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn invalid_route_yields_request_builder_error() -> TestResult {
    let dir = tempdir()?;
    let (server, client, _) = server_and_client(&dir)?;
    let err = client
        .get::<Value>(" bad route ")
        .await
        .err()
        .ok_or("expected error")?;
    let Error::RequestBuilder { method, route, .. } = &err else {
        return Err(format!("expected RequestBuilder, got {err:?}").into());
    };
    assert_eq!(method, "GET");
    assert_eq!(route, " bad route ");
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn truncated_body_yields_body_read_error() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("trunc.socket");
    let listener = tokio::net::UnixListener::bind(&sock)?;

    let client = FcClient::new(&sock);

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"partial\"";
            let _ = stream.writable().await;
            let _ = stream.try_write(resp);
            // stream dropped here to simulate truncation
        }
    });

    let err = client
        .get::<Value>("/vm")
        .await
        .err()
        .ok_or("expected error")?;
    let Error::BodyRead {
        path,
        method,
        route,
        status,
        ..
    } = &err
    else {
        return Err(format!("expected BodyRead, got {err:?}").into());
    };
    assert_eq!(path, &sock);
    assert_eq!(method, "GET");
    assert_eq!(route, "/vm");
    assert_eq!(*status, 200);
    Ok(())
}

#[tokio::test]
async fn fake_server_shutdown_does_not_block() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;
    tokio::time::timeout(std::time::Duration::from_secs(1), server.shutdown())
        .await
        .map_err(|_| "shutdown timed out")?;
    Ok(())
}

#[tokio::test]
async fn fake_server_raii_drops_socket() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("raii.socket");
    {
        let _server = FakeServer::new(&sock)?;
        assert!(sock.exists());
    }
    // Give it a short moment for drop to complete its async-like effects (if any, though File removal is sync)
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert!(!sock.exists(), "Socket file should be removed by Drop");
    Ok(())
}
