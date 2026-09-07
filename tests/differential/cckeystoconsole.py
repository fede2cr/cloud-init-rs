"""Packaged `cc_keys_to_console` against a scripted machine.

Paired with `crates/ci-modules/examples/dump-cc-keys-to-console.rs`.

Usage: cckeystoconsole.py <case-json>
       cckeystoconsole.py --batch <cases-file>

The case says which paths exist, what the helper printed or failed with, and
what the distro's `usr_lib_exec` is. The record is the log, the ordered list of
things the module did, and what it put on the console.

Nothing runs and nothing reaches a real console: `os.path.exists`, `subp.subp`
and `log_util.multi_log` are stubbed onto the script.

Every function under test is the packaged one; nothing is reimplemented.
"""

import json
import logging
import os
import sys

from cloudinit.config import cc_keys_to_console as m
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
    name = case.get("name", "keys_to_console")
    cfg = case.get("cfg") or {}
    host = case.get("host") or {}
    present = host.get("present") or []
    commands = host.get("commands") or {}

    calls = []
    console = []
    out = {}

    def exists_stub(path):
        calls.append(f"exists {path}")
        return str(path) in present

    def subp_stub(args, **_kwargs):
        key = " ".join(str(token) for token in args)
        calls.append(f"subp {key}")
        result = commands.get(key)
        if result is None:
            raise m.subp.ProcessExecutionError(
                cmd=list(args),
                exit_code=127,
                stdout="",
                stderr="%s: not found" % (args[0] if args else ""),
            )
        if isinstance(result, dict):
            raise m.subp.ProcessExecutionError(
                cmd=result.get("command", key),
                exit_code=result.get("exit_code", 1),
                stdout=result.get("stdout", ""),
                stderr=result.get("stderr", ""),
            )
        return m.subp.SubpResult(result, "")

    def multi_log(text, **_kwargs):
        calls.append("multi_log")
        console.append(text)

    class FakeDistro:
        pass

    distro = FakeDistro()
    if "usr_lib_exec" in case:
        distro.usr_lib_exec = case["usr_lib_exec"]
    else:
        distro.usr_lib_exec = "/usr/lib"

    class FakeCloud:
        pass

    cloud = FakeCloud()
    cloud.distro = distro

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved = {
        "exists": os.path.exists,
        "subp": m.subp.subp,
        "multi_log": m.log_util.multi_log,
    }
    os.path.exists = exists_stub
    m.subp.subp = subp_stub
    m.log_util.multi_log = multi_log

    try:
        m.handle(name, dict(cfg), cloud, [])
    except Exception as error:  # noqa: BLE001 - upstream re-raises
        out["error"] = str(error)
    finally:
        os.path.exists = saved["exists"]
        m.subp.subp = saved["subp"]
        m.log_util.multi_log = saved["multi_log"]
        root.removeHandler(recorder)

    out["log"] = recorder.lines
    out["calls"] = calls
    out["console"] = console
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
        sys.stderr.write("usage: cckeystoconsole.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
