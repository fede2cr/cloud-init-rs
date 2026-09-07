"""Dump `util.get_hostname_fqdn` for one case, for differential testing.

Usage: `hostname.py <root> <cfg-json> <metadata-json>`.

`root` holds the fixture's `proc/sys/kernel/hostname` and `etc/hosts`, which
the no-config path falls back to. Upstream reads both through
`socket.gethostname()` and a hard-coded `/etc/hosts`, neither of which takes a
prefix, so they are monkeypatched here rather than in the fixture.
"""

import json
import os
import sys

from cloudinit import util
from cloudinit.sources import DataSource


class Cloud:
    """The two attributes `get_hostname_fqdn` touches on a `Cloud`."""

    def __init__(self, metadata):
        self.metadata = metadata

    def get_hostname(self, fqdn=False, resolve_ip=False, metadata_only=False):
        return DataSource.get_hostname(
            self, fqdn=fqdn, resolve_ip=resolve_ip, metadata_only=metadata_only
        )


def main(argv):
    if len(argv) != 3:
        sys.stderr.write("usage: hostname.py <root> <cfg-json> <metadata-json>\n")
        return 2
    root, cfg, metadata = argv[0], json.loads(argv[1]), json.loads(argv[2])

    def gethostname():
        path = os.path.join(root, "proc/sys/kernel/hostname")
        try:
            with open(path) as handle:
                return handle.read().rstrip("\n")
        except OSError:
            return ""

    real_get_fqdn = util.get_fqdn_from_hosts
    util.get_hostname = gethostname
    util.get_fqdn_from_hosts = lambda hostname: real_get_fqdn(
        hostname, filename=os.path.join(root, "etc/hosts")
    )

    got = util.get_hostname_fqdn(cfg, Cloud(metadata))
    print(
        json.dumps(
            {
                "hostname": got.hostname,
                "fqdn": got.fqdn,
                "is_default": got.is_default,
            },
            indent=1,
            sort_keys=True,
            separators=(",", ": "),
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
