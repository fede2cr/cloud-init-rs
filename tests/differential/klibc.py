"""Dump the klibc initramfs network config source, for differential testing.

Usage: `klibc.py <run-dir> <cmdline>`

`<run-dir>` stands in for `/run`, which upstream hardcodes, so the file list is
handed to `KlibcNetworkConfigSource` explicitly — in the same sorted order the
port uses, since `glob` leaves it to the filesystem. The mac addresses come
from this host's real `/sys/class/net` on both sides.
"""

import glob
import json
import os
import sys

from cloudinit.net import cmdline


def dump(value):
    return json.dumps(value, indent=1, sort_keys=True, separators=(",", ": "))


def net_cfg_files(run_dir):
    return sorted(glob.glob(os.path.join(run_dir, "net-*.conf"))) + sorted(
        glob.glob(os.path.join(run_dir, "net6-*.conf"))
    )


def main(argv):
    run_dir, kernel_cmdline = argv[1], argv[2]
    files = net_cfg_files(run_dir)
    source = cmdline.KlibcNetworkConfigSource(
        _files=files, _cmdline=kernel_cmdline
    )

    try:
        config = source.render_config()
    except ValueError as e:
        config = "ValueError: %s" % e

    print(
        dump(
            {
                "files": [os.path.basename(f) for f in files],
                "is_applicable": source.is_applicable(),
                "config": config,
            }
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
