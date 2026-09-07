"""Packaged `cc_mcollective` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-mcollective.rs`.

Usage: ccmcollective.py <case-json>
       ccmcollective.py --batch <cases-file>

The case carries the config, the files that already exist, the paths whose read
or copy fails with something other than `ENOENT`, and any call that should
fail. The record is the log, the ordered list of things the module did, and
what each file ended up holding.

Nothing runs a command or touches a filesystem: `subp.subp`,
`util.load_binary_file`, `util.write_file`, `util.copy` and
`Distro.install_packages` are stubbed onto the module. `ConfigObj` is the
packaged `configobj` 5.0.9 and runs as written -- it is the whole point of the
comparison, since every byte of `/etc/mcollective/server.cfg` goes through it.

`util.write_file` encodes before it records, so a `public-cert:` that is not a
string raises without leaving a call behind; the port does the same. The bytes
`ConfigObj.write` produced are recorded decoded, which is safe because
`ConfigObj` encodes them as ASCII or raises.
"""

import errno
import json
import logging
import os
import sys

from cloudinit import distros
from cloudinit.config import cc_mcollective as m
from cloudinit.distros import PackageInstallerError
from cloudinit.log import loggers

loggers.define_extra_loggers()


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
    name = case.get("name", "mcollective")
    cfg = case.get("cfg") or {}
    host = case.get("host") or {}
    files = host.get("files") or {}
    failures = host.get("failures") or {}
    errors = host.get("errors") or {}

    calls = []
    written = []
    out = {}

    def record(call):
        calls.append(call)
        return call

    def raise_os(path):
        code = failures[path]
        raise OSError(code, os.strerror(code), path)

    def load_binary_file(path, quiet=False, **_kwargs):
        record("load_binary_file %s" % path)
        if path in failures:
            raise_os(path)
        if path in files:
            return files[path].encode("utf-8")
        raise FileNotFoundError(
            errno.ENOENT, os.strerror(errno.ENOENT), path
        )

    def write_file(path, content, mode=0o644, **_kwargs):
        content = m.util.decode_binary(m.util.encode_text(content))
        call = record("write_file %s mode=%04o" % (path, mode))
        written.append([path, content])
        if call in errors:
            raise OSError(errors[call])

    def copy(src, dest):
        record("copy %s %s" % (src, dest))
        if src in failures:
            raise_os(src)
        if src in files:
            return
        raise FileNotFoundError(errno.ENOENT, os.strerror(errno.ENOENT), src)

    def subp_stub(args, capture=True, **_kwargs):
        call = record("subp %s" % " ".join(args))
        if call in errors:
            raise m.subp.ProcessExecutionError(
                cmd=args, exit_code=1, stdout="", stderr=errors[call]
            )
        return m.subp.SubpResult("", "")

    def install_packages(packages):
        call = record("install_packages %r" % (packages,))
        if call in errors:
            raise PackageInstallerError(errors[call])

    cls = distros.fetch("ubuntu")
    distro = cls("ubuntu", {}, None)
    distro.install_packages = install_packages

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
        "load_binary_file": m.util.load_binary_file,
        "write_file": m.util.write_file,
        "copy": m.util.copy,
    }
    m.subp.subp = subp_stub
    m.util.load_binary_file = load_binary_file
    m.util.write_file = write_file
    m.util.copy = copy

    try:
        m.handle(name, dict(cfg), cloud, [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        m.subp.subp = saved["subp"]
        m.util.load_binary_file = saved["load_binary_file"]
        m.util.write_file = saved["write_file"]
        m.util.copy = saved["copy"]
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
        sys.stderr.write("usage: ccmcollective.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
