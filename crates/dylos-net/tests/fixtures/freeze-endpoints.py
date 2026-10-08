import errno
import fcntl
import ipaddress
import json
import os
import select
import struct
import subprocess
import sys
import time


def frame(version, marker):
    payload = marker.encode()
    udp = struct.pack('!HHHH', 10000, 10001, 8 + len(payload), 0) + payload
    if version == '6':
        source = ipaddress.IPv6Address('fd00::1').packed
        target = ipaddress.IPv6Address('fd00::2').packed
        pseudo = source + target + struct.pack('!I3xB', len(udp), 17)
        padded = pseudo + udp + (b'\0' if len(udp) % 2 else b'')
        checksum = sum(struct.unpack('!' + 'H' * (len(padded) // 2), padded))
        while checksum >> 16:
            checksum = (checksum & 65535) + (checksum >> 16)
        udp = udp[:6] + struct.pack('!H', (~checksum & 65535) or 65535) + udp[8:]
        packet = struct.pack('!IHBB', 6 << 28, len(udp), 17, 64) + source + target + udp
        protocol = 0x86DD
    else:
        packet = struct.pack('!BBHHHBBH4s4s', 0x45, 0, 20 + len(udp), 0, 0, 64,
                             17, 0, ipaddress.IPv4Address('192.0.2.1').packed,
                             ipaddress.IPv4Address('192.0.2.2').packed)
        checksum = sum(struct.unpack('!10H', packet))
        while checksum >> 16:
            checksum = (checksum & 65535) + (checksum >> 16)
        packet = packet[:10] + struct.pack('!H', ~checksum & 65535) + packet[12:] + udp
        protocol = 0x0800
    return bytes(10) + bytes.fromhex('ffffffffffff020000000001') + struct.pack('!H', protocol) + packet


taps = {}
try:
    # These endpoints use raw Ethernet, including IPv6 packets. Disable only the namespace's
    # host IPv6 stacks to prevent automatic MLD/DAD frames contaminating the reader queues.
    # This does not disable Ethernet bridge forwarding of IPv6 guest frames.
    for name in list(sys.argv[1:]) + ['br-left', 'br-right']:
        with open(f'/proc/sys/net/ipv6/conf/{name}/disable_ipv6', 'w') as setting:
            setting.write('1')
    for name in sys.argv[1:]:
        fd = os.open('/dev/net/tun', os.O_RDWR | os.O_NONBLOCK)
        try:
            fcntl.ioctl(fd, 0x400454CA, struct.pack('16sH22x', name.encode(), 0x5002))
        except BaseException:
            os.close(fd)
            raise
        taps[name] = fd
    print('ready', flush=True)
    for line in sys.stdin:
        command, *args = line.split()
        if command == 'send':
            source, target, version, marker = args
            os.write(taps[source], frame(version, marker))
            assert select.select([taps[target]], [], [], 2)[0], 'frame never reached TAP queue'
        elif command == 'read':
            target, version, marker = args
            expected = frame(version, marker)
            deadline = time.monotonic() + 2
            while True:
                remaining = deadline - time.monotonic()
                assert remaining > 0, 'expected frame never delivered'
                assert select.select([taps[target]], [], [], remaining)[0], 'read timeout'
                if os.read(taps[target], 65536) == expected:
                    break
        elif command == 'flood':
            source, version = args
            for index in range(256):
                try:
                    os.write(taps[source], frame(version, f'frozen-{index}'))
                except OSError as error:
                    assert error.errno == errno.EIO, error
                else:
                    raise AssertionError('frozen TAP accepted a frame')
        elif command == 'empty':
            readable = select.select(list(taps.values()), [], [], 0.05)[0]
            assert not readable, f'unexpected queued frames: {[(name, os.read(fd, 65536).hex()) for name, fd in taps.items() if fd in readable]}'
        elif command == 'stats':
            links = json.loads(subprocess.check_output(['ip', '-j', '-s', 'link', 'show']))
            counters = {link['ifname']: [link['stats64']['rx']['packets'],
                                        link['stats64']['tx']['packets']]
                        for link in links if link['ifname'] in taps}
            print(json.dumps(counters, sort_keys=True), flush=True)
            continue
        else:
            raise AssertionError(command)
        print('ok', flush=True)
finally:
    for fd in taps.values():
        os.close(fd)
