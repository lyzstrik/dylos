//! Implementer tests for typed Firecracker routes (LYZ-33).
//!
//! Each method of `FcClient` is tested against `FakeServer`:
//! - Success path (204 No Content): method, path, and serialized JSON body match.
//! - Error path (4xx with `fault_message`): propagated as `Error::Api`.
//! - ID validation: empty or invalid characters reject before network call.
//! - Tracing: per-call span records method and concrete path.

mod support;

use std::error::Error as StdError;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dylos_fc::config::{
    BootSource, CacheType, Drive, InstanceActionInfo, MachineConfiguration, NetworkInterface,
};
use dylos_fc::snapshot::{MemoryBackend, SnapshotCreateParams, SnapshotLoadParams, Vm};
use dylos_fc::{Error, FcClient};
use hyper::StatusCode;
use serde_json::json;
use support::FakeServer;
use tempfile::{TempDir, tempdir};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

type TestResult = Result<(), Box<dyn StdError>>;

fn server_and_client() -> Result<(FakeServer, FcClient, TempDir, PathBuf), Box<dyn StdError>> {
    let dir = tempdir()?;
    let sock = dir.path().join("api.socket");
    let server = FakeServer::new(&sock)?;
    let client = FcClient::new(&sock);
    Ok((server, client, dir, sock))
}

fn assert_last_request(
    server: &FakeServer,
    expected_method: &str,
    expected_path: &str,
    expected_body: &serde_json::Value,
) -> TestResult {
    let history = server.history();
    let last = history.last().ok_or("no request recorded")?;
    assert_eq!(last.method, expected_method);
    assert_eq!(last.path, expected_path);
    let body: serde_json::Value = serde_json::from_slice(&last.body)?;
    assert_eq!(&body, expected_body);
    Ok(())
}

fn assert_api_error(
    err: &Error,
    sock: &Path,
    expected_method: &str,
    expected_route: &str,
    expected_status: u16,
    expected_fault: &str,
) -> TestResult {
    let Error::Api {
        path,
        method,
        route,
        status,
        fault_message,
    } = err
    else {
        return Err(format!("expected Error::Api, got {err:?}").into());
    };
    assert_eq!(path, sock);
    assert_eq!(method, expected_method);
    assert_eq!(route, expected_route);
    assert_eq!(*status, expected_status);
    assert_eq!(fault_message, expected_fault);
    Ok(())
}

async fn assert_route_success_and_error<F, Fut>(
    server: &FakeServer,
    sock: &Path,
    method: &str,
    path: &str,
    expected_body: &serde_json::Value,
    call: F,
) -> TestResult
where
    F: Fn() -> Fut,
    Fut: Future<Output = dylos_fc::Result<()>>,
{
    server.set_reply(StatusCode::NO_CONTENT, vec![]);
    call().await?;
    assert_last_request(server, method, path, expected_body)?;

    server.set_reply(
        StatusCode::BAD_REQUEST,
        br#"{"fault_message": "simulated fault"}"#.to_vec(),
    );
    let err = call().await.err().ok_or("expected error")?;
    assert_api_error(&err, sock, method, path, 400, "simulated fault")
}

#[tokio::test]
async fn machine_config_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    let config = MachineConfiguration::new(2, 1024);
    assert_route_success_and_error(
        &server,
        &sock,
        "PUT",
        "/machine-config",
        &json!({ "vcpu_count": 2, "mem_size_mib": 1024 }),
        || client.put_machine_config(&config),
    )
    .await?;

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn boot_source_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    let boot = BootSource::new("/srv/vmlinux.bin");
    assert_route_success_and_error(
        &server,
        &sock,
        "PUT",
        "/boot-source",
        &json!({ "kernel_image_path": "/srv/vmlinux.bin" }),
        || client.put_boot_source(&boot),
    )
    .await
}

#[tokio::test]
async fn drive_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    let mut drive = Drive::new("root_fs-1", true);
    drive.path_on_host = Some("/srv/disks/root.ext4".to_string());
    drive.cache_type = Some(CacheType::Unsafe);
    assert_route_success_and_error(
        &server,
        &sock,
        "PUT",
        "/drives/root_fs-1",
        &json!({
            "drive_id": "root_fs-1",
            "is_root_device": true,
            "path_on_host": "/srv/disks/root.ext4",
            "cache_type": "Unsafe"
        }),
        || client.put_drive(&drive),
    )
    .await
}

#[tokio::test]
async fn drive_rejects_invalid_id() -> TestResult {
    let (_server, client, _dir, _sock) = server_and_client()?;
    for id in ["", "drive/1", "../actions", "drive 1", "drive.0", "drive@1"] {
        let drive = Drive::new(id, false);
        let err = client
            .put_drive(&drive)
            .await
            .err()
            .ok_or("expected error")?;
        let Error::InvalidId { id: bad_id } = &err else {
            return Err(format!("expected Error::InvalidId for {id:?}, got {err:?}").into());
        };
        assert_eq!(bad_id, id);
    }
    Ok(())
}

#[tokio::test]
async fn network_interface_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    let mut iface = NetworkInterface::new("net_0-lan", "tap-vm1-0");
    iface.guest_mac = Some("AA:FC:00:00:00:01".to_string());
    assert_route_success_and_error(
        &server,
        &sock,
        "PUT",
        "/network-interfaces/net_0-lan",
        &json!({
            "iface_id": "net_0-lan",
            "host_dev_name": "tap-vm1-0",
            "guest_mac": "AA:FC:00:00:00:01"
        }),
        || client.put_network_interface(&iface),
    )
    .await
}

#[tokio::test]
async fn network_interface_rejects_invalid_id() -> TestResult {
    let (_server, client, _dir, _sock) = server_and_client()?;
    for id in ["", "eth/0", "net 0", "eth.0", "net?0", "net#0"] {
        let iface = NetworkInterface::new(id, "tap0");
        let err = client
            .put_network_interface(&iface)
            .await
            .err()
            .ok_or("expected error")?;
        let Error::InvalidId { id: bad_id } = &err else {
            return Err(format!("expected Error::InvalidId for {id:?}, got {err:?}").into());
        };
        assert_eq!(bad_id, id);
    }
    Ok(())
}

#[tokio::test]
async fn start_instance_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    assert_route_success_and_error(
        &server,
        &sock,
        "PUT",
        "/actions",
        &json!(InstanceActionInfo::instance_start()),
        || client.start_instance(),
    )
    .await
}

#[tokio::test]
async fn pause_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    assert_route_success_and_error(&server, &sock, "PATCH", "/vm", &json!(Vm::pause()), || {
        client.pause()
    })
    .await
}

#[tokio::test]
async fn resume_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    assert_route_success_and_error(&server, &sock, "PATCH", "/vm", &json!(Vm::resume()), || {
        client.resume()
    })
    .await
}

#[tokio::test]
async fn create_snapshot_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    let params = SnapshotCreateParams::full("/srv/snap.bin", "/srv/mem.bin");
    assert_route_success_and_error(
        &server,
        &sock,
        "PUT",
        "/snapshot/create",
        &json!(params),
        || client.create_snapshot(&params),
    )
    .await
}

#[tokio::test]
async fn load_snapshot_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    let params = SnapshotLoadParams::new("/srv/snap.bin", MemoryBackend::file("/srv/mem.bin"));
    assert_route_success_and_error(
        &server,
        &sock,
        "PUT",
        "/snapshot/load",
        &json!(params),
        || client.load_snapshot(&params),
    )
    .await
}

struct SpanVisitor<'a>(&'a mut String, &'a mut String);

impl Visit for SpanVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "method" && self.0.is_empty() {
            *self.0 = format!("{value:?}").trim_matches('"').to_string();
        } else if field.name() == "route" && self.1.is_empty() {
            *self.1 = format!("{value:?}").trim_matches('"').to_string();
        }
    }
}

#[derive(Default, Clone)]
struct SpanRecorder {
    spans: Arc<Mutex<Vec<(String, String)>>>,
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for SpanRecorder {
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        let mut method = String::new();
        let mut route = String::new();
        attrs.record(&mut SpanVisitor(&mut method, &mut route));
        if let Ok(mut spans_lock) = self.spans.lock()
            && !method.is_empty()
            && !route.is_empty()
        {
            spans_lock.push((method, route));
        }
    }
}

#[tokio::test]
async fn span_records_concrete_path_for_parameterized_routes() -> TestResult {
    let recorder = SpanRecorder::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(recorder.clone()));

    let (_server, client, _dir, _sock) = server_and_client()?;

    let drive = Drive::new("drive_alpha-1", true);
    client.put_drive(&drive).await?;

    let iface = NetworkInterface::new("net_eth-0", "tap0");
    client.put_network_interface(&iface).await?;

    let captured = recorder.spans.lock().map_err(|e| e.to_string())?.clone();
    assert!(
        captured
            .iter()
            .any(|(m, r)| m == "PUT" && r == "/drives/drive_alpha-1"),
        "expected span for PUT /drives/drive_alpha-1, got {captured:?}"
    );
    assert!(
        captured
            .iter()
            .any(|(m, r)| m == "PUT" && r == "/network-interfaces/net_eth-0"),
        "expected span for PUT /network-interfaces/net_eth-0, got {captured:?}"
    );
    Ok(())
}
