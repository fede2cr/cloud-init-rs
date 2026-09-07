"""Dump network activator availability and selection, for differential testing.

Usage: `activators.py [priority,priority,...]`
"""

import json
import sys

from cloudinit.net import activators


def dump(value):
    return json.dumps(value, indent=1, sort_keys=True, separators=(",", ": "))


def main(argv):
    priority = None
    if argv:
        priority = argv[0].split(",") if argv[0] else []

    out = {
        "available": {
            name: bool(cls.available())
            for name, cls in activators.NAME_TO_ACTIVATOR.items()
        }
    }

    # `search_activator` has no default; `select_activator` supplies one.
    searched = priority if priority is not None else activators.DEFAULT_PRIORITY
    try:
        found = activators.search_activator(searched)
        out["search"] = repr(found) if found else None
    except Exception as err:
        out["search_error"] = str(err)

    try:
        out["select"] = repr(activators.select_activator(priority=priority))
    except Exception as err:
        out["select_error"] = str(err)

    print(dump(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
