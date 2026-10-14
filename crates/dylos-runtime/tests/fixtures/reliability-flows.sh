iperf3 -6 -c fd64:796c:6f73:2::2 -p 5201 -t 0 -b 1M > /tmp/flow6.log 2>&1 < /dev/null &
iperf3 -4 -c 10.0.2.2 -p 5202 -t 0 -b 1M > /tmp/flow4.log 2>&1 < /dev/null &
true
