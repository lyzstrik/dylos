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

# 1. Verify file is a valid ELF
if ! elf_header="$(readelf -h "$ELF" 2>&1)"; then
    fail "$ELF is not a valid ELF file: $elf_header"
fi

# 2. Verify program headers can be read and no PT_INTERP program header is requested
if ! program_headers="$(readelf -l "$ELF" 2>&1)"; then
    fail "Failed to read program headers from $ELF: $program_headers"
fi

if echo "$program_headers" | grep -E '^[[:space:]]*INTERP[[:space:]]|Requesting program interpreter' >/dev/null; then
    fail "$ELF is dynamically linked (found PT_INTERP program interpreter)"
fi

# 3. Verify dynamic section can be read and no NEEDED dynamic section entries exist
if ! dynamic_section="$(readelf -d "$ELF" 2>&1)"; then
    fail "Failed to read dynamic section from $ELF: $dynamic_section"
fi

if echo "$dynamic_section" | grep -E '\(NEEDED\)' >/dev/null; then
    fail "$ELF has dynamic dependencies (found NEEDED entries in dynamic section)"
fi

echo -e "\e[32m[OK]\e[0m $ELF is a static ELF (no PT_INTERP, no NEEDED)."
