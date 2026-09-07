"""Packaged `cc_resizefs` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-resizefs.rs`.

Usage: ccresizefs.py <case-json>
       ccresizefs.py --batch <cases-file>

The case carries the config, the module's run arguments, and every answer the
module could get from a machine: command output and exit codes, which paths
exist, what `stat` says, what is mounted where and with which options. The
record is the log and the ordered list of questions the module asked.

Nothing runs a command or touches a filesystem: `subp.subp`, `util.fork_cb` and
the `os` probes are stubbed onto the script, so the comparison is of decisions
and their order. `cc_resizefs` interleaves the two -- `growfs -N` exiting 1 *is*
the decision that nothing needs doing, and which command to use is only known
after `btrfs --version` answers -- which is why this harness scripts a machine
instead of feeding a plan.

Every function under test is the packaged one; nothing is reimplemented.
"""

import json
import logging
import os
import sys

from cloudinit.config import cc_resizefs as m
from cloudinit.log import loggers

# `lifecycle.deprecate` picks its level from whether this has run, which it
# has by the time a module is handled.
loggers.define_extra_loggers()


class Recorder(logging.Handler):
    """Every record as `<file>[<LEVEL>]: <message>`, the way the port keeps
    them. The timestamp upstream prints as well is not comparable."""

    def __init__(self):
        super().__init__()
        self.lines = []

    def emit(self, record):
        message = record.getMessage()
        # `performance.Timed` logs a duration when the context is slow, which
        # is a property of the machine and not of the decision.
        if message.endswith(" seconds") and " took " in message:
            return
        self.lines.append(f"{record.filename}[{record.levelname}]: {message}")


def run_case(case):
    name = case.get("name", "resizefs")
    cfg = case.get("cfg") or {}
    args = case.get("args")
    if args is None:
        args = []
    host = case.get("host") or {}
    calls = []
    out = {}

    commands = host.get("commands") or {}
    exists = host.get("exists") or []
    dirs = host.get("dirs") or []
    stats = host.get("stat") or {}
    mounts = host.get("mounts") or {}
    devs = host.get("devs") or {}

    def subp_stub(args, **_kwargs):
        argv = [str(token) for token in args]
        key = " ".join(argv)
        calls.append("subp %s" % key)
        result = commands.get(key)
        if result is None:
            raise m.subp.ProcessExecutionError(
                stdout="",
                stderr="%s: not found" % (argv[0] if argv else ""),
                exit_code=127,
                cmd=list(args),
            )
        code = result.get("exit_code", 0)
        stdout = result.get("stdout", "")
        stderr = result.get("stderr", "")
        if code == 0:
            return m.subp.SubpResult(stdout, stderr)
        raise m.subp.ProcessExecutionError(
            stdout=stdout, stderr=stderr, exit_code=code, cmd=list(args)
        )

    def exists_stub(path):
        calls.append(f"exists {path}")
        return str(path) in exists

    def isdir_stub(path):
        calls.append(f"isdir {path}")
        return str(path) in dirs

    def stat_stub(path, **_kwargs):
        calls.append(f"stat {path}")
        mode = stats.get(str(path))
        if mode is None:
            raise FileNotFoundError(2, "No such file or directory", str(path))
        return os.stat_result((int(mode, 8), 0, 0, 0, 0, 0, 0, 0, 0, 0))

    def get_mount_info(path, log=None, get_mnt_opts=False):
        calls.append(
            "%s %s" % ("mount_opts" if get_mnt_opts else "mount_info", path)
        )
        found = mounts.get(str(path))
        if not found:
            return None
        return tuple(found) if get_mnt_opts else tuple(found[:3])

    def is_container():
        calls.append("is_container")
        return bool(host.get("container", False))

    def get_cmdline():
        calls.append("cmdline")
        return host.get("cmdline", "")

    def find_devs_with(criteria=None, **_kwargs):
        calls.append(f"find_devs_with {criteria}")
        return list(devs.get(str(criteria), []))

    def fork_cb(child_cb, *cb_args, **_kwargs):
        # Upstream forks; the pid it logs is not comparable, and the child's
        # work is what the non-forking branch already covers.
        calls.append("fork %s" % " ".join(str(token) for token in cb_args[0]))

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved_os = {"stat": os.stat}
    saved_path = {"exists": os.path.exists, "isdir": os.path.isdir}
    saved_util = {
        name_: getattr(m.util, name_)
        for name_ in (
            "get_mount_info",
            "is_container",
            "get_cmdline",
            "find_devs_with",
            "fork_cb",
        )
    }
    saved_subp = m.subp.subp

    os.stat = stat_stub
    os.path.exists = exists_stub
    os.path.isdir = isdir_stub
    m.util.get_mount_info = get_mount_info
    m.util.is_container = is_container
    m.util.get_cmdline = get_cmdline
    m.util.find_devs_with = find_devs_with
    m.util.fork_cb = fork_cb
    m.subp.subp = subp_stub

    try:
        m.handle(name, cfg, None, args)
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        for key, value in saved_os.items():
            setattr(os, key, value)
        for key, value in saved_path.items():
            setattr(os.path, key, value)
        for key, value in saved_util.items():
            setattr(m.util, key, value)
        m.subp.subp = saved_subp
        root.removeHandler(recorder)

    out["log"] = recorder.lines
    out["calls"] = calls
    return out


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
        sys.stderr.write("usage: ccresizefs.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


def emit(text):
    try:
        case = json.loads(text)
    except ValueError:
        case = None
    if not isinstance(case, dict):
        print(
            json.dumps(
                {"error": "<case-json> must be an object"},
                indent=1,
                sort_keys=True,
            )
        )
        return
    print(json.dumps(run_case(case), indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
