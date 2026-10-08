# AGENTS.md

Instructions for every AI coding agent working on this repository (Claude Code, Antigravity CLI, OpenCode, or any other).
Read this file entirely before doing anything. When this file and your own habits disagree, this file wins.

## 1. What Dylos is

Dylos is an engine for realistic multi-machine environments that can be snapshotted and forked consistently.
It runs networks of Firecracker microVMs, freezes them at a single logical instant, and clones them N times.

Current phase: **technical spike**: prove that a network of microVMs can be snapshotted consistently and forked in under 500 ms per clone.
Out of scope for now: web UI, multi-host, Windows guests, eBPF, public API.

## 2. Sources of truth

| What | Where |
| --- | --- |
| Tasks, scope, acceptance criteria | Linear, team **Lyzstrik**, project **Dylos** (issues `LYZ-xx`) |
| Design and rationale | Design doc linked from the Linear project |
| Architecture decisions | `docs/adr/` |
| Host setup, pinned versions | `docs/host.md` |
| How to review a PR | `docs/agents/review.md` |

The Linear issue is your contract: do what its acceptance criteria say, nothing more, nothing less.
If it is ambiguous, contradicts an ADR, or cannot be done as written: **stop and ask**.

## 3. Workflow (mandatory)

### `main` is protected: never touch it directly

- **Never commit, edit files, or push on `main`.** Never run your task in the `main` worktree.
- Every change, however small (typo, doc, config), goes through a **pull request targeting `main`**.
- Start from an up-to-date `origin/main` in your own worktree and branch.
- If you notice you are on `main`, stop immediately and tell the human.

### Steps

1. **One issue at a time**, only if all its blocking issues are `Done`.
2. **One issue = one worktree = one branch = one PR.** Branch name: the `gitBranchName` of the Linear issue (it contains `lyz-xx`, which links the PR to the issue).
3. Keep PRs **under ~400 changed lines** (excluding lockfiles and generated files). If more is needed, stop and propose a split.
   Acceptance tests added to the branch by an agent of another model family do not count toward this limit; the implementation alone must stay under it.
4. Run `just check` (section 6). Never open a PR with a failing check.
5. Open the PR with `gh pr create --base main`, using the format in section 8.
6. **Never merge. Never approve your own work. Never mark a Linear issue `Done`.** A human does that.

### Public repository

Everything you write in commits, PRs and comments is public. No secrets, no internal notes, no personal data. Professional tone.
No AI attribution: no `Co-Authored-By` trailer for an AI model and no "Generated with ..." line in commits, PRs or comments.

### Issues labeled `critical-path`

Critical path (VM lifecycle, network fabric, freeze, snapshot, restore). Do **not** implement them unless the human explicitly asked you to in this session; then open a **draft** PR only.

### Scope discipline

- No drive-by refactors, renames or reformatting outside what your issue needs.
- No extra features. Put ideas under "Out of scope / follow-ups" in the PR.
- Do not edit `AGENTS.md`, ADRs, CI, `justfile` or `deny.toml` unless the issue is about them.

## 4. Repository layout

```
crates/
  dylos-core/     Pure domain: LabSpec, validation, snapshot manifest. No system access.
  dylos-fc/       Typed client for the Firecracker HTTP API over a Unix socket.
  dylos-net/      Network namespaces, bridges, TAP devices, fabric freeze/thaw.
  dylos-store/    Lab directory layout, reflink cloning of disks.
  dylos-runtime/  Orchestration: up, down, snapshot, restore, fork.
  dylos-agent/    In-guest agent over vsock. Built static (musl).
  dylos-cli/      The `dylos` binary.
xtask/            Complex build/test logic in Rust, called from the justfile.
justfile          Single entry point for every dev command.
docs/  labs/ (LabSpec YAML only, never runtime data)  scripts/
```

Dependency direction: `dylos-cli -> dylos-runtime -> {dylos-fc, dylos-net, dylos-store} -> dylos-core`.
`dylos-core` depends on nothing system-related (no tokio, nix, libc, rtnetlink). `dylos-agent` is standalone.
A crate never depends on a crate above it.

## 5. Rust conventions

- Edition 2024, toolchain pinned in `rust-toolchain.toml`. Do not change it.
- Code, comments, commits and PRs are in **English**.
- Dependency versions live in `[workspace.dependencies]`. Any new dependency needs a one-line justification in the PR and must pass `cargo deny`.
- **Errors**: libraries use `thiserror` with contextual variants (which lab, which VM, which path); binaries use `anyhow` with `.context(...)`.
- **No `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!` outside tests.** Clippy denies them. Never silently discard an error.
- **`unsafe`**: `#![forbid(unsafe_code)]` everywhere except `dylos-net` and `dylos-store`. There, each block is minimal, has a `// SAFETY:` comment justifying every invariant, and is wrapped in a safe function. Mention any `unsafe` change under "Risks".
- **Async**: `tokio` only. Never block in async code; use `spawn_blocking` for blocking syscalls. Lab-wide steps run concurrently, but each step waits for **all** VMs before the next one. Never reorder the snapshot/restore sequences of the design doc and ADR-001.
- **Observability**: `tracing` only, no `println!` outside CLI output. One span per lab and per VM, with step durations.
- **Comments**: only where the code is complicated, non-intuitive or a trap for a reviewer: units, invariants, non-obvious defaults, constraints from the Firecracker spec or an ADR. Never a comment that restates the name or the type. Update comments in the same PR as the code.
- **Test location**: tests go in `crates/<crate>/tests/*.rs` and use the public API. Keep an in-crate `#[cfg(test)]` module only for a private helper that cannot be reached through the public API.

### Networking: IPv6-first, dual-stack

Lab networks are IPv6-first and dual-stack by default (ADR-0002).
Every segment has an IPv6 prefix and, by default, an IPv4 prefix; addresses are static and declared in the LabSpec.
Code, tests and examples use IPv6 first and also cover IPv4. Never add an IPv4-only code path or test.

### Resource hygiene (critical)

Dylos creates processes, network namespaces, bridges, TAP devices, sockets and files on the host.
Everything created must be destroyed: RAII guards plus idempotent teardown that succeeds when called twice or after a partial failure.
On error mid-operation, roll back before returning. Resource names are deterministic (derived from LabSpec and lab id). Tests assert nothing is left behind.

## 6. Commands: always through `just`

All dev commands go through the `justfile`. Do not invent ad-hoc command sequences; if a recipe is missing, say so in the PR.

| Recipe | What it does |
| --- | --- |
| `just` | List recipes |
| `just fmt` | Format all code |
| `just check` | Everything CI runs: fmt check, clippy `-D warnings`, nextest, cargo-deny |
| `just test` | Unit and property tests (nextest) |
| `just host-check` | Verify the host (KVM, pinned Firecracker, reflink) |
| `just e2e` | `host-check`, then end-to-end tests (needs KVM) |

Before every PR: `just check`. If you touched VM, network, storage or runtime behavior: also `just e2e`.
CI runs `just check`, so local and CI results must match.

## 7. Tests

- Each acceptance criterion maps to at least one test, or the PR explains why not.
- Tests in `crates/<crate>/tests/*.rs` through the public API (section 5); property tests (`proptest`) for pure logic in `dylos-core`.
- Test data longer than a few lines (YAML lab specs, JSON, shell scripts) goes in `crates/<crate>/tests/fixtures/` as real files, loaded with `include_str!` or run by path. Only one-line snippets stay inline.
- KVM/privileged tests are e2e tests (`just e2e`), skipped with an explicit message when `/dev/kvm` is missing.
- **Never delete, weaken or `#[ignore]` a test to make a check pass.** If a test seems wrong, stop and ask.
- No sleeps as synchronization: wait on a condition with a timeout.
- Tests touch the host only inside their own network namespace and temp directories.

## 8. Commits and pull requests

Commits: Conventional Commits, scope = crate or area, issue in footer:

```
feat(net): create per-lab network namespace

Refs: LYZ-15
```

PR title: `<type>(<scope>): <summary> [LYZ-xx]`. PR description:

```markdown
## Issue
LYZ-xx: <title>
## Summary
2 to 4 sentences.
## Acceptance criteria
- [x] Criterion: how it is satisfied / which test covers it
## Risks
unsafe? new dependency? behavior change? where to look first.
## Checks
just check / just e2e: results.
## Out of scope / follow-ups
```

## 9. Reviewing

If you are asked to review a PR, follow `docs/agents/review.md` exactly.

## 10. Security

- A PR touching `dylos-net`, `dylos-runtime`, `dylos-store`, `dylos-agent`, `dylos-fc`, parsing in `dylos-core`, `xtask/images`, `scripts/` or CI gets a dedicated security review pass, following the checklist and `docs/security/threat-model.md`; other PRs only go through the checklist in the normal review. For a PR authored by several model families, the security pass uses a family that wrote none of it, or, if none is left, the family that wrote the least and never the one that wrote the code under review. The pass is recorded as one PR comment with the header `## Security review: <model>`. Keep it short and factual; no claim of protection that the code does not have.
- Never commit or print secrets, tokens or credentials.
- Never commit binaries or runtime data (kernels, rootfs, snapshots, memory files, disks). Record versions and SHA-256 in `docs/host.md`.
- Never use `sudo` or change host configuration (sysctl, firewall, services, kernel modules) without asking first. Host network changes happen only inside the lab's namespace.
- See [ADR-0004](docs/adr/0004-privilege-model.md) for privileged operations. Tests stay unprivileged with fakes; e2e tests may need root and are run by a human, never via `sudo` inside tests.
- Never disable a check, lint or `cargo deny` rule to get a PR through.

## 11. Stop and ask the human when

- acceptance criteria are ambiguous, contradictory or impossible,
- a change would contradict an ADR or the design doc,
- you need a new crate, an architecture change, or new dependency category,
- you would touch `critical-path` code without being asked,
- a check fails and one honest attempt did not explain why,
- you are about to exceed ~400 changed lines,
- you find yourself on `main`.

Asking early is always cheaper than a wrong PR.
