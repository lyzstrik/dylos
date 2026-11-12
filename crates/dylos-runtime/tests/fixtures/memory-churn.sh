(
set -eu
python3 - <<'PY' >/dev/null 2>&1 &
from pathlib import Path

region = Path('/mnt/memory-proof/churn')
patterns = [bytes([0x69]) * (1024 * 1024), bytes(1024 * 1024)]
while True:
    for pattern in patterns:
        region.write_bytes(pattern)
PY
python3 - <<'PY'
from pathlib import Path
import time

region = Path('/mnt/memory-proof/churn')
deadline = time.monotonic() + 10
first = None
while time.monotonic() < deadline:
    if region.exists() and region.stat().st_size:
        stamp = region.stat().st_mtime_ns
        if first is not None and stamp != first:
            break
        first = stamp
else:
    raise RuntimeError('memory writer did not advance')
PY
)
