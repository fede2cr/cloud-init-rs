"""Packaged `cc_growpart` against a scripted machine, for the differential.

Paired with `crates/ci-modules/examples/dump-cc-growpart.rs`.

Usage: ccgrowpart.py <case-json>
       ccgrowpart.py --batch <cases-file>

The case carries the config and every answer the module could get from a
machine: command output and exit codes, which paths exist, what `stat` and
`lseek` say, what is mounted where. The record is the log, the ordered list of
questions the module asked, and the `(device, action, message)` triples it
ended with.

Nothing runs a command or touches a device: `subp.subp` and the `os` probes are
stubbed onto the script, so the comparison is of decisions and their order.
`cc_growpart` interleaves the two -- `growpart --dry-run` exiting 1 *is* the
decision that nothing needs doing -- which is why this harness scripts a
machine instead of feeding a plan.

Every function under test is the packaged one; nothing is reimplemented.
"""

import contextlib
import json
import logging
import os
import sys

from cloudinit import lifecycle
from cloudinit.config import cc_growpart as m
from cloudinit.distros import Distro
from cloudinit.log import loggers

# `lifecycle.deprecate` picks its level from whether this has run, which it
# has by the time a module is handled.
loggers.define_extra_loggers()

# Bound before anything is patched, so a replacement can still fall back.
REAL = {
    "exists": os.path.exists,
    "isfile": os.path.isfile,
    "realpath": os.path.realpath,
    "stat": os.stat,
    "open": os.open,
    "lseek": os.lseek,
    "close": os.close,
    "mkdir": os.mkdir,
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
    out = {}

    commands = host.get("commands") or {}
    which = host.get("which") or []
    exists = host.get("exists") or []
    files = host.get("files") or []
    realpaths = host.get("realpath") or {}
    stats = host.get("stat") or {}
    sizes = host.get("sizes") or {}
    texts = host.get("text") or {}
    mounts = host.get("mounts") or {}
    devs = host.get("devs") or {}

    def missing(path):
        return FileNotFoundError(2, "No such file or directory", path)

    def subp_stub(args, data=None, update_env=None, rcs=None, **_kwargs):
        argv = [str(token) for token in args]
        key = " ".join(argv)
        env = ",".join(f"{k}={v}" for k, v in (update_env or {}).items())
        calls.append(
            "subp %s env=%s data=%d"
            % (key, env, -1 if data is None else len(data))
        )
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
        if code == 0 or (rcs and code in rcs):
            return m.subp.SubpResult(stdout, stderr)
        raise m.subp.ProcessExecutionError(
            stdout=stdout, stderr=stderr, exit_code=code, cmd=list(args)
        )

    def which_stub(program, **_kwargs):
        calls.append(f"which {program}")
        return f"/usr/bin/{program}" if program in which else None

    def exists_stub(path):
        calls.append(f"exists {path}")
        # `os.path.exists` swallows OSError and ValueError but not the
        # TypeError `os.stat` raises for something that is not a path.
        if not isinstance(path, (str, bytes, int)):
            raise TypeError(
                "stat: path should be string, bytes, os.PathLike or "
                "integer, not %s" % type(path).__name__
            )
        return str(path) in exists

    def isfile_stub(path):
        calls.append(f"isfile {path}")
        return str(path) in files

    def realpath_stub(path):
        calls.append(f"realpath {path}")
        return realpaths.get(str(path), str(path))

    def stat_stub(path, **_kwargs):
        calls.append(f"stat {path}")
        mode = stats.get(str(path))
        if mode is None:
            raise missing(str(path))
        return os.stat_result((int(mode, 8), 0, 0, 0, 0, 0, 0, 0, 0, 0))

    open_sizes = {}

    def open_stub(path, *_args, **_kwargs):
        calls.append(f"size {path}")
        size = sizes.get(str(path))
        if size is None:
            raise missing(str(path))
        # A list is one entry per read, so a device can be bigger the second
        # time `get_size` asks; a null is a read that finds nothing, and the
        # last value repeats.
        if isinstance(size, list):
            if not size:
                raise missing(str(path))
            value = size[0]
            if len(size) > 1:
                del size[0]
            if value is None:
                raise missing(str(path))
            size = value
        fd = 900 + len(open_sizes)
        open_sizes[fd] = size
        return fd

    def lseek_stub(fd, offset, whence):
        return open_sizes.get(fd, 0)

    def close_stub(fd):
        open_sizes.pop(fd, None)

    def mkdir_stub(path, *_args, **_kwargs):
        calls.append(f"mkdir {path}")

    def load_text_file(path, *_args, **_kwargs):
        calls.append(f"read {path}")
        text = texts.get(str(path))
        if text is None:
            raise missing(str(path))
        return text

    def get_mount_info(path, *_args, **_kwargs):
        calls.append(f"mount_info {path}")
        found = mounts.get(str(path))
        return tuple(found) if found else None

    def is_container():
        calls.append("is_container")
        return bool(host.get("container", False))

    def get_cmdline():
        calls.append("cmdline")
        return host.get("cmdline", "")

    def find_devs_with(criteria=None, **_kwargs):
        calls.append(f"find_devs_with {criteria}")
        return list(devs.get(str(criteria), []))

    @contextlib.contextmanager
    def tempdir_stub(dir=None, **_kwargs):
        calls.append(f"mkdtemp {dir}")
        tdir = host.get("tmpdir", "/var/tmp/cloud-init/tmpfixture")
        try:
            yield tdir
        finally:
            calls.append(f"rmtree {tdir}")

    class FakeKeydata:
        """`KEYDATA_PATH`, which is a `pathlib.Path` upstream."""

        def __init__(self):
            self.text = host.get("keydata")

        def __str__(self):
            return "/cc_growpart_keydata"

        def exists(self):
            calls.append("read_keydata")
            return self.text is not None

        @contextlib.contextmanager
        def open(self):
            import io

            yield io.StringIO(self.text or "")

        def unlink(self):
            calls.append("unlink_keydata")
            self.text = None

    class FakeDistro:
        """Only the four things `cc_growpart` reaches for; the two static
        helpers are the packaged ones."""

        def get_tmp_exec_path(self):
            calls.append("tmp_exec_path")
            return host.get("tmp_exec", "/var/tmp/cloud-init")

        @staticmethod
        def get_mapped_device(blockdev):
            return Distro.get_mapped_device(blockdev)

        @staticmethod
        def device_part_info(devpath):
            return Distro.device_part_info(devpath)

        def manage_service(self, action, service, *_args, **_kwargs):
            calls.append(f"manage_service {action} {service}")

    class FakeCloud:
        def __init__(self):
            self.distro = FakeDistro()

    resized = []
    real_resize_devices = m.resize_devices

    def resize_devices(resizer, devices, distro):
        found = real_resize_devices(resizer, devices, distro)
        resized.extend(found)
        return found

    recorder = Recorder()
    root = logging.getLogger()
    root.addHandler(recorder)
    root.setLevel(logging.DEBUG)
    # Deprecations are deduplicated for the life of the process; each case is
    # a separate boot.
    if hasattr(lifecycle.deprecate, "log"):
        lifecycle.deprecate.log.clear()

    saved_os = {name: getattr(os, name) for name in ("stat", "open", "lseek", "close", "mkdir")}
    saved_path = {name: getattr(os.path, name) for name in ("exists", "isfile", "realpath")}
    saved_util = {
        name: getattr(m.util, name)
        for name in (
            "load_text_file",
            "get_mount_info",
            "is_container",
            "get_cmdline",
            "find_devs_with",
        )
    }
    saved = {
        "subp": m.subp.subp,
        "which": m.subp.which,
        "tempdir": m.temp_utils.tempdir,
        "keydata": m.KEYDATA_PATH,
        "resize_devices": m.resize_devices,
    }

    os.stat = stat_stub
    os.open = open_stub
    os.lseek = lseek_stub
    os.close = close_stub
    os.mkdir = mkdir_stub
    os.path.exists = exists_stub
    os.path.isfile = isfile_stub
    os.path.realpath = realpath_stub
    m.util.load_text_file = load_text_file
    m.util.get_mount_info = get_mount_info
    m.util.is_container = is_container
    m.util.get_cmdline = get_cmdline
    m.util.find_devs_with = find_devs_with
    m.subp.subp = subp_stub
    m.subp.which = which_stub
    m.temp_utils.tempdir = tempdir_stub
    m.KEYDATA_PATH = FakeKeydata()
    m.resize_devices = resize_devices

    try:
        m.handle("growpart", cfg, FakeCloud(), [])
    except Exception as error:  # noqa: BLE001 - upstream lets these escape
        out["error"] = str(error)
    finally:
        for name, value in saved_os.items():
            setattr(os, name, value)
        for name, value in saved_path.items():
            setattr(os.path, name, value)
        for name, value in saved_util.items():
            setattr(m.util, name, value)
        m.subp.subp = saved["subp"]
        m.subp.which = saved["which"]
        m.temp_utils.tempdir = saved["tempdir"]
        m.KEYDATA_PATH = saved["keydata"]
        m.resize_devices = saved["resize_devices"]
        root.removeHandler(recorder)

    out["log"] = recorder.lines
    out["calls"] = calls
    if "error" not in out:
        out["resized"] = [list(row) for row in resized]
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
        sys.stderr.write("usage: ccgrowpart.py <case-json>\n")
        raise SystemExit(2)
    emit(argv[0])


def emit(text):
    try:
        case = json.loads(text)
    except ValueError:
        case = None
    if not isinstance(case, dict):
        print(json.dumps({"error": "<case-json> must be an object"}, indent=1, sort_keys=True))
        return
    print(json.dumps(run_case(case), indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
