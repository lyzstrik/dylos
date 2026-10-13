#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK_SCRIPT="${REPO_ROOT}/scripts/check-static-elf.sh"

if [ ! -x "$CHECK_SCRIPT" ]; then
    echo -e "\e[31m[ERROR]\e[0m Checker script not found or not executable at $CHECK_SCRIPT" >&2
    exit 1
fi

STATIC_TARGET="${1:-${REPO_ROOT}/target/x86_64-unknown-linux-musl/debug/dylos-agent}"

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

echo "==> Running regression tests for check-static-elf.sh..."

# 1. Missing argument rejected
assert_fails "Missing argument rejected" "Usage:" "$CHECK_SCRIPT"

# 2. Missing file rejected
assert_fails "Non-existent file rejected" "not found" "$CHECK_SCRIPT" "/nonexistent/path/dylos-test-binary"

# 3. Non-ELF file (/etc/hosts) rejected
assert_fails "Non-ELF file (/etc/hosts) rejected" "not a valid ELF file" "$CHECK_SCRIPT" "/etc/hosts"

# 4. Non-ELF text and empty files rejected
TMP_TXT="$(mktemp "${TMPDIR:-/tmp}/dylos_test_txt.XXXXXX")"
TMP_EMPTY="$(mktemp "${TMPDIR:-/tmp}/dylos_test_empty.XXXXXX")"
trap 'rm -f "$TMP_TXT" "$TMP_EMPTY"' EXIT

echo "plain text payload" > "$TMP_TXT"
assert_fails "Non-ELF text file rejected" "not a valid ELF file" "$CHECK_SCRIPT" "$TMP_TXT"
assert_fails "Empty file rejected" "not a valid ELF file" "$CHECK_SCRIPT" "$TMP_EMPTY"

# 5. Dynamic binary with PT_INTERP rejected
TRUE_BIN="$(type -P true 2>/dev/null || which true 2>/dev/null || echo /usr/bin/true)"
if [ -f "$TRUE_BIN" ]; then
    assert_fails "Dynamic executable (PT_INTERP) rejected" "PT_INTERP" "$CHECK_SCRIPT" "$TRUE_BIN"
fi

# 6. Dynamic binary/library with NEEDED entries rejected
SO_FILE=""
for dir in /usr/lib /usr/lib64 /lib /lib64 /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu; do
    [ -d "$dir" ] || continue
    for f in "$dir"/libsqlite3.so* "$dir"/libz.so* "$dir"/liba52.so* "$dir"/*.so*; do
        [ -f "$f" ] || continue
        if ! readelf -l "$f" 2>/dev/null | grep -E '^[[:space:]]*INTERP[[:space:]]|Requesting program interpreter' >/dev/null && \
           readelf -d "$f" 2>/dev/null | grep -E '\(NEEDED\)' >/dev/null; then
            SO_FILE="$f"
            break 2
        fi
    done
done

if [ -n "$SO_FILE" ]; then
    assert_fails "Dynamic library without PT_INTERP (NEEDED) rejected" "NEEDED" "$CHECK_SCRIPT" "$SO_FILE"
fi

# 7. Static ELF binary accepted
if [ -f "$STATIC_TARGET" ]; then
    assert_passes "Static musl ELF ($STATIC_TARGET) accepted" "$CHECK_SCRIPT" "$STATIC_TARGET"
fi

echo -e "\e[32m[OK]\e[0m All check-static-elf regression tests passed."
