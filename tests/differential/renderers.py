"""Dump network renderer availability and selection, for differential testing.

Usage: `renderers.py [priority,priority,...]`
"""

import json
import sys

from cloudinit.net import renderers


def dump(value):
    return json.dumps(value, indent=1, sort_keys=True, separators=(",", ": "))


def main(argv):
    priority = None
    if argv:
        priority = argv[0].split(",") if argv[0] else []

    out = {
        "available": {
            name: bool(renderers.search(priority=[name], first=False))
            for name in renderers.NAME_TO_RENDERER
        }
    }

    try:
        out["search"] = [name for name, _ in renderers.search(priority=priority)]
    except Exception as err:
        out["search_error"] = str(err)

    try:
        out["select"] = renderers.select(priority=priority)[0]
    except Exception as err:
        out["select_error"] = str(err)

    print(dump(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
