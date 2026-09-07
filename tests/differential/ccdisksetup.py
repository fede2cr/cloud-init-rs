"""Packaged `cc_disk_setup` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-disk-setup.rs`.

Usage: ccdisksetup.py <case-json>
       ccdisksetup.py --batch <cases-file>

The case carries the config and every answer the module could get from a
machine: command output and exit codes, which paths exist, which of them are
block devices, what `realpath` resolves to and whether the end-of-disk wipe
succeeds. The record is the log and the ordered list of questions the module
asked -- which for this module is also its outcome, since everything it does it
does by running a command.

Nothing runs a command or touches a device: `subp.subp`, `subp.which`, the `os`
probes, `pathlib.Path` and the module's own `open` are stubbed onto the script.
The `open` one matters most -- it is what `purge_disk_ptable` uses to zero the
first and last mebibyte of a disk.

Every function under test is the packaged one; nothing is reimplemented.
"""

import json
import logging
import os
import sys

from cloudinit.config import cc_disk_setup as m
from cloudinit.log import loggers

# `lifecycle.deprecate` picks its level from whether this has run, which it
# has by the time a module is handled.
loggers.define_extra_loggers()

REAL = {
    "exists": os.path.exists,
    "realpath": os.path.realpath,
}


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
    cfg = case.get("cfg") or {}
    host = case.get("host") or {}
    calls = []

    commands = host.get("commands") or {}
    shells = host.get("shell") or {}
    which = host.get("which") or {}
    exists = host.get("exists") or []
    blocks = host.get("block") or []
    realpaths = host.get("realpath") or {}
    wipes = host.get("wipe") or {}

    def result_of(argv, result, rcs):
        if result is None:
            raise m.subp.ProcessExecutionError(
                stdout="",
                stderr="%s: not found" % (argv[0] if argv else ""),
                exit_code=127,
                cmd=argv if len(argv) > 1 else argv[0],
            )
        code = result.get("exit_code", 0)
        stdout = result.get("stdout", "")
        stderr = result.get("stderr", "")
        if code in (rcs or [0]):
            return m.subp.SubpResult(stdout, stderr)
        raise m.subp.ProcessExecutionError(
            stdout=stdout,
            stderr=stderr,
            exit_code=code,
            cmd=argv if len(argv) > 1 else argv[0],
        )

    def subp_stub(args, data=None, update_env=None, rcs=None, shell=False, **_kw):
        if shell:
            calls.append("shell %s" % args)
            return result_of([args], shells.get(args), [0])
        argv = [str(token) for token in args]
        key = " ".join(argv)
        env = ",".join(f"{k}={v}" for k, v in (update_env or {}).items())
        calls.append(
            "subp %s env=%s data=%s rcs=%s"
            % (
                key,
                env,
                "-" if data is None else json.dumps(data),
                ",".join(str(code) for code in (rcs or [0])),
            )
        )
        return result_of(argv, commands.get(key), rcs)

    def which_stub(program, **_kwargs):
        calls.append(f"which {program}")
        return which.get(program)

    def exists_stub(path):
        calls.append(f"exists {path}")
        # `os.path.exists` swallows OSError and ValueError but not the
        # TypeError `os.stat` raises for something that is not a path.
        if not isinstance(path, (str, bytes, int)):
            raise TypeError(
                "stat: path should be string, bytes, os.PathLike or integer,"
                " not %s" % type(path).__name__
            )
        return path in exists

    def realpath_stub(path):
        calls.append(f"realpath {path}")
        return realpaths.get(path, path)

    class FakePath:
        """Only `is_block_device`, which is all `mkfs` asks of `pathlib`."""

        def __init__(self, path):
            self.path = str(path)

        def is_block_device(self):
            calls.append(f"isblk {self.path}")
            return self.path in blocks

    class FakeFile:
        """`purge_disk_ptable`'s `open(device, "rb+")`: the write, the seek to
        the last mebibyte and the flush are one act, so one call is recorded
        for the lot."""

        def __init__(self, device):
            self.device = device

        def __enter__(self):
            return self

        def __exit__(self, *_args):
            return False

        def write(self, _data):
            return None

        def seek(self, _offset, _whence=0):
            return None

        def flush(self):
            return None

    def open_stub(device, *_args, **_kwargs):
        calls.append(f"wipe {device}")
        failure = wipes.get(device)
        if failure is not None:
            raise OSError(failure)
        return FakeFile(device)

    class FakeCloud:
        """`cloud.device_name_to_device` is the only thing the module asks the
        cloud for, and the base `DataSource` answers `None`."""

        @staticmethod
        def device_name_to_device(_name):
            return None

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved_path = {
        name: getattr(os.path, name) for name in ("exists", "realpath")
    }
    saved = {
        "subp": m.subp.subp,
        "which": m.subp.which,
        "Path": m.Path,
        "open": getattr(m, "open", None),
    }

    os.path.exists = exists_stub
    os.path.realpath = realpath_stub
    m.subp.subp = subp_stub
    m.subp.which = which_stub
    m.Path = FakePath
    # A module-level `open` shadows the builtin for this module alone.
    m.open = open_stub

    out = {}
    try:
        m.handle("disk_setup", cfg, FakeCloud(), [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        for name, value in saved_path.items():
            setattr(os.path, name, value)
        m.subp.subp = saved["subp"]
        m.subp.which = saved["which"]
        m.Path = saved["Path"]
        if saved["open"] is None:
            del m.open
        else:
            m.open = saved["open"]
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
        sys.stderr.write("usage: ccdisksetup.py <case-json>\n")
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
