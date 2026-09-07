"""Dump `cc_snap.handle`'s decisions, for differential testing.

Usage: `snap.py <cfg-json> <snap-present>`
       `snap.py --batch <cases-file>`

Matches `dump-cc-snap`. `<snap-present>` is `0` or `1`, standing in for
`subp.which("snap")` -- a fact about the live machine that the seeding wait
turns on, supplied so the comparison covers both answers.

Everything the module would actually do is stubbed and recorded: it writes an
assertions file under the instance directory and then runs `snap ack` and
whatever the config asked for, none of which should happen to the machine
running a test.

What this does *not* cover is a failing command. Upstream aggregates
`str(ProcessExecutionError)`, whose six-line template this port does not
reproduce (see docs/COMPAT.md); comparing it would only re-report a deviation
already recorded. Every simulated command here succeeds.
"""

import json
import sys

from cloudinit import subp, util
from cloudinit.config import cc_snap

CALLS = []

IPATH = "/var/lib/cloud/instance"


class Recorder:
    def debug(self, fmt, *args):
        CALLS.append({"message": fmt % args, "op": "debug"})

    def warning(self, fmt, *args):
        CALLS.append({"message": fmt % args, "op": "warning"})


class Paths:
    def get_ipath_cur(self):
        return IPATH


class Cloud:
    paths = Paths()

    def run(self, name, functor, args, freq=None):
        # Upstream guards this with a PER_ONCE semaphore. A fresh instance
        # always runs, which is the case worth comparing.
        CALLS.append({"freq": freq, "name": name, "op": "cloud_run"})
        return functor(*args)


def one(fields):
    cfg = json.loads(fields[0])
    present = fields[1] == "1" if len(fields) > 1 else False

    del CALLS[:]

    def write_file(path, content, *args, **kwargs):
        # The module hands `util.write_file` bytes, not text.
        if isinstance(content, bytes):
            content = content.decode("utf-8")
        CALLS.append({"content": content, "op": "write_file", "path": path})

    def run_command(args, **kwargs):
        CALLS.append(
            {
                "args": args,
                "op": "subp",
                "shell": bool(kwargs.get("shell", False)),
            }
        )
        return ("", "")

    cc_snap.util.write_file = write_file
    cc_snap.subp.subp = run_command
    subp.subp = run_command
    subp.which = lambda name: "/usr/bin/snap" if present else None
    util.subp.which = subp.which
    cc_snap.LOG = Recorder()
    util.LOG = Recorder()
    subp.LOG = Recorder()

    try:
        cc_snap.handle("snap", cfg, Cloud(), [])
    except Exception as error:  # noqa: BLE001 -- the shape is the point
        # The message only, not the class: the port's error channel is a
        # string, so `TypeError` and `RuntimeError` are indistinguishable to
        # it. Every other harness here does the same.
        return {"calls": list(CALLS), "error": str(error)}
    return {"calls": list(CALLS)}


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
        sys.stderr.write("usage: snap.py <cfg-json> <snap-present> | --batch <cases>\n")
        return 2
    emit(one(argv))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
