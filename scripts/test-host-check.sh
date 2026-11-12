#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT
mkdir -p "$TEST_ROOT/repo/scripts" "$TEST_ROOT/repo/labs" "$TEST_ROOT/bin"
# Only the test copy substitutes the KVM device; production has no test hooks.
sed "s|/dev/kvm|$TEST_ROOT/kvm|g" "$REPO_ROOT/scripts/host-check.sh" > "$TEST_ROOT/repo/scripts/host-check.sh"
touch "$TEST_ROOT/kvm"
cat > "$TEST_ROOT/bin/firecracker" <<'BIN'
#!/usr/bin/env bash
echo 'Firecracker test fixture'
BIN
cp "$TEST_ROOT/bin/firecracker" "$TEST_ROOT/bin/jailer"
cat > "$TEST_ROOT/bin/readelf" <<'BIN'
#!/usr/bin/env bash
exit 0
BIN
cat > "$TEST_ROOT/bin/cp" <<'BIN'
#!/usr/bin/env bash
set -euo pipefail
[ "$1" = --reflink=always ] || exit 1
if [ "${CANCEL_PROBE:-}" = 1 ]; then
    kill -TERM "$PPID"
    exit 1
fi
case "$*" in *incompatible*) exit 1 ;; esac
if [[ "$*" == *'.reflink_probe_kernel.'* ]] &&
    [[ "${*: -1}" == *"/${KERNEL_FAIL_DEST:-no-match}/"* ]]; then
    exit 1
fi
exec /bin/cp "${@:2}"
BIN
cat > "$TEST_ROOT/bin/mktemp" <<'BIN'
#!/usr/bin/env bash
case "$*" in *unwritable*) exit 1 ;; esac
exec /usr/bin/mktemp "$@"
BIN
chmod +x "$TEST_ROOT/bin/"*
export PATH="$TEST_ROOT/bin:$PATH"
export DYLOS_FC_BIN_DIR="$TEST_ROOT/bin"
export DYLOS_SYS_MODULE_DIR="$TEST_ROOT/sys/module"
export DYLOS_KERNEL_MODULE_DIR="$TEST_ROOT/modules/running"
mkdir -p "$DYLOS_SYS_MODULE_DIR/sch_netem" "$DYLOS_KERNEL_MODULE_DIR"
unset DYLOS_LAB_DIR DYLOS_IMAGE_DIR DYLOS_IMAGE DYLOS_JAIL_DIR DYLOS_KERNEL
mkdir -p "$TEST_ROOT/repo/kernels"
printf 'guest kernel fixture\n' > "$TEST_ROOT/repo/kernels/vmlinux.bin"
chmod 444 "$TEST_ROOT/repo/kernels/vmlinux.bin"
kernel_before="$(stat -c '%a:%u:%g:%s:%Y' "$TEST_ROOT/repo/kernels/vmlinux.bin")"
CHECK_SCRIPT="$TEST_ROOT/repo/scripts/host-check.sh"

assert_clean() {
    local residual
    residual="$(find "$TEST_ROOT" -name '.reflink_probe_*' -print)"
    if [ -n "$residual" ]; then
        echo "FAIL: residual probe files: $residual" >&2
        exit 1
    fi
}

assert_result() {
    local expected="$1" description="$2" pattern="$3" output status=0
    shift 3
    output="$("$@" 2>&1)" || status=$?
    if { [ "$expected" = pass ] && [ "$status" -ne 0 ]; } ||
       { [ "$expected" = fail ] && [ "$status" -eq 0 ]; } ||
       { [[ "$expected" =~ ^[0-9]+$ ]] && [ "$status" -ne "$expected" ]; } ||
       ! grep -Fq "$pattern" <<< "$output"; then
        echo "FAIL: $description (status $status): $output" >&2
        exit 1
    fi
    assert_clean
    echo "PASS: $description"
}

assert_result pass 'default probe succeeds' 'All host prerequisites are met' bash "$CHECK_SCRIPT"
assert_result pass 'read-only kernel reflinks into jail and store' 'Guest kernel reflink : OK' bash "$CHECK_SCRIPT"
mkdir -p "$TEST_ROOT/kernel override" "$TEST_ROOT/incompatible/kernel"
/bin/cp "$TEST_ROOT/repo/kernels/vmlinux.bin" "$TEST_ROOT/kernel override/vmlinux.bin"
/bin/cp "$TEST_ROOT/repo/kernels/vmlinux.bin" "$TEST_ROOT/incompatible/kernel/vmlinux.bin"
assert_result pass 'kernel path override accepts spaces' 'Guest kernel reflink : OK' \
    env DYLOS_KERNEL="$TEST_ROOT/kernel override/vmlinux.bin" bash "$CHECK_SCRIPT"
assert_result fail 'kernel on incompatible mount fails closed' 'Guest kernel reflink required: cannot clone' \
    env DYLOS_KERNEL="$TEST_ROOT/incompatible/kernel/vmlinux.bin" bash "$CHECK_SCRIPT"
for destination in jails labs; do
    assert_result fail "kernel reflink unsupported in $destination fails closed" 'Guest kernel reflink required: cannot clone' \
        env KERNEL_FAIL_DEST="$destination" bash "$CHECK_SCRIPT"
done
for location in "$TEST_ROOT/missing-kernel" "$TEST_ROOT/repo/kernels"; do
    assert_result fail 'missing or non-regular kernel rejected' 'Guest kernel is not a readable regular file' \
        env DYLOS_KERNEL="$location" bash "$CHECK_SCRIPT"
done
cmp "$TEST_ROOT/repo/kernels/vmlinux.bin" "$TEST_ROOT/kernel override/vmlinux.bin"
cmp "$TEST_ROOT/repo/kernels/vmlinux.bin" "$TEST_ROOT/incompatible/kernel/vmlinux.bin"
test "$kernel_before" = "$(stat -c '%a:%u:%g:%s:%Y' "$TEST_ROOT/repo/kernels/vmlinux.bin")"
assert_result pass 'loaded netem succeeds without a module tree' 'sch_netem : loaded' \
    env DYLOS_KERNEL_MODULE_DIR="$TEST_ROOT/missing" bash "$CHECK_SCRIPT"
rmdir "$DYLOS_SYS_MODULE_DIR/sch_netem"
assert_result fail 'missing running kernel modules reports reboot hint' 'A reboot after a kernel upgrade may be needed' \
    env DYLOS_KERNEL_MODULE_DIR="$TEST_ROOT/missing" bash "$CHECK_SCRIPT"
assert_result fail 'empty module tree rejects netem' 'netem qdisc unavailable for running kernel' bash "$CHECK_SCRIPT"
mkdir -p "$TEST_ROOT/modules/other/kernel/net/sched"
touch "$TEST_ROOT/modules/other/kernel/net/sched/sch_netem.ko"
assert_result fail 'another kernel module does not satisfy running kernel' 'netem qdisc unavailable' bash "$CHECK_SCRIPT"
printf '%s\n' 'kernel/net/sched/not_sch_netem.ko' > "$DYLOS_KERNEL_MODULE_DIR/modules.builtin"
assert_result fail 'unrelated builtin does not satisfy netem' 'netem qdisc unavailable' bash "$CHECK_SCRIPT"
printf '%s\n' 'kernel/net/sched/sch_netem.ko' >> "$DYLOS_KERNEL_MODULE_DIR/modules.builtin"
assert_result pass 'builtin netem succeeds' 'sch_netem : built in' bash "$CHECK_SCRIPT"
rm "$DYLOS_KERNEL_MODULE_DIR/modules.builtin"
mkdir -p "$DYLOS_KERNEL_MODULE_DIR/kernel/net/sched"
for suffix in ko ko.xz ko.zst ko.gz; do
    module="$DYLOS_KERNEL_MODULE_DIR/kernel/net/sched/sch_netem.$suffix"
    touch "$module"
    assert_result pass "loadable netem .$suffix succeeds" 'sch_netem : available' bash "$CHECK_SCRIPT"
    rm "$module"
done
mkdir -p "$DYLOS_SYS_MODULE_DIR/sch_netem"
for variable in DYLOS_LAB_DIR DYLOS_JAIL_DIR DYLOS_IMAGE_DIR DYLOS_IMAGE; do
    assert_result fail "$variable incompatible pair reported" 'Reflink probe failed for the following pair(s)' \
        env "$variable=$TEST_ROOT/incompatible/location" bash "$CHECK_SCRIPT"
done
# The derived sibling jail stays inside this exclusively owned parent.
assert_result fail 'store failure preserves sibling contents' 'FAIL (lab store <-> image location)' \
    env DYLOS_LAB_DIR="$TEST_ROOT/incompatible/store" bash "$CHECK_SCRIPT"
touch "$TEST_ROOT/incompatible/jails/keep"
assert_result fail 'existing jail contents preserved' 'Reflink probe failed' \
    env DYLOS_LAB_DIR="$TEST_ROOT/incompatible/store" bash "$CHECK_SCRIPT"
test -f "$TEST_ROOT/incompatible/jails/keep"

# Inject a creation denial so this also exercises the failure when run as root.
for variable in DYLOS_LAB_DIR DYLOS_IMAGE_DIR DYLOS_JAIL_DIR; do
    output="$(env "$variable=$TEST_ROOT/unwritable" bash "$CHECK_SCRIPT" 2>&1)" && {
        echo "FAIL: unwritable $variable unexpectedly passed" >&2
        exit 1
    }
    for pattern in "Cannot write a probe file in $TEST_ROOT/unwritable" \
        "owner: $(stat -c '%U (%u)' -- "$TEST_ROOT/unwritable")" \
        'Run as the directory owner or fix ownership/write permissions'; do
        grep -Fq "$pattern" <<< "$output"
    done
    if grep -Eq 'Reflink probe.*(FAIL|failed)|reflink-compatible mount' <<< "$output"; then
        echo "FAIL: unwritable $variable diagnosed as reflink incompatibility" >&2
        exit 1
    fi
    assert_clean
    echo "PASS: unwritable $variable reports ownership instead of reflink failure"
done

ln -s "$TEST_ROOT/repo/labs" "$TEST_ROOT/labs-alias"
for variable in DYLOS_LAB_DIR DYLOS_JAIL_DIR DYLOS_IMAGE_DIR DYLOS_IMAGE; do
    for location in "$TEST_ROOT/repo/labs/new" "$TEST_ROOT/labs-alias/new"; do
        assert_result fail "$variable rejects tracked labs and symlink aliases" 'must not resolve inside tracked labs/' \
            env "$variable=$location" bash "$CHECK_SCRIPT"
    done
done
test -z "$(find "$TEST_ROOT/repo/labs" -mindepth 1 -print -quit)"
# Default sibling selection must also be validated before mkdir.
assert_result fail 'derived jail inside labs rejected' 'must not resolve inside tracked labs/' \
    env DYLOS_LAB_DIR="$TEST_ROOT/repo/labs/store" bash "$CHECK_SCRIPT"
assert_result 143 'cancellation terminates and cleans probes' '' \
    env CANCEL_PROBE=1 bash "$CHECK_SCRIPT"
echo 'PASS: all host-check regression tests'
