"""Packaged `cc_chef` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-chef.rs`.

Usage: ccchef.py <case-json>
       ccchef.py --batch <cases-file>

The case carries the config, the files and directories that already exist, the
the paths `subp.is_exe` answers yes for, what each URL serves, the directory
`cloud.get_template_filename` looks in, and any call that should fail. The
record is the log, the ordered list of things the module did, and what each
file ended up holding.

Nothing runs a command or touches a filesystem: `os`, `shutil.move`,
`subp.subp`, `subp.is_exe`, `url_helper.readurl`, `temp_utils.tempdir` and the
`util` helpers are stubbed onto the module, and `Distro.install_packages` onto
a real distro object. `templater.render_from_file` is *not* stubbed -- the real
jinja renderer runs over the real `chef_client.rb.tmpl`, which is the point of
the comparison.

`util.make_header()` embeds the current time, so it is patched to a constant
that the Rust fixture answers with too.
"""

import json
import logging
import os
import posixpath
import sys
import types

from cloudinit import distros
from cloudinit.cloud import Cloud
from cloudinit.config import cc_chef as m
from cloudinit.distros import PackageInstallerError
from cloudinit.log import loggers

loggers.define_extra_loggers()

REAL_OS = os


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
    name = case.get("name", "chef")
    cfg = case.get("cfg") or {}
    host = case.get("host") or {}
    files = dict(host.get("files") or {})
    dirs = host.get("dirs") or {}
    exes = host.get("exes") or []
    urls = host.get("urls") or {}
    errors = host.get("errors") or {}
    templates_dir = host.get("templates_dir", "/etc/cloud/templates")
    instance_id = host.get("instance_id", "i-testing")
    header = host.get("header", "# generated")
    tmpdir = host.get("tmpdir", "/tmp/tmpdir")

    calls = []
    written = []
    out = {}

    def record(call):
        calls.append(call)
        return call

    def check(call):
        if call in errors:
            raise RuntimeError(errors[call])

    def ensure_dir(path, mode=None, user=None, group=None):
        check(record("ensure_dir %s" % (path,)))

    def ensure_dirs(dirlist, mode=0o755):
        check(record("ensure_dirs %r" % (sorted(dirlist),)))

    def exists(path):
        record("exists %s" % (path,))
        return path in files or path in dirs

    def isfile(path):
        record("isfile %s" % (path,))
        return path in files

    def listdir(path):
        check(record("listdir %s" % (path,)))
        return list(dirs.get(path, []))

    def move(src, dest):
        check(record("move %s %s" % (src, dest)))

    def write_file(filename, content, mode=0o644, **_kwargs):
        content = m.util.decode_binary(m.util.encode_text(content))
        call = record("write_file %s mode=%04o" % (filename, mode))
        written.append([str(filename), content])
        check(call)

    def unlink(path):
        check(record("unlink %s" % (path,)))

    def load_text_file(path, **_kwargs):
        check(record("load_text_file %s" % (path,)))
        if path in files:
            return files[path]
        with open(path) as handle:
            return handle.read()

    def make_header(comment_char="#", base="created"):
        record("make_header")
        return header

    def is_exe(fpath):
        record("is_exe %s" % (fpath,))
        return fpath in exes

    def install_packages(packages):
        call = record("install_packages %r" % (list(packages),))
        if call in errors:
            raise PackageInstallerError(errors[call])

    def subp_stub(args, capture=True, **_kwargs):
        call = record("subp %s" % " ".join(str(a) for a in args))
        if call in errors:
            raise m.subp.ProcessExecutionError(
                cmd=args, exit_code=1, stdout="", stderr=errors[call]
            )
        return m.subp.SubpResult("", "")

    def readurl(url=None, retries=None, **_kwargs):
        call = record("readurl %s retries=%s" % (url, retries))
        check(call)
        if url not in urls:
            raise RuntimeError("Unable to read %s" % url)
        return types.SimpleNamespace(contents=urls[url].encode("utf-8"))

    class FakeTempdir:
        def __enter__(self):
            check(record("tempdir"))
            return tmpdir

        def __exit__(self, *_exc):
            return False

    def tempdir(**_kwargs):
        return FakeTempdir()

    def sym_link(source, link, force=False):
        check(record("sym_link %s %s" % (source, link)))

    fake_path = types.SimpleNamespace(
        exists=exists,
        isfile=isfile,
        join=posixpath.join,
        dirname=posixpath.dirname,
    )
    fake_os = types.SimpleNamespace(
        path=fake_path, listdir=listdir, unlink=unlink
    )

    cls = distros.fetch("ubuntu")
    distro = cls("ubuntu", {}, None)
    distro.install_packages = install_packages
    distro.get_tmp_exec_path = lambda: "/var/tmp/cloud-init"

    def get_instance_id():
        record("get_instance_id")
        return instance_id

    # The real `Cloud.get_template_filename`, so its warning is compared too.
    # It uses the real `os`, not the shim, and so really looks at the disk.
    cloud = Cloud.__new__(Cloud)
    cloud.paths = types.SimpleNamespace(
        template_tpl=REAL_OS.path.join(templates_dir, "%s.tmpl")
    )
    cloud.distro = distro
    cloud.datasource = types.SimpleNamespace(get_instance_id=get_instance_id)

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)

    saved = {
        "os": m.os,
        "shutil_move": m.shutil.move,
        "subp": m.subp.subp,
        "is_exe": m.subp.is_exe,
        "readurl": m.url_helper.readurl,
        "tempdir": m.temp_utils.tempdir,
        "ensure_dir": m.util.ensure_dir,
        "ensure_dirs": m.util.ensure_dirs,
        "write_file": m.util.write_file,
        "load_text_file": m.util.load_text_file,
        "make_header": m.util.make_header,
        "sym_link": m.util.sym_link,
    }
    m.os = fake_os
    m.shutil.move = move
    m.subp.subp = subp_stub
    m.subp.is_exe = is_exe
    m.url_helper.readurl = readurl
    m.temp_utils.tempdir = tempdir
    m.util.ensure_dir = ensure_dir
    m.util.ensure_dirs = ensure_dirs
    m.util.write_file = write_file
    m.util.load_text_file = load_text_file
    m.util.make_header = make_header
    m.util.sym_link = sym_link

    try:
        m.handle(name, cfg, cloud, [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        m.os = saved["os"]
        m.shutil.move = saved["shutil_move"]
        m.subp.subp = saved["subp"]
        m.subp.is_exe = saved["is_exe"]
        m.url_helper.readurl = saved["readurl"]
        m.temp_utils.tempdir = saved["tempdir"]
        m.util.ensure_dir = saved["ensure_dir"]
        m.util.ensure_dirs = saved["ensure_dirs"]
        m.util.write_file = saved["write_file"]
        m.util.load_text_file = saved["load_text_file"]
        m.util.make_header = saved["make_header"]
        m.util.sym_link = saved["sym_link"]
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
        return 0
    if not argv:
        print("usage: ccchef.py <case-json>", file=sys.stderr)
        return 2
    emit(argv[0])
    return 0


if __name__ == "__main__":
    sys.exit(main())
