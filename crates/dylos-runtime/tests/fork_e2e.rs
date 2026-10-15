#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dylos_core::LabSpec;
use dylos_runtime::jailer::JailerConfig;
use dylos_runtime::{Lab, LabConfig};
use futures_util::FutureExt;
use futures_util::future::join_all;

const A_V6: &str = "fd64:796c:6f73:1::2";
const A_V4: &str = "10.0.1.2";
const C_V6: &str = "fd64:796c:6f73:2::2";
const C_V4: &str = "10.0.2.2";
// A host address in a clone's netns, on the left segment: a destination A has never seen.
const HOST_V6: &str = "fd64:796c:6f73:1::fe";
const HOST_V4: &str = "10.0.1.254";

fn netns(config: &LabConfig, lab: &Lab) -> PathBuf {
    config.netns_dir.join(format!("dylos-{}", lab.id()))
}

async fn host(netns: Option<&Path>, args: &[&str]) -> std::process::Output {
    let mut command = if let Some(ns) = netns {
        let mut c = tokio::process::Command::new("nsenter");
        c.arg(format!("--net={}", ns.display())).args(args);
        c
    } else {
        let mut c = tokio::process::Command::new(args[0]);
        c.args(&args[1..]);
        c
    };
    command.output().await.unwrap()
}

async fn guest(lab: &Lab, node: &str, command: &str) -> String {
    let out = lab
        .exec(node, command, Duration::from_secs(10))
        .await
        .unwrap_or_else(|e| panic!("{} {node}: {command}: {e}", lab.id()));
    assert!(out.success(), "{} {node}: {command}: {out:?}", lab.id());
    out.stdout
}

/// Guest neighbor tables and link counters, and the clone netns links and bridge FDB.
async fn capture(config: &LabConfig, lab: &Lab, label: &str) -> String {
    let mut out = format!("==== {} {label} ====\n", lab.id());
    for node in ["A", "B", "C"] {
        let text = match lab
            .exec(
                node,
                "ip -s -4 neigh show; ip -s -6 neigh show; ip -s link show",
                Duration::from_secs(10),
            )
            .await
        {
            Ok(o) => o.stdout,
            Err(e) => format!("exec error: {e}"),
        };
        writeln!(out, "-- guest {node}:\n{text}").unwrap();
    }
    let ns = netns(config, lab);
    for args in [
        &["ip", "-s", "link", "show"][..],
        &["bridge", "fdb", "show"],
    ] {
        let o = host(Some(&ns), args).await;
        writeln!(
            out,
            "-- host {}:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
        .unwrap();
    }
    out
}

/// One echo from `from`, which must be answered: no retry.
async fn ping(config: &LabConfig, lab: &Lab, from: &str, family: &str, to: &str) {
    let command = format!("ping {family} -c 1 -W 5 {to} 2>&1");
    let out = lab
        .exec(from, &command, Duration::from_secs(10))
        .await
        .unwrap_or_else(|e| panic!("{} {from}: {command}: {e}", lab.id()));
    if !out.success() {
        let state = capture(config, lab, "after failed ping").await;
        panic!("{} {from}: {command}: {out:?}\n{state}", lab.id());
    }
}

async fn dual_stack(config: &LabConfig, lab: &Lab, from: &str, to_v6: &str, to_v4: &str) {
    ping(config, lab, from, "-6", to_v6).await;
    ping(config, lab, from, "-4", to_v4).await;
}

/// Echo requests received, (`IPv6`, `IPv4`), from the guest's SNMP counters.
async fn echoes(lab: &Lab, node: &str) -> (u64, u64) {
    let text = guest(lab, node, "cat /proc/net/snmp6 /proc/net/snmp").await;
    let v6 = text
        .lines()
        .find_map(|l| l.strip_prefix("Icmp6InEchos"))
        .map(|v| v.trim().parse().unwrap())
        .expect("Icmp6InEchos");
    let mut icmp = text.lines().filter(|l| l.starts_with("Icmp: "));
    let (names, values) = (icmp.next().unwrap(), icmp.next().unwrap());
    let column = names
        .split_whitespace()
        .position(|n| n == "InEchos")
        .unwrap();
    let v4 = values
        .split_whitespace()
        .nth(column)
        .unwrap()
        .parse()
        .unwrap();
    (v6, v4)
}

/// Echo counters of A and C in every clone.
async fn all_echoes(clones: &[Lab]) -> Vec<[(u64, u64); 2]> {
    join_all(
        clones
            .iter()
            .map(|c| async { [echoes(c, "A").await, echoes(c, "C").await] }),
    )
    .await
}

/// Asserts that only clone `target` (if any) received echoes between `before` and `after`.
fn assert_echoes_only_in(
    clones: &[Lab],
    before: &[[(u64, u64); 2]],
    after: &[[(u64, u64); 2]],
    target: Option<usize>,
) {
    for (i, clone) in clones.iter().enumerate() {
        for (n, node) in ["A", "C"].iter().enumerate() {
            let (b, a) = (before[i][n], after[i][n]);
            let delta = (a.0 - b.0, a.1 - b.1);
            if Some(i) == target {
                assert!(
                    delta.0 >= 1 && delta.1 >= 1,
                    "{} {node} missed its own echoes (IPv6, IPv4): {delta:?}",
                    clone.id()
                );
            } else {
                assert_eq!(
                    delta,
                    (0, 0),
                    "{} {node} received echoes sent to another netns (IPv6, IPv4)",
                    clone.id()
                );
            }
        }
    }
}

/// Firecracker net device counters since the previous flush, per VM and interface.
async fn fc_net(lab: &Lab) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for vm in lab.vms() {
        let client = dylos_fc::FcClient::new(vm.paths().api_socket());
        client
            .put::<_, serde_json::Value>(
                "/actions",
                &dylos_fc::config::InstanceActionInfo::new(
                    dylos_fc::config::ActionType::FlushMetrics,
                ),
            )
            .await
            .unwrap();
        let text = std::fs::read_to_string(vm.paths().root.join("run/metrics.json")).unwrap();
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        for (device, fields) in last.as_object().unwrap() {
            if !device.starts_with("net_") {
                continue;
            }
            for (field, value) in fields.as_object().unwrap() {
                if let Some(n) = value.as_u64().filter(|n| *n > 0) {
                    out.insert(format!("{}.{device}.{field}", vm.node()), n);
                }
            }
        }
    }
    out
}

/// Guest IP and ICMP SNMP counters of every node.
async fn snmp(lab: &Lab) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for node in ["A", "B", "C"] {
        let text = guest(lab, node, "cat /proc/net/snmp").await;
        let lines: Vec<_> = text.lines().collect();
        for pair in lines.windows(2) {
            let (names, values) = (pair[0], pair[1]);
            for prefix in ["Ip:", "Icmp:"] {
                let header = names
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|n| n.parse::<u64>().is_err());
                if header && names.starts_with(prefix) && values.starts_with(prefix) {
                    for (n, v) in names
                        .split_whitespace()
                        .zip(values.split_whitespace())
                        .skip(1)
                    {
                        if let Ok(v) = v.parse::<u64>() {
                            out.insert(format!("{node}.{prefix}{n}"), v);
                        }
                    }
                }
            }
        }
    }
    out
}

fn delta(before: &BTreeMap<String, u64>, after: &BTreeMap<String, u64>) -> String {
    after
        .iter()
        .filter_map(|(k, v)| {
            let d = v.saturating_sub(before.get(k).copied().unwrap_or(0));
            (d > 0).then(|| format!("{k}+{d}"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Default, Clone, Copy)]
#[allow(dead_code)] // Read through Debug in the logs.
struct Backing {
    rss: u64,
    pss: u64,
    shared_clean: u64,
    private_clean: u64,
    private_dirty: u64,
}

/// KiB accounting of the mappings of inode `ino` (the shared vm.mem) in process `pid`.
fn backing(pid: u32, ino: u64) -> Backing {
    let smaps = std::fs::read_to_string(format!("/proc/{pid}/smaps")).unwrap();
    let mut account = Backing::default();
    let mut inside = false;
    for line in smaps.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        // A mapping header: address perms offset dev inode [path]
        if fields.len() >= 5 && fields[0].contains('-') && !fields[0].ends_with(':') {
            inside = fields[4].parse::<u64>().is_ok_and(|i| i == ino);
            continue;
        }
        if !inside || fields.len() < 2 {
            continue;
        }
        let kib: u64 = fields[1].parse().unwrap_or(0);
        match fields[0] {
            "Rss:" => account.rss += kib,
            "Pss:" => account.pss += kib,
            "Shared_Clean:" => account.shared_clean += kib,
            "Private_Clean:" => account.private_clean += kib,
            "Private_Dirty:" => account.private_dirty += kib,
            _ => {}
        }
    }
    account
}

fn backing_of(lab: &Lab) -> BTreeMap<String, Backing> {
    lab.vms()
        .iter()
        .map(|vm| {
            let ino = std::fs::metadata(vm.paths().root.join("vm.mem"))
                .unwrap()
                .ino();
            (vm.node().to_owned(), backing(vm.pid().unwrap(), ino))
        })
        .collect()
}

async fn down(config: &LabConfig, lab: Lab) {
    let (id, socket) = (lab.id().to_owned(), lab.control_socket());
    let server = tokio::spawn(lab.supervise());
    Lab::request_teardown(&socket, Duration::from_secs(30))
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    assert!(!config.store.base.join(&id).exists());
    assert!(!config.netns_dir.join(format!("dylos-{id}")).exists());
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn fork_ten_simultaneous_clones_independent_and_isolated() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let Some(bin) = std::env::var_os("DYLOS_FC_BIN_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED fork_e2e: DYLOS_FC_BIN_DIR missing");
        return;
    };
    let kernel = workspace.join("kernels/vmlinux.bin");
    let image = workspace.join("target/images/rootfs.ext4");
    if std::fs::metadata("/proc/self").unwrap().uid() != 0
        || [
            Path::new("/dev/kvm"),
            &kernel,
            &image,
            &bin.join("jailer"),
            &bin.join("firecracker"),
        ]
        .iter()
        .any(|p| !p.exists())
    {
        eprintln!("SKIPPED fork_e2e: requires root, KVM and built images/binaries");
        return;
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut jailer = JailerConfig::new(dir.path().join("j"), 65534, 65534);
    jailer.jailer = bin.join("jailer");
    jailer.firecracker = bin.join("firecracker");
    let config = LabConfig {
        store: dylos_store::Store {
            base: dir.path().join("labs"),
            image,
        },
        jailer,
        kernel,
        netns_dir: dir.path().join("netns"),
        ..LabConfig::default()
    };

    // Sized to fit the 4 vCPU, 16 GB runner: 10 clones of three 128 MiB, 1 vCPU nodes.
    let mut spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    for node in &mut spec.nodes {
        node.memory = 128;
        node.vcpus = 1;
    }

    let mut original = Lab::up_with(&config, &spec, "src").await.unwrap();

    let result = std::panic::AssertUnwindSafe(async {
        dual_stack(&config, &original, "A", C_V6, C_V4).await;
        let manifest = original.snapshot("snap").await.unwrap();
        let snapshot = config.store.snapshot_dir("src", "snap");
        // Clones must not depend on the source lab.
        original.teardown().await.unwrap();
        assert!(!config.store.base.join("src").exists());
        assert!(!config.netns_dir.join("dylos-src").exists());

        let clone_ids = Lab::generate_clone_ids("src", 10).unwrap();
        let expected: Vec<_> = (1..=10).map(|i| format!("src-c{i}")).collect();
        assert_eq!(clone_ids, expected);
        let started = Instant::now();
        let mut clones = Lab::fork_with(&config, &snapshot, &clone_ids)
            .await
            .unwrap();
        eprintln!(
            "Forked {} clones in {} ms total",
            clones.len(),
            started.elapsed().as_millis()
        );
        for clone in &clones {
            let latency = clone.restore_latency().expect("netns to thaw latency");
            eprintln!(
                "Clone {}: netns to thaw = {} ms (target < 500 ms)",
                clone.id(),
                latency.as_millis()
            );
        }

        // First-packet investigation: one IPv4 probe per clone right after the fork.
        let mut lost_probes = 0;
        for clone in &clones {
            let fc_restore = fc_net(clone).await;
            let snmp_before = snmp(clone).await;
            let out = clone
                .exec(
                    "A",
                    "ping -4 -c 1 -W 5 10.0.2.2 2>&1",
                    Duration::from_secs(10),
                )
                .await
                .unwrap();
            let fc_probe = fc_net(clone).await;
            let snmp_after = snmp(clone).await;
            eprintln!("PROBE {} exit={}", clone.id(), out.exit_code);
            eprintln!("FC-SINCE-RESTORE {} {fc_restore:?}", clone.id());
            eprintln!("FC-PROBE {} {fc_probe:?}", clone.id());
            eprintln!(
                "SNMP-PROBE {} {}",
                clone.id(),
                delta(&snmp_before, &snmp_after)
            );
            if !out.success() {
                lost_probes += 1;
                eprintln!("{}", capture(&config, clone, "after-lost-probe").await);
            }
        }
        eprintln!(
            "PROBE summary: {lost_probes}/{} first IPv4 probes lost",
            clones.len()
        );

        // Shared, immutable memory backing; private state.
        for node in ["A", "B", "C"] {
            let file = |c: &Lab, name: &str| {
                std::fs::metadata(c.vm(node).unwrap().paths().root.join(name)).unwrap()
            };
            let shared = file(&clones[0], "vm.mem");
            let mut states = HashSet::new();
            for clone in &clones {
                let meta = file(clone, "vm.mem");
                assert_eq!(meta.ino(), shared.ino(), "{} {node}", clone.id());
                assert_eq!(meta.mode() & 0o7777, 0o444, "{} {node}", clone.id());
                assert_eq!(meta.uid(), 0, "{} {node}", clone.id());
                assert!(states.insert(file(clone, "vm.state").ino()));
            }
        }

        // One distinct netns per clone, and no lab device in the host netns.
        let mut namespaces = HashSet::new();
        for clone in &clones {
            let ino = std::fs::metadata(netns(&config, clone)).unwrap().ino();
            assert!(namespaces.insert(ino), "{}", clone.id());
        }
        for device in ["br-left", "br-right", "tap-A-eth0", "tap-C-eth0"] {
            let o = host(None, &["ip", "link", "show", device]).await;
            assert!(!o.status.success(), "{device} visible in the host netns");
        }

        // Every clone works on its own: one echo per family, no retry (ADR-0002: IPv6 first).
        for clone in &clones {
            dual_stack(&config, clone, "A", C_V6, C_V4).await;
        }

        // The host netns reaches no clone.
        let before = all_echoes(&clones).await;
        for (family, to) in [("-6", A_V6), ("-4", A_V4), ("-6", C_V6), ("-4", C_V4)] {
            let o = host(None, &["ping", family, "-c", "1", "-W", "1", to]).await;
            assert!(!o.status.success(), "host netns reached {to}");
        }
        let mut last = all_echoes(&clones).await;
        assert_echoes_only_in(&clones, &before, &last, None);

        // From inside each netns, the host reaches only that clone, and that clone's A reaches
        // only its own C, although every clone has the same addresses and MACs.
        for (i, clone) in clones.iter().enumerate() {
            let ns = netns(&config, clone);
            let (v6, v4) = (format!("{HOST_V6}/64"), format!("{HOST_V4}/24"));
            for args in [
                &["ip", "-6", "addr", "add", &v6, "dev", "br-left", "nodad"][..],
                &["ip", "-4", "addr", "add", &v4, "dev", "br-left"],
            ] {
                let o = host(Some(&ns), args).await;
                assert!(o.status.success(), "{}: {o:?}", clone.id());
            }
            let cold = format!("ip neigh show {HOST_V6}; ip neigh show {HOST_V4}");
            let neigh = guest(clone, "A", &cold).await;
            assert!(neigh.trim().is_empty(), "{}: not cold: {neigh}", clone.id());
            for (family, to) in [("-6", A_V6), ("-4", A_V4)] {
                let args = ["ping", family, "-c", "1", "-W", "5", "-I", "br-left", to];
                let o = host(Some(&ns), &args).await;
                if !o.status.success() {
                    let state = capture(&config, clone, "after failed host ping").await;
                    panic!("{} host -> {to}: {o:?}\n{state}", clone.id());
                }
            }
            dual_stack(&config, clone, "A", C_V6, C_V4).await;
            let now = all_echoes(&clones).await;
            assert_echoes_only_in(&clones, &last, &now, Some(i));
            last = now;
        }

        // PSS of every clone after the same workload (N = 10).
        let mut totals = Vec::new();
        for clone in &clones {
            let pss = clone.measure_pss().await.unwrap();
            let total: u64 = pss.iter().map(|(_, kib)| kib).sum();
            eprintln!("PSS N=10 {}: total {total} KiB, per VM {pss:?}", clone.id());
            totals.push(total);
        }
        let mut sorted = totals.clone();
        sorted.sort_unstable();
        eprintln!(
            "PSS N=10 all clones: sum {} KiB, min {} KiB, median {} KiB, max {} KiB",
            sorted.iter().sum::<u64>(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1]
        );
        let survivor = clones.len() - 1;
        let n10 = totals[survivor];
        let shared_backing = backing_of(&clones[survivor]);
        eprintln!(
            "vm.mem mappings N=10 {}: {shared_backing:?}",
            clones[survivor].id()
        );
        for (node, b) in &shared_backing {
            assert!(b.shared_clean > 0, "{node}: nothing shared at N=10: {b:?}");
        }

        // Snapshot a clone while its siblings run: new files, shared backing untouched.
        let shared_path = clones[1].vm("A").unwrap().paths().root.join("vm.mem");
        let stamp = |m: &std::fs::Metadata| (m.ino(), m.len(), m.mtime(), m.mtime_nsec(), m.mode());
        let shared_before = stamp(&std::fs::metadata(&shared_path).unwrap());
        let clone_manifest = clones[1].snapshot("csnap").await.unwrap();
        let clone_snapshot = config.store.snapshot_dir(clones[1].id(), "csnap");
        let opener = dylos_store::SnapshotOpener::new(&clone_snapshot).unwrap();
        clone_manifest.verify(opener.verify_opener()).unwrap();
        let taken = std::fs::metadata(clone_snapshot.join("vms/A/vm.mem")).unwrap();
        assert_ne!(taken.ino(), shared_before.0);
        assert_eq!(
            stamp(&std::fs::metadata(&shared_path).unwrap()),
            shared_before
        );
        let opener = dylos_store::SnapshotOpener::new(&snapshot).unwrap();
        manifest.verify(opener.verify_opener()).unwrap();
        dual_stack(&config, &clones[1], "A", C_V6, C_V4).await;

        // Independent teardown of nine clones; the survivor runs nothing until measured again.
        let last_clone = clones.pop().unwrap();
        for clone in clones.drain(..) {
            down(&config, clone).await;
        }

        // Same clone, same workload stage, now alone (N = 1).
        let pss = last_clone.measure_pss().await.unwrap();
        let n1: u64 = pss.iter().map(|(_, kib)| kib).sum();
        let alone_backing = backing_of(&last_clone);
        eprintln!("vm.mem mappings N=1 {}: {alone_backing:?}", last_clone.id());
        eprintln!(
            "PSS {} N=10 {n10} KiB -> N=1 {n1} KiB ({:.2}x)",
            last_clone.id(),
            n1 as f64 / n10.max(1) as f64
        );
        assert!(
            n1 > n10,
            "sharing must lower PSS: N=10 {n10} KiB, N=1 {n1} KiB"
        );

        dual_stack(&config, &last_clone, "A", C_V6, C_V4).await;
        down(&config, last_clone).await;
        let left: Vec<_> = std::fs::read_dir(&config.netns_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "netns left: {left:?}");
        let opener = dylos_store::SnapshotOpener::new(&snapshot).unwrap();
        manifest.verify(opener.verify_opener()).unwrap();
    })
    .catch_unwind()
    .await;

    if config.store.base.join("src").exists() {
        let _ = original.teardown().await;
    }
    assert!(!config.store.base.join("src").exists());
    assert!(!config.netns_dir.join("dylos-src").exists());

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
