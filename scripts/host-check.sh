#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo -e "\e[31m[ERROR]\e[0m $1" >&2
    exit 1
}

# Reference paths
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOC_HOST="${REPO_ROOT}/docs/host.md"

# 1. Check /dev/kvm
echo "==> Checking KVM..."
[ -e /dev/kvm ] || fail "/dev/kvm does not exist."
[ -r /dev/kvm ] && [ -w /dev/kvm ] || fail "Current user does not have read/write permissions on /dev/kvm."

# 2. Check Firecracker, Jailer and their checksums
echo "==> Checking Firecracker and Jailer..."
FC_BIN="$(command -v firecracker 2>/dev/null)" || fail "Firecracker is not installed in PATH."
JAILER_BIN="$(command -v jailer 2>/dev/null)" || fail "Jailer is not installed in PATH."

echo "  - Firecracker : $(firecracker --version | head -n1)"
echo "  - Jailer      : $(jailer --version | head -n1)"

if [ -f "$DOC_HOST" ]; then
    echo "==> Checking consistency with docs/host.md..."

    EXPECTED_FC_SHA="$(grep -E 'SHA-256 firecracker[[:space:]]*:' "$DOC_HOST" | head -n1 | sed -E 's/.*:[[:space:]]*([a-fA-F0-9]{64}).*/\1/')"
    EXPECTED_JAILER_SHA="$(grep -E 'SHA-256 jailer[[:space:]]*:' "$DOC_HOST" | head -n1 | sed -E 's/.*:[[:space:]]*([a-fA-F0-9]{64}).*/\1/')"

    if [ -n "$EXPECTED_FC_SHA" ]; then
        ACTUAL_FC_SHA="$(sha256sum "$FC_BIN" | awk '{print $1}')"
        if [ "$ACTUAL_FC_SHA" != "$EXPECTED_FC_SHA" ]; then
            fail "Firecracker checksum mismatch!\n  Expected (docs/host.md) : $EXPECTED_FC_SHA\n  Computed ($FC_BIN)     : $ACTUAL_FC_SHA"
        fi
        echo "  - SHA-256 Firecracker : OK"
    fi

    if [ -n "$EXPECTED_JAILER_SHA" ]; then
        ACTUAL_JAILER_SHA="$(sha256sum "$JAILER_BIN" | awk '{print $1}')"
        if [ "$ACTUAL_JAILER_SHA" != "$EXPECTED_JAILER_SHA" ]; then
            fail "Jailer checksum mismatch!\n  Expected (docs/host.md) : $EXPECTED_JAILER_SHA\n  Computed ($JAILER_BIN)     : $ACTUAL_JAILER_SHA"
        fi
        echo "  - SHA-256 Jailer      : OK"
    fi
else
    echo -e "\e[33m[WARNING]\e[0m $DOC_HOST not found, skipping checksum check."
fi

# 3. Check reflink support in the repo (~/dylos/labs)
echo "==> Checking reflink support..."
TARGET_DIR="${REPO_ROOT}/labs"
mkdir -p "$TARGET_DIR"

TMP_SRC=$(mktemp "$TARGET_DIR/reflink_test_src.XXXXXX")
TMP_DST=$(mktemp -u "$TARGET_DIR/reflink_test_dst.XXXXXX")

trap 'rm -f "$TMP_SRC" "$TMP_DST"' EXIT

echo "test-reflink" > "$TMP_SRC"

if ! cp --reflink=always "$TMP_SRC" "$TMP_DST" 2>/dev/null; then
    fail "Filesystem on $TARGET_DIR does not support --reflink=always."
fi

echo "  - Tested directory : $TARGET_DIR (reflink operational)"
echo -e "\e[32m[SUCCESS]\e[0m All host prerequisites are met."
