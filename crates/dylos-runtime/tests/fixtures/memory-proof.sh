(
set -eu
mkdir -p /mnt/memory-proof
mount -t tmpfs tmpfs /mnt/memory-proof
python3 - <<'PY'
import hashlib
from pathlib import Path

region = Path('/mnt/memory-proof/region')
pattern = bytes([0xa5]) * (1024 * 1024)
region.write_bytes(pattern)
expected = hashlib.sha256(pattern).digest()
# Negative control: omitting any nonzero page must change the checksum.
with region.open('r+b') as memory:
    memory.seek(4096)
    memory.write(bytes(4096))
assert hashlib.sha256(region.read_bytes()).digest() != expected
region.write_bytes(pattern)
assert hashlib.sha256(region.read_bytes()).digest() == expected
# Kernel ELF notes are read-only data loaded at boot, not rewritten by this test.
notes = Path('/sys/kernel/notes').read_bytes()
assert notes and any(notes)
PY
sha256sum /mnt/memory-proof/region /sys/kernel/notes
)
