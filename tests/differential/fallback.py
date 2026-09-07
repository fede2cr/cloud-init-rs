"""Dump fallback network config generation, for differential testing.

Usage: `fallback.py`

Like `renderers.py` this one has no fixture: every function here reads
`/sys/class/net` on the machine it runs on. That is also the comparison that
matters, because this config is what a boot with no datasource actually
renders — if the two implementations disagree about it, one of them writes the
wrong netplan.
"""

import json
import sys

from cloudinit import net


def dump(value):
    return json.dumps(value, indent=1, sort_keys=True, separators=(",", ": "))


def main(argv):
    out = {
        "candidates": net.find_candidate_nics(),
        "fallback_nic": net.find_fallback_nic(),
        "config": net.generate_fallback_config(),
        "config_driver": net.generate_fallback_config(config_driver=True),
    }
    print(dump(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
