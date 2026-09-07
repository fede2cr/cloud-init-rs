"""Packaged `cc_mounts` decisions, for the differential harness.

Paired with `crates/ci-modules/examples/dump-cc-mounts.rs`.

Usage: ccmounts.py <root> <cfg-json> <transformer-json> <systemd> [env-json]
       ccmounts.py --batch <cases-file>

With four fields, covers the four passes `handle` makes over the `mounts`
config, which is where the device sanitising lives.

With a fifth field, runs the real `handle` end to end: the swap plan and the
exact fstab bytes as well. The field carries the host facts no rooted path can
supply -- `fstype`, `kernel_version`, `memtotal`, `available` -- which are
injected by stubbing `get_mount_info`, `kernel_version`, `read_meminfo` and
`os.statvfs`. Commands are recorded rather than run.

Every function under test is the packaged one; nothing is reimplemented.

Upstream probes absolute paths (`/dev/...`, `/sys/block/...`), so the fixture
tree is reached by prefixing `os.path.exists` and `os.path.realpath` inside the
module's own namespace.
"""

import json
import logging
import os
import sys

from cloudinit.config import cc_mounts as m

ROOT = ""

# Bound before anything is patched: the replacements below call these, and
# calling the patched names instead would recurse until the stack ran out.
REAL_EXISTS = os.path.exists
REAL_REALPATH = os.path.realpath
REAL_LEXISTS = os.path.lexists
REAL_STATVFS = os.statvfs


def rooted(path):
    # Idempotent: `FSTAB_PATH` is rewritten to a rooted path up front, and
    # `parse_fstab` then passes it back through `os.path.exists`.
    path = str(path)
    if ROOT and (path == ROOT or path.startswith(ROOT + "/")):
        return path
    return os.path.join(ROOT, path.lstrip("/"))


def patched_exists(path):
    return REAL_LEXISTS(rooted(path))


def patched_realpath(path):
    # Resolve inside the tree, then take the prefix back off so the caller
    # still sees an absolute-looking name.
    resolved = REAL_REALPATH(rooted(path))
    prefix = REAL_REALPATH(ROOT)
    if resolved.startswith(prefix):
        resolved = resolved[len(prefix):] or "/"
    return resolved


class Recorder(logging.Handler):
    def __init__(self):
        super().__init__()
        self.lines = []

    def emit(self, record):
        self.lines.append(f"{record.levelname} {record.getMessage()}")


class FakeCloud:
    """`device_name_to_device` and, in handle mode, `distro.uses_systemd`."""

    class Distro:
        def __init__(self, systemd):
            self.systemd = systemd

        def uses_systemd(self):
            return self.systemd

    def __init__(self, mapping, systemd=False):
        self.mapping = mapping
        self.distro = self.Distro(systemd)

    def device_name_to_device(self, name):
        return self.mapping.get(name)


def one(fields):
    global ROOT
    root, cfg_json, transformer_json, systemd = (fields + ["", "", "", ""])[:4]
    if len(fields) > 4 and fields[4]:
        return one_handle(fields)
    ROOT = root

    try:
        cfg = json.loads(cfg_json)
    except Exception:
        cfg = None
    if not isinstance(cfg, dict):
        return {"error": "<cfg-json> must be an object"}

    try:
        transformer = json.loads(transformer_json)
    except Exception:
        transformer = {}
    if not isinstance(transformer, dict):
        transformer = {}

    default_mount_options = (
        "defaults,nofail,x-systemd.after=cloud-init-network.service,_netdev"
        if systemd == "1"
        else "defaults,nobootwait"
    )
    hardcoded = [None, None, "auto", default_mount_options, "0", "2"]
    default_fields = cfg.get("mount_default_fields", hardcoded)
    mounts = cfg.get("mounts", [])
    device_aliases = cfg.get("device_aliases", {})
    cloud = FakeCloud(transformer)

    recorder = Recorder()
    m.LOG.addHandler(recorder)
    m.LOG.setLevel(logging.DEBUG)
    m.LOG.propagate = False

    real_exists = os.path.exists
    real_realpath = os.path.realpath
    real_fstab = m.FSTAB_PATH
    os.path.exists = patched_exists
    os.path.realpath = patched_realpath
    m.FSTAB_PATH = rooted("/etc/fstab")
    out = {}
    try:
        m.LOG.debug("mounts configuration is %s", mounts)
        _lines, fstab_devs, _removed = m.parse_fstab()
        try:
            updated = m.sanitize_mounts_configuration(
                mounts, fstab_devs, device_aliases, default_fields, cloud
            )
            updated = m.add_default_mounts_to_cfg(
                updated,
                default_mount_options,
                fstab_devs,
                device_aliases,
                cloud,
            )
            updated = m.remove_nonexistent_devices(updated)
            out["mounts"] = m.add_comment(updated)
        except Exception as error:
            out["error"] = str(error)
        out["log"] = recorder.lines
        return out
    finally:
        os.path.exists = real_exists
        os.path.realpath = real_realpath
        m.FSTAB_PATH = real_fstab
        m.LOG.removeHandler(recorder)


def one_handle(fields):
    """Run the real `handle`, recording what it would have carried out."""
    global ROOT
    root, cfg_json, transformer_json, systemd, env_json = (
        fields + ["", "", "", "", ""]
    )[:5]
    ROOT = root

    try:
        cfg = json.loads(cfg_json)
    except Exception:
        cfg = None
    if not isinstance(cfg, dict):
        return {"error": "<cfg-json> must be an object"}

    try:
        transformer = json.loads(transformer_json)
    except Exception:
        transformer = {}
    if not isinstance(transformer, dict):
        transformer = {}

    env = json.loads(env_json)
    cloud = FakeCloud(transformer, systemd == "1")

    calls = []
    written = [None]

    def ensure_dir(path, *_args, **_kwargs):
        calls.append(f"ensure-dir {path}")

    def write_file(path, contents, *_args, **_kwargs):
        calls.append("write-fstab")
        written[0] = contents

    def chmod(path, mode):
        calls.append(f"chmod {mode:o} {path}")

    def del_file(path):
        calls.append(f"del-file {path}")

    def get_mount_info(path, *_args, **_kwargs):
        # Upstream subscripts this straight away, so `None` has to stay `None`
        # for the resulting TypeError to be the one being compared.
        if env.get("fstype") is None:
            return None
        return (None, env["fstype"], None)

    def read_meminfo(*_args, **_kwargs):
        if env.get("memtotal") is None:
            raise IOError("stubbed meminfo read failure")
        return {"total": env["memtotal"]}

    def statvfs(path):
        if env.get("available") is None:
            raise OSError("stubbed statvfs failure")

        class Result:
            f_frsize = 1
            f_bfree = env["available"]

        return Result()

    def load_text_file(path, *_args, **_kwargs):
        with open(rooted(path)) as handle:
            return handle.read()

    def run(cmd, *_args, **_kwargs):
        calls.append(" ".join(str(token) for token in cmd))
        return ("", "")

    def mount_if_needed(uses_systemd, changes_made, dirs):
        # Recorded as its inputs: whether it fires turns on the live mount
        # table, which the Rust side cannot inject either.
        calls.append(
            "mount-if-needed reload=%d changes=%d dirs=%s"
            % (int(uses_systemd), int(changes_made), ",".join(dirs))
        )

    recorder = Recorder()
    m.LOG.addHandler(recorder)
    m.LOG.setLevel(logging.DEBUG)
    m.LOG.propagate = False

    saved = {
        "exists": os.path.exists,
        "realpath": os.path.realpath,
        "statvfs": os.statvfs,
        "fstab": m.FSTAB_PATH,
        "mount_if_needed": m.mount_if_needed,
        "subp": m.subp.subp,
    }
    saved_util = {
        name: getattr(m.util, name)
        for name in (
            "ensure_dir",
            "write_file",
            "chmod",
            "del_file",
            "get_mount_info",
            "read_meminfo",
            "load_text_file",
            "kernel_version",
        )
    }

    os.path.exists = patched_exists
    os.path.realpath = patched_realpath
    os.statvfs = statvfs
    m.FSTAB_PATH = rooted("/etc/fstab")
    m.mount_if_needed = mount_if_needed
    m.subp.subp = run
    m.util.ensure_dir = ensure_dir
    m.util.write_file = write_file
    m.util.chmod = chmod
    m.util.del_file = del_file
    m.util.get_mount_info = get_mount_info
    m.util.read_meminfo = read_meminfo
    m.util.load_text_file = load_text_file
    m.util.kernel_version = lambda: tuple(env.get("kernel_version", []))

    out = {}
    try:
        m.handle("cc_mounts", cfg, cloud, [])
    except Exception as error:
        out["error"] = str(error)
    finally:
        os.path.exists = saved["exists"]
        os.path.realpath = saved["realpath"]
        os.statvfs = saved["statvfs"]
        m.FSTAB_PATH = saved["fstab"]
        m.mount_if_needed = saved["mount_if_needed"]
        m.subp.subp = saved["subp"]
        for name, value in saved_util.items():
            setattr(m.util, name, value)
        m.LOG.removeHandler(recorder)

    out["steps"] = calls
    out["fstab"] = written[0]
    out["log"] = recorder.lines
    return out


def dump(record):
    print(
        json.dumps(
            record,
            indent=1,
            sort_keys=True,
            separators=(",", ": "),
        )
    )


def main(argv):
    if len(argv) > 2 and argv[1] == "--batch":
        with open(argv[2]) as handle:
            for line in handle.read().splitlines():
                if not line:
                    continue
                print(f"## {line}")
                dump(one(line.split("\t")))
        return 0
    if len(argv) < 5:
        sys.stderr.write(
            "usage: ccmounts.py <root> <cfg-json> <transformer-json>"
            " <systemd> [env-json]\n"
        )
        return 2
    dump(one(list(argv[1:6])))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
