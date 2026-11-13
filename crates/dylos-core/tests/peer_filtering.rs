#![allow(clippy::unwrap_used)]

use dylos_core::LabSpec;

fn spec() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("fixtures/valid_lab.yaml")).unwrap()
}

#[test]
fn policy_inherits_and_segment_overrides_in_both_directions() {
    let mut lab = spec();
    for lab_policy in [false, true] {
        lab.peer_filtering = lab_policy;
        lab.segments[0].peer_filtering = None;
        assert_eq!(lab.peer_filtering_for(&lab.segments[0]), lab_policy);
        for override_policy in [false, true] {
            lab.segments[0].peer_filtering = Some(override_policy);
            assert_eq!(lab.peer_filtering_for(&lab.segments[0]), override_policy);
            let yaml = serde_saphyr::to_string(&lab).unwrap();
            assert_eq!(LabSpec::from_yaml_str(&yaml).unwrap(), lab);
            let json = serde_json::to_string(&lab).unwrap();
            assert_eq!(serde_json::from_str::<LabSpec>(&json).unwrap(), lab);
        }
    }
}

#[test]
fn old_specs_and_snapshot_specs_keep_protection_inactive_during_rollout() {
    let lab = spec();
    let mut json = serde_json::to_value(&lab).unwrap();
    json.as_object_mut().unwrap().remove("peer_filtering");
    for segment in json["segments"].as_array_mut().unwrap() {
        segment.as_object_mut().unwrap().remove("peer_filtering");
    }
    let restored: LabSpec = serde_json::from_value(json).unwrap();
    assert!(!restored.peer_filtering);
    assert!(restored.segments.iter().all(|s| s.peer_filtering.is_none()));
}

#[test]
fn malformed_policy_is_rejected() {
    for value in [
        serde_json::json!("true"),
        serde_json::json!(1),
        serde_json::Value::Null,
    ] {
        let mut json = serde_json::to_value(spec()).unwrap();
        json["peer_filtering"] = value.clone();
        assert!(serde_json::from_value::<LabSpec>(json).is_err());
        let mut json = serde_json::to_value(spec()).unwrap();
        json["segments"][0]["peer_filtering"] = value.clone();
        if !value.is_null() {
            assert!(serde_json::from_value::<LabSpec>(json).is_err());
        }
    }
}

#[test]
fn topology_diff_reports_effective_policy_changes() {
    let a = spec();
    let mut b = a.clone();
    b.peer_filtering = true;
    let diff = dylos_core::diff::TopologyDiff::diff(&a, &b);
    assert_eq!(diff.segments.changed.len(), a.segments.len());
    assert!(
        diff.segments
            .changed
            .iter()
            .all(|s| s.peer_filtering.as_ref().unwrap().to)
    );
    b.segments[0].peer_filtering = Some(false);
    let diff = dylos_core::diff::TopologyDiff::diff(&a, &b);
    assert_eq!(diff.segments.changed.len(), a.segments.len() - 1);
}
