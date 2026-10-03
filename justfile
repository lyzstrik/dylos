export NEXTEST_NO_TESTS := "warn"

# List recipes
default:
    @just --list

# Format all code
fmt:
    cargo fmt

# Run clippy with pedantic warnings denied
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Run all checks (format, clippy, tests, cargo-deny)
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo nextest run --workspace
    cargo deny check

# Run unit and property tests
test:
    cargo nextest run --workspace

# Verify host prerequisites (KVM, Firecracker, reflink)
host-check:
    ./scripts/host-check.sh

# Run end-to-end tests (requires host prerequisites)
e2e: host-check
    cargo xtask e2e
