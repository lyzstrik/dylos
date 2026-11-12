#!/usr/bin/env bash
set -euo pipefail

fail() {
    echo -e "\e[31m[ERROR]\e[0m $1" >&2
    exit 1
}

# Reference paths
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOC_HOST="${REPO_ROOT}/docs/host.md"

# Check the running kernel without loading modules or changing qdiscs.
echo "==> Checking netem qdisc support..."
KERNEL_RELEASE="$(uname -r)"
SYS_MODULE_DIR="${DYLOS_SYS_MODULE_DIR:-/sys/module}"
KERNEL_MODULE_DIR="${DYLOS_KERNEL_MODULE_DIR:-/lib/modules/$KERNEL_RELEASE}"
if [ -d "$SYS_MODULE_DIR/sch_netem" ]; then
    echo "  - sch_netem : loaded"
elif [ -r "$KERNEL_MODULE_DIR/modules.builtin" ] &&
    grep -Eq '(^|/)sch_netem\.ko$' "$KERNEL_MODULE_DIR/modules.builtin"; then
    echo "  - sch_netem : built in"
elif [ -d "$KERNEL_MODULE_DIR" ] &&
    NETEM_MODULE="$(find "$KERNEL_MODULE_DIR/" -type f -name 'sch_netem.ko*' -print -quit)" &&
    [ -n "$NETEM_MODULE" ]; then
    echo "  - sch_netem : available ($NETEM_MODULE)"
else
    fail "netem qdisc unavailable for running kernel $KERNEL_RELEASE: sch_netem is neither loaded, built in, nor available in $KERNEL_MODULE_DIR. A reboot after a kernel upgrade may be needed (see docs/host.md)."
fi

# 1. Check /dev/kvm
echo "==> Checking KVM..."
[ -e /dev/kvm ] || fail "/dev/kvm does not exist."
[ -r /dev/kvm ] && [ -w /dev/kvm ] || fail "Current user does not have read/write permissions on /dev/kvm."

# 2. Check Firecracker, Jailer and their checksums
echo "==> Checking Firecracker and Jailer..."
FC_BIN_DIR="${DYLOS_FC_BIN_DIR:-$HOME/.local/share/dylos/bin}"
FC_BIN="$FC_BIN_DIR/firecracker"
JAILER_BIN="$FC_BIN_DIR/jailer"
[ -x "$FC_BIN" ] || fail "Firecracker not found at $FC_BIN (see docs/host.md, or set DYLOS_FC_BIN_DIR)."
[ -x "$JAILER_BIN" ] || fail "Jailer not found at $JAILER_BIN (see docs/host.md, or set DYLOS_FC_BIN_DIR)."

# The jailer runs Firecracker inside an empty chroot: a dynamically linked build cannot start there.
if readelf -l "$FC_BIN" | grep -q "Requesting program interpreter"; then
    fail "$FC_BIN is dynamically linked; use the static upstream release binary (see docs/host.md)."
fi

echo "  - Firecracker : $("$FC_BIN" --version | head -n1) (static)"
echo "  - Jailer      : $("$JAILER_BIN" --version | head -n1)"

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

# 3. Check reflink support across lab store, image location, and jail directory
echo "==> Checking reflink support..."

LAB_DIR="${DYLOS_LAB_DIR:-${REPO_ROOT}/target/labs}"
if [ -n "${DYLOS_JAIL_DIR:-}" ]; then
    JAIL_DIR="$DYLOS_JAIL_DIR"
else
    JAIL_DIR="$(dirname "$LAB_DIR")/jails"
fi

if [ -n "${DYLOS_IMAGE_DIR:-}" ]; then
    IMAGE_DIR="$DYLOS_IMAGE_DIR"
elif [ -n "${DYLOS_IMAGE:-}" ]; then
    if [ -d "$DYLOS_IMAGE" ]; then
        IMAGE_DIR="$DYLOS_IMAGE"
    else
        IMAGE_DIR="$(dirname "$DYLOS_IMAGE")"
    fi
else
    IMAGE_DIR="${REPO_ROOT}/target/images"
fi

echo "  - Lab store      : $LAB_DIR"
echo "  - Image location : $IMAGE_DIR"
echo "  - Jail directory : $JAIL_DIR"

# Resolve symlinks and missing path components before creating any directory.
TRACKED_LABS="$(realpath -m "$REPO_ROOT/labs")"
for location in "$LAB_DIR" "$IMAGE_DIR" "$JAIL_DIR"; do
    resolved="$(realpath -m "$location")" || fail "Cannot resolve probe location $location"
    case "$resolved/" in
        "$TRACKED_LABS/"*) fail "Probe location must not resolve inside tracked labs/: $location" ;;
    esac
done

mkdir -p "$LAB_DIR" || fail "Cannot create or access lab store directory at $LAB_DIR"
mkdir -p "$IMAGE_DIR" || fail "Cannot create or access image directory at $IMAGE_DIR"
mkdir -p "$JAIL_DIR" || fail "Cannot create or access jail directory at $JAIL_DIR"

PROBE_FILES=()
cleanup_probes() {
    for f in "${PROBE_FILES[@]}"; do
        rm -f "$f"
    done
}
trap cleanup_probes EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

probe_reflink_direction() {
    local src_dir="$1"
    local dst_dir="$2"
    local src_file
    local dst_file

    if ! src_file="$(mktemp "$src_dir/.reflink_probe_src.XXXXXX" 2>/dev/null)"; then
        fail "Cannot write a probe file in $src_dir (owner: $(stat -c '%U (%u)' -- "$src_dir")). Run as the directory owner or fix ownership/write permissions; also check available space."
    fi
    PROBE_FILES+=("$src_file")
    if ! dst_file="$(mktemp "$dst_dir/.reflink_probe_dst.XXXXXX" 2>/dev/null)"; then
        rm -f "$src_file"
        fail "Cannot write a probe file in $dst_dir (owner: $(stat -c '%U (%u)' -- "$dst_dir")). Run as the directory owner or fix ownership/write permissions; also check available space."
    fi
    PROBE_FILES+=("$dst_file")

    if ! echo "test-reflink" > "$src_file" 2>/dev/null; then
        rm -f "$src_file" "$dst_file"
        return 1
    fi
    if ! cp --reflink=always "$src_file" "$dst_file" 2>/dev/null; then
        rm -f "$src_file" "$dst_file"
        return 1
    fi

    rm -f "$src_file" "$dst_file"
    return 0
}

probe_reflink_pair() {
    local dir_a="$1"
    local dir_b="$2"

    if ! probe_reflink_direction "$dir_a" "$dir_b"; then
        return 1
    fi
    if [ "$dir_a" != "$dir_b" ]; then
        if ! probe_reflink_direction "$dir_b" "$dir_a"; then
            return 1
        fi
    fi
    return 0
}

FAILED_PAIRS=()

if probe_reflink_pair "$LAB_DIR" "$IMAGE_DIR"; then
    echo "  - Reflink probe  : OK (lab store <-> image location)"
else
    echo "  - Reflink probe  : FAIL (lab store <-> image location)"
    FAILED_PAIRS+=("lab store ($LAB_DIR) and image location ($IMAGE_DIR)")
fi

if probe_reflink_pair "$LAB_DIR" "$JAIL_DIR"; then
    echo "  - Reflink probe  : OK (lab store <-> jail directory)"
else
    echo "  - Reflink probe  : FAIL (lab store <-> jail directory)"
    FAILED_PAIRS+=("lab store ($LAB_DIR) and jail directory ($JAIL_DIR)")
fi

if probe_reflink_pair "$IMAGE_DIR" "$JAIL_DIR"; then
    echo "  - Reflink probe  : OK (image location <-> jail directory)"
else
    echo "  - Reflink probe  : FAIL (image location <-> jail directory)"
    FAILED_PAIRS+=("image location ($IMAGE_DIR) and jail directory ($JAIL_DIR)")
fi

if [ ${#FAILED_PAIRS[@]} -gt 0 ]; then
    ERR_MSG="Reflink probe failed for the following pair(s):\n"
    for pair in "${FAILED_PAIRS[@]}"; do
        ERR_MSG="${ERR_MSG}  - ${pair}\n"
    done
    ERR_MSG="${ERR_MSG}Image, lab store and jail directory must share a reflink-compatible mount (see docs/host.md)."
    fail "$ERR_MSG"
fi

# Probe the actual kernel source: a writable image directory does not establish
# that this file can be cloned (it may reside on another mount).
KERNEL_FILE="${DYLOS_KERNEL:-${REPO_ROOT}/kernels/vmlinux.bin}"
[ -f "$KERNEL_FILE" ] && [ -r "$KERNEL_FILE" ] ||
    fail "Guest kernel is not a readable regular file: $KERNEL_FILE (see docs/host.md, or set DYLOS_KERNEL)."
for destination in "$JAIL_DIR" "$LAB_DIR"; do
    if ! kernel_probe="$(mktemp "$destination/.reflink_probe_kernel.XXXXXX" 2>/dev/null)"; then
        fail "Cannot write a kernel probe file in $destination; check ownership/write permissions and available space."
    fi
    PROBE_FILES+=("$kernel_probe")
    if ! cp --reflink=always -- "$KERNEL_FILE" "$kernel_probe" 2>/dev/null; then
        fail "Guest kernel reflink required: cannot clone $KERNEL_FILE into $destination. Kernel, jail and store must share a reflink-compatible mount (see docs/host.md)."
    fi
    rm -f "$kernel_probe"
done
echo "  - Guest kernel reflink : OK ($KERNEL_FILE -> jail and store)"

echo -e "\e[32m[SUCCESS]\e[0m All host prerequisites are met."
