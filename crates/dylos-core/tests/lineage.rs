#![allow(clippy::unwrap_used, clippy::expect_used)]

use dylos_core::lineage::{SnapshotNode, build_lineage_tree, format_tree};
use dylos_core::manifest::{SnapshotManifest, SnapshotParent};

fn sample_manifest(parent: Option<SnapshotParent>) -> SnapshotManifest {
    let mut manifest =
        SnapshotManifest::from_json_str(include_str!("fixtures/sample_manifest.json")).unwrap();
    manifest.parent = parent;
    manifest
}

#[test]
fn parent_backward_compatible_json_roundtrip() {
    let raw_json = include_str!("fixtures/sample_manifest.json");
    let manifest = SnapshotManifest::from_json_str(raw_json).unwrap();
    assert_eq!(manifest.parent, None);

    let serialized = manifest.to_json_string().unwrap();
    assert!(!serialized.contains("\"parent\""));

    let with_parent = sample_manifest(Some(SnapshotParent {
        lab_id: "lab-origin".into(),
        snapshot: Some("snap-0".into()),
    }));
    let serialized_parent = with_parent.to_json_string().unwrap();
    assert!(serialized_parent.contains("\"parent\""));
    assert!(serialized_parent.contains("\"lab-origin\""));
    assert!(serialized_parent.contains("\"snap-0\""));

    let deserialized = SnapshotManifest::from_json_str(&serialized_parent).unwrap();
    assert_eq!(deserialized.parent, with_parent.parent);
}

#[test]
fn parent_validation_rejects_malformed_ids() {
    let mut manifest = sample_manifest(Some(SnapshotParent {
        lab_id: "../escape".into(),
        snapshot: None,
    }));
    assert!(manifest.validate_parent().is_err());
    assert!(manifest.to_json_string().is_err());

    manifest.parent = Some(SnapshotParent {
        lab_id: "valid-lab".into(),
        snapshot: Some("../escape".into()),
    });
    assert!(manifest.validate_parent().is_err());

    manifest.parent = Some(SnapshotParent {
        lab_id: "valid-lab".into(),
        snapshot: Some("valid-parent".into()),
    });
    assert!(manifest.validate_parent().is_ok());

    manifest.parent = Some(SnapshotParent {
        lab_id: "valid-lab".into(),
        snapshot: Some("valid-lab/valid-parent".into()),
    });
    assert!(manifest.validate_parent().is_ok());
}

#[test]
fn build_and_format_lineage_tree_hierarchies() {
    let nodes = vec![
        SnapshotNode {
            id: "snap-root".into(),
            lab_id: "lab-a".into(),
            created_at_unix_ms: 100,
            parent_lab_id: None,
            parent_snapshot_id: None,
            missing: false,
            children: vec![],
        },
        SnapshotNode {
            id: "snap-child-1".into(),
            lab_id: "lab-a".into(),
            created_at_unix_ms: 200,
            parent_lab_id: Some("lab-a".into()),
            parent_snapshot_id: Some("snap-root".into()),
            missing: false,
            children: vec![],
        },
        SnapshotNode {
            id: "snap-child-2".into(),
            lab_id: "lab-b".into(),
            created_at_unix_ms: 300,
            parent_lab_id: Some("lab-a".into()),
            parent_snapshot_id: Some("snap-root".into()),
            missing: false,
            children: vec![],
        },
        SnapshotNode {
            id: "snap-grandchild".into(),
            lab_id: "lab-b".into(),
            created_at_unix_ms: 400,
            parent_lab_id: Some("lab-b".into()),
            parent_snapshot_id: Some("snap-child-2".into()),
            missing: false,
            children: vec![],
        },
    ];

    let tree = build_lineage_tree(nodes);
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].id, "snap-root");
    assert_eq!(tree[0].children.len(), 2);
    assert_eq!(tree[0].children[0].id, "snap-child-1");
    assert_eq!(tree[0].children[1].id, "snap-child-2");
    assert_eq!(tree[0].children[1].children.len(), 1);
    assert_eq!(tree[0].children[1].children[0].id, "snap-grandchild");

    let formatted = format_tree(&tree);
    let expected = "\
snap-root (lab: lab-a)
├── snap-child-1 (lab: lab-a)
└── snap-child-2 (lab: lab-b)
    └── snap-grandchild (lab: lab-b)";
    assert_eq!(formatted.trim(), expected);
}

#[test]
fn lineage_tree_survives_parent_deletion() {
    let nodes = vec![SnapshotNode {
        id: "snap-orphan".into(),
        lab_id: "lab-c".into(),
        created_at_unix_ms: 500,
        parent_lab_id: Some("lab-deleted".into()),
        parent_snapshot_id: Some("snap-deleted".into()),
        missing: false,
        children: vec![],
    }];

    let tree = build_lineage_tree(nodes);
    assert_eq!(tree.len(), 1);
    assert!(tree[0].missing);
    assert_eq!(tree[0].id, "snap-deleted");
    assert_eq!(tree[0].children.len(), 1);
    assert_eq!(tree[0].children[0].id, "snap-orphan");

    let formatted = format_tree(&tree);
    let expected = "\
[missing: snap-deleted]
└── snap-orphan (lab: lab-c)";
    assert_eq!(formatted.trim(), expected);
}
