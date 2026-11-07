#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK_SCRIPT="${REPO_ROOT}/scripts/host-check.sh"

if [ ! -x "$CHECK_SCRIPT" ]; then
    echo -e "\e[31m[ERROR]\e[0m Host check script not found or not executable at $CHECK_SCRIPT" >&2
    exit 1
fi

assert_fails() {
    local description="$1"
    local expected_pattern="$2"
    shift 2
    local output
    if output="$("$@" 2>&1)"; then
        echo -e "\e[31m[FAIL]\e[0m $description: expected failure, but command succeeded." >&2
        echo "Output was: $output" >&2
        exit 1
    fi
    if [ -n "$expected_pattern" ] && ! echo "$output" | grep -E "$expected_pattern" >/dev/null; then
        echo -e "\e[31m[FAIL]\e[0m $description: output did not match pattern '$expected_pattern'." >&2
        echo "Output was: $output" >&2
        exit 1
    fi
    echo -e "\e[32m[PASS]\e[0m $description"
}

assert_passes() {
    local description="$1"
    shift
    local output
    if ! output="$("$@" 2>&1)"; then
        echo -e "\e[31m[FAIL]\e[0m $description: expected success, but command failed." >&2
        echo "Output was: $output" >&2
        exit 1
    fi
    echo -e "\e[32m[PASS]\e[0m $description"
}

echo "==> Running regression tests for host-check.sh..."

# Verify KVM and Firecracker prerequisites exist before testing reflink probe
if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    echo -e "\e[33m[SKIP]\e[0m /dev/kvm is not accessible, skipping host-check tests."
    exit 0
fi

FC_BIN_DIR="${DYLOS_FC_BIN_DIR:-$HOME/.local/share/dylos/bin}"
if [ ! -x "$FC_BIN_DIR/firecracker" ] || [ ! -x "$FC_BIN_DIR/jailer" ]; then
    echo -e "\e[33m[SKIP]\e[0m Firecracker/jailer not found in $FC_BIN_DIR, skipping host-check tests."
    exit 0
fi

# 1. Default run succeeds and never touches labs/
LABS_DIR="${REPO_ROOT}/labs"
BEFORE_LABS="$(ls -la "$LABS_DIR")"

assert_passes "Default host-check passes" "$CHECK_SCRIPT"

AFTER_LABS="$(ls -la "$LABS_DIR")"
if [ "$BEFORE_LABS" != "$AFTER_LABS" ]; then
    echo -e "\e[31m[FAIL]\e[0m labs/ directory was modified by host-check.sh!" >&2
    exit 1
fi
echo -e "\e[32m[PASS]\e[0m labs/ directory was not touched"

# 2. Reflink probe rejects incompatible jail directory mount
TEST_JAIL="/dev/shm/test_jail_$$"
mkdir -p "$TEST_JAIL"
trap 'rm -rf "$TEST_JAIL" "${TEST_IMAGE:-}" "${TEST_STORE:-}" "${TEST_DEFAULT_JAIL:-}"' EXIT

assert_fails "Incompatible jail directory rejected" \
    "FAIL \(lab store <-> jail directory\)" \
    env DYLOS_JAIL_DIR="$TEST_JAIL" "$CHECK_SCRIPT"

rm -rf "$TEST_JAIL"

# 3. Reflink probe rejects incompatible image directory mount
TEST_IMAGE="/dev/shm/test_image_$$"
mkdir -p "$TEST_IMAGE"

assert_fails "Incompatible image directory rejected" \
    "FAIL \(lab store <-> image location\)" \
    env DYLOS_IMAGE_DIR="$TEST_IMAGE" "$CHECK_SCRIPT"

rm -rf "$TEST_IMAGE"

# 4. Reflink probe rejects incompatible lab store mount
TEST_STORE="/dev/shm/test_store_$$"
TEST_DEFAULT_JAIL="/dev/shm/jails"
mkdir -p "$TEST_STORE"

assert_fails "Incompatible lab store rejected" \
    "FAIL \(lab store <-> image location\)" \
    env DYLOS_LAB_DIR="$TEST_STORE" "$CHECK_SCRIPT"

rm -rf "$TEST_STORE" "$TEST_DEFAULT_JAIL"

echo -e "\e[32m[OK]\e[0m All host-check regression tests passed."
