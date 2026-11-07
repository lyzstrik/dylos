# Snapshot Lineage & Lineage Tree Design Note

This document specifies the design and implementation of snapshot lineage tracking and tree visualization in Dylos (issue `LYZ-117`), complementing `dylos lab diff`.

## 1. Overview & Goal

Dylos microVM environments can be snapshotted, restored, and forked into parallel clone fleets. As labs branch across experiments and iterations, operators need visibility into snapshot provenance:
- Which lab did a snapshot originate from?
- Which origin snapshot was that lab restored or forked from?
- How do snapshots across time and clone labs relate to one another?

Lineage tracking records this relationship deterministically and renders the ancestry as a snapshot lineage tree via `dylos snapshot tree` (and `dylos lab lineage`).

## 2. Where Lineage is Stored

Lineage metadata is stored **directly inside each snapshot's `manifest.json`** in an optional `parent` field:

```json
{
  "format_version": 1,
  "parent": {
    "lab_id": "lab-alpha",
    "snapshot": "snap-20261105-01"
  },
  "lab_spec": { ... },
  "vms": { ... },
  ...
}
```

- `parent.lab_id`: The identifier of the lab from which this snapshot was captured.
- `parent.snapshot`: The identifier of the origin snapshot from which this lab was restored or forked (or the prior snapshot taken of the same lab), if any.
- **Backward compatibility:** The `parent` field is optional (`Option<SnapshotParent>`). Legacy manifests created before this feature lack the `parent` field; they deserialize cleanly with `parent: None` and are displayed as independent root nodes in the lineage tree.

Lineage metadata is recorded at snapshot capture time and sealed alongside the microVM state, guest memory, and rootfs disks when the snapshot is published atomically.

## 3. Survival Across Deletes

Lineage metadata is **decentralized and self-contained**: each snapshot's manifest independently records its immediate ancestry without relying on a central database, mutable index, or external registry.

- **Independent survival:** If an operator deletes, archives, or moves an ancestor snapshot, descendant snapshots remain completely valid, verifiable, and restorable.
- **Missing parent representation:** During tree resolution, if a snapshot references a parent that cannot be found in the scanned storage path (e.g., deleted), the lineage tree synthesizer generates a missing parent anchor:
  ```text
  [missing: snap-20261105-01]
  └── snap-20261105-02 (lab: lab-alpha)
  ```
  This preserves the relative relationships and branches among remaining sibling or descendant snapshots without breaking tree reconstruction.

## 4. Integrity & Verification (Fail-Closed)

Snapshots in Dylos are sealed using Linux fs-verity where supported, falling back to full SHA-256 validation (ADR-0005). Lineage tracking adheres to the same fail-closed security and integrity guarantees:

1. **Manifest Validation:** At deserialization time, `validate_parent()` rigorously checks that `lab_id` and `snapshot` conform to safe identifier syntax (ASCII alphanumeric and hyphens, no directory traversal components like `..`, no absolute paths). Malformed or unpermitted names are rejected immediately with `Error::InvalidName`.
2. **Strict Schema:** Unknown fields in the `parent` object or manifest are strictly rejected (`#[serde(deny_unknown_fields)]`).
3. **Fail-Closed Verification:** When `dylos snapshot tree` or `dylos lab lineage` scans snapshots, every candidate snapshot is verified through `verify_snapshot()` (checking manifest validity, file sizes, inventory, and fs-verity digests or SHA-256 hashes). If any snapshot is tampered with, corrupted, or incomplete, tree construction aborts immediately with a verification error rather than presenting untrusted state.
