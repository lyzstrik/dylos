# ADR-0005: Verify published snapshot content with fs-verity

- Status: accepted
- Date: 2026-11-07
- Issue: LYZ-109

## Context

Fork and restore currently read and SHA-256 hash every memory, state and disk file.
A successful CI E2E run (42391842486) measured 15928 ms for five-VM manifest
verification. Repeating that work makes fork verification grow with snapshot size.
Checking only the manifest cannot detect changed content.

The store and its ancestors are controlled by the trusted host operator, as in
ADR-0004 and the threat model. Guests and their jailer uid are untrusted; they
must not change published snapshot content. Shared memory inodes and private
reflink disk copies must continue to work.

## Decision

After capture and thaw, enable Linux fs-verity on every captured regular file,
then measure its SHA-256 verity digest and record it alongside the existing
content SHA-256 and size in the manifest. Publication remains atomic and follows
completion of all files and the manifest. Verity builds a kernel Merkle tree;
its digest commits to the file size, tree parameters and every content page.

Fork and restore compare each opened file's measured verity digest and size with
the published values. This costs O(number of files), independent of their sizes.
The kernel verifies content against the tree on each page read, including reads
through Firecracker's private memory mappings. An unread corrupted page is
rejected when consumed, rather than eagerly scanned before restore. Read errors
must propagate and fail the operation; successful measurement alone does not
mean every storage block was read. Disks and state are reflinked into writable
private inodes; memory retains the verified shared inode. Reflink consumes the sealed source inode through the trusted kernel; btrfs
shares its checksummed extents with the destination. Private writable clones are
not verity files: their deliberate guest writes are no longer snapshot content.
This relies on correct kernel reflink behavior and btrfs data checksums for
subsequent physical corruption of private copies, rather than claiming that
verity follows a reflink into the mutable destination.

On filesystems or kernels without fs-verity, retain full content SHA-256
verification. Unsupported enablement is an explicit fallback, never a metadata
or manifest-only check. A declared verity digest that cannot be measured or does
not match fails closed. Legacy manifests use full hashing. Size-independent
verification is promised only on verity-capable storage; fallback remains O(bytes).

CI E2E and benchmarks use btrfs loop storage with compress=zstd:3 on pinned
ubuntu-24.04 runners, matching the owner's filesystem. A setup probe measures a
verity file, reflinks it and maps it privately; real Firecracker E2E tests remain
the compatibility gate. XFS provides reflink but not fs-verity on these runners,
so it cannot exercise the fast path. ext4 supports verity but lacks the required
reflink cloning.

## Threat model and limits

Verity files cannot be modified or truncated through normal filesystem writes,
even by root, and verity cannot be disabled on an existing inode. Root can still
replace files and the manifest, alter kernel behavior, or write the block device;
a trusted manifest and running kernel are required. Root is trusted, not an
adversary this mechanism claims to contain. A replacement file must match the
recorded digest, not just its path, inode number or timestamp. Block corruption
is detected when the affected page is read. This is integrity, not availability
or provenance authentication; an operator who can replace the manifest can
publish a different snapshot.

## Rejected alternatives

- Manifest-only hashing: leaves memory, disk and state content unchecked.
- Immutable flags plus inode/size seals: immutable files cannot gain or lose
  hard links, conflicting with shared memory and idempotent teardown. Repeatedly
  unsealing them introduces avoidable mutable windows and root-only cleanup.
- Timestamp or inode caches: not a cryptographic content commitment and require
  a separate trusted cache, invalidation and race protocol.
- Faster full hashing: useful fallback, but still O(bytes) per verification.
- Switching only CI to ext4: loses reflink, which the store requires.

## Validation

The implementation must cover state, memory and disk changes, missing or
mismatched verity digests, legacy fallback, reflink consumption, shared memory
mappings and cleanup. CI logs must compare unchanged snapshot verification and
fork timings against the baseline. The first PR supplies the decision and
storage probes; following stacked PRs implement publication and consumption.

## Revision: 2026-11-13 (LYZ-136)

Before sealing, lazily probe a 32 MiB page-sparse file on the first store
filesystem, including fsync, verity enablement, cache eviction and full comparison.
Some btrfs kernels accept enablement but return EIO when sparse content is read
back. Any probe failure disables new sealing process-wide and retains full
SHA-256 verification; existing declared verity digests still fail closed.
The probe runs once after thaw, never on restore or fork. Other store devices
conservatively retain full hashing rather than reuse a different device's success.
