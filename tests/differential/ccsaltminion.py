"""Packaged `cc_salt_minion` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-salt-minion.rs`.

Usage: ccsaltminion.py <case-json>
       ccsaltminion.py --batch <cases-file>

The case carries the config, the directories that already exist and any call
that should fail. The record is the log, the ordered list of things the module
did, and what each file ended up holding.

Nothing runs a command or touches a filesystem: `subp.subp`, `util.write_file`,
`util.ensure_dir`, `os.path.isdir` and the two `Distro` methods are stubbed
onto the module. `SaltConstants`, `util.umask` and `safeyaml.dumps` are the
packaged ones and run as written.

`util.ensure_dir` records the umask in force only when it is not the 0o022 this
driver pins on entry, which is how the `util.umask(0o77)` block around the key
pair shows up in the record. It records unconditionally rather than skipping a
directory that already exists, so `dirs` answers only the module's own
`os.path.isdir`. `util.write_file` encodes before it records, so a key that is
not a string raises without leaving a call behind -- the port does the same.
"""

import json
import logging
import os
import sys

from cloudinit import distros, util
from cloudinit.config import cc_salt_minion as m
from cloudinit.distros import PackageInstallerError
from cloudinit.log import loggers

loggers.define_extra_loggers()

# `SaltConstants` opens with `util.is_FreeBSD()`, whose first call reads and
# logs `/etc/os-release`. Warming it here keeps that out of the record: the
# port has no FreeBSD branch to take (deviation 167).
util.is_FreeBSD()

BASE_UMASK = 0o022


class Recorder(logging.Handler):
    """Every record as `<file>[<LEVEL>]: <message>`, the way the port keeps
    them. The timestamp upstream prints as well is not comparable."""

    def __init__(self):
        super().__init__()
        self.lines = []

    def emit(self, record):
        message = record.getMessage()
        if message.endswith(" seconds") and " took " in message:
            return
        self.lines.append(f"{record.filename}[{record.levelname}]: {message}")


def run_case(case):
    name = case.get("name", "salt_minion")
    cfg = case.get("cfg") or {}
    host = case.get("host") or {}
    failures = host.get("failures") or {}
    dirs = set(host.get("dirs") or [])

    calls = []
    written = []
    out = {}

    def record(call):
        calls.append(call)
        return call

    def note(path, content):
        for slot in written:
            if slot[0] == path:
                slot[1] = content
                return
        written.append([path, content])

    def current_umask():
        mask = os.umask(BASE_UMASK)
        os.umask(mask)
        return mask

    def subp_stub(args, capture=True, **_kwargs):
        call = record("subp %s" % " ".join(args))
        if call in failures:
            raise m.subp.ProcessExecutionError(
                cmd=call, exit_code=1, stdout="", stderr=failures[call]
            )
        return m.subp.SubpResult("", "")

    def install_packages(packages):
        call = record("install_packages %r" % (packages,))
        if call in failures:
            raise PackageInstallerError(failures[call])

    def manage_service(action, service, *_extra, **_kwargs):
        call = record("manage_service %s %s" % (action, service))
        if call in failures:
            raise m.subp.ProcessExecutionError(
                cmd=call, exit_code=1, stdout="", stderr=failures[call]
            )
        return m.subp.SubpResult("", "")

    def ensure_dir(path, mode=None, **_kwargs):
        # The real one leads with `os.path.isdir(path)` and follows with
        # `os.makedirs(path)`. A path that is not a path dies in one or the
        # other, with a different message: an int is a file descriptor as far
        # as `stat` is concerned, so it gets as far as `makedirs`.
        if isinstance(path, (bool, int)):
            raise TypeError(
                "expected str, bytes or os.PathLike object, not %s"
                % type(path).__name__
            )
        if not isinstance(path, (str, bytes, os.PathLike)):
            raise TypeError(
                "stat: path should be string, bytes, os.PathLike or integer,"
                " not %s" % type(path).__name__
            )
        mask = current_umask()
        if mask == BASE_UMASK:
            call = record("ensure_dir %s" % path)
        else:
            call = record("ensure_dir %s umask=%04o" % (path, mask))
        if call in failures:
            raise OSError(failures[call])

    def write_file(path, content, mode=None, **_kwargs):
        content = m.util.decode_binary(m.util.encode_text(content))
        call = record("write_file %s" % path)
        if call in failures:
            raise OSError(failures[call])
        note(path, content)

    class FakePath:
        """Only the two members `cc_salt_minion` reaches for. Standing in for
        `os.path` wholesale keeps the recorded `isdir` to the module's own
        call, rather than the ones `util.ensure_dir` makes underneath."""

        join = staticmethod(os.path.join)

        @staticmethod
        def isdir(path):
            record("is_dir %s" % path)
            return path in dirs

    class FakeOs:
        path = FakePath()

    cls = distros.fetch("ubuntu")
    distro = cls("ubuntu", {}, None)
    distro.install_packages = install_packages
    distro.manage_service = manage_service

    class FakeCloud:
        pass

    cloud = FakeCloud()
    cloud.distro = distro

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved = {
        "subp": m.subp.subp,
        "write_file": m.util.write_file,
        "ensure_dir": m.util.ensure_dir,
        "os": m.os,
    }
    m.subp.subp = subp_stub
    m.util.write_file = write_file
    m.util.ensure_dir = ensure_dir
    m.os = FakeOs()
    entry_umask = os.umask(BASE_UMASK)

    try:
        m.handle(name, dict(cfg), cloud, [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        os.umask(entry_umask)
        m.subp.subp = saved["subp"]
        m.util.write_file = saved["write_file"]
        m.util.ensure_dir = saved["ensure_dir"]
        m.os = saved["os"]
        root.removeHandler(recorder)

    out["log"] = recorder.lines
    out["calls"] = calls
    out["written"] = written
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
        sys.stderr.write("usage: ccsaltminion.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
