"""Dump `util.parse_mount_info` and `util.parse_mtab`, for differential testing.

Usage: `mountinfo.py <kind> <path> <lines>`
       `mountinfo.py --batch <cases-file>`

Matches `dump-mountinfo`. `<kind>` is `mountinfo` or `mtab`; `<lines>` is the
file's lines joined by U+001F, so that a whole synthetic `mountinfo` fits in
one tab-separated field alongside the path being resolved.

The contents are passed in rather than read because the interesting cases are
the ones the host is not in: bind mounts, btrfs subvolumes, an overmount of a
directory, and the malformed lines that make the parser abandon the file. The
one exception is a case built from the real `/proc/self/mountinfo`, which both
sides read from the same place.

`parse_mtab` reads `/etc/mtab` itself, so `load_text_file` is stubbed for the
duration -- pointing it at the real file would only ever test the host.
"""

import json
import sys

from cloudinit import util

LINE_SEP = "\x1f"

DEBUG = []


class Recorder:
    """Stands in for the module logger, which only ever gets `debug`."""

    def debug(self, fmt, *args):
        DEBUG.append(fmt % args)


def one(fields):
    kind = fields[0] if fields else ""
    path = fields[1] if len(fields) > 1 else ""
    blob = fields[2] if len(fields) > 2 else ""
    lines = blob.split(LINE_SEP) if blob else []

    del DEBUG[:]
    if kind == "mtab":
        text = "\n".join(lines)
        original = util.load_text_file
        util.load_text_file = lambda path, **kwargs: text
        try:
            result = util.parse_mtab(path)
        finally:
            util.load_text_file = original
    else:
        result = util.parse_mount_info(path, lines, Recorder(), True)

    return {
        "debug": list(DEBUG),
        "result": list(result) if result else None,
    }


def emit(record):
    print(
        json.dumps(
            record,
            indent=1,
            sort_keys=True,
            separators=(",", ": "),
        )
    )


def main(argv):
    if argv[:1] == ["--batch"]:
        with open(argv[1]) as handle:
            for line in handle:
                line = line.rstrip("\n")
                if not line:
                    continue
                print("## " + line)
                emit(one(line.split("\t")))
        return 0
    if not argv:
        sys.stderr.write("usage: mountinfo.py <kind> <path> <lines> | --batch <cases>\n")
        return 2
    emit(one(argv))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
