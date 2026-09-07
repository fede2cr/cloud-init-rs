"""Packaged `cc_ssh_import_id` against a scripted machine.

Paired with `crates/ci-modules/examples/dump-cc-ssh-import-id.rs`.

Usage: ccsshimportid.py <case-json>
       ccsshimportid.py --batch <cases-file>

The case says which programs and accounts exist, which commands fail, what the
config holds and what the *already normalized* user map looks like.
`ug_util.normalize_users_groups` is stubbed out rather than exercised -- it has
its own section -- so both sides start from the same map.

The record is the log and the ordered list of things the module did.

Every function under test is the packaged one; nothing is reimplemented.
"""

import json
import logging
import sys

from cloudinit.config import cc_ssh_import_id as m
from cloudinit.log import loggers

loggers.define_extra_loggers()


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
    cfg = case.get("cfg") or {}
    users = case.get("users") or {}
    args = case.get("args")
    if args is None:
        args = []
    host = case.get("host") or {}
    present = host.get("present") or []
    accounts = host.get("users") or []
    failures = host.get("failures") or {}

    calls = []
    out = {}

    def which_stub(program):
        calls.append(f"which {program}")
        return "/usr/bin/%s" % program if program in present else None

    def getpwnam(user):
        calls.append(f"getpwnam {user}")
        if user not in accounts:
            raise KeyError(user)
        return None

    def subp_stub(args_, **_kwargs):
        key = " ".join(str(token) for token in args_)
        calls.append(f"subp {key}")
        spec = failures.get(key)
        if spec is None:
            return m.subp.SubpResult("", "")
        raise m.subp.ProcessExecutionError(
            cmd=spec.get("command", list(args_)),
            exit_code=spec.get("exit_code", 1),
        )

    class FakeCloud:
        distro = None

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved = {
        "which": m.subp.which,
        "subp": m.subp.subp,
        "getpwnam": m.pwd.getpwnam,
        "normalize": m.ug_util.normalize_users_groups,
    }
    m.subp.which = which_stub
    m.subp.subp = subp_stub
    m.pwd.getpwnam = getpwnam
    m.ug_util.normalize_users_groups = lambda _cfg, _distro: (users, {})

    try:
        m.handle("ssh_import_id", dict(cfg), FakeCloud(), list(args))
    except Exception as error:  # noqa: BLE001 - upstream re-raises
        out["error"] = str(error)
    finally:
        m.subp.which = saved["which"]
        m.subp.subp = saved["subp"]
        m.pwd.getpwnam = saved["getpwnam"]
        m.ug_util.normalize_users_groups = saved["normalize"]
        root.removeHandler(recorder)

    out["log"] = recorder.lines
    out["calls"] = calls
    return out


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
        sys.stderr.write("usage: ccsshimportid.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
