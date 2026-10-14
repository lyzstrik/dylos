export NEXTEST_NO_TESTS := "warn"

# List recipes
default:
    @just --list

# Format all code
fmt:
    cargo fmt

# Verify formatting
fmt-check:
    cargo fmt --check

# Run clippy with pedantic warnings denied
clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Run all checks (format, clippy, doc, tests, cargo-deny, static agent, workflow lint)
check:
    just fmt-check
    just clippy
    just doc
    just test
    just deny
    just agent-check
    just ci-lint

# Check dependency policy
deny:
    cargo deny --locked check

# Build and verify static musl guest agent
agent-check:
    cargo build --locked -p dylos-agent --target x86_64-unknown-linux-musl
    ./scripts/check-static-elf.sh target/x86_64-unknown-linux-musl/debug/dylos-agent
    ./scripts/test-check-static-elf.sh target/x86_64-unknown-linux-musl/debug/dylos-agent

# Lint GitHub Actions workflows
ci-lint:
    actionlint
    zizmor .github/workflows

# Generate and verify documentation
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked


# Run unit and property tests
test:
    cargo nextest run --workspace --locked

# Verify host prerequisites (KVM, Firecracker, reflink)
host-check:
    ./scripts/host-check.sh

# Run end-to-end tests (requires host prerequisites)
e2e: host-check
    cargo xtask e2e

# Build guest images (rootfs) with rootless podman
images:
    cargo xtask images

# Build every fuzz harness with the isolated nightly toolchain
fuzz-check:
    cd fuzz && cargo deny --locked check
    cd fuzz && cargo fuzz build

# Fuzz one parser for a bounded number of seconds
fuzz target seconds="120":
    #!/usr/bin/env bash
    set -euo pipefail
    case '{{target}}' in labspec|manifest|agent_protocol) ;; *) echo "Unknown fuzz target" >&2; exit 2 ;; esac
    [[ '{{seconds}}' =~ ^[1-9][0-9]*$ ]] || { echo "Seconds must be positive" >&2; exit 2; }
    cd fuzz
    mkdir -p 'corpus/{{target}}'
    cp -n seeds/{{target}}/* 'corpus/{{target}}/'
    cargo fuzz run '{{target}}' -- -max_total_time='{{seconds}}'
