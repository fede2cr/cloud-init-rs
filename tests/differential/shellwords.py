"""Dump shlex.split and util.load_shell_content, for differential testing.

Usage: `shellwords.py <file>`

The file holds one base64 blob per line, so a case can carry newlines, quotes
and backslashes without the shell in between having an opinion.

Not named after the module it imports: this directory is `sys.path[0]`, and a
`shlex.py` here would shadow the stdlib one.
"""

import base64
import json
import shlex
import sys

from cloudinit import util


def dump(value):
    return json.dumps(value, indent=1, sort_keys=True, separators=(",", ": "))


def outcome(fn):
    try:
        return fn()
    except ValueError as e:
        return "ValueError: %s" % e


def main(argv):
    with open(argv[1]) as fp:
        lines = [line.strip() for line in fp if line.strip()]

    out = []
    for line in lines:
        text = base64.b64decode(line).decode("utf-8", "replace")
        out.append(
            {
                "input": text,
                "split": outcome(lambda t=text: shlex.split(t)),
                "split_comments": outcome(
                    lambda t=text: shlex.split(t, comments=True)
                ),
                "shell_content": outcome(
                    lambda t=text: util.load_shell_content(t)
                ),
            }
        )

    print(dump(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
