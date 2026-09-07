#!/usr/bin/env python3
"""`util.human2bytes` for the differential harness.

Paired with `crates/ci-core/examples/dump-human2bytes.rs`. Named `h2b.py` so it
cannot shadow a module the harness or cloudinit imports.

Usage: h2b.py --batch <cases-file>
       h2b.py <base64-of-size>

The size arrives base64-encoded so that whitespace and the empty string survive
the cases file.
"""

import base64
import json
import sys

from cloudinit import util


def one(encoded):
    size = base64.b64decode(encoded).decode()
    record = {"size": size}
    try:
        record["result"] = util.human2bytes(size)
        record["error"] = None
    except Exception as error:  # noqa: BLE001 - the wording is the comparison
        record["result"] = None
        record["error"] = str(error)
    return record


def dump(record):
    return json.dumps(
        record,
        indent=1,
        sort_keys=True,
        separators=(",", ": "),
    )


def main(argv):
    if argv[1:2] == ["--batch"]:
        with open(argv[2]) as fp:
            for line in fp:
                line = line.rstrip("\n")
                if not line:
                    continue
                print("##", line)
                print(dump(one(line)))
        return 0
    if len(argv) < 2:
        sys.stderr.write("usage: h2b.py <base64-size> | --batch <cases>\n")
        return 2
    print(dump(one(argv[1])))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
