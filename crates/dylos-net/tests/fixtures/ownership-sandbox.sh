set -eu
for mapping in /proc/self/uid_map /proc/self/gid_map; do
    if ! awk '$1 <= 1000 && 1000 < $1 + $3 { mapped = 1 } END { exit !mapped }' "$mapping"; then
        echo "uid/gid 1000 is not mapped in $mapping" >&2
        exit 1
    fi
done
exec "$@"
