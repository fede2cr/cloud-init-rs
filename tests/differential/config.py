"""Dumps the merged system config as JSON, matching `dump-config`.

Reaches into `Init._cfg` because that is the only place the fully merged
configuration exists: every public accessor strips or narrows it.
"""

import json
import sys

from cloudinit import stages


def main(argv):
    init = stages.Init(ds_deps=[])
    init.read_cfg(extra_fns=argv or None)
    print(json.dumps(init._cfg, indent=1, sort_keys=True, separators=(",", ": ")))


if __name__ == "__main__":
    main(sys.argv[1:])
