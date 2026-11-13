//! Benchmarks for Dylos technical spike.
//!
//! Measures cold boot latency, snapshot freeze-to-thaw pause window, per-stage breakdown,
//! total publication timing, parallel clone fork latencies (fork -n 1, 10, 20), and per-clone
//! PSS once its reading settles, on a 5-VM lab (256 MiB each).
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, DirBuilder, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use dylos_core::LabSpec;
use dylos_runtime::jailer::{JailPaths, JailerConfig};
use dylos_runtime::{Lab, LabConfig, LabError, MAX_UNIX_SOCKET_PATH, validate_unix_socket_path};
use futures_util::future::join_all;

const DEFAULT_REPETITIONS_FULL: usize = 20;
const DEFAULT_REPETITIONS_QUICK: usize = 2;
const DEFAULT_FORK_COUNTS: &[usize] = &[1, 10, 20];
const FIVE_VM_SPEC_PATH: &str = "labs/five.yaml";
const KERNEL_PATH: &str = "kernels/vmlinux.bin";
const ROOTFS_PATH: &str = "target/images/rootfs.ext4";
const NOBODY_UID_GID: u32 = 65534;

/// Configuration options for the benchmark run.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub lab: PathBuf,
    pub working_set_mib: u64,
    pub quick: bool,
    pub repetitions: usize,
    pub fork_counts: Vec<usize>,
    pub output_path: Option<PathBuf>,
    pub tmp_dir: Option<PathBuf>,
    pub print_help: bool,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            lab: FIVE_VM_SPEC_PATH.into(),
            working_set_mib: 0,
            quick: false,
            repetitions: DEFAULT_REPETITIONS_FULL,
            fork_counts: DEFAULT_FORK_COUNTS.to_vec(),
            output_path: None,
            tmp_dir: None,
            print_help: false,
        }
    }
}

/// Parses command line arguments for the bench command.
pub fn parse_bench_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<BenchOptions> {
    let mut options = BenchOptions::default();
    let mut repetitions_explicit = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lab" => options.lab = args.next().context("missing value for --lab")?.into(),
            "--working-set-mib" => {
                options.working_set_mib = args
                    .next()
                    .context("missing value for --working-set-mib")?
                    .parse()
                    .context("invalid --working-set-mib")?;
            }
            "--quick" => {
                options.quick = true;
                if !repetitions_explicit {
                    options.repetitions = DEFAULT_REPETITIONS_QUICK;
                }
            }
            "--repetitions" => {
                let val = args
                    .next()
                    .context("missing argument value for --repetitions")?;
                options.repetitions = val
                    .parse::<usize>()
                    .context("invalid integer value for --repetitions")?;
                if options.repetitions == 0 {
                    bail!("--repetitions must be at least 1");
                }
                repetitions_explicit = true;
            }
            "--fork-counts" => {
                let val = args
                    .next()
                    .context("missing argument value for --fork-counts")?;
                let mut counts = Vec::new();
                for part in val.split(',') {
                    let count = part
                        .trim()
                        .parse::<usize>()
                        .with_context(|| format!("invalid fork count '{part}'"))?;
                    if count == 0 {
                        bail!("fork count must be at least 1");
                    }
                    counts.push(count);
                }
                if counts.is_empty() {
                    bail!("--fork-counts cannot be empty");
                }
                options.fork_counts = counts;
            }
            "--output" => {
                let val = args.next().context("missing argument value for --output")?;
                options.output_path = Some(PathBuf::from(val));
            }
            "--tmp-dir" => {
                let val = args
                    .next()
                    .context("missing argument value for --tmp-dir")?;
                options.tmp_dir = Some(PathBuf::from(val));
            }
            "-h" | "--help" => {
                options.print_help = true;
                return Ok(options);
            }
            other => {
                bail!("unknown benchmark argument '{other}'; use --help for usage");
            }
        }
    }

    Ok(options)
}

/// Prints benchmark command usage.
pub fn print_usage() {
    println!(
        "Usage: cargo xtask bench [OPTIONS]\n\
         \n\
         Runs quantified benchmarks for the Dylos technical spike on a selected lab.\n\
         Measures cold boot latency, snapshot freeze/pause/total timings, parallel\n\
         clone fork latencies (fork -n 1, 10, 20), and per-clone PSS after it settles.\n\
         \n\
         Options:\n\
           --lab <PATH>          LabSpec inside repository labs/ (default labs/five.yaml)\n\
           --working-set-mib <N>  Resident non-zero MiB per guest; rewrite 25% before later snapshots (default 0)\n\
           --quick                Run in reduced mode (default {DEFAULT_REPETITIONS_QUICK} repetitions) for CI verification\n\
           --repetitions <N>      Number of repetitions per measurement (default: {DEFAULT_REPETITIONS_FULL}, or {DEFAULT_REPETITIONS_QUICK} with --quick)\n\
           --fork-counts <LIST>   Comma-separated clone counts to benchmark (default: 1,10,20)\n\
           --output <PATH>        Write Markdown report directly to specified file\n\
           --tmp-dir <PATH>       Parent for the private run directory (must support reflink; it and its ancestors must be operator-controlled; only the run child is removed)\n\
           -h, --help             Show this help message\n"
    );
}

/// Resolve symlinks before enforcing the repository labs boundary.
pub fn validate_lab_path(workspace: &Path, path: &Path) -> anyhow::Result<PathBuf> {
    let labs = workspace.join("labs").canonicalize()?;
    let resolved = workspace
        .join(path)
        .canonicalize()
        .with_context(|| format!("cannot resolve lab {}", path.display()))?;
    if !resolved.starts_with(labs) || !resolved.is_file() {
        bail!(
            "lab must resolve to a file inside repository labs/: {}",
            resolved.display()
        );
    }
    Ok(resolved)
}

/// Clone private memory is conservatively bounded by all configured guest RAM.
pub fn preflight_estimate_mib(spec: &LabSpec, options: &BenchOptions) -> anyhow::Result<u64> {
    let mut configured = 0_u64;
    let mut resident = 0_u64;
    for node in &spec.nodes {
        if options.working_set_mib > u64::from(node.memory).saturating_sub(128) {
            bail!(
                "working set {} MiB leaves less than 128 MiB guest headroom on {} ({} MiB)",
                options.working_set_mib,
                node.name,
                node.memory
            );
        }
        configured += u64::from(node.memory);
        resident = resident
            .checked_add(options.working_set_mib)
            .context("working set overflow")?;
    }
    let clones = u64::try_from(options.fork_counts.iter().copied().max().unwrap_or(0))?;
    resident
        .checked_mul(2)
        .and_then(|v| {
            configured
                .checked_mul(clones)
                .and_then(|c| v.checked_add(c))
        })
        .and_then(|v| v.checked_add(1024))
        .context("memory estimate overflow")
}

fn available_mib() -> anyhow::Result<u64> {
    let info = fs::read_to_string("/proc/meminfo")?;
    let value = info
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))
        .context("MemAvailable missing from /proc/meminfo")?;
    Ok(value
        .split_whitespace()
        .next()
        .context("missing MemAvailable value")?
        .parse::<u64>()?
        / 1024)
}

#[derive(Default)]
struct MemoryWatch {
    reason: Mutex<Option<String>>,
    samples: Mutex<String>,
}

impl MemoryWatch {
    fn check(&self) -> anyhow::Result<()> {
        if let Some(reason) = &*self.reason.lock().map_err(|e| anyhow::anyhow!("{e}"))? {
            bail!("ABORTED: {reason}");
        }
        Ok(())
    }

    fn record(&self, sample: &str) -> anyhow::Result<()> {
        self.samples
            .lock()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .push_str(sample);
        Ok(())
    }
}

fn lab_summary(spec: &LabSpec, options: &BenchOptions) -> anyhow::Result<String> {
    let configured: Vec<_> = spec
        .nodes
        .iter()
        .map(|n| format!("{}: {} MiB", n.name, n.memory))
        .collect();
    Ok(format!(
        "Lab: {}\n\nVM count: {}; configured RAM: {}; total: {} MiB.\nWorking set: {} MiB per VM; later snapshots rewrite the first 25% (rounded up to a page).\nPeak preflight estimate: {} MiB (2 x resident working set + largest fork count x configured RAM + 1024 MiB).\nCapture/buffering share unavailable: SnapshotTimings exposes only aggregate storage and API stages.\n\n",
        options.lab.display(),
        spec.nodes.len(),
        configured.join(", "),
        spec.nodes.iter().map(|n| u64::from(n.memory)).sum::<u64>(),
        options.working_set_mib,
        preflight_estimate_mib(spec, options)?
    ))
}

/// Host hardware, kernel, filesystem, and tool configuration.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub cpu_model: String,
    pub vcpu_count: usize,
    pub ram_kib: u64,
    pub ram_gib: f64,
    pub kernel_release: String,
    pub filesystem: String,
    pub reflink_supported: bool,
    pub firecracker_version: String,
}

impl HostConfig {
    /// Detects current host environment configuration.
    pub fn detect(fc_bin: &Path, test_dir: &Path) -> anyhow::Result<Self> {
        let (cpu_model, vcpu_count) = read_cpu_info()?;
        let ram_kib = read_ram_kib()?;
        let ram_gib = ram_kib as f64 / (1024.0 * 1024.0);
        let kernel_release = read_kernel_release()?;
        let (filesystem, reflink_supported) = detect_filesystem(test_dir)?;
        let firecracker_version = detect_firecracker_version(fc_bin)?;

        Ok(Self {
            cpu_model,
            vcpu_count,
            ram_kib,
            ram_gib,
            kernel_release,
            filesystem,
            reflink_supported,
            firecracker_version,
        })
    }
}

fn read_cpu_info() -> anyhow::Result<(String, usize)> {
    let content = fs::read_to_string("/proc/cpuinfo").context("could not read /proc/cpuinfo")?;
    let mut model = String::from("Unknown CPU");
    let mut processors = 0;

    for line in content.lines() {
        if line.starts_with("model name")
            && let Some((_, val)) = line.split_once(':')
            && model == "Unknown CPU"
        {
            val.trim().clone_into(&mut model);
        }
        if line.starts_with("processor") {
            processors += 1;
        }
    }

    Ok((model, processors.max(1)))
}

fn read_ram_kib() -> anyhow::Result<u64> {
    let content = fs::read_to_string("/proc/meminfo").context("could not read /proc/meminfo")?;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            let mut parts = line.split_whitespace();
            let _prefix = parts.next();
            if let Some(num) = parts.next() {
                let kib = num
                    .parse::<u64>()
                    .context("invalid integer in /proc/meminfo MemTotal")?;
                return Ok(kib);
            }
        }
    }
    bail!("MemTotal not found in /proc/meminfo");
}

fn read_kernel_release() -> anyhow::Result<String> {
    let output = Command::new("uname")
        .arg("-r")
        .output()
        .context("could not run uname -r")?;
    if !output.status.success() {
        bail!("uname -r returned exit code {:?}", output.status.code());
    }
    let rel = String::from_utf8(output.stdout)
        .context("invalid utf-8 from uname -r")?
        .trim()
        .to_owned();
    Ok(rel)
}

fn detect_filesystem(test_dir: &Path) -> anyhow::Result<(String, bool)> {
    let output = Command::new("df")
        .args(["-T", test_dir.to_str().unwrap_or(".")])
        .output()
        .context("could not run df -T")?;

    let mut fs_type = String::from("unknown");
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = stdout.lines().nth(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 2 {
                cols[1].clone_into(&mut fs_type);
            }
        }
    }

    // Verify reflink capability with a small test file
    let src = test_dir.join("dylos_reflink_src.tmp");
    let dst = test_dir.join("dylos_reflink_dst.tmp");
    let reflink_supported = match fs::write(&src, b"reflink test") {
        Ok(()) => {
            let res = Command::new("cp")
                .arg("--reflink=always")
                .arg(&src)
                .arg(&dst)
                .output();
            let _ = fs::remove_file(&src);
            let _ = fs::remove_file(&dst);
            res.is_ok_and(|out| out.status.success())
        }
        Err(_) => false,
    };

    Ok((fs_type, reflink_supported))
}

fn detect_firecracker_version(fc_bin: &Path) -> anyhow::Result<String> {
    let output = Command::new(fc_bin)
        .arg("--version")
        .output()
        .with_context(|| format!("could not run {} --version", fc_bin.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(line) = stdout.lines().next() {
        return Ok(line.trim().to_owned());
    }
    Ok(String::from("Firecracker (version unknown)"))
}

/// Collection of float samples for calculating median, p95, min, max, mean.
#[derive(Debug, Clone, Default)]
pub struct MetricSamples {
    pub values: Vec<f64>,
}

impl MetricSamples {
    pub fn new() -> Self {
        Self { values: Vec::new() }
    }

    pub fn add(&mut self, val: f64) {
        self.values.push(val);
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn count(&self) -> usize {
        self.values.len()
    }

    pub fn min(&self) -> f64 {
        self.values.iter().copied().fold(f64::NAN, f64::min)
    }

    pub fn max(&self) -> f64 {
        self.values.iter().copied().fold(f64::NAN, f64::max)
    }

    pub fn mean(&self) -> f64 {
        if self.values.is_empty() {
            return 0.0;
        }
        self.values.iter().sum::<f64>() / self.values.len() as f64
    }

    pub fn median(&self) -> f64 {
        percentile(&self.values, 0.50)
    }

    pub fn p95(&self) -> f64 {
        percentile(&self.values, 0.95)
    }
}

/// Computes percentile (0.0 to 1.0) using standard linear interpolation.
pub fn percentile(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    if values.len() == 1 {
        return values[0];
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = pct.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    let weight = rank - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

/// README fork criterion: one clone ready in under this many milliseconds.
const CLONE_BUDGET_MS: f64 = 500.0;
/// README snapshot criterion: pause window (freeze to thaw) in under this many milliseconds.
const SNAPSHOT_BUDGET_MS: f64 = 1000.0;
const FREEZE_BUDGET_MS: f64 = 1000.0;
const SKEW_BUDGET_MS: f64 = 100.0;
const PSS_BUDGET_MIB: f64 = 256.0;

/// True when `samples` holds data and its p95 is under `limit`; missing data never passes.
fn p95_under(samples: Option<&MetricSamples>, limit: f64) -> bool {
    samples.is_some_and(|s| !s.is_empty() && s.p95() < limit)
}

fn go(ok: bool) -> &'static str {
    if ok { "GO" } else { "NO-GO" }
}

fn pass(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

/// Median and p95 of `samples` divided by `divisor`, or "no data" when there are none.
fn stat(
    samples: Option<&MetricSamples>,
    divisor: f64,
    precision: usize,
    unit: &str,
) -> (String, String) {
    match samples.filter(|s| !s.is_empty()) {
        Some(s) => (
            format!("{:.precision$} {unit}", s.median() / divisor),
            format!("{:.precision$} {unit}", s.p95() / divisor),
        ),
        None => ("no data".into(), "no data".into()),
    }
}

/// One line of the results table, shared by the console and Markdown renderers.
pub struct Row {
    pub label: String,
    pub threshold: String,
    pub median: String,
    pub p95: String,
    pub verdict: String,
}

impl Row {
    fn new(label: &str, threshold: &str, (median, p95): (String, String), verdict: &str) -> Self {
        Self {
            label: label.into(),
            threshold: threshold.into(),
            median,
            p95,
            verdict: verdict.into(),
        }
    }
}

/// Detailed stage breakdown for the snapshot operation across all lab VMs.
#[derive(Debug, Clone, Default)]
pub struct SnapshotStages {
    pub freeze_ms: MetricSamples,
    pub pause_ms: MetricSamples,
    pub api_snapshot_ms: MetricSamples,
    pub storage_ms: MetricSamples,
    pub resume_ms: MetricSamples,
    pub thaw_ms: MetricSamples,
    pub hash_ms: MetricSamples,
    pub manifest_ms: MetricSamples,
}

impl SnapshotStages {
    pub fn as_slice(&self) -> [(&'static str, &MetricSamples); 8] {
        [
            ("freeze", &self.freeze_ms),
            ("pause", &self.pause_ms),
            ("API snapshot", &self.api_snapshot_ms),
            ("storage", &self.storage_ms),
            ("resume", &self.resume_ms),
            ("thaw", &self.thaw_ms),
            ("hashing", &self.hash_ms),
            ("manifest write", &self.manifest_ms),
        ]
    }

    /// Returns the name and p95 duration of the stage with the largest p95 duration,
    /// or None if all stage metrics are empty.
    pub fn dominant_stage(&self) -> Option<(&'static str, f64)> {
        self.as_slice()
            .into_iter()
            .filter(|(_, s)| !s.is_empty())
            .max_by(|(_, a), (_, b)| a.p95().total_cmp(&b.p95()))
            .map(|(name, s)| (name, s.p95()))
    }
}

/// Aggregated benchmark results across all measurements.
#[derive(Debug, Clone)]
pub struct BenchResults {
    pub host: HostConfig,
    pub repetitions: usize,
    pub is_quick: bool,
    /// Detected from the environment when the run starts, so rendering stays deterministic.
    pub ci_runner: bool,
    pub cold_boot_ms: MetricSamples,
    pub freeze_duration_ms: MetricSamples,
    pub pause_skew_ms: MetricSamples,
    /// Wall time from starting fabric freeze through completing thaw, while VMs are paused and fabric is frozen.
    pub snapshot_freeze_to_thaw_ms: MetricSamples,
    /// Total time from starting snapshot until published and restorable.
    pub snapshot_total_ms: MetricSamples,
    /// Per-stage breakdown of the snapshot sequence.
    pub snapshot_stages: SnapshotStages,
    /// Per-repetition VM capture counts (Diff, Full), reported by the runtime.
    pub snapshot_captures: Vec<(usize, usize)>,
    /// End-to-end `Lab::fork_with` duration for the whole batch of clones.
    pub fork_wall_time_ms: BTreeMap<usize, MetricSamples>,
    /// Batch duration divided by the clone count.
    pub fork_amortized_ms: BTreeMap<usize, MetricSamples>,
    /// Per-clone restore sub-interval from netns creation through fabric thaw.
    pub fork_restore_ms: BTreeMap<usize, MetricSamples>,
    /// Per-clone PSS after the settling condition ended, grouped by clone count.
    pub per_clone_pss_kib: BTreeMap<usize, MetricSamples>,
    /// Clones per count whose PSS had not settled when the deadline expired.
    pub pss_unsettled: BTreeMap<usize, usize>,
}

impl BenchResults {
    pub fn freeze_verdict(&self) -> &'static str {
        pass(p95_under(Some(&self.freeze_duration_ms), FREEZE_BUDGET_MS))
    }

    pub fn skew_verdict(&self) -> &'static str {
        pass(p95_under(Some(&self.pause_skew_ms), SKEW_BUDGET_MS))
    }

    /// The README criterion is judged on the freeze-to-thaw pause window, not on the total time
    /// including background hashing and publication.
    pub fn snapshot_verdict(&self) -> &'static str {
        go(p95_under(
            Some(&self.snapshot_freeze_to_thaw_ms),
            SNAPSHOT_BUDGET_MS,
        ))
    }

    /// Every clone count that any fork or PSS measurement mentions.
    pub fn fork_counts(&self) -> BTreeSet<usize> {
        [
            &self.fork_wall_time_ms,
            &self.fork_amortized_ms,
            &self.fork_restore_ms,
            &self.per_clone_pss_kib,
        ]
        .into_iter()
        .flat_map(|map| map.keys().copied())
        .collect()
    }

    fn end_to_end_ok(&self, count: usize) -> bool {
        p95_under(
            self.fork_wall_time_ms.get(&count),
            CLONE_BUDGET_MS * count as f64,
        )
    }

    fn amortized_ok(&self, count: usize) -> bool {
        p95_under(self.fork_amortized_ms.get(&count), CLONE_BUDGET_MS)
    }

    /// The README criterion is judged on the complete fork operation, never on the restore
    /// sub-interval; the sub-interval only has to be present so the breakdown is complete.
    fn fork_count_ok(&self, count: usize) -> bool {
        self.end_to_end_ok(count)
            && self.amortized_ok(count)
            && self
                .fork_restore_ms
                .get(&count)
                .is_some_and(|s| !s.is_empty())
    }

    pub fn fork_verdict(&self) -> &'static str {
        let counts = self.fork_counts();
        go(!counts.is_empty() && counts.iter().all(|&c| self.fork_count_ok(c)))
    }

    fn pss_count_ok(&self, count: usize) -> bool {
        p95_under(self.per_clone_pss_kib.get(&count), PSS_BUDGET_MIB * 1024.0)
            && self.pss_unsettled.get(&count).copied().unwrap_or(0) == 0
    }

    pub fn pss_verdict(&self) -> &'static str {
        let counts = self.fork_counts();
        pass(!counts.is_empty() && counts.iter().all(|&c| self.pss_count_ok(c)))
    }

    /// Names of the criteria that failed or have no measurement, in report order.
    pub fn failing_criteria(&self) -> Vec<&'static str> {
        [
            ("snapshot", self.snapshot_verdict() == "GO"),
            ("fork", self.fork_verdict() == "GO"),
            ("freeze", self.freeze_verdict() == "PASS"),
            ("pause skew", self.skew_verdict() == "PASS"),
            ("PSS", self.pss_verdict() == "PASS"),
        ]
        .into_iter()
        .filter_map(|(name, ok)| (!ok).then_some(name))
        .collect()
    }

    pub fn overall_verdict(&self) -> String {
        let failing = self.failing_criteria();
        if failing.is_empty() {
            String::from(
                "PENDING (measured criteria pass; E2E results and bare-metal reference run require separate evaluation)",
            )
        } else {
            format!("NO-GO (not met or not measured: {})", failing.join(", "))
        }
    }

    /// True for a full run outside CI, the only run the README reference numbers can come from.
    pub fn is_reference_run(&self) -> bool {
        !self.is_quick && !self.ci_runner
    }

    pub fn mode_label(&self) -> &'static str {
        match (self.is_quick, self.ci_runner) {
            (true, true) => "CI runner, quick run (indicative)",
            (true, false) => "non-CI host, quick run (indicative)",
            (false, true) => "CI runner, full run",
            (false, false) => "non-CI host, full run (reference)",
        }
    }

    fn pss_count_verdict(&self, count: usize) -> String {
        let unsettled = self.pss_unsettled.get(&count).copied().unwrap_or(0);
        if unsettled > 0 {
            format!("FAIL ({unsettled} unsettled)")
        } else {
            pass(self.pss_count_ok(count)).into()
        }
    }

    /// All measurement lines with their thresholds and verdicts.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![
            Row::new(
                "Cold boot",
                "baseline",
                stat(Some(&self.cold_boot_ms), 1.0, 1, "ms"),
                "INFO",
            ),
            Row::new(
                "Fabric freeze duration",
                "< 1000 ms",
                stat(Some(&self.freeze_duration_ms), 1.0, 1, "ms"),
                self.freeze_verdict(),
            ),
            Row::new(
                "Pause confirmation skew",
                "< 100 ms",
                stat(Some(&self.pause_skew_ms), 1.0, 3, "ms"),
                self.skew_verdict(),
            ),
            Row::new(
                "Snapshot pause window, freeze to thaw",
                "< 1000 ms",
                stat(Some(&self.snapshot_freeze_to_thaw_ms), 1.0, 1, "ms"),
                self.snapshot_verdict(),
            ),
            Row::new(
                "Snapshot total until published",
                "informational",
                stat(Some(&self.snapshot_total_ms), 1.0, 1, "ms"),
                if self.snapshot_total_ms.is_empty() {
                    "NO DATA"
                } else {
                    "INFO"
                },
            ),
        ];
        for count in self.fork_counts() {
            let budget = CLONE_BUDGET_MS * count as f64;
            rows.push(Row::new(
                &format!("Fork end-to-end (n={count})"),
                &format!("< {budget:.0} ms"),
                stat(self.fork_wall_time_ms.get(&count), 1.0, 1, "ms"),
                go(self.end_to_end_ok(count)),
            ));
            rows.push(Row::new(
                &format!("Fork amortized per clone (n={count})"),
                &format!("< {CLONE_BUDGET_MS:.0} ms/clone"),
                stat(self.fork_amortized_ms.get(&count), 1.0, 1, "ms"),
                go(self.amortized_ok(count)),
            ));
            let restore = self.fork_restore_ms.get(&count);
            rows.push(Row::new(
                &format!("Restore sub-interval, netns to thaw (n={count})"),
                "informational",
                stat(restore, 1.0, 1, "ms"),
                if restore.is_some_and(|s| !s.is_empty()) {
                    "INFO"
                } else {
                    "NO DATA"
                },
            ));
        }
        for count in self.fork_counts() {
            rows.push(Row::new(
                &format!("Per-clone PSS after settling (n={count})"),
                "< 256 MiB",
                stat(self.per_clone_pss_kib.get(&count), 1024.0, 1, "MiB"),
                &self.pss_count_verdict(count),
            ));
        }
        rows
    }
}

/// Locates and validates host prerequisites.
struct Prerequisites {
    workspace: PathBuf,
    fc_bin: PathBuf,
    jailer_bin: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
    spec: LabSpec,
}

fn check_prerequisites(options: &BenchOptions) -> anyhow::Result<Prerequisites> {
    // 1. Must run as root for network namespace, TAP, and jailer chroot
    let is_root = fs::metadata("/proc/self").is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        m.uid() == 0
    });
    if !is_root {
        bail!(
            "bench requires root privileges for microVM and network namespace management; run with sudo"
        );
    }

    // 2. KVM access
    let kvm = Path::new("/dev/kvm");
    if !kvm.exists() {
        bail!("/dev/kvm does not exist; hardware virtualization is required");
    }

    // 3. Workspace path
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("cannot determine workspace directory")?
        .canonicalize()
        .context("cannot resolve workspace path")?;

    // 4. Firecracker and Jailer binaries
    let bin_dir = std::env::var_os("DYLOS_FC_BIN_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            let opt = PathBuf::from("/opt/dylos/bin");
            if opt.join("firecracker").exists() && opt.join("jailer").exists() {
                Some(opt)
            } else {
                None
            }
        })
        .context("DYLOS_FC_BIN_DIR not set and /opt/dylos/bin not populated")?;

    let fc_bin = bin_dir.join("firecracker");
    let jailer_bin = bin_dir.join("jailer");
    if !fc_bin.exists() || !jailer_bin.exists() {
        bail!("Firecracker or Jailer missing in {}", bin_dir.display());
    }

    // 5. Kernel and Rootfs
    let kernel = workspace.join(KERNEL_PATH);
    if !kernel.exists() {
        bail!("guest kernel missing at {}", kernel.display());
    }
    let rootfs = workspace.join(ROOTFS_PATH);
    if !rootfs.exists() {
        bail!(
            "guest rootfs missing at {}; run 'just images' first",
            rootfs.display()
        );
    }

    let spec_file = validate_lab_path(&workspace, &options.lab)?;
    let spec_content = fs::read_to_string(&spec_file)
        .with_context(|| format!("cannot read {}", spec_file.display()))?;
    let spec = LabSpec::from_yaml_str(&spec_content)
        .with_context(|| format!("invalid LabSpec in {}", spec_file.display()))?;

    Ok(Prerequisites {
        workspace,
        fc_bin,
        jailer_bin,
        kernel,
        rootfs,
        spec,
    })
}

/// Creates an exclusive run directory with mode 0700 inside `parent_dir`.
///
/// `parent_dir` and all of its ancestors must be controlled by the operator running the
/// benchmark: only the final component is checked for symlinks, so a hostile ancestor
/// could still redirect the run. Never removes or modifies the parent or its contents.
pub fn setup_private_bench_dir(parent_dir: &Path) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(parent_dir)
        .with_context(|| format!("cannot create parent directory {}", parent_dir.display()))?;
    let meta = fs::symlink_metadata(parent_dir).with_context(|| {
        format!(
            "cannot read metadata for parent directory {}",
            parent_dir.display()
        )
    })?;
    if meta.file_type().is_symlink() {
        bail!(
            "temporary parent directory '{}' is a symlink; refusing to proceed",
            parent_dir.display()
        );
    }
    if !meta.is_dir() {
        bail!(
            "temporary parent '{}' is not a directory",
            parent_dir.display()
        );
    }

    let pid = std::process::id();
    // Kept short: the VM control socket paths below it must fit in sockaddr_un (107 bytes).
    let mut run_dir = parent_dir.join(format!("r{pid:x}"));
    let mut disambig = 0;
    loop {
        match DirBuilder::new().mode(0o700).create(&run_dir) {
            Ok(()) => return Ok(run_dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                disambig += 1;
                run_dir = parent_dir.join(format!("r{pid:x}-{disambig:x}"));
                if disambig > 255 {
                    bail!(
                        "cannot create exclusive run directory under {}",
                        parent_dir.display()
                    );
                }
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "cannot create exclusive run directory {}",
                        run_dir.display()
                    )
                });
            }
        }
    }
}

/// Removes a run directory created by [`setup_private_bench_dir`].
/// Refuses to remove symlinks and propagates any removal error.
pub fn cleanup_private_bench_dir(bench_dir: &Path) -> anyhow::Result<()> {
    if bench_dir.exists() {
        let meta = fs::symlink_metadata(bench_dir).with_context(|| {
            format!(
                "cannot read metadata for bench directory {}",
                bench_dir.display()
            )
        })?;
        if meta.file_type().is_symlink() {
            bail!(
                "refusing to remove symlinked bench directory {}",
                bench_dir.display()
            );
        }
        fs::remove_dir_all(bench_dir)
            .with_context(|| format!("failed to remove bench directory {}", bench_dir.display()))?;
    }
    Ok(())
}

/// Finds the shallowest ancestor of `path` that exists on the filesystem.
fn shallowest_existing_ancestor(mut path: &Path) -> Option<&Path> {
    while !path.exists() {
        path = path.parent()?;
    }
    Some(path)
}

/// Checks whether `path` is on a tmpfs or ramfs filesystem.
/// Fails closed (returns `true`) if the filesystem type cannot be verified.
fn is_tmpfs(path: &Path) -> bool {
    let check_path = shallowest_existing_ancestor(path).unwrap_or(path);
    let Ok(output) = Command::new("df")
        .arg("-T")
        .arg(check_path.as_os_str())
        .output()
    else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    let Ok(stdout) = std::str::from_utf8(&output.stdout) else {
        return true;
    };
    let Some(line) = stdout.lines().nth(1) else {
        return true;
    };
    let cols: Vec<&str> = line.split_whitespace().collect();
    let Some(fs_type) = cols.get(1) else {
        return true;
    };
    *fs_type == "tmpfs" || *fs_type == "ramfs"
}

/// Checks whether a directory exists, is not a symlink, and permits exclusive safe creation
/// within a private subdirectory without following symlinks or modifying existing entries.
pub fn is_writable_dir(path: &Path) -> anyhow::Result<bool> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(false);
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(false);
    }
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let probe_dir = path.join(format!(".probe-{pid}-{nanos:x}"));

    match DirBuilder::new().mode(0o700).create(&probe_dir) {
        Ok(()) => {}
        Err(e)
            if e.kind() == std::io::ErrorKind::PermissionDenied
                || e.kind() == std::io::ErrorKind::ReadOnlyFilesystem
                || e.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            return Ok(false);
        }
        Err(_) => return Ok(false),
    }

    let probe_file = probe_dir.join("p");
    let open_res = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe_file);

    match open_res {
        Ok(file) => {
            drop(file);
            let rm_file = fs::remove_file(&probe_file)
                .with_context(|| format!("cannot remove probe file {}", probe_file.display()));
            let rm_dir = fs::remove_dir(&probe_dir)
                .with_context(|| format!("cannot remove probe directory {}", probe_dir.display()));
            rm_file?;
            rm_dir?;
            Ok(true)
        }
        Err(open_err) => {
            let rm_dir = fs::remove_dir(&probe_dir).with_context(|| {
                format!(
                    "cannot remove probe directory {} during rollback",
                    probe_dir.display()
                )
            });
            rm_dir?;
            Err(anyhow::Error::from(open_err).context(format!(
                "cannot create probe file in {}",
                probe_dir.display()
            )))
        }
    }
}

/// Computes the longest Unix domain socket path that the benchmark suite will create
/// under `run_dir` across all measurement phases (cold boot, snapshot, and parallel forks).
pub fn longest_bench_socket_path(
    run_dir: &Path,
    options: &BenchOptions,
    spec: &LabSpec,
    fc_bin: &Path,
) -> PathBuf {
    let mut jailer = JailerConfig::new(run_dir.join("j"), NOBODY_UID_GID, NOBODY_UID_GID);
    jailer.firecracker = fc_bin.to_path_buf();

    let longest_node = spec
        .nodes
        .iter()
        .map(|n| n.name.as_str())
        .max_by_key(|n| n.len())
        .unwrap_or("node1");

    let cold_boot_id = format!("b-boot-{}", options.repetitions);
    let source_id = "b-src";
    let fork_ids: Vec<String> = options
        .fork_counts
        .iter()
        .map(|&count| format!("f-n{count}-r{}-c{count}", options.repetitions))
        .collect();

    let candidate_ids = std::iter::once(cold_boot_id)
        .chain(std::iter::once(source_id.to_string()))
        .chain(fork_ids);

    candidate_ids
        .map(|lab_id| JailPaths::api_socket_path(&jailer, &lab_id, longest_node))
        .max_by_key(|p| p.as_os_str().len())
        .unwrap_or_else(|| JailPaths::api_socket_path(&jailer, source_id, longest_node))
}

/// Validates that the longest Unix domain socket path created under `run_dir` does not
/// exceed Linux's `sockaddr_un` limit.
///
/// # Errors
/// Returns an error naming the path, the byte length, the 107-byte limit, and suggesting
/// `--tmp-dir` if the socket path is too long.
pub fn validate_bench_socket_path(
    run_dir: &Path,
    options: &BenchOptions,
    spec: &LabSpec,
    fc_bin: &Path,
) -> anyhow::Result<PathBuf> {
    let longest_path = longest_bench_socket_path(run_dir, options, spec, fc_bin);
    if let Err(len) = validate_unix_socket_path(&longest_path) {
        bail!(
            "socket path \"{}\" ({len} bytes) exceeds limit of {} bytes; specify a shorter directory with --tmp-dir",
            longest_path.display(),
            MAX_UNIX_SOCKET_PATH
        );
    }
    Ok(longest_path)
}

/// Checks whether every socket path created under `candidate` stays within the 107-byte limit,
/// assuming worst-case run directory component length (including collision suffix).
fn candidate_socket_path_fits(
    candidate: &Path,
    options: &BenchOptions,
    spec: &LabSpec,
    fc_bin: &Path,
) -> bool {
    // setup_private_bench_dir uses r{pid:x} or r{pid:x}-{disambig:x} up to disambig = 255 (0xff).
    // The longest possible run directory component is r{pid:x}-ff.
    let test_run_dir = candidate.join(format!("r{:x}-ff", std::process::id()));
    let longest_socket = longest_bench_socket_path(&test_run_dir, options, spec, fc_bin);
    validate_unix_socket_path(&longest_socket).is_ok()
}

/// Selects a parent directory for the exclusive benchmark run directory on the same filesystem
/// as `workspace` such that all Unix domain socket paths stay within Linux's 107-byte limit.
///
/// # Errors
/// Returns an error if any probe cleanup fails, or if no verified candidate directory
/// on the workspace filesystem mount can satisfy the 107-byte socket limit.
pub fn select_default_bench_parent(
    workspace: &Path,
    options: &BenchOptions,
    spec: &LabSpec,
    fc_bin: &Path,
) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let canonical_ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let ws_dev = fs::metadata(&canonical_ws)
        .with_context(|| {
            format!(
                "cannot read metadata for workspace {}",
                canonical_ws.display()
            )
        })?
        .dev();

    let check_candidate = |candidate: &Path| -> anyhow::Result<bool> {
        let Some(existing) = shallowest_existing_ancestor(candidate) else {
            return Ok(false);
        };
        let Ok(meta) = fs::symlink_metadata(existing) else {
            return Ok(false);
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Ok(false);
        }
        if meta.dev() != ws_dev {
            return Ok(false);
        }
        if is_tmpfs(existing) {
            return Ok(false);
        }
        if !is_writable_dir(existing)? {
            return Ok(false);
        }
        Ok(candidate_socket_path_fits(candidate, options, spec, fc_bin))
    };

    let probe_b = |base: &Path| -> anyhow::Result<Option<PathBuf>> {
        let dot_b = base.join(".b");
        if check_candidate(&dot_b)? {
            return Ok(Some(dot_b));
        }
        let plain_b = base.join("b");
        if check_candidate(&plain_b)? {
            return Ok(Some(plain_b));
        }
        if check_candidate(base)? {
            return Ok(Some(base.to_path_buf()));
        }
        Ok(None)
    };

    // 1. Cargo's default scratch location under target/b (if it fits within the socket limit)
    let target_b = canonical_ws.join("target/b");
    if check_candidate(&target_b)? {
        return Ok(target_b);
    }

    // 2. Standard persistent temporary directory (/var/tmp)
    let var_tmp = Path::new("/var/tmp");
    if check_candidate(var_tmp)? {
        return Ok(var_tmp.to_path_buf());
    }

    // 3. Operator's home directory if on the same mount
    if let Some(home) = std::env::var_os("HOME")
        && let Some(p) = probe_b(Path::new(&home))?
    {
        return Ok(p);
    }

    // 4. Mount root of the required filesystem
    let mut mount_root = canonical_ws.clone();
    while let Some(parent) = mount_root.parent() {
        if fs::metadata(parent).is_ok_and(|m| m.dev() == ws_dev) {
            mount_root = parent.to_path_buf();
        } else {
            break;
        }
    }
    if let Some(p) = probe_b(&mount_root)? {
        return Ok(p);
    }

    // 5. Shallowest writable ancestor towards workspace
    if let Ok(rel) = canonical_ws.strip_prefix(&mount_root) {
        let mut current = mount_root.clone();
        for component in rel.components() {
            current.push(component);
            if let Some(p) = probe_b(&current)? {
                return Ok(p);
            }
        }
    }

    // 6. Hidden .b or b directory directly under workspace
    if let Some(p) = probe_b(&canonical_ws)? {
        return Ok(p);
    }

    bail!(
        "no verified benchmark directory found on workspace filesystem mount where socket paths fit within {MAX_UNIX_SOCKET_PATH} bytes; specify a directory with --tmp-dir"
    )
}

/// Resolves the benchmark parent directory: uses `options.tmp_dir` when provided,
/// or falls back to selecting a verified default directory on the workspace mount.
///
/// # Errors
/// Returns an error if default directory selection fails.
pub fn resolve_bench_parent(
    workspace: &Path,
    options: &BenchOptions,
    spec: &LabSpec,
    fc_bin: &Path,
) -> anyhow::Result<PathBuf> {
    if let Some(ref dir) = options.tmp_dir {
        return Ok(dir.clone());
    }
    select_default_bench_parent(workspace, options, spec, fc_bin)
}

/// Result of the measurement phases.
///
/// The execution result and the teardown failures are kept apart: a measurement error says
/// nothing about leftover host resources, and only a clean teardown makes it safe to
/// delete the files backing them.
pub struct PhaseOutcome<T> {
    pub result: anyhow::Result<T>,
    pub teardown_failures: Vec<anyhow::Error>,
}

/// Turns a failed lab operation into the error to propagate, and records it in `failures` when
/// the runtime could not roll the lab back: its resources may remain, so the run directory
/// backing them must be preserved. A failure whose rollback completed is an ordinary error.
pub fn lab_failure(
    failures: &mut Vec<anyhow::Error>,
    error: LabError,
    context: String,
) -> anyhow::Error {
    let recorded = error
        .leaves_resources()
        .then(|| anyhow::anyhow!("{context}: {error}"));
    failures.extend(recorded);
    anyhow::Error::from(error).context(context)
}

/// Runs `phases` inside a fresh private directory below `parent_dir`, then disposes of it.
///
/// - Clean teardown: the directory (snapshots, disks) is removed whether or not a
///   measurement failed.
/// - Any teardown failure: the directory is kept because VMs, mounts or namespaces may
///   still use it, and every failure is reported.
///
/// Anything that fails inside `phases` (host detection, subdirectory creation, ...) is an
/// execution error and goes through the same cleanup.
pub async fn run_in_private_dir<T, F, Fut>(parent_dir: &Path, phases: F) -> anyhow::Result<T>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: Future<Output = PhaseOutcome<T>>,
{
    let parent = parent_dir.to_owned();
    let run_dir = tokio::task::spawn_blocking(move || setup_private_bench_dir(&parent))
        .await
        .context("directory setup task panicked")??;

    let PhaseOutcome {
        result,
        teardown_failures,
    } = phases(run_dir.clone()).await;

    if !teardown_failures.is_empty() {
        let failures = teardown_failures
            .iter()
            .map(|e| format!("{e:#}"))
            .collect::<Vec<_>>()
            .join("; ");
        let run = result.err().map_or_else(
            || String::from("measurements completed"),
            |e| format!("{e:#}"),
        );
        bail!(
            "{run}; teardown failed, host resources may remain and {} was preserved for recovery: {failures}",
            run_dir.display()
        );
    }

    let dir = run_dir.clone();
    let cleanup = tokio::task::spawn_blocking(move || cleanup_private_bench_dir(&dir))
        .await
        .context("cleanup task panicked")?;
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(cleanup)) => {
            Err(err.context(format!("run directory cleanup also failed: {cleanup:#}")))
        }
    }
}

/// Settling condition for a PSS reading: two consecutive reads `poll` apart differ by at most
/// `max_delta_kib`. This observes stability of the reading, not guest quiescence.
#[derive(Debug, Clone, Copy)]
pub struct SettleParams {
    pub poll: Duration,
    pub max_delta_kib: u64,
    pub timeout: Duration,
}

pub const PSS_SETTLE: SettleParams = SettleParams {
    poll: Duration::from_millis(250),
    max_delta_kib: 1024,
    timeout: Duration::from_secs(10),
};

/// A PSS reading and whether it met the settling condition before the deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PssReading {
    pub kib: u64,
    pub settled: bool,
}

/// Polls `read` until two consecutive values are within `params.max_delta_kib`, or until
/// `params.timeout`, in which case the last value is returned with `settled == false`.
pub async fn settle_pss<F, Fut>(mut read: F, params: SettleParams) -> anyhow::Result<PssReading>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<u64>>,
{
    let deadline = Instant::now() + params.timeout;
    let mut previous = read().await?;
    loop {
        if Instant::now() >= deadline {
            return Ok(PssReading {
                kib: previous,
                settled: false,
            });
        }
        tokio::time::sleep(params.poll).await;
        let current = read().await?;
        if current.abs_diff(previous) <= params.max_delta_kib {
            return Ok(PssReading {
                kib: current,
                settled: true,
            });
        }
        previous = current;
    }
}

struct CloneReading {
    restore_ms: f64,
    pss: PssReading,
}

async fn measure_clones(clones: &[Lab]) -> anyhow::Result<Vec<CloneReading>> {
    join_all(clones.iter().map(|clone| async move {
        let restore = clone
            .restore_latency()
            .with_context(|| format!("missing restore latency on clone {}", clone.id()))?;
        let pss = settle_pss(
            || async move { clone.total_pss_kib().await.map_err(anyhow::Error::from) },
            PSS_SETTLE,
        )
        .await
        .with_context(|| format!("failed to measure PSS for clone {}", clone.id()))?;
        Ok::<_, anyhow::Error>(CloneReading {
            restore_ms: restore.as_secs_f64() * 1000.0,
            pss,
        })
    }))
    .await
    .into_iter()
    .collect()
}

/// Tears every clone down and returns all failures, each tagged with its clone id.
async fn teardown_clones(clones: &mut [Lab]) -> Vec<anyhow::Error> {
    join_all(clones.iter_mut().map(|clone| async move {
        let id = clone.id().to_owned();
        clone
            .teardown()
            .await
            .map_err(|e| anyhow::Error::from(e).context(format!("clone {id} teardown failed")))
    }))
    .await
    .into_iter()
    .filter_map(Result::err)
    .collect()
}

#[derive(Default)]
struct ForkMetrics {
    wall: BTreeMap<usize, MetricSamples>,
    amortized: BTreeMap<usize, MetricSamples>,
    restore: BTreeMap<usize, MetricSamples>,
    pss: BTreeMap<usize, MetricSamples>,
    pss_unsettled: BTreeMap<usize, usize>,
}

/// Snapshot to remove before a repetition, retaining the immediately previous baseline.
pub fn obsolete_snapshot_id(repetition: usize) -> Option<String> {
    repetition
        .checked_sub(2)
        .filter(|rep| *rep > 0)
        .map(|rep| format!("snap-{rep}"))
}

#[derive(Default)]
struct SnapshotMetrics {
    captures: Vec<(usize, usize)>,
    freeze: MetricSamples,
    pause: MetricSamples,
    skew: MetricSamples,
    api_snapshot: MetricSamples,
    storage: MetricSamples,
    resume: MetricSamples,
    thaw: MetricSamples,
    freeze_to_thaw: MetricSamples,
    hash: MetricSamples,
    manifest: MetricSamples,
    total: MetricSamples,
}

struct Bench<'a> {
    options: &'a BenchOptions,
    prereqs: &'a Prerequisites,
    lab_config: LabConfig,
    memory: &'a MemoryWatch,
}

impl Bench<'_> {
    async fn cold_boot(&self, failures: &mut Vec<anyhow::Error>) -> anyhow::Result<MetricSamples> {
        println!("--- [1/3] Benchmarking Cold Boot Time ---");
        let mut cold_boot_ms = MetricSamples::new();
        for rep in 1..=self.options.repetitions {
            self.memory.check()?;
            let start = Instant::now();
            let mut lab = Lab::up_with(
                &self.lab_config,
                &self.prereqs.spec,
                &format!("b-boot-{rep}"),
            )
            .await
            .map_err(|e| {
                lab_failure(failures, e, format!("cold boot failed on repetition {rep}"))
            })?;
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            cold_boot_ms.add(ms);
            println!(
                "  Repetition {rep:>2}/{}: cold boot ready in {ms:>7.1} ms",
                self.options.repetitions
            );
            if let Err(e) = lab.teardown().await {
                failures.push(
                    anyhow::Error::from(e)
                        .context(format!("cold boot repetition {rep} teardown failed")),
                );
                bail!("cold boot teardown failed on repetition {rep}");
            }
            self.memory
                .record(&format!("Cold boot {rep}: {ms:.1} ms\n"))?;
        }
        println!(
            "Cold boot summary: median = {:.1} ms, p95 = {:.1} ms\n",
            cold_boot_ms.median(),
            cold_boot_ms.p95()
        );
        self.memory.check()?;
        Ok(cold_boot_ms)
    }

    /// Takes the snapshots; returns the metrics and the directory of the last snapshot.
    async fn snapshots(&self, source: &mut Lab) -> anyhow::Result<(SnapshotMetrics, PathBuf)> {
        println!(
            "--- [2/3] Benchmarking Snapshot Timings (Freeze-to-Thaw Pause, Skew, Total Publication) ---"
        );
        let mut metrics = SnapshotMetrics::default();
        let mut last_snapshot_dir: Option<PathBuf> = None;
        for rep in 1..=self.options.repetitions {
            self.memory.check()?;
            // Keep the preceding memory image available as the Diff baseline.
            if let Some(obsolete) = obsolete_snapshot_id(rep) {
                let prev_dir = self.lab_config.store.snapshot_dir("b-src", &obsolete);
                let prev = prev_dir.clone();
                tokio::task::spawn_blocking(move || fs::remove_dir_all(&prev))
                    .await
                    .context("snapshot cleanup task panicked")?
                    .with_context(|| {
                        format!("cannot remove intermediate snapshot {}", prev_dir.display())
                    })?;
            }

            if self.options.working_set_mib > 0 {
                let command = include_str!("bench-working-set.sh")
                    .replace("WORKING_SET_MIB", &self.options.working_set_mib.to_string())
                    .replace("SNAPSHOT_REPETITION", &rep.to_string());
                for node in &self.prereqs.spec.nodes {
                    self.memory.check()?;
                    let output = source
                        .exec(&node.name, &command, Duration::from_secs(120))
                        .await?;
                    if !output.success() {
                        bail!("working set failed on {}: {:?}", node.name, output);
                    }
                }
            }
            self.memory.check()?;
            let snap_id = format!("snap-{rep}");
            let (_manifest, timings) = source
                .snapshot_with_timings(&snap_id)
                .await
                .with_context(|| format!("snapshot failed on repetition {rep}"))?;
            let freeze = timings.freeze_ms as f64;
            let pause = timings.pause_ms as f64;
            let skew = timings.pause_skew_us as f64 / 1000.0;
            let api_snapshot = timings.api_snapshot_ms as f64;
            let storage = timings.storage_ms as f64;
            let resume = timings.resume_ms as f64;
            let thaw = timings.thaw_ms as f64;
            let freeze_to_thaw = timings.freeze_to_thaw_ms as f64;
            let hash = timings.hash_ms as f64;
            let manifest = timings.manifest_ms as f64;
            let total = timings.total_ms as f64;

            self.memory.record(&format!(
                "Snapshot {rep}: pause {} ms, total {} ms\n",
                timings.freeze_to_thaw_ms, timings.total_ms
            ))?;
            self.memory.check()?;
            metrics.captures.push(timings.capture_counts);
            metrics.freeze.add(freeze);
            metrics.pause.add(pause);
            metrics.skew.add(skew);
            metrics.api_snapshot.add(api_snapshot);
            metrics.storage.add(storage);
            metrics.resume.add(resume);
            metrics.thaw.add(thaw);
            metrics.freeze_to_thaw.add(freeze_to_thaw);
            metrics.hash.add(hash);
            metrics.manifest.add(manifest);
            metrics.total.add(total);

            println!(
                "  Repetition {rep:>2}/{}: Diff = {}, Full = {}, pause window (freeze-to-thaw) = {freeze_to_thaw:>5.1} ms, total until published = {total:>6.1} ms, pause skew = {skew:>5.3} ms",
                self.options.repetitions, timings.capture_counts.0, timings.capture_counts.1
            );
            last_snapshot_dir = Some(self.lab_config.store.snapshot_dir("b-src", &snap_id));
        }
        let (diff, full) = metrics
            .captures
            .iter()
            .fold((0, 0), |(d, f), &(next_d, next_f)| (d + next_d, f + next_f));
        println!("Capture summary (VM captures): Diff = {diff}, Full = {full}.");
        println!(
            "Snapshot summary: pause window median = {:.1} ms (p95: {:.1} ms), total published median = {:.1} ms (p95: {:.1} ms), pause skew median = {:.3} ms (p95: {:.3} ms)\n",
            metrics.freeze_to_thaw.median(),
            metrics.freeze_to_thaw.p95(),
            metrics.total.median(),
            metrics.total.p95(),
            metrics.skew.median(),
            metrics.skew.p95()
        );
        let dir = last_snapshot_dir.context("missing snapshot for fork benchmarks")?;
        Ok((metrics, dir))
    }

    async fn forks(
        &self,
        snapshot_dir: &Path,
        failures: &mut Vec<anyhow::Error>,
    ) -> anyhow::Result<ForkMetrics> {
        println!("--- [3/3] Benchmarking Parallel Fork Latency and PSS ---");
        let mut metrics = ForkMetrics::default();
        for &count in &self.options.fork_counts {
            println!(
                "  Benchmarking fork -n {count} (parallel restoration into {count} independent labs)..."
            );
            let mut wall = MetricSamples::new();
            let mut amortized = MetricSamples::new();
            let mut restore = MetricSamples::new();
            let mut pss = MetricSamples::new();
            let mut unsettled = 0;

            for rep in 1..=self.options.repetitions {
                self.memory.check()?;
                let clone_ids = Lab::generate_clone_ids(&format!("f-n{count}-r{rep}"), count)
                    .with_context(|| format!("could not generate {count} clone ids"))?;

                let fork_start = Instant::now();
                let mut clones = Lab::fork_with(&self.lab_config, snapshot_dir, &clone_ids)
                    .await
                    .map_err(|e| {
                        lab_failure(
                            failures,
                            e,
                            format!("fork failed for n={count} on repetition {rep}"),
                        )
                    })?;
                let wall_ms = fork_start.elapsed().as_secs_f64() * 1000.0;
                wall.add(wall_ms);
                amortized.add(wall_ms / count as f64);

                let readings = if self.memory.check().is_ok() {
                    measure_clones(&clones).await
                } else {
                    Err(anyhow::anyhow!(
                        "ABORTED: memory monitor requested teardown"
                    ))
                };
                let teardown_failures = teardown_clones(&mut clones).await;
                let torn_down = teardown_failures.is_empty();
                failures.extend(teardown_failures);
                self.memory
                    .record(&format!("Fork n={count} rep={rep}: {wall_ms:.1} ms\n"))?;
                self.memory.check()?;
                let readings = readings?;
                if !torn_down {
                    bail!("clone teardown failed for n={count} on repetition {rep}");
                }

                for reading in readings {
                    restore.add(reading.restore_ms);
                    pss.add(reading.pss.kib as f64);
                    unsettled += usize::from(!reading.pss.settled);
                }
                println!(
                    "    n={count:>2} rep {rep:>2}/{}: end-to-end = {wall_ms:>6.1} ms ({:>5.1} ms/clone amortized), median clone restore (netns to thaw) = {:>5.1} ms",
                    self.options.repetitions,
                    wall_ms / count as f64,
                    restore.median()
                );
            }

            println!(
                "  Fork -n {count} summary: end-to-end p95 = {:.1} ms, amortized p95 = {:.1} ms/clone, restore (netns to thaw) median = {:.1} ms (p95: {:.1} ms), PSS unsettled clones = {unsettled}",
                wall.p95(),
                amortized.p95(),
                restore.median(),
                restore.p95()
            );
            metrics.wall.insert(count, wall);
            metrics.amortized.insert(count, amortized);
            metrics.restore.insert(count, restore);
            metrics.pss.insert(count, pss);
            metrics.pss_unsettled.insert(count, unsettled);
        }
        Ok(metrics)
    }

    /// Runs the snapshot and fork phases against one source lab. The lab is always torn
    /// down; a teardown failure is recorded in `failures` rather than the returned result.
    async fn snapshot_and_forks(
        &self,
        failures: &mut Vec<anyhow::Error>,
    ) -> anyhow::Result<(SnapshotMetrics, ForkMetrics)> {
        let mut source = Lab::up_with(&self.lab_config, &self.prereqs.spec, "b-src")
            .await
            .map_err(|e| {
                lab_failure(
                    failures,
                    e,
                    "failed to start source lab for snapshot benchmarking".into(),
                )
            })?;
        let result = async {
            self.memory.check()?;
            let (snapshot, snapshot_dir) = self.snapshots(&mut source).await?;
            // Keep the source quiet so it does not consume CPU during fork measurements.
            source
                .pause_all()
                .await
                .context("could not pause source lab VMs")?;
            let forks = self.forks(&snapshot_dir, failures).await?;
            Ok((snapshot, forks))
        }
        .await;
        if let Err(e) = source.teardown().await {
            failures.push(anyhow::Error::from(e).context("source lab teardown failed"));
        }
        result
    }
}

fn print_header(host: &HostConfig, options: &BenchOptions) {
    println!("================================================================================");
    println!("DYLOS TECHNICAL SPIKE BENCHMARK SUITE");
    println!("================================================================================");
    println!("Host Configuration:");
    println!(
        "  CPU:                 {} ({} vCPUs)",
        host.cpu_model, host.vcpu_count
    );
    println!(
        "  RAM:                 {:.1} GiB ({} KiB)",
        host.ram_gib, host.ram_kib
    );
    println!("  Kernel:              {}", host.kernel_release);
    println!(
        "  Filesystem:          {} (reflink: {})",
        host.filesystem, host.reflink_supported
    );
    println!("  Firecracker:         {}", host.firecracker_version);
    println!(
        "Mode:                  {} (repetitions: {}, fork counts: {:?})",
        if options.quick {
            "QUICK (reduced, indicative)"
        } else {
            "FULL"
        },
        options.repetitions,
        options.fork_counts
    );
    println!("================================================================================\n");
}

async fn measure(
    options: &BenchOptions,
    prereqs: &Prerequisites,
    run_dir: PathBuf,
    memory: &MemoryWatch,
) -> PhaseOutcome<BenchResults> {
    let mut teardown_failures = Vec::new();
    let result = measure_into(options, prereqs, &run_dir, &mut teardown_failures, memory).await;
    PhaseOutcome {
        result,
        teardown_failures,
    }
}

async fn measure_into(
    options: &BenchOptions,
    prereqs: &Prerequisites,
    run_dir: &Path,
    failures: &mut Vec<anyhow::Error>,
    memory: &MemoryWatch,
) -> anyhow::Result<BenchResults> {
    validate_bench_socket_path(run_dir, options, &prereqs.spec, &prereqs.fc_bin)?;

    let fc_bin = prereqs.fc_bin.clone();
    let detect_dir = run_dir.to_owned();
    let host = tokio::task::spawn_blocking(move || HostConfig::detect(&fc_bin, &detect_dir))
        .await
        .context("host detection task panicked")??;
    print_header(&host, options);

    let [jail_dir, store_dir, netns_dir] = ["j", "s", "n"].map(|name| run_dir.join(name));
    let dirs = [jail_dir.clone(), store_dir.clone(), netns_dir.clone()];
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        for dir in dirs {
            fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    })
    .await
    .context("subdirectory creation task panicked")??;

    let mut jailer = JailerConfig::new(jail_dir, NOBODY_UID_GID, NOBODY_UID_GID);
    jailer.jailer = prereqs.jailer_bin.clone();
    jailer.firecracker = prereqs.fc_bin.clone();
    let lab_config = LabConfig {
        store: dylos_store::Store {
            base: store_dir,
            image: prereqs.rootfs.clone(),
        },
        jailer,
        kernel: prereqs.kernel.clone(),
        netns_dir,
        boot_timeout: Duration::from_secs(45),
        ..LabConfig::default()
    };
    let bench = Bench {
        options,
        prereqs,
        lab_config,
        memory,
    };

    let cold_boot_ms = bench.cold_boot(failures).await?;
    let (snapshot, forks) = bench.snapshot_and_forks(failures).await?;

    Ok(BenchResults {
        host,
        repetitions: options.repetitions,
        is_quick: options.quick,
        ci_runner: std::env::var_os("GITHUB_ACTIONS").is_some() || std::env::var_os("CI").is_some(),
        cold_boot_ms,
        freeze_duration_ms: snapshot.freeze.clone(),
        pause_skew_ms: snapshot.skew,
        snapshot_freeze_to_thaw_ms: snapshot.freeze_to_thaw,
        snapshot_total_ms: snapshot.total,
        snapshot_captures: snapshot.captures,
        snapshot_stages: SnapshotStages {
            freeze_ms: snapshot.freeze,
            pause_ms: snapshot.pause,
            api_snapshot_ms: snapshot.api_snapshot,
            storage_ms: snapshot.storage,
            resume_ms: snapshot.resume,
            thaw_ms: snapshot.thaw,
            hash_ms: snapshot.hash,
            manifest_ms: snapshot.manifest,
        },
        fork_wall_time_ms: forks.wall,
        fork_amortized_ms: forks.amortized,
        fork_restore_ms: forks.restore,
        per_clone_pss_kib: forks.pss,
        pss_unsettled: forks.pss_unsettled,
    })
}

/// Executes the full benchmark suite.
pub async fn run_benchmarks(options: BenchOptions) -> anyhow::Result<BenchResults> {
    let prereqs = check_prerequisites(&options)?;
    let estimate = preflight_estimate_mib(&prereqs.spec, &options)?;
    let available = available_mib()?;
    if estimate > available {
        bail!(
            "memory preflight refused: peak estimate {estimate} MiB exceeds MemAvailable {available} MiB (guest + capture = 2 x working set, clones conservatively private at configured RAM, plus 1024 MiB margin)"
        );
    }
    let parent_dir =
        resolve_bench_parent(&prereqs.workspace, &options, &prereqs.spec, &prereqs.fc_bin)?;

    let summary = lab_summary(&prereqs.spec, &options)?;
    println!("{summary}");
    let memory = Arc::new(MemoryWatch::default());
    let watcher = Arc::clone(&memory);
    let monitor = tokio::spawn(async move {
        let mut poll = tokio::time::interval(Duration::from_millis(100));
        loop {
            poll.tick().await;
            let reason = match tokio::task::spawn_blocking(available_mib).await {
                Ok(Ok(mib)) if mib >= 1024 => continue,
                Ok(Ok(mib)) => format!("MemAvailable {mib} MiB fell below 1024 MiB"),
                other => format!("cannot monitor MemAvailable: {other:?}"),
            };
            *watcher.reason.lock().map_err(|e| anyhow::anyhow!("{e}"))? = Some(reason);
            return Ok::<(), anyhow::Error>(());
        }
    });
    let outcome = run_in_private_dir(&parent_dir, |run_dir| {
        measure(&options, &prereqs, run_dir, &memory)
    })
    .await;
    monitor.abort();
    match monitor.await {
        Ok(result) => result?,
        Err(e) if e.is_cancelled() => {}
        Err(e) => return Err(e).context("memory monitor failed"),
    }
    let outcome = outcome.and_then(|results| {
        memory.check()?;
        Ok(results)
    });
    let results = match outcome {
        Ok(results) => results,
        Err(error) => {
            let partial = format!(
                "# ABORTED benchmark\n\n{summary}Reason: {error:#}\n\nCompleted measurements:\n\n{}",
                memory.samples.lock().map_err(|e| anyhow::anyhow!("{e}"))?
            );
            let path = options
                .output_path
                .clone()
                .unwrap_or_else(|| prereqs.workspace.join("target/bench-aborted.md"));
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&path, partial)
                .await
                .with_context(|| format!("cannot write partial report {}", path.display()))?;
            return Err(error).context(format!("partial report: {}", path.display()));
        }
    };

    print_results_table(&results);
    if let Some(out_path) = options.output_path.clone() {
        let markdown = format!("{summary}{}", render_markdown_report(&results));
        let written = out_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            if let Some(parent) = written.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&written, markdown)
                .with_context(|| format!("cannot write report to {}", written.display()))
        })
        .await
        .context("report write task panicked")??;
        println!("\nBenchmark report written to {}", out_path.display());
    }
    Ok(results)
}

/// Prints formatted results table to stdout.
pub fn print_results_table(results: &BenchResults) {
    println!("\n================================================================================");
    println!("DYLOS SPIKE BENCHMARK RESULTS & GO / NO-GO CRITERIA COMPARISON");
    println!("================================================================================");
    println!(
        "{:<52} {:>12} {:>12} {:>16} {:>10}",
        "Measurement", "Median", "p95", "Go Threshold", "Verdict"
    );
    println!(
        "{:-<52} {:-<12} {:-<12} {:-<16} {:-<10}",
        "", "", "", "", ""
    );
    for row in results.rows() {
        println!(
            "{:<52} {:>12} {:>12} {:>16} {:>10}",
            row.label, row.median, row.p95, row.threshold, row.verdict
        );
    }
    if !results.snapshot_stages.freeze_ms.is_empty() {
        println!("\nSnapshot Stage Breakdown:");
        println!("{:<32} {:>12} {:>12}", "Stage", "Median", "p95");
        println!("{:-<32} {:-<12} {:-<12}", "", "", "");
        for (label, s) in results.snapshot_stages.as_slice() {
            let (median, p95) = stat(Some(s), 1.0, 1, "ms");
            println!("{label:<32} {median:>12} {p95:>12}");
        }
        if let Some((dom_name, dom_p95)) = results.snapshot_stages.dominant_stage() {
            println!("Dominant stage: {dom_name} ({dom_p95:.1} ms p95)");
        }
    }
    println!("================================================================================");
    println!(
        "Overall Technical Spike Verdict: {}",
        results.overall_verdict()
    );
    println!("================================================================================\n");
}

fn p95_text(samples: Option<&MetricSamples>, precision: usize, unit: &str) -> String {
    stat(samples, 1.0, precision, unit).1
}

fn fork_summary(results: &BenchResults) -> String {
    results
        .fork_counts()
        .into_iter()
        .map(|count| {
            format!(
                "n={count}: end-to-end p95 {}, amortized p95 {}/clone, restore sub-interval p95 {}",
                p95_text(results.fork_wall_time_ms.get(&count), 1, "ms"),
                p95_text(results.fork_amortized_ms.get(&count), 1, "ms"),
                p95_text(results.fork_restore_ms.get(&count), 1, "ms"),
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn pss_summary(results: &BenchResults) -> String {
    results
        .fork_counts()
        .into_iter()
        .map(|count| {
            let unsettled = results.pss_unsettled.get(&count).copied().unwrap_or(0);
            format!(
                "n={count}: p95 {}, {unsettled} unsettled",
                stat(results.per_clone_pss_kib.get(&count), 1024.0, 1, "MiB").1
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn conclusion(results: &BenchResults) -> String {
    let failing = results.failing_criteria();
    let outcome = if failing.is_empty() {
        String::from("Every measured criterion passed in this run.")
    } else {
        format!(
            "Criteria not met or not measured in this run: {}.",
            failing.join(", ")
        )
    };
    let scope = if results.is_reference_run() {
        "This is a full run outside CI."
    } else {
        "This is not the 20-repetition reference run outside CI, so the figures are indicative."
    };
    format!(
        "{outcome} {scope} The unmeasured criteria are covered by E2E as listed above; this benchmark does not observe their results."
    )
}

/// Generates the complete Markdown report for docs/bench/spike.md.
///
/// Measured verdicts are derived from `results`; missing data renders as
/// "no data" and counts as unmet. Unmeasured criteria reference separate E2E tests.
pub fn render_markdown_report(results: &BenchResults) -> String {
    let host = &results.host;
    let mut out = String::new();
    let snap_pause_p95 = p95_text(Some(&results.snapshot_freeze_to_thaw_ms), 1, "ms");
    let _ = write!(
        out,
        "# Technical Spike: Benchmark Report & Go / No-Go Decision

Quantified benchmark results evaluating Dylos technical spike microVM network consistency, snapshot performance, and fork latencies against the criteria defined in [`README.md`](../../README.md).

## Executive Summary & Verdict

- **Overall Spike Verdict:** **{overall}** ({mode}, {reps} repetitions per measurement)
- The overall verdict is computed only from what this benchmark measures.
- **Evaluation against Go Criteria:**
  - **Snapshot lab in under 1 s (pause window, freeze to thaw):** **{snapshot}** (p95: {snap_pause_p95} vs target < 1000 ms)
  - **Fork of one clone in under 500 ms (complete fork operation, end-to-end and amortized per clone):** **{fork}** ({forks})
  - **Fabric freeze in under 1 s:** **{freeze}** (p95: {freeze_p95})
  - **Pause confirmation skew in under 100 ms:** **{skew}** (p95: {skew_p95})
  - **Per-clone PSS under 256 MiB once the reading settled:** **{pss}** ({pss_text})
  - **Dual-stack TCP survivability in 100% clones:** **Covered by E2E** (`dylos-runtime::reliability_e2e tcp_survives_in_all_ten_clones`; run `just e2e`)
  - **Zero address conflict across concurrent clones:** **Covered by E2E** (`dylos-runtime::fork_e2e fork_ten_simultaneous_clones_independent_and_isolated`; run `just e2e`)
  - **100 snapshot/fork/destroy cycles without resource leak:** **Covered by E2E** (`dylos-runtime::reliability_e2e snapshot_fork_down_endurance`; run `just e2e`; 100 cycles by default, 5 in CI)

## Host Configuration ({mode})

- **CPU:** {cpu} ({vcpus} vCPUs)
- **RAM:** {ram_gib:.1} GiB ({ram_kib} KiB)
- **Kernel:** {kernel}
- **Filesystem:** {fs} (reflink supported: {reflink})
- **Firecracker:** {fc}
- **Reference run:** {reference}

## Performance Measurements & Go / No-Go Comparison

The benchmark uses the lab configuration recorded above (dual-stack IPv6/IPv4 network):

| Measurement | Go Criteria Threshold | Median | p95 | Verdict |
| :--- | :--- | :--- | :--- | :--- |
",
        overall = results.overall_verdict(),
        mode = results.mode_label(),
        reps = results.repetitions,
        snapshot = results.snapshot_verdict(),
        snap_pause_p95 = snap_pause_p95,
        fork = results.fork_verdict(),
        forks = fork_summary(results),
        freeze = results.freeze_verdict(),
        freeze_p95 = p95_text(Some(&results.freeze_duration_ms), 1, "ms"),
        skew = results.skew_verdict(),
        skew_p95 = p95_text(Some(&results.pause_skew_ms), 3, "ms"),
        pss = results.pss_verdict(),
        pss_text = pss_summary(results),
        cpu = host.cpu_model,
        vcpus = host.vcpu_count,
        ram_gib = host.ram_gib,
        ram_kib = host.ram_kib,
        kernel = host.kernel_release,
        fs = host.filesystem,
        reflink = host.reflink_supported,
        fc = host.firecracker_version,
        reference = if results.is_reference_run() {
            "this run (full, outside CI)"
        } else {
            "outstanding (20 repetitions outside CI)"
        },
    );
    for row in results.rows() {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            row.label, row.threshold, row.median, row.p95, row.verdict
        );
    }

    let _ = writeln!(
        out,
        "\n### Snapshot Capture Types\n\n| Repetition | Diff VMs | Full VMs |\n| :--- | ---: | ---: |"
    );
    let mut diff = 0;
    let mut full = 0;
    for (rep, &(d, f)) in results.snapshot_captures.iter().enumerate() {
        let _ = writeln!(out, "| {} | {d} | {f} |", rep + 1);
        diff += d;
        full += f;
    }
    let _ = writeln!(
        out,
        "\nCapture summary (VM captures): Diff = {diff}, Full = {full}."
    );

    let stage_freeze = stat(Some(&results.snapshot_stages.freeze_ms), 1.0, 1, "ms");
    let stage_pause = stat(Some(&results.snapshot_stages.pause_ms), 1.0, 1, "ms");
    let stage_api = stat(Some(&results.snapshot_stages.api_snapshot_ms), 1.0, 1, "ms");
    let stage_storage = stat(Some(&results.snapshot_stages.storage_ms), 1.0, 1, "ms");
    let stage_resume = stat(Some(&results.snapshot_stages.resume_ms), 1.0, 1, "ms");
    let stage_thaw = stat(Some(&results.snapshot_stages.thaw_ms), 1.0, 1, "ms");
    let stage_hash = stat(Some(&results.snapshot_stages.hash_ms), 1.0, 1, "ms");
    let stage_manifest = stat(Some(&results.snapshot_stages.manifest_ms), 1.0, 1, "ms");

    let _ = write!(
        out,
        "
### Snapshot Stage Breakdown

| Stage | Median | p95 | Description |
| :--- | :--- | :--- | :--- |
| Freeze | {stage_freeze_med} | {stage_freeze_p95} | Network fabric freeze |
| Pause | {stage_pause_med} | {stage_pause_p95} | VM pause API |
| API snapshot | {stage_api_med} | {stage_api_p95} | Memory & state serialization |
| Storage | {stage_storage_med} | {stage_storage_p95} | Disk reflink & file capture |
| Resume | {stage_resume_med} | {stage_resume_p95} | VM resume API |
| Thaw | {stage_thaw_med} | {stage_thaw_p95} | Network fabric thaw |
| Hashing | {stage_hash_med} | {stage_hash_p95} | SHA-256 integrity hashing (post-thaw) |
| Manifest write | {stage_manifest_med} | {stage_manifest_p95} | Manifest write & atomic publication |
",
        stage_freeze_med = stage_freeze.0,
        stage_freeze_p95 = stage_freeze.1,
        stage_pause_med = stage_pause.0,
        stage_pause_p95 = stage_pause.1,
        stage_api_med = stage_api.0,
        stage_api_p95 = stage_api.1,
        stage_storage_med = stage_storage.0,
        stage_storage_p95 = stage_storage.1,
        stage_resume_med = stage_resume.0,
        stage_resume_p95 = stage_resume.1,
        stage_thaw_med = stage_thaw.0,
        stage_thaw_p95 = stage_thaw.1,
        stage_hash_med = stage_hash.0,
        stage_hash_p95 = stage_hash.1,
        stage_manifest_med = stage_manifest.0,
        stage_manifest_p95 = stage_manifest.1,
    );

    let cold_boot = stat(Some(&results.cold_boot_ms), 1.0, 1, "ms").0;
    let fork_n1 = stat(results.fork_wall_time_ms.get(&1), 1.0, 1, "ms").0;
    let snap_total_p95 = p95_text(Some(&results.snapshot_total_ms), 1, "ms");
    let dominant = results.snapshot_stages.dominant_stage().map_or_else(
        || "no data".into(),
        |(name, p95)| format!("{name} (p95: {p95:.1} ms)"),
    );

    let _ = write!(
        out,
        "
## Measurement Intervals & Statistical Methodology

- **Percentile Calculation:** Values are computed using sorted linear interpolation at rank $p \\times (N - 1)$.
- **Repetitions & Samples:** Cold boot and snapshot are evaluated across independent repetitions. Fork and PSS evaluate $N \\times \\text{{repetitions}}$ clone observations grouped by clone count (clones of one batch are correlated). In quick mode (2 repetitions), p95 is an interpolation close to the maximum and is indicative only.
- **Snapshot Baseline:** Keep the previous snapshot as the Diff baseline; delete the snapshot from two repetitions back before measurement, retaining at most two snapshots and forking from the last.
- **Interval Definitions:**
  - *Fabric Freeze Duration:* duration of the fabric-freeze call on the network fabric. It does not measure guest-observed network downtime.
  - *Pause Confirmation Skew:* duration from the first successful VM pause API response to the last.
  - *Snapshot Pause Window (freeze to thaw):* duration from starting fabric freeze through completing thaw, while VMs are paused and fabric is frozen. This is the go/no-go snapshot criterion.
  - *Snapshot Total Until Published:* end-to-end operation including VM pause, disk sync, memory state serialization, VM resume, fabric thaw, SHA-256 integrity calculation, final manifest write and atomic publication. VMs are resumed and working during hashing and publication.
  - *Fork End-to-End:* duration of `Lab::fork_with` for the whole batch, from entry through all clones restored and ready. It covers manifest verification, storage reflink cloning, parallel VM launch, resume and readiness. For n = 1 it is the README criterion directly.
  - *Fork Amortized per Clone:* end-to-end duration divided by the clone count. Per-clone completion latency inside a batch is not measured; the end-to-end duration bounds it from above.
  - *Restore Sub-Interval (netns to thaw):* per-clone interval from network namespace setup through Firecracker startup, snapshot memory loading, VM resume and network unfreeze. It excludes verification and storage preparation and is informational only: it never decides the fork verdict.
  - *Per-Clone PSS:* sum of Proportional Set Size across all VM processes of a clone. After the batch is ready, the reading is repeated every 250 ms until two consecutive readings differ by at most 1 MiB, with a 10 s deadline. Clones that miss the deadline are counted as unsettled and fail the PSS criterion. Stable PSS is an observed condition, not proof that the guests are idle.

## Analysis & Findings

1. **Snapshot:** pause window (freeze to thaw) p95 {snap_pause_p95} (limit 1000 ms), pause skew p95 {skew_p95}, total snapshot until published p95 {snap_total_p95}. Per-stage breakdown (p95): freeze {stage_freeze_p95}, pause {stage_pause_p95}, API snapshot {stage_api_p95}, storage {stage_storage_p95}, resume {stage_resume_p95}, thaw {stage_thaw_p95}, hashing {stage_hash_p95}, manifest write {stage_manifest_p95}. Dominant stage: {dominant}.
2. **Fork:** {forks}.
3. **Memory:** per-clone PSS after settling: {pss_text}. PSS alone does not prove copy-on-write sharing: lazy, largely untouched guest memory also produces low PSS. A sharing / private-dirty measurement is required for that claim.
4. **Cold boot comparison:** cold boot of the lab has a median of {cold_boot}; the fork end-to-end median for n = 1 is {fork_n1}.

## Conclusion & Next Steps

{conclusion}
",
        snap_pause_p95 = snap_pause_p95,
        skew_p95 = p95_text(Some(&results.pause_skew_ms), 3, "ms"),
        snap_total_p95 = snap_total_p95,
        stage_freeze_p95 = stage_freeze.1,
        stage_pause_p95 = stage_pause.1,
        stage_api_p95 = stage_api.1,
        stage_storage_p95 = stage_storage.1,
        stage_resume_p95 = stage_resume.1,
        stage_thaw_p95 = stage_thaw.1,
        stage_hash_p95 = stage_hash.1,
        stage_manifest_p95 = stage_manifest.1,
        dominant = dominant,
        forks = fork_summary(results),
        pss_text = pss_summary(results),
        cold_boot = cold_boot,
        fork_n1 = fork_n1,
        conclusion = conclusion(results),
    );
    out
}
