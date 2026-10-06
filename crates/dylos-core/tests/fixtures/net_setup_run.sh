#!/bin/sh
# Usage: net_setup_run.sh <net-setup script> <cmdline file> [<iface> <mac>]...
# Creates dummy interfaces, runs net-setup, then dumps the resulting state.
set -e
net_setup=$1
cmdline=$2
shift 2

mount -t sysfs none /sys
while [ $# -ge 2 ]; do
    ip link add "$1" type dummy
    ip link set "$1" address "$2"
    shift 2
done

"$(realpath "$net_setup")" "$cmdline"

echo "===ADDR==="
ip -j addr show
echo "===ROUTE4==="
ip -j -4 route show
echo "===ROUTE6==="
ip -j -6 route show
echo "===SYSCTL==="
sysctl -n net.ipv6.conf.all.accept_dad
sysctl -n net.ipv6.conf.eth0.accept_dad
sysctl -n net.ipv6.conf.all.accept_ra
sysctl -n net.ipv6.conf.eth0.accept_ra
sysctl -n net.ipv6.conf.all.autoconf
sysctl -n net.ipv6.conf.eth0.autoconf
sysctl -n net.ipv6.conf.all.forwarding
sysctl -n net.ipv4.ip_forward
