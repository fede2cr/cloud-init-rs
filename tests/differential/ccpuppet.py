"""Packaged `cc_puppet` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-puppet.rs`.

Usage: ccpuppet.py <case-json>
       ccpuppet.py --batch <cases-file>

The case carries the config and every answer the machine could give: what each
command prints, which calls fail, which files already exist and what they hold,
and the three fixed answers (`socket.getfqdn`, the instance id, the temp
directory). The record is the log, the ordered list of things the module did,
and what each file ended up holding.

Nothing runs a command or touches a filesystem. The module's own
`get_config_value`, `install_puppet_aio` and `_manage_puppet_services` all run
as written; only the leaves they reach -- `subp.subp`, the `util` file
helpers, `url_helper.readurl`, `temp_utils.tempdir`, `socket.getfqdn` and the
two `Cloud`/`Distro` methods -- are stubbed onto the module.

Every function under test is the packaged one; nothing is reimplemented.
"""

import contextlib
import json
import logging
import sys

from cloudinit import distros
from cloudinit.config import cc_puppet as m
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
    name = case.get("name", "puppet")
    cfg = case.get("cfg") or {}
    host = case.get("host") or {}
    failures = host.get("failures") or {}
    stdout = host.get("stdout") or {}
    files = dict(host.get("files") or {})
    fqdn = host.get("fqdn", "host.example.com")
    iid = host.get("iid", "i-abcdef")
    tmpdir = host.get("tmpdir", "/var/tmp/cloud-init/tmpdir")

    calls = []
    written = []
    out = {}

    def record(call):
        calls.append(call)
        return call

    def fail(call):
        """The scripted failure for a call, as the exception upstream sees."""
        if call not in failures:
            return None
        return m.subp.ProcessExecutionError(
            cmd=call, exit_code=1, stdout="", stderr=failures[call]
        )

    def note(path, content):
        files[path] = content
        for slot in written:
            if slot[0] == path:
                slot[1] = content
                return
        written.append([path, content])

    def subp_stub(args, capture=True, **_kwargs):
        call = record("subp %s capture=%s" % (" ".join(args), capture))
        error = fail(call)
        if error:
            raise error
        return m.subp.SubpResult(stdout.get(call, ""), "")

    def install_packages(packages):
        call = record("install_packages %r" % (packages,))
        if call in failures:
            raise PackageInstallerError(failures[call])

    def manage_service(action, service, *_extra, **_kwargs):
        call = record("manage_service %s %s" % (action, service))
        error = fail(call)
        if error:
            raise error
        return m.subp.SubpResult("", "")

    def load_text_file(path, **_kwargs):
        call = record("load_text_file %s" % path)
        if call in failures:
            raise OSError(failures[call])
        if path not in files:
            raise FileNotFoundError(
                2, "No such file or directory", path
            )
        return files[path]

    def write_file(path, content, mode=None, **_kwargs):
        if mode is None:
            call = record("write_file %s" % path)
        else:
            call = record("write_file %s mode=%04o" % (path, mode))
        if call in failures:
            raise OSError(failures[call])
        note(path, content)

    def ensure_dir(path, mode=None):
        if mode is None:
            call = record("ensure_dir %s" % path)
        else:
            call = record("ensure_dir %s mode=%04o" % (path, mode))
        if call in failures:
            raise OSError(failures[call])

    def chownbyname(path, user=None, group=None):
        call = record("chownbyname %s %s %s" % (path, user, group))
        if call in failures:
            raise OSError(failures[call])

    def rename(src, dst):
        call = record("rename %s %s" % (src, dst))
        if call in failures:
            raise OSError(failures[call])
        if src in files:
            note(dst, files[src])

    def readurl(url=None, **_kwargs):
        call = record("readurl %s" % url)
        if call in failures:
            raise OSError(failures[call])

        class Response:
            contents = stdout.get(call, "")

        return Response()

    @contextlib.contextmanager
    def tempdir_stub(**_kwargs):
        record("tempdir")
        if "tempdir" in failures:
            raise OSError(failures["tempdir"])
        yield tmpdir

    def getfqdn(*_args):
        record("getfqdn")
        return fqdn

    cls = distros.fetch("ubuntu")
    distro = cls("ubuntu", {}, None)
    distro.install_packages = install_packages
    distro.manage_service = manage_service
    distro.get_tmp_exec_path = lambda: "/var/tmp"

    class FakeCloud:
        def get_instance_id(self):
            record("get_instance_id")
            return iid

    cloud = FakeCloud()
    cloud.distro = distro

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved = {
        "subp": m.subp.subp,
        "load_text_file": m.util.load_text_file,
        "write_file": m.util.write_file,
        "ensure_dir": m.util.ensure_dir,
        "chownbyname": m.util.chownbyname,
        "rename": m.util.rename,
        "readurl": m.url_helper.readurl,
        "tempdir": m.temp_utils.tempdir,
        "getfqdn": m.socket.getfqdn,
    }
    m.subp.subp = subp_stub
    m.util.load_text_file = load_text_file
    m.util.write_file = write_file
    m.util.ensure_dir = ensure_dir
    m.util.chownbyname = chownbyname
    m.util.rename = rename
    m.url_helper.readurl = readurl
    m.temp_utils.tempdir = tempdir_stub
    m.socket.getfqdn = getfqdn

    try:
        m.handle(name, dict(cfg), cloud, [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        m.subp.subp = saved["subp"]
        m.util.load_text_file = saved["load_text_file"]
        m.util.write_file = saved["write_file"]
        m.util.ensure_dir = saved["ensure_dir"]
        m.util.chownbyname = saved["chownbyname"]
        m.util.rename = saved["rename"]
        m.url_helper.readurl = saved["readurl"]
        m.temp_utils.tempdir = saved["tempdir"]
        m.socket.getfqdn = saved["getfqdn"]
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
        sys.stderr.write("usage: ccpuppet.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


if __name__ == "__main__":
    main()
