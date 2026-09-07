"""Packaged `cc_ssh_authkey_fingerprints` against a scripted machine.

Paired with `crates/ci-modules/examples/dump-cc-ssh-authkey-fp.rs`.

Usage: ccsshauthkeyfp.py <case-json>
       ccsshauthkeyfp.py --batch <cases-file>

The case says what `ssh_util.extract_authorized_keys` found for each user and
what the *already normalized* user map looks like;
`ug_util.normalize_users_groups` is stubbed rather than exercised, because it
has its own section.

The record is the log, which users were asked about, and the exact lines that
reached the console -- the module is nothing but formatting, so the console
text is what matters.

Every function under test is the packaged one, `simpletable.SimpleTable` and
`util.center` included; nothing is reimplemented.
"""

import json
import logging
import sys

from cloudinit.config import cc_ssh_authkey_fingerprints as m
from cloudinit.log import loggers

loggers.define_extra_loggers()


class KeyEntry:
    """`ssh_util.AuthKeyLine`, as far as the module reads it."""

    def __init__(self, keytype="", base64="", comment="", options=""):
        self.keytype = keytype
        self.base64 = base64
        self.comment = comment
        self.options = options


class Recorder(logging.Handler):
    def __init__(self):
        super().__init__()
        self.lines = []

    def emit(self, record):
        message = record.getMessage()
        if message.endswith(" seconds") and " took " in message:
            return
        self.lines.append(f"{record.filename}[{record.levelname}]: {message}")


def run_case(case):
    name = case.get("name", "ssh_authkey_fingerprints")
    cfg = case.get("cfg") or {}
    users = case.get("users") or {}
    host = case.get("host") or {}
    keys = host.get("keys") or {}

    calls = []
    console = []

    def extract(user):
        calls.append(f"authorized_keys {user}")
        found = keys.get(user)
        if not found:
            return ("", [])
        path = found[0]
        rows = found[1] if len(found) > 1 else []
        return (path, [KeyEntry(*row) for row in rows])

    def multi_log(text, **_kwargs):
        console.append(text)

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved = {
        "extract": m.ssh_util.extract_authorized_keys,
        "multi_log": m.log_util.multi_log,
        "normalize": m.ug_util.normalize_users_groups,
    }
    m.ssh_util.extract_authorized_keys = extract
    m.log_util.multi_log = multi_log
    m.ug_util.normalize_users_groups = lambda _cfg, _distro: (users, {})

    class FakeCloud:
        distro = None

    try:
        m.handle(name, dict(cfg), FakeCloud(), [])
    finally:
        m.ssh_util.extract_authorized_keys = saved["extract"]
        m.log_util.multi_log = saved["multi_log"]
        m.ug_util.normalize_users_groups = saved["normalize"]
        root.removeHandler(recorder)

    return {"log": recorder.lines, "calls": calls, "console": console}


def emit(text):
    try:
        case = json.loads(text)
    except ValueError:
        case = None
    if not isinstance(case, dict):
        record = {"error": "<case-json> must be an object"}
    else:
        record = run_case(case)
    print(json.dumps(record, indent=1, sort_keys=True))


def main():
    argv = sys.argv[1:]
    if argv and argv[0] == "--batch":
        with open(argv[1]) as handle:
            for line in handle:
                line = line.rstrip("\n")
                if not line:
                    continue
                print(f"## {line}")
                emit(line)
        return
    if not argv:
        sys.stderr.write("usage: ccsshauthkeyfp.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
