use serde_saphyr::{DuplicateKeyPolicy, Options};

#[test]
fn yaml_defaults_preserve_pre_upgrade_limits() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::default();
    let budget = options.budget.ok_or("default YAML budget is disabled")?;
    assert_eq!(budget.max_depth, 64);
    assert_eq!(budget.max_reader_input_bytes, Some(256 * 1024 * 1024));
    assert_eq!(budget.max_total_scalar_bytes, 64 * 1024 * 1024);
    assert_eq!(budget.max_events, 1_000_000);
    assert_eq!(budget.max_nodes, 250_000);
    assert_eq!(budget.max_aliases, 50_000);
    assert_eq!(budget.max_anchors, 50_000);
    assert_eq!(budget.max_recorded_anchor_events, 1_000_000);
    assert_eq!(budget.max_recorded_anchor_bytes, 64 * 1024 * 1024);
    assert!(budget.enforce_alias_anchor_ratio);
    assert_eq!(budget.alias_anchor_min_aliases, 100);
    assert_eq!(budget.alias_anchor_ratio_multiplier, 10);
    assert_eq!(options.alias_limits.max_total_replayed_events, 1_000_000);
    assert_eq!(options.alias_limits.max_replay_stack_depth, 64);
    assert_eq!(
        options.alias_limits.max_alias_expansions_per_anchor,
        usize::MAX
    );
    assert!(matches!(options.duplicate_keys, DuplicateKeyPolicy::Error));
    Ok(())
}
