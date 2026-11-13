set -eu
mkdir -p /mnt/bench-memory
if [ SNAPSHOT_REPETITION = 1 ]; then
    mount -t tmpfs -o size=WORKING_SET_MIBm tmpfs /mnt/bench-memory
fi
python3 - <<'PY'
import mmap
from pathlib import Path

size = WORKING_SET_MIB * 1024 * 1024
first = SNAPSHOT_REPETITION == 1
path = Path('/mnt/bench-memory/resident')
with path.open('w+b' if first else 'r+b') as f:
    if first:
        f.truncate(size)
    with mmap.mmap(f.fileno(), size) as region:
        # tmpfs pages remain resident: the guest has no swap configured.
        end = size if first else ((size // 4 + 4095) // 4096) * 4096
        chunk = bytes([1 + SNAPSHOT_REPETITION % 254]) * min(end, 1024 * 1024)
        for offset in range(0, end, len(chunk)):
            region[offset:min(offset + len(chunk), end)] = chunk[:min(len(chunk), end - offset)]
PY
