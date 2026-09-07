#!/usr/bin/env python3
"""Dump a dhcpcd lease parsed by upstream's `Dhcpcd`, from a fixture.

Paired with the `dump-dhcp` example in ci-net. `parse_dhcpcd_lease` reads the
binary lease packet off `/var/lib/dhcpcd/` itself, so the fixture's `packet` is
fed in by monkeypatching that read — the port takes the bytes as an argument
instead, which is the same input by a different route.

Usage: dhcp.py <fixture-json>
"""
import json
import sys

sys.path.insert(0, "/usr/lib/python3/dist-packages")

from cloudinit.net import dhcp  # noqa: E402


def main(argv):
    if not argv:
        print("usage: dhcp.py <fixture-json>", file=sys.stderr)
        return 1
    with open(argv[0]) as handle:
        fixture = json.load(handle)

    packet = fixture.get("packet")
    dhcp.util.load_binary_file = lambda *a, **kw: (
        bytes(packet) if packet is not None else b""
    )

    if "routes" in fixture:
        for dest, gateway in dhcp.Dhcpcd.parse_static_routes(fixture["routes"]):
            print("route %s %s" % (dest, gateway))

    try:
        lease = dhcp.Dhcpcd.parse_dhcpcd_lease(
            fixture.get("dump", ""), fixture.get("interface", "eth0")
        )
    except dhcp.InvalidDHCPLeaseFileError as error:
        print("error=%s" % error)
        return 1
    for key, value in lease.items():
        print("%s=%s" % (key, value))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
