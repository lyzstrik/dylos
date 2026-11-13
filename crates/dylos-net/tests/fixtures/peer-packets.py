import errno
import fcntl
import hashlib
import ipaddress
import json
import os
import select
import struct
import subprocess
import sys
import time


def mac(node):
    seed = b''.join(len(s).to_bytes(8, 'little') + s for s in [node.encode(), b'eth0'])
    return b'\x02' + hashlib.sha256(seed).digest()[:5]


def packed(address):
    return ipaddress.ip_address(address).packed


def checksum(data):
    data += bytes(len(data) % 2)
    total = sum(struct.unpack('!' + 'H' * (len(data) // 2), data))
    while total >> 16:
        total = (total & 65535) + (total >> 16)
    return (~total & 65535) or 65535


macs = {node: mac(node) for node in ['a', 'b']}
locals6 = {node: str(ipaddress.IPv6Address(b'\xfe\x80' + bytes(6) +
           bytes([address[0] ^ 2]) + address[1:3] + b'\xff\xfe' + address[3:]))
           for node, address in macs.items()}


def ether(payload, protocol=0x86dd, source='a', hardware=None):
    target = 'b' if source == 'a' else 'a'
    return macs[target] + (hardware or macs[source]) + struct.pack('!H', protocol) + payload


def ipv6(payload, protocol=58, address='fd00::1', target='fd00::2', hop=255):
    source_ip, target_ip = packed(address), packed(target)
    if protocol in [17, 58]:
        offset = 6 if protocol == 17 else 2
        pseudo = source_ip + target_ip + struct.pack('!I3xB', len(payload), protocol)
        value = checksum(pseudo + payload)
        payload = payload[:offset] + struct.pack('!H', value) + payload[offset + 2:]
    return struct.pack('!IHBB', 6 << 28, len(payload), protocol, hop) + source_ip + target_ip + payload


def ipv4(payload, protocol=1, address='192.0.2.1', fragment=0, ihl=5):
    if protocol == 1:
        payload = payload[:2] + struct.pack('!H', checksum(payload)) + payload[4:]
    header = struct.pack('!BBHHHBBH4s4s', 0x40 | ihl, 0, 20 + len(payload), 0,
                         fragment, 64, protocol, 0, packed(address), packed('192.0.2.2'))
    header = header[:10] + struct.pack('!H', checksum(header)) + header[12:]
    return header + payload


def udp(source=1234, target=4321, payload=b'data'):
    return struct.pack('!HHHH', source, target, 8 + len(payload), 0) + payload


def neighbor(kind=135, target='fd00::2', option=None, code=0):
    return struct.pack('!BBHI', kind, code, 0, 0) + packed(target) + (option or b'')


def arp(hardware=None, address='192.0.2.1', operation=1):
    return struct.pack('!HHBBH', 1, 0x800, 6, 4, operation) + (hardware or macs['a']) + packed(address) + macs['b'] + packed('192.0.2.2')


fds = {}
try:
    for interface in ['br-lan', 'tap-a-eth0', 'tap-b-eth0']:
        with open(f'/proc/sys/net/ipv6/conf/{interface}/disable_ipv6', 'w') as setting:
            setting.write('1')
    for node in ['a', 'b']:
        fd = os.open('/dev/net/tun', os.O_RDWR | os.O_NONBLOCK)
        fds[node] = fd
        fcntl.ioctl(fd, 0x400454ca, struct.pack('16sH22x', f'tap-{node}-eth0'.encode(), 0x5002))
    if sys.argv[1] == 'frozen':
        for node, fd in fds.items():
            try:
                os.write(fd, bytes(10) + ether(ipv6(udp(), 17)))
            except OSError as error:
                assert error.errno == errno.EIO, error
            else:
                raise AssertionError(f'frozen {node} accepted traffic')
        sys.exit(0)
    deadline = time.monotonic() + 5
    while True:
        links = json.loads(subprocess.check_output(['ip', '-j', '-d', 'link', 'show']))
        states = [link.get('linkinfo', {}).get('info_slave_data', {}).get('state')
                  for link in links if link['ifname'] in ['tap-a-eth0', 'tap-b-eth0']]
        if states == ['forwarding', 'forwarding']:
            break
        assert time.monotonic() < deadline, f'TAP ports never became forwarding: {states}'
    for fd in fds.values():
        while select.select([fd], [], [], 0)[0]:
            os.read(fd, 65536)
    sequence = 0

    def expect(packet, allowed=True, source='a'):
        global sequence
        sequence += 1
        target = 'b' if source == 'a' else 'a'
        address, destination = ('fd00::1', 'fd00::2') if source == 'a' else ('fd00::2', 'fd00::1')
        if sys.argv[1] == 'unspecified':
            address = locals6[source]
        barrier = ether(ipv6(udp(payload=f'barrier-{sequence}'.encode()), 17,
                              address, destination), source=source)
        os.write(fds[source], bytes(10) + packet)
        os.write(fds[source], bytes(10) + barrier)
        seen = False
        deadline = time.monotonic() + 5
        while True:
            remaining = deadline - time.monotonic()
            assert remaining > 0 and select.select([fds[target]], [], [], remaining)[0], 'forwarding barrier timeout'
            received = os.read(fds[target], 65536)[10:]
            if received == barrier:
                break
            seen |= received == packet
        assert seen == allowed, f'case {sequence}: expected allowed={allowed}, frame={packet.hex()}'

    if sys.argv[1] == 'unspecified':
        expect(ether(ipv6(neighbor(), address='::')), False)
        sys.exit(0)
    if sys.argv[1] == 'unfiltered':
        expect(ether(ipv6(udp(), 17, address='fd00::99'), hardware=b'\x02' + b'\x99' * 5))
        expect(ether(ipv4(udp(67, 68), 17, address='192.0.2.99'), 0x800))
        sys.exit(0)
    # Cold neighbor exchange followed by echo traffic, IPv6 first, then IPv4 ARP.
    expect(ether(ipv6(neighbor(option=b'\x01\x01' + macs['a']))))
    expect(ether(ipv6(neighbor(136, 'fd00::2', b'\x02\x01' + macs['b']),
                      address='fd00::2', target='fd00::1'), source='b'), source='b')
    expect(ether(ipv6(b'\x80' + bytes(7))))
    expect(ether(ipv6(b'\x81' + bytes(7), address='fd00::2', target='fd00::1'), source='b'), source='b')
    expect(ether(ipv6(neighbor(136, 'fd00::1'))))
    expect(ether(ipv6(neighbor(), address=locals6['a'])))
    expect(ether(arp(), 0x806))
    expect(ether(arp(operation=2), 0x806))
    expect(ether(ipv4(b'\x08' + bytes(7)), 0x800))
    expect(ether(ipv6(udp(), 17)))
    expect(ether(ipv4(udp(), 17), 0x800))
    bad = [ether(ipv6(udp(), 17), hardware=b'\x02' + b'\x99' * 5),
           ether(ipv6(udp(), 17, address='fd00::99')),
           ether(ipv6(udp(), 17, address='fe80::99')),
           ether(ipv4(udp(), 17, address='192.0.2.99'), 0x800),
           ether(arp(address='192.0.2.99'), 0x806), ether(arp(hardware=macs['b']), 0x806),
           ether(arp(operation=3), 0x806), ether(ipv6(neighbor(), address='::')),
           ether(ipv6(neighbor(), hop=64)), ether(ipv6(neighbor(code=1))),
           ether(ipv6(neighbor(136, 'fd00::2'))), ether(ipv6(neighbor(136, 'fe80::99')))]
    for option in [b'\x01\x01' + macs['b'], b'\x02\x01' + macs['a'],
                   b'\x01\x00' + macs['a'], b'\x01\x01' + macs['a'] * 2, b'\x01']:
        bad.append(ether(ipv6(neighbor(option=option))))
    for kind in [134, 137]:
        bad.append(ether(ipv6(bytes([kind]) + bytes(31))))
    for protocol in [0, 43, 44, 50, 51, 60]:
        bad.append(ether(ipv6(b'\x3a' + bytes(7) + bytes([134]) + bytes(31), protocol)))
    for source_port, target_port in [(547, 546), (547, 547)]:
        bad.append(ether(ipv6(udp(source_port, target_port), 17)))
    for source_port, target_port in [(67, 68), (67, 67)]:
        bad.append(ether(ipv4(udp(source_port, target_port), 17), 0x800))
    for fragment in [0x2000, 1]:
        bad.append(ether(ipv4(udp(), 17, fragment=fragment), 0x800))
    bad.append(ether(ipv4(udp(), 17, ihl=6), 0x800))
    for protocol in [0x8100, 0x88a8]:
        bad.append(ether(struct.pack('!HH', 1, 0x86dd) + ipv6(udp(), 17), protocol))
    bad.extend([ether(bytes(32), 0x88b5), ether(ipv6(neighbor())[:-1])])
    for packet in bad:
        expect(packet, False)
    print(f'{sequence} packet cases passed', flush=True)
finally:
    for fd in fds.values():
        os.close(fd)
