#!/usr/bin/env python3
"""Dump what `util.read_seeded` fetches for a seedfrom base.

Paired with the `dump-seed` example in ci-datasource. Metadata and network
config go through `json_dumps` because upstream parses them as YAML; the two
payloads are base64 so arbitrary bytes survive the comparison. A failure is
reported as `error`: the port does not reproduce upstream's exception text.

Usage: seed.py BASE
"""
import base64
import sys

sys.path.insert(0, "/usr/lib/python3/dist-packages")

from cloudinit import url_helper, util  # noqa: E402
from cloudinit.atomic_helper import json_dumps  # noqa: E402


def main(argv):
    try:
        md, ud, vd, network = util.read_seeded(argv[0], timeout=1, retries=0)
    except url_helper.UrlError:
        print("error")
        return 0
    print("meta-data=%s" % json_dumps(md))
    for name, blob in (("user-data", ud), ("vendor-data", vd)):
        if blob is None:
            print("%s=<none>" % name)
        else:
            print("%s=%s" % (name, base64.b64encode(blob).decode()))
    print("network-config=%s" % (
        "<none>" if network is None else json_dumps(network)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
