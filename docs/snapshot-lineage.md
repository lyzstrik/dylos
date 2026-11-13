# Snapshot lineage

A version 1 manifest may contain `parent: { "lab_id": "source", "snapshot": "snap" }`.
Absent `parent` means no recorded origin; legacy manifests parse and verify unchanged.
Both identifiers follow the existing lab/snapshot rules: at most 32 ASCII letters,
digits or hyphens, beginning with a letter or digit. Slashes are never allowed.
Malformed parents fail parsing, serialization and verification.

Lineage is informational. It has the manifest's integrity: it detects nothing
against someone who can write the snapshot directory. Payload checksums also live
in that directory; no metadata checksum or authentication is added. Signed
manifests would be a separate future decision. Parent values are display-only:
they are never used to restore, hash files, or make a security decision.

Each child stores its own parent identity. Deleting a parent leaves its surviving
children readable; the tree displays the absent identity as `[missing]`. There is
no separate index to update, and deletion loses the deleted node's own ancestry.
Identities are scoped by lab so equal snapshot names in different labs do not
collide. Duplicate identities and cycles are errors; output order is deterministic.

Runtime recording and CLI discovery are implemented in the stacked follow-up.
Lineage adds no work between fabric freeze and thaw (ADR-0001), and does not
change diff snapshot or baseline selection.
