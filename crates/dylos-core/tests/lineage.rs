#![allow(clippy::unwrap_used)]
use dylos_core::lineage::{LineageError, SnapshotNode, format_tree};
use dylos_core::manifest::{SnapshotManifest, SnapshotParent};

fn id(lab: &str, snapshot: &str) -> SnapshotParent {
    SnapshotParent {
        lab_id: lab.into(),
        snapshot: snapshot.into(),
    }
}

#[test]
fn legacy_and_parent_roundtrip_and_verify() {
    let mut manifest =
        SnapshotManifest::from_json_str(include_str!("fixtures/legacy-lineage.json")).unwrap();
    assert!(manifest.parent.is_none());
    assert!(!manifest.to_json_string().unwrap().contains("\"parent\""));
    manifest.verify(|_| Ok(std::io::empty())).unwrap();
    manifest.parent = Some(id("origin", "snapshot"));
    let json = manifest.to_json_string().unwrap();
    assert_eq!(SnapshotManifest::from_json_str(&json).unwrap(), manifest);
    manifest.verify(|_| Ok(std::io::empty())).unwrap();
    for invalid in [
        "../escape",
        "/absolute",
        "a/b",
        "",
        "under_score",
        &"a".repeat(33),
    ] {
        for parent in [id(invalid, "valid"), id("valid", invalid)] {
            manifest.parent = Some(parent);
            assert!(manifest.to_json_string().is_err());
            assert!(manifest.verify(|_| Ok(std::io::empty())).is_err());
            assert!(
                SnapshotManifest::from_json_str(&serde_json::to_string(&manifest).unwrap())
                    .is_err()
            );
        }
    }
}

#[test]
fn scoped_deterministic_tree_and_missing_parent() {
    let root = SnapshotNode {
        id: id("a", "root"),
        parent: None,
    };
    let child = SnapshotNode {
        id: id("b", "child"),
        parent: Some(root.id.clone()),
    };
    let other = SnapshotNode {
        id: id("b", "root"),
        parent: None,
    };
    let tree = format_tree(&[other.clone(), child.clone(), root.clone()]).unwrap();
    assert_eq!(tree, "a/root\n  b/child\nb/root\n");
    assert_eq!(
        format_tree(&[root, child.clone(), other.clone()]).unwrap(),
        tree
    );
    assert_eq!(
        format_tree(&[child, other]).unwrap(),
        "a/root [missing]\n  b/child\nb/root\n"
    );
}

#[test]
fn cycles_and_duplicates_fail_closed() {
    let a = SnapshotNode {
        id: id("lab", "a"),
        parent: Some(id("lab", "b")),
    };
    let b = SnapshotNode {
        id: id("lab", "b"),
        parent: Some(a.id.clone()),
    };
    assert!(matches!(
        format_tree(&[a.clone(), b]),
        Err(LineageError::Cycle)
    ));
    assert!(matches!(
        format_tree(&[a.clone(), a]),
        Err(LineageError::Duplicate(_))
    ));
    let a = SnapshotNode {
        id: id("lab", "a"),
        parent: Some(id("lab", "a")),
    };
    assert!(matches!(format_tree(&[a]), Err(LineageError::Cycle)));
    assert_eq!(format_tree(&[]).unwrap(), "");
}
