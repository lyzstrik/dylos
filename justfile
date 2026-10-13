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

# Run all checks (format, clippy, doc, tests, cargo-deny)
check:
    just fmt-check
    just clippy
    just doc
    just test
    just deny

# Check dependency policy
deny:
    cargo deny --locked check

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
