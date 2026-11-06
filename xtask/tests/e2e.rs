use std::process::{Command, Output};

fn e2e(args: &[&str], exit_code: &str) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("e2e")
        .args(args)
        .env(
            "CARGO",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cargo-e2e.sh"),
        )
        .env("E2E_TEST_EXIT_CODE", exit_code)
        .output()
}

#[test]
fn serial_e2e_runs_all_tests_or_one_partition_with_visible_output() -> anyhow::Result<()> {
    let all = e2e(&[], "0")?;
    assert!(all.status.success());
    let args = String::from_utf8(all.stdout)?;
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "nextest",
            "run",
            "--workspace",
            "--run-ignored",
            "only",
            "--test-threads",
            "1",
            "--success-output",
            "immediate",
            "--failure-output",
            "immediate",
        ]
    );

    let shard = e2e(&["hash:2/3"], "0")?;
    assert!(shard.status.success());
    assert_eq!(
        String::from_utf8(shard.stdout)?,
        format!("{args}--partition\nhash:2/3\n")
    );
    let filtered = e2e(&["filter:binary(fork_e2e) | binary(snapshot_e2e)"], "0")?;
    assert!(filtered.status.success());
    assert_eq!(
        String::from_utf8(filtered.stdout)?,
        format!("{args}--filterset\nbinary(fork_e2e) | binary(snapshot_e2e)\n")
    );
    assert_eq!(e2e(&[""], "0")?.stdout, args.as_bytes());
    assert!(!e2e(&[], "1")?.status.success());
    let extra = e2e(&["hash:1/3", "extra"], "0")?;
    assert!(!extra.status.success());
    assert!(extra.stdout.is_empty(), "must reject before invoking cargo");
    Ok(())
}
