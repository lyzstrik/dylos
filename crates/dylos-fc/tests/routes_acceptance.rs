//! Acceptance tests for the typed Firecracker routes (LYZ-33).
//!
//! Written by a different model family than the implementation, from the issue's
//! acceptance criteria and the PR's stated contract rather than from the code.
//! Request bodies are spelled out as literal JSON from the Firecracker v1.17.0
//! API instead of being derived from the crate's own types, so a wrong serde
//! attribute cannot make the test and the code agree on a wrong wire format.
//!
//! Criteria covered:
//! 1. One method per route sends exactly one request with the right method, path and body.
//! 2. `drive_id` / `iface_id` outside `[A-Za-z0-9_-]` (or empty) never reach the socket.
//! 3. 4xx / 5xx replies surface as `Error::Api` with `fault_message`, method and concrete route.
//! 4. Each call is traced with its method and concrete path.

mod support;

use std::collections::{BTreeSet, HashMap};
use std::error::Error as StdError;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dylos_fc::config::{
    BootSource, CacheType, Drive, IoEngine, MachineConfiguration, NetworkInterface,
};
use dylos_fc::snapshot::{
    HugePagesConfig, MemoryBackend, NetworkOverride, SnapshotCreateParams, SnapshotLoadParams,
    SnapshotType, VsockOverride,
};
use dylos_fc::{Error, FcClient};
use hyper::StatusCode;
use serde_json::{Value, json};
use support::FakeServer;
use tempfile::{TempDir, tempdir};
use tracing::field::{Field, Visit};
use tracing::span::Attributes;
use tracing::span::Id;
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

enum Call {
    MachineConfig(MachineConfiguration),
    BootSource(BootSource),
    Drive(Drive),
    Iface(NetworkInterface),
    Start,
    Pause,
    Resume,
    SnapshotCreate(SnapshotCreateParams),
    SnapshotLoad(SnapshotLoadParams),
}

impl Call {
    async fn run(&self, client: &FcClient) -> dylos_fc::Result<()> {
        match self {
            Self::MachineConfig(c) => client.put_machine_config(c).await,
            Self::BootSource(b) => client.put_boot_source(b).await,
            Self::Drive(d) => client.put_drive(d).await,
            Self::Iface(i) => client.put_network_interface(i).await,
            Self::Start => client.start_instance().await,
            Self::Pause => client.pause().await,
            Self::Resume => client.resume().await,
            Self::SnapshotCreate(p) => client.create_snapshot(p).await,
            Self::SnapshotLoad(p) => client.load_snapshot(p).await,
        }
    }
}

struct Case {
    name: &'static str,
    call: Call,
    method: &'static str,
    path: &'static str,
    body: Value,
}

fn cases() -> Vec<Case> {
    let mut all = machine_and_boot_cases();
    all.extend(device_cases());
    all.extend(action_and_snapshot_cases());
    all
}

fn machine_and_boot_cases() -> Vec<Case> {
    let mut machine = MachineConfiguration::new(2, 512);
    machine.smt = Some(false);
    machine.track_dirty_pages = Some(true);

    let mut boot = BootSource::new("/k/vmlinux");
    boot.boot_args = Some("console=ttyS0 reboot=k".to_string());
    boot.initrd_path = Some("/k/initrd".to_string());

    vec![
        Case {
            name: "machine-config minimal",
            call: Call::MachineConfig(MachineConfiguration::new(1, 128)),
            method: "PUT",
            path: "/machine-config",
            body: json!({"vcpu_count": 1, "mem_size_mib": 128}),
        },
        Case {
            name: "machine-config full",
            call: Call::MachineConfig(machine),
            method: "PUT",
            path: "/machine-config",
            body: json!({
                "vcpu_count": 2, "mem_size_mib": 512, "smt": false, "track_dirty_pages": true
            }),
        },
        Case {
            name: "boot-source minimal",
            call: Call::BootSource(BootSource::new("/k/vmlinux")),
            method: "PUT",
            path: "/boot-source",
            body: json!({"kernel_image_path": "/k/vmlinux"}),
        },
        Case {
            name: "boot-source full",
            call: Call::BootSource(boot),
            method: "PUT",
            path: "/boot-source",
            body: json!({
                "kernel_image_path": "/k/vmlinux",
                "boot_args": "console=ttyS0 reboot=k",
                "initrd_path": "/k/initrd"
            }),
        },
    ]
}

fn device_cases() -> Vec<Case> {
    let mut drive = Drive::new("rootfs", true);
    drive.path_on_host = Some("/d/root.ext4".to_string());
    drive.is_read_only = Some(false);
    drive.partuuid = Some("0eaa91a0-01".to_string());
    drive.cache_type = Some(CacheType::Writeback);
    drive.io_engine = Some(IoEngine::Sync);

    let mut iface = NetworkInterface::new("eth0", "tap-a");
    iface.guest_mac = Some("06:00:AC:10:00:02".to_string());
    iface.mtu = Some(1500);
    vec![
        Case {
            name: "drive minimal",
            call: Call::Drive(Drive::new("scratch_1", false)),
            method: "PUT",
            path: "/drives/scratch_1",
            body: json!({"drive_id": "scratch_1", "is_root_device": false}),
        },
        Case {
            name: "drive full",
            call: Call::Drive(drive),
            method: "PUT",
            path: "/drives/rootfs",
            body: json!({
                "drive_id": "rootfs",
                "is_root_device": true,
                "path_on_host": "/d/root.ext4",
                "is_read_only": false,
                "partuuid": "0eaa91a0-01",
                "cache_type": "Writeback",
                "io_engine": "Sync"
            }),
        },
        Case {
            name: "network-interface minimal",
            call: Call::Iface(NetworkInterface::new("eth1", "tap-z")),
            method: "PUT",
            path: "/network-interfaces/eth1",
            body: json!({"iface_id": "eth1", "host_dev_name": "tap-z"}),
        },
        Case {
            name: "network-interface full",
            call: Call::Iface(iface),
            method: "PUT",
            path: "/network-interfaces/eth0",
            body: json!({
                "iface_id": "eth0",
                "host_dev_name": "tap-a",
                "guest_mac": "06:00:AC:10:00:02",
                "mtu": 1500
            }),
        },
    ]
}

fn action_and_snapshot_cases() -> Vec<Case> {
    let mut create =
        SnapshotCreateParams::new("/s/vm.snap", "/s/vm.mem").with_sync_snapshot_files(false);
    create.snapshot_type = Some(SnapshotType::Diff);

    let mut load = SnapshotLoadParams::new("/s/vm.snap", MemoryBackend::uffd("/s/uffd.sock"))
        .with_resume_vm(false)
        .with_network_overrides(vec![NetworkOverride::new("eth0", "tap-b")]);
    load.track_dirty_pages = Some(true);
    load.vsock_override = Some(VsockOverride::new("/s/v.sock"));
    load.clock_realtime = Some(true);
    load.huge_pages = Some(HugePagesConfig::HugePages2M);

    vec![
        Case {
            name: "start_instance",
            call: Call::Start,
            method: "PUT",
            path: "/actions",
            body: json!({"action_type": "InstanceStart"}),
        },
        Case {
            name: "pause",
            call: Call::Pause,
            method: "PATCH",
            path: "/vm",
            body: json!({"state": "Paused"}),
        },
        Case {
            name: "resume",
            call: Call::Resume,
            method: "PATCH",
            path: "/vm",
            body: json!({"state": "Resumed"}),
        },
        Case {
            name: "snapshot create minimal",
            call: Call::SnapshotCreate(SnapshotCreateParams::new("/s/a.snap", "/s/a.mem")),
            method: "PUT",
            path: "/snapshot/create",
            body: json!({"snapshot_path": "/s/a.snap", "mem_file_path": "/s/a.mem"}),
        },
        Case {
            name: "snapshot create diff, no fsync",
            call: Call::SnapshotCreate(create),
            method: "PUT",
            path: "/snapshot/create",
            body: json!({
                "snapshot_path": "/s/vm.snap",
                "mem_file_path": "/s/vm.mem",
                "snapshot_type": "Diff",
                "sync_snapshot_files": false
            }),
        },
        Case {
            name: "snapshot load minimal",
            call: Call::SnapshotLoad(SnapshotLoadParams::with_file_backend(
                "/s/a.snap",
                "/s/a.mem",
            )),
            method: "PUT",
            path: "/snapshot/load",
            body: json!({
                "snapshot_path": "/s/a.snap",
                "mem_backend": {"backend_path": "/s/a.mem", "backend_type": "File"}
            }),
        },
        Case {
            name: "snapshot load full",
            call: Call::SnapshotLoad(load),
            method: "PUT",
            path: "/snapshot/load",
            body: json!({
                "snapshot_path": "/s/vm.snap",
                "mem_backend": {"backend_path": "/s/uffd.sock", "backend_type": "Uffd"},
                "resume_vm": false,
                "network_overrides": [{"iface_id": "eth0", "host_dev_name": "tap-b"}],
                "track_dirty_pages": true,
                "vsock_override": {"uds_path": "/s/v.sock"},
                "clock_realtime": true,
                "huge_pages": "2M"
            }),
        },
    ]
}

#[tokio::test]
async fn every_route_sends_exactly_one_request_with_literal_wire_format() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    for case in cases() {
        let before = server.history().len();
        case.call.run(&client).await?;
        let history = server.history();
        assert_eq!(
            history.len(),
            before + 1,
            "{}: one call = one request",
            case.name
        );
        let last = history.last().ok_or("no request")?;
        assert_eq!(last.method, case.method, "{}", case.name);
        assert_eq!(last.path, case.path, "{}", case.name);
        assert_eq!(
            last.content_type.as_deref(),
            Some("application/json"),
            "{}",
            case.name
        );
        let body: Value = serde_json::from_slice(&last.body)?;
        assert_eq!(body, case.body, "{}", case.name);
    }
    Ok(())
}

#[tokio::test]
async fn unset_optional_fields_are_absent_not_null() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    client.put_drive(&Drive::new("d", false)).await?;
    client
        .put_network_interface(&NetworkInterface::new("n", "tap"))
        .await?;
    client
        .load_snapshot(&SnapshotLoadParams::with_file_backend("/a", "/b"))
        .await?;
    for record in server.history() {
        let body: Value = serde_json::from_slice(&record.body)?;
        let object = body.as_object().ok_or("body is not an object")?;
        assert!(
            object.values().all(|v| !v.is_null()),
            "null field in {}: {body}",
            record.path
        );
    }
    Ok(())
}

#[tokio::test]
async fn path_id_always_equals_body_id() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    for id in ["a", "root-fs_9", "Z"] {
        client.put_drive(&Drive::new(id, false)).await?;
        client
            .put_network_interface(&NetworkInterface::new(id, "tap0"))
            .await?;
    }
    for record in server.history() {
        let body: Value = serde_json::from_slice(&record.body)?;
        let body_id = body
            .get("drive_id")
            .or_else(|| body.get("iface_id"))
            .and_then(Value::as_str)
            .ok_or("no id in body")?;
        assert!(
            record.path.ends_with(&format!("/{body_id}")),
            "path {} vs body id {body_id}",
            record.path
        );
    }
    Ok(())
}

const HOSTILE_IDS: &[&str] = &[
    "",
    ".",
    "..",
    "../actions",
    "../../vm",
    "a/../actions",
    "a/b",
    "/",
    "/actions",
    "a?x=1",
    "a#frag",
    "a b",
    " a",
    "a ",
    "a\n",
    "a\r\nHost: evil",
    "a\0",
    "a\t",
    "%2e%2e",
    "a%2fb",
    "a%00",
    "a;b",
    "a:b",
    "a@b",
    "a.b",
    "a\\b",
    "é",
    "caf\u{e9}",
    "\u{ff11}",
    "\u{0661}",
    "a\u{200b}",
    "a\u{2215}b",
];

#[tokio::test]
async fn hostile_drive_ids_are_rejected_before_any_request() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    for id in HOSTILE_IDS {
        let err = client
            .put_drive(&Drive::new(*id, false))
            .await
            .err()
            .ok_or_else(|| format!("drive id {id:?} was accepted"))?;
        let Error::InvalidId { id: reported } = &err else {
            return Err(format!("drive id {id:?}: expected InvalidId, got {err:?}").into());
        };
        assert_eq!(
            reported, id,
            "InvalidId must carry the offending id verbatim"
        );
    }
    assert!(
        server.history().is_empty(),
        "rejected ids reached the socket: {:?}",
        server.history()
    );
    Ok(())
}

#[tokio::test]
async fn hostile_iface_ids_are_rejected_before_any_request() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    for id in HOSTILE_IDS {
        let err = client
            .put_network_interface(&NetworkInterface::new(*id, "tap0"))
            .await
            .err()
            .ok_or_else(|| format!("iface id {id:?} was accepted"))?;
        let Error::InvalidId { id: reported } = &err else {
            return Err(format!("iface id {id:?}: expected InvalidId, got {err:?}").into());
        };
        assert_eq!(reported, id);
    }
    assert!(
        server.history().is_empty(),
        "rejected ids reached the socket: {:?}",
        server.history()
    );
    Ok(())
}

#[tokio::test]
async fn invalid_id_is_rejected_even_without_a_server() -> TestResult {
    let dir = tempdir()?;
    let client = FcClient::new(dir.path().join("nobody-listens.socket"));
    let err = client
        .put_drive(&Drive::new("../actions", false))
        .await
        .err()
        .ok_or("expected error")?;
    assert!(
        matches!(err, Error::InvalidId { .. }),
        "validation must run before connecting, got {err:?}"
    );
    Ok(())
}

#[tokio::test]
async fn id_boundaries_that_are_valid_are_accepted() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    let long = "x".repeat(255);
    let ids = [
        "a",
        "Z",
        "0",
        "-",
        "_",
        "--",
        "-leading",
        "trailing-",
        "9lives",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-",
        long.as_str(),
    ];
    for id in ids {
        client.put_drive(&Drive::new(id, false)).await?;
        client
            .put_network_interface(&NetworkInterface::new(id, "tap0"))
            .await?;
    }
    let paths: Vec<String> = server.history().into_iter().map(|r| r.path).collect();
    for id in ids {
        assert!(
            paths.contains(&format!("/drives/{id}")),
            "missing drive {id}"
        );
        assert!(
            paths.contains(&format!("/network-interfaces/{id}")),
            "missing iface {id}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn rejected_id_does_not_poison_the_client() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    assert!(client.put_drive(&Drive::new("a/b", false)).await.is_err());
    client.put_drive(&Drive::new("ok", false)).await?;
    let history = server.history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].path, "/drives/ok");
    Ok(())
}

#[tokio::test]
async fn api_errors_propagate_from_every_route() -> TestResult {
    let (server, client, _dir, sock) = server_and_client()?;
    let replies = [
        (StatusCode::BAD_REQUEST, "bad request body"),
        (StatusCode::NOT_FOUND, "no such thing"),
        (StatusCode::INTERNAL_SERVER_ERROR, "vmm exploded"),
    ];
    for case in cases() {
        for (status, fault) in replies {
            server.set_reply(
                status,
                json!({"fault_message": fault}).to_string().into_bytes(),
            );
            let before = server.history().len();
            let err = case
                .call
                .run(&client)
                .await
                .err()
                .ok_or_else(|| format!("{}: {status} was treated as success", case.name))?;
            let Error::Api {
                path,
                method,
                route,
                status: got,
                fault_message,
            } = err
            else {
                return Err(format!("{}: expected Error::Api", case.name).into());
            };
            assert_eq!(path, sock, "{}", case.name);
            assert_eq!(method, case.method, "{}", case.name);
            assert_eq!(route, case.path, "{}", case.name);
            assert_eq!(got, status.as_u16(), "{}", case.name);
            assert_eq!(fault_message, fault, "{}", case.name);
            assert_eq!(
                server.history().len(),
                before + 1,
                "{}: a failed call must not be retried",
                case.name
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn non_json_error_body_is_reported_verbatim() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    server.set_reply(StatusCode::BAD_GATEWAY, b"upstream gone".to_vec());
    let err = client.pause().await.err().ok_or("expected error")?;
    let Error::Api {
        status,
        fault_message,
        ..
    } = err
    else {
        return Err("expected Error::Api".into());
    };
    assert_eq!(status, 502);
    assert_eq!(fault_message, "upstream gone");
    Ok(())
}

#[tokio::test]
async fn success_with_a_body_is_still_success() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    server.set_reply(StatusCode::OK, b"{}".to_vec());
    for case in cases() {
        case.call.run(&client).await?;
    }
    Ok(())
}

#[tokio::test]
async fn missing_socket_gives_connect_error_on_every_route() -> TestResult {
    let dir = tempdir()?;
    let sock = dir.path().join("gone.socket");
    let client = FcClient::new(&sock);
    for case in cases() {
        let err = case.call.run(&client).await.err().ok_or("expected error")?;
        let Error::Connect {
            path,
            method,
            route,
            ..
        } = err
        else {
            return Err(format!("{}: expected Error::Connect", case.name).into());
        };
        assert_eq!(path, sock, "{}", case.name);
        assert_eq!(method, case.method, "{}", case.name);
        assert_eq!(route, case.path, "{}", case.name);
    }
    Ok(())
}

#[tokio::test]
async fn calls_reach_the_server_in_call_order() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    client.pause().await?;
    client
        .create_snapshot(&SnapshotCreateParams::full("/s/a.snap", "/s/a.mem"))
        .await?;
    client.resume().await?;

    let seen: Vec<(String, String, Value)> = server
        .history()
        .into_iter()
        .map(|r| Ok((r.method, r.path, serde_json::from_slice(&r.body)?)))
        .collect::<Result<_, serde_json::Error>>()?;
    assert_eq!(seen.len(), 3);
    assert_eq!(
        seen[0],
        ("PATCH".into(), "/vm".into(), json!({"state": "Paused"}))
    );
    assert_eq!(seen[1].1, "/snapshot/create");
    assert_eq!(
        seen[2],
        ("PATCH".into(), "/vm".into(), json!({"state": "Resumed"}))
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_calls_do_not_mix_ids_or_bodies() -> TestResult {
    let (server, client, _dir, _sock) = server_and_client()?;
    let d: Vec<Drive> = (0..4)
        .map(|i| Drive::new(format!("disk{i}"), false))
        .collect();
    let n: Vec<NetworkInterface> = (0..4)
        .map(|i| NetworkInterface::new(format!("nic{i}"), format!("tap{i}")))
        .collect();

    let results = tokio::join!(
        client.put_drive(&d[0]),
        client.put_network_interface(&n[0]),
        client.put_drive(&d[1]),
        client.put_network_interface(&n[1]),
        client.put_drive(&d[2]),
        client.put_network_interface(&n[2]),
        client.put_drive(&d[3]),
        client.put_network_interface(&n[3]),
    );
    results.0?;
    results.1?;
    results.2?;
    results.3?;
    results.4?;
    results.5?;
    results.6?;
    results.7?;

    let mut seen = BTreeSet::new();
    for r in server.history() {
        let body: Value = serde_json::from_slice(&r.body)?;
        if let Some(id) = r.path.strip_prefix("/drives/") {
            assert_eq!(body["drive_id"], id);
        } else {
            let id = r
                .path
                .strip_prefix("/network-interfaces/")
                .ok_or("unexpected path")?;
            assert_eq!(body["iface_id"], id);
            assert_eq!(body["host_dev_name"], id.replace("nic", "tap"));
        }
        assert!(seen.insert(r.path), "duplicate request");
    }
    assert_eq!(seen.len(), 8);
    server.shutdown().await;
    Ok(())
}

#[derive(Default)]
struct FieldMap(HashMap<String, String>);

impl Visit for FieldMap {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

#[derive(Clone, Default)]
struct Spans(Arc<Mutex<Vec<HashMap<String, String>>>>);

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Spans {
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        let mut fields = FieldMap::default();
        attrs.record(&mut fields);
        if let Ok(mut spans) = self.0.lock() {
            spans.push(fields.0);
        }
    }
}

#[tokio::test]
async fn every_route_is_traced_with_method_and_concrete_path_even_on_failure() -> TestResult {
    let spans = Spans::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(spans.clone()));
    let (server, client, _dir, _sock) = server_and_client()?;

    for ok in [true, false] {
        if ok {
            server.set_reply(StatusCode::NO_CONTENT, vec![]);
        } else {
            server.set_reply(
                StatusCode::BAD_REQUEST,
                br#"{"fault_message":"x"}"#.to_vec(),
            );
        }
        for case in cases() {
            let _ = case.call.run(&client).await;
            let captured = spans.0.lock().map_err(|e| e.to_string())?.clone();
            assert!(
                captured
                    .iter()
                    .any(|f| f.get("method").map(String::as_str) == Some(case.method)
                        && f.get("route").map(String::as_str) == Some(case.path)),
                "{} (ok={ok}): no span with method={} route={}; got {captured:?}",
                case.name,
                case.method,
                case.path
            );
        }
    }
    let captured = spans.0.lock().map_err(|e| e.to_string())?.clone();
    assert!(
        captured
            .iter()
            .filter_map(|f| f.get("route"))
            .all(|r| !r.contains('{') && !r.contains("drive_id") && !r.contains("iface_id")),
        "a span recorded a route template instead of the concrete path: {captured:?}"
    );
    Ok(())
}
