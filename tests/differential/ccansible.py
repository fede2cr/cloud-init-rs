"""Packaged `cc_ansible` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-ansible.rs`.

Usage: ccansible.py <case-json>
       ccansible.py --batch <cases-file>

The case carries the config and every answer the machine could give: what each
command prints, which calls fail, which programs `which` finds, whether
`import pip` works and whether the stdlib is marked externally managed. The
record is the log, the ordered list of things the module did, and what it
wrote to stdout.

Nothing runs a command. `AnsiblePull` and its two subclasses run as written;
only the leaves they reach -- `subp.subp`, `subp.which`, `distro.do_as`,
`distro.install_packages`, the `import pip` probe, the `EXTERNALLY-MANAGED`
check and `sys.stdout` -- are stubbed. `sys.executable` and `$HOME` are set
from the case so both sides name the same interpreter and home.

Every function under test is the packaged one; nothing is reimplemented.
"""

import importlib.abc
import importlib.util
import json
import logging
import os
import sys
import types

from cloudinit import distros
from cloudinit.config import cc_ansible as m
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


class DummyLoader(importlib.abc.Loader):
    """Enough of a loader to make `import pip` succeed without a `pip`."""

    def create_module(self, spec):
        return types.ModuleType(spec.name)

    def exec_module(self, module):
        pass


class PipFinder(importlib.abc.MetaPathFinder):
    """Answers the one `import pip` upstream does, and records that it was
    asked. Sitting first on `sys.meta_path` means the answer does not depend
    on whether the machine running the differential happens to have pip."""

    def __init__(self, record, present):
        self.record = record
        self.present = present

    def find_spec(self, fullname, path=None, target=None):
        if fullname != "pip":
            return None
        self.record("import pip")
        if not self.present:
            raise ModuleNotFoundError("No module named 'pip'", name="pip")
        return importlib.util.spec_from_loader("pip", DummyLoader())


class Console:
    """`sys.stdout` for the module, one entry per write."""

    def __init__(self, lines):
        self.lines = lines

    def write(self, text):
        self.lines.append(text)
        return len(text)

    def flush(self):
        pass


def run_case(case):
    cfg = case.get("cfg") or {}
    host = case.get("host") or {}
    failures = host.get("failures") or {}
    stdout = host.get("stdout") or {}
    present = host.get("present") or []
    has_pip = host.get("pip", True)
    managed = host.get("managed", False)
    python = case.get("python", "/usr/bin/python3")
    home = case.get("home", "/root")

    calls = []
    console = []
    out = {}

    def record(call):
        calls.append(call)
        return call

    def answer(call):
        if call in failures:
            raise m.subp.ProcessExecutionError(
                cmd=call, exit_code=1, stdout="", stderr=failures[call]
            )
        return m.subp.SubpResult(stdout.get(call, ""), "")

    def subp_stub(command, update_env=None, cwd=None, **_kwargs):
        env = ",".join(
            "%s=%s" % (key, value) for key, value in (update_env or {}).items()
        )
        call = record(
            "subp %s env=[%s] cwd=%s"
            % (" ".join(command), env, "" if cwd is None else cwd)
        )
        return answer(call)

    def which_stub(program):
        record("which %s" % program)
        return "/usr/bin/%s" % program if program in present else None

    def do_as(command, user, cwd=None, **_kwargs):
        call = record(
            "do_as %s %s cwd=%s"
            % (user, " ".join(command), "" if cwd is None else cwd)
        )
        return answer(call)

    def install_packages(packages):
        call = record("install_packages %r" % (packages,))
        if call in failures:
            raise RuntimeError(failures[call])

    real_exists = os.path.exists

    def exists_stub(path):
        if str(path).endswith("EXTERNALLY-MANAGED"):
            record("externally_managed")
            return managed
        return real_exists(path)

    cls = distros.fetch("ubuntu")
    distro = cls("ubuntu", {}, None)
    distro.do_as = do_as
    distro.install_packages = install_packages

    class FakeCloud:
        pass

    cloud = FakeCloud()
    cloud.distro = distro

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    finder = PipFinder(record, has_pip)
    had_pip = sys.modules.pop("pip", None)
    saved = {
        "subp": m.subp.subp,
        "which": m.subp.which,
        "exists": os.path.exists,
        "executable": sys.executable,
        "stdout": sys.stdout,
        "home": os.environ.get("HOME"),
    }
    m.subp.subp = subp_stub
    m.subp.which = which_stub
    os.path.exists = exists_stub
    sys.executable = python
    os.environ["HOME"] = home
    sys.meta_path.insert(0, finder)
    sys.stdout = Console(console)

    try:
        m.handle("ansible", dict(cfg), cloud, [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        sys.stdout = saved["stdout"]
        sys.meta_path.remove(finder)
        m.subp.subp = saved["subp"]
        m.subp.which = saved["which"]
        os.path.exists = saved["exists"]
        sys.executable = saved["executable"]
        if saved["home"] is None:
            os.environ.pop("HOME", None)
        else:
            os.environ["HOME"] = saved["home"]
        sys.modules.pop("pip", None)
        if had_pip is not None:
            sys.modules["pip"] = had_pip
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
        sys.stderr.write("usage: ccansible.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
