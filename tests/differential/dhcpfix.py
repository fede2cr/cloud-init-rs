#!/usr/bin/env python3
"""Write the dhcpcd lease fixtures the differential compares.

A packet is 240 bytes of bootp and magic cookie followed by code/length/value
triples, which is tedious to write by hand and easy to get subtly wrong, so it
is built here instead of being checked in as literals.

Usage: dhcpfix.py <output-dir>
"""
import json
import os
import sys

PLAIN = (
    "broadcast_address='192.168.15.255'\n"
    "dhcp_lease_time='3600'\n"
    "dhcp_message_type='5'\n"
    "dhcp_server_identifier='192.168.0.1'\n"
    "domain_name='us-east-2.compute.internal'\n"
    "domain_name_servers='192.168.0.2'\n"
    "host_name='ip-192-168-0-212'\n"
    "interface_mtu='9001'\n"
    "ip_address='192.168.0.212'\n"
    "network_number='192.168.0.0'\n"
    "routers='192.168.0.1'\n"
    "subnet_cidr='20'\n"
    "subnet_mask='255.255.240.0'\n"
)

ROUTES = PLAIN + "classless_static_routes='0.0.0.0/0 10.0.0.1 168.63.129.16/32 10.0.0.1'\n"


def packet(*options):
    """240 bytes of preamble, then (code, value) triples."""
    data = bytearray(240)
    for code, value in options:
        data.append(code)
        data.append(len(value))
        data.extend(value)
    return list(data)


CASES = {
    # The dump on its own, with no packet to read options out of.
    "plain": {"dump": PLAIN, "interface": "eth0"},
    # `classless_static_routes` is renamed and moved to the end, and the same
    # string is parsed into pairs separately.
    "routes": {
        "dump": ROUTES,
        "interface": "eth0",
        "routes": "0.0.0.0/0 10.0.0.1 168.63.129.16/32 10.0.0.1",
    },
    # The Azure case: option 245 carries the wireserver address.
    "wireserver": {
        "dump": PLAIN,
        "interface": "eth0",
        "packet": packet((53, b"\x05"), (245, bytes([168, 63, 129, 16]))),
    },
    # 245 last, after options of other lengths, so the walk has to step.
    "other-options": {
        "dump": PLAIN,
        "interface": "eth0",
        "packet": packet(
            (1, bytes([255, 255, 240, 0])),
            (3, bytes([192, 168, 0, 1])),
            (51, bytes([0, 0, 14, 16])),
            (245, bytes([10, 1, 2, 3])),
        ),
    },
    # A packet with no 245 at all leaves the key absent.
    "no-options": {
        "dump": PLAIN,
        "interface": "eth0",
        "packet": packet((53, b"\x05"), (58, bytes([0, 0, 7, 8]))),
    },
    # Every single quote is stripped, wherever it appears.
    "quoted": {
        "dump": "host_name='it's-quoted'\nrouters='192.168.0.1'\n",
        "interface": "eth0",
    },
    # A trailing destination with no gateway is dropped by `zip`.
    "odd-routes": {
        "dump": PLAIN,
        "interface": "eth0",
        "routes": "0.0.0.0/0 10.0.0.1 1.2.3.4/32",
    },
    "empty-routes": {"dump": PLAIN, "interface": "eth0", "routes": "   "},
    # Lines without an `=` are skipped rather than failing the parse.
    "blank-line": {
        "dump": "\nrouters='192.168.0.1'\n\nnot a setting\nip_address='10.0.0.4'\n",
        "interface": "ens3",
    },
    # Nothing assignable at all is the one parse failure.
    "no-assignments": {"dump": "no equals here\nnor here\n", "interface": "eth0"},
    "empty-dump": {"dump": "", "interface": "eth0"},
    # Two keys that collide once underscores become hyphens: the later wins.
    "collide": {
        "dump": "host_name='a'\nhost-name='b'\nrouters='192.168.0.1'\n",
        "interface": "eth0",
    },
    # An option whose length runs past the end of the packet is clamped, not
    # an error, because Python's slicing clamps.
    "trailing-option": {
        "dump": PLAIN,
        "interface": "eth0",
        "packet": packet((53, b"\x05")) + [245, 8, 10, 0],
    },
}


def main(argv):
    if not argv:
        print("usage: dhcpfix.py <output-dir>", file=sys.stderr)
        return 1
    out = argv[0]
    os.makedirs(out, exist_ok=True)
    for name, fixture in CASES.items():
        with open(os.path.join(out, name + ".json"), "w") as handle:
            json.dump(fixture, handle, indent=2, sort_keys=True)
            handle.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
