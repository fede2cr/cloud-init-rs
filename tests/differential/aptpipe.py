"""Dump `cc_apt_pipelining.handle`'s decisions, for differential testing.

Usage: `aptpipe.py <cfg-json>`
       `aptpipe.py --batch <cases-file>`

Matches `dump-cc-apt-pipelining`. The module's only escape is
`util.write_file` against `/etc/apt/apt.conf.d`, which is a real directory on
the machine running this, so it is stubbed and recorded instead.

Batch mode exists because this process costs about a third of a second to start
and `import cloudinit` is most of it; across a few thousand cases that startup
is the whole runtime of the comparison. One line of the case file is one case's
tab-separated argument list.

Not named after the module it imports: a `cc_apt_pipelining.py` next to this
one would shadow the packaged module on `sys.path`.
"""

import json
import sys

from cloudinit.config import cc_apt_pipelining


def one(fields):
    cfg = json.loads(fields[0])
    calls = []

    def write_file(path, content, *args, **kwargs):
        calls.append({"content": content, "op": "write_file", "path": path})

    class Recorder:
        def debug(self, fmt, *args):
            calls.append({"message": fmt % args, "op": "debug"})

        def warning(self, fmt, *args):
            calls.append({"message": fmt % args, "op": "warning"})

    cc_apt_pipelining.util.write_file = write_file
    cc_apt_pipelining.LOG = Recorder()

    cc_apt_pipelining.handle("apt_pipelining", cfg, None, [])
    return {"calls": calls}


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
        sys.stderr.write("usage: aptpipe.py <cfg-json> | --batch <cases>\n")
        return 2
    emit(one(argv))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
