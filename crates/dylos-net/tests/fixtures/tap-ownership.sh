set -eu
mount --make-rprivate /
mount -t sysfs sysfs /sys
cat "/sys/class/net/$1/owner" "/sys/class/net/$1/group"
