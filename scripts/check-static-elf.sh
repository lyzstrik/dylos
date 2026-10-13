#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo -e "\e[31m[ERROR]\e[0m $1" >&2
    exit 1
}

ELF="${1:-}"
if [ -z "$ELF" ]; then
    fail "Usage: $0 <path-to-elf>"
fi

if [ ! -f "$ELF" ]; then
    fail "ELF file not found at $ELF"
fi

command -v readelf >/dev/null 2>&1 || fail "readelf command not found"

# 1. Verify no PT_INTERP program header (program interpreter requested)
if readelf -l "$ELF" | grep -q -E '^[[:space:]]*INTERP[[:space:]]|Requesting program interpreter'; then
    fail "$ELF is dynamically linked (found PT_INTERP program interpreter)"
fi

# 2. Verify no NEEDED dynamic section entries (dynamic library dependencies)
if readelf -d "$ELF" 2>/dev/null | grep -q -E '\(NEEDED\)'; then
    fail "$ELF has dynamic dependencies (found NEEDED entries in dynamic section)"
fi

echo -e "\e[32m[OK]\e[0m $ELF is a static ELF (no PT_INTERP, no NEEDED)."
