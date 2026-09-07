"""Packaged `cc_rsyslog` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-rsyslog.rs`.

Usage: ccrsyslog.py <case-json>
       ccrsyslog.py --batch <cases-file>

The case carries the config, the `system_info` block the distro was built with,
and every answer the machine could give: which programs `which` finds, which
calls fail and how, and which paths refuse to be written. The record is the
log, the ordered list of things the module did, and what each file ended up
holding.

Nothing runs a command or touches a filesystem: `subp.which`, `subp.subp`,
`util.write_file`, the two distro methods and the two logging ones are stubbed
onto the script. `cc_rsyslog` interleaves deciding and doing -- whether the
package is installed depends on `which`, whether the last line is logged
depends on whether the reload worked -- which is why this scripts a machine
instead of feeding it a plan.

Every function under test is the packaged one; nothing is reimplemented.
"""

import json
import logging
import sys

from cloudinit import distros, lifecycle
from cloudinit.config import cc_rsyslog as m
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


def error_for(spec, command):
    """The `ProcessExecutionError` a scripted failure raises.

    `command` is what the case says `str(cmd)` should be, because that is the
    only part of the message the caller cannot reconstruct.
    """
    return m.subp.ProcessExecutionError(
        cmd=spec.get("command", command),
        exit_code=spec.get("exit_code", 1),
        stdout=spec.get("stdout", ""),
        stderr=spec.get("stderr", ""),
    )


def run_case(case):
    name = case.get("name", "rsyslog")
    cfg = case.get("cfg") or {}
    system_info = case.get("system_info") or {}
    host = case.get("host") or {}
    present = host.get("present") or []
    failures = host.get("failures") or {}
    unwritable = host.get("unwritable") or {}

    calls = []
    written = []
    out = {}

    # `lifecycle.deprecate` remembers what it has already said for the life of
    # the process, so in a batch the same warning would be logged once. Each
    # case is meant to stand alone.
    if hasattr(lifecycle.deprecate, "log"):
        lifecycle.deprecate.log.clear()

    def which_stub(program):
        calls.append(f"which {program}")
        return "/usr/sbin/%s" % program if program in present else None

    def install_packages(packages):
        call = "install_packages %r" % (packages,)
        calls.append(call)
        if call in failures:
            raise error_for(failures[call], call)

    def manage_service(action, service, *extra, **_kwargs):
        call = "manage_service %s %s" % (action, service)
        calls.append(call)
        if call in failures:
            raise error_for(failures[call], call)
        return m.subp.SubpResult("", "")

    def subp_stub(args, **_kwargs):
        call = "subp %s" % (args,)
        calls.append(call)
        if call in failures:
            raise error_for(failures[call], "%s" % (args,))
        return m.subp.SubpResult("", "")

    def write_file(path, content, omode="wb", **_kwargs):
        calls.append("write_file %s %s" % (path, omode))
        if path in unwritable:
            raise OSError(unwritable[path])
        for slot in written:
            if slot[0] == path:
                slot[1] = slot[1] + content if omode == "ab" else content
                return
        written.append([path, content])

    def reset_logging():
        calls.append("reset_logging")

    cls = distros.fetch("ubuntu")
    distro = cls("ubuntu", dict(system_info), None)
    distro.install_packages = install_packages
    distro.manage_service = manage_service

    class FakeCloud:
        pass

    cloud = FakeCloud()
    cloud.distro = distro
    cloud.cfg = cfg

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved = {
        "which": m.subp.which,
        "subp": m.subp.subp,
        "write_file": m.util.write_file,
        "reset_logging": m.loggers.reset_logging,
        "setup_logging": m.loggers.setup_logging,
    }
    m.subp.which = which_stub
    m.subp.subp = subp_stub
    m.util.write_file = write_file
    m.loggers.reset_logging = reset_logging
    m.loggers.setup_logging = lambda _cfg: None

    try:
        m.handle(name, dict(cfg), cloud, [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        m.subp.which = saved["which"]
        m.subp.subp = saved["subp"]
        m.util.write_file = saved["write_file"]
        m.loggers.reset_logging = saved["reset_logging"]
        m.loggers.setup_logging = saved["setup_logging"]
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
        sys.stderr.write("usage: ccrsyslog.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
