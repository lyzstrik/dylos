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
if [ "${CANCEL_PROBE:-}" = 1 ]; then
    kill -TERM "$PPID"
    exit 1
fi
case "$*" in *incompatible*) exit 1 ;; esac
exec /bin/cp "${@:2}"
BIN
chmod +x "$TEST_ROOT/bin/"*
export PATH="$TEST_ROOT/bin:$PATH"
export DYLOS_FC_BIN_DIR="$TEST_ROOT/bin"
unset DYLOS_LAB_DIR DYLOS_IMAGE_DIR DYLOS_IMAGE DYLOS_JAIL_DIR
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
