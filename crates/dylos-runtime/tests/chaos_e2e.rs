#![allow(clippy::unwrap_used, clippy::expect_used)]
use dylos_core::LabSpec;
use dylos_runtime::{Lab, LabConfig, jailer::JailerConfig};
use futures_util::FutureExt;
use std::time::Duration;
mod support;

async fn rtt(lab: &Lab, family: &str, address: &str) -> f64 {
    let output = lab
        .exec(
            "A",
            &format!("ping {family} -c 5 -i 0.2 -W 3 {address}"),
            Duration::from_secs(15),
        )
        .await
        .unwrap();
    assert!(output.success(), "{output:?}");
    let summary = output
        .stdout
        .lines()
        .find(|line| line.contains("min/avg/max"))
        .unwrap();
    summary
        .split('=')
        .nth(1)
        .unwrap()
        .trim()
        .split('/')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
#[ignore = "end-to-end: needs root and KVM, run with `just e2e`"]
async fn fork_chaos_delays_only_selected_clone_dual_stack() {
    let Some(p) =
        support::check_e2e_prerequisites("fork_chaos_delays_only_selected_clone_dual_stack")
    else {
        return;
    };
    let dir = support::test_tempdir();
    let mut jailer = JailerConfig::new(dir.path().join("j"), 65534, 65534);
    jailer.jailer = p.bin.join("jailer");
    jailer.firecracker = p.bin.join("firecracker");
    let mut config = LabConfig {
        store: dylos_store::Store {
            base: dir.path().join("labs"),
            image: p.image,
        },
        jailer,
        kernel: p.kernel,
        netns_dir: dir.path().join("netns"),
        ..LabConfig::default()
    };
    let mut spec = LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap();
    for node in &mut spec.nodes {
        node.memory = 128;
        node.vcpus = 1;
    }
    let mut source = Lab::up_with(&config, &spec, "chaos-src").await.unwrap();
    let mut clones = Vec::new();
    let result = std::panic::AssertUnwindSafe(async {
        source.snapshot("instant").await.unwrap();
        config.chaos.insert(
            "slow".into(),
            dylos_net::ChaosProfile::parse("50:0:0").unwrap(),
        );
        clones = Lab::fork_with(
            &config,
            &config.store.snapshot_dir("chaos-src", "instant"),
            &["slow".into(), "control".into()],
        )
        .await
        .unwrap();
        // A -> B -> C -> B -> A crosses four shaped TAP egress queues.
        for (family, address) in [("-6", "fd64:796c:6f73:2::2"), ("-4", "10.0.2.2")] {
            let control = rtt(&clones[1], family, address).await;
            let slow = rtt(&clones[0], family, address).await;
            eprintln!("CHAOS {family}: control={control}ms slow={slow}ms");
            assert!(
                slow >= control + 150.0,
                "{family}: control={control}, slow={slow}"
            );
            assert!(
                control < 100.0,
                "{family}: unshaped clone delayed: {control}"
            );
        }
        // Freeze/thaw preserves the independent netem qdiscs.
        clones[0].snapshot("shaped").await.unwrap();
        assert!(rtt(&clones[0], "-6", "fd64:796c:6f73:2::2").await >= 150.0);
    })
    .catch_unwind()
    .await;
    for clone in &mut clones {
        let id = clone.id().to_owned();
        clone.teardown().await.unwrap();
        clone.teardown().await.unwrap();
        assert!(!config.netns_dir.join(format!("dylos-{id}")).exists());
    }
    source.teardown().await.unwrap();
    assert_eq!(std::fs::read_dir(&config.netns_dir).unwrap().count(), 0);
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
