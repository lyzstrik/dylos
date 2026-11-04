# Contributing to Dylos

Thank you for contributing to Dylos. This guide provides a concise overview for human contributors. For detailed repository rules, see [`AGENTS.md`](AGENTS.md) (which is binding for AI coding agents and recommended reading for all contributors).

## Prerequisites

Development requires Linux with KVM, a pinned Firecracker release, and a reflink-capable filesystem (btrfs or XFS). See [`docs/host.md`](docs/host.md) for machine setup, pinned tool versions, and installation steps.

Verify your environment with:

```sh
just host-check
```

Optional: `.mcp.json` declares a project-scoped `rust-analyzer` MCP server for coding agents. Install it with `cargo install rust-analyzer-mcp`. It also needs `rust-analyzer` (`rustup component add rust-analyzer`).

## Workflow

1. **Issues**: Every change starts from an issue (tracked in Linear as `LYZ-xx`).
2. **Branch**: Create a branch off an up-to-date `origin/main`. `main` is protected; never commit directly to `main`.
3. **Commits**:
   - Follow [Conventional Commits](https://www.conventionalcommits.org/) (`<type>(<scope>): <description>`).
   - Include the issue reference in the commit footer (`Refs: LYZ-xx`).
   - Every commit must be signed (`git config commit.gpgsign true`).
4. **Pull Request**: Open a pull request targeting `main`.
   - Keep PRs small (under ~400 changed lines).
   - Use the repository pull request template (`.github/pull_request_template.md`).
   - Reviews follow [`docs/agents/review.md`](docs/agents/review.md).

## Quality Checks

All commands run through `just` (see `justfile`):

- Run `just check` before opening a pull request. It checks formatting (`cargo fmt`), clippy warnings, tests (`cargo nextest`), `cargo deny`, static agent compilation, and workflow linting.
- If your change touches microVM lifecycle, networking, storage, or runtime behavior, also run `just e2e` locally. End-to-end tests require KVM and run in CI on disposable runners; on a developer host they are run by a human (never run `sudo` inside tests).

## Security

Please report vulnerabilities privately following [`SECURITY.md`](SECURITY.md). Do not open public issues for security vulnerabilities.
