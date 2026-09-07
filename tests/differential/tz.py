"""Dump `cc_timezone.handle`'s decisions, for differential testing.

Usage: `tz.py <cfg-json> <distro> <zone-exists> <localtime> <systemd>`

Matches `dump-cc-timezone`. Every filesystem effect is stubbed and recorded
rather than carried out: running this for real would relink the
`/etc/localtime` of the machine doing the comparison.

The three facts the decision turns on are supplied as arguments and injected
through a shim `os` bound into `cloudinit.distros` alone -- not by patching
`os.path` globally, which would reach `json` and `logging` too. `os.path.join`
is passed straight through, because `_find_tz_file` needs it to do its real
job.

Not named after the module it imports.
"""

import json
import os as real_os
import sys
import types

from cloudinit import distros, util
from cloudinit.config import cc_timezone

CALLS = []


def shim_os(zone_exists, localtime):
    """`cloudinit.distros`' view of `os`, with the three predicates pinned."""

    def symlink(source, link):
        CALLS.append({"link": link, "op": "symlink", "source": source})

    path = types.SimpleNamespace(
        join=real_os.path.join,
        isfile=lambda p: zone_exists,
        islink=lambda p: localtime == "symlink",
        exists=lambda p: localtime != "absent",
    )
    return types.SimpleNamespace(path=path, symlink=symlink)


def install_stubs():
    def write_file(path, content, *args, **kwargs):
        CALLS.append({"content": content, "op": "write_file", "path": path})

    def del_file(path):
        CALLS.append({"op": "del_file", "path": path})

    def copy(src, dest):
        CALLS.append({"dest": dest, "op": "copy", "src": src})

    def sym_link(source, link, force=False):
        CALLS.append({"link": link, "op": "symlink", "source": source})

    # One module object shared by every `from cloudinit import util`, so this
    # reaches `distros/__init__.py`, `aosc.py`, `rhel.py` and `opensuse.py`
    # at once.
    util.write_file = write_file
    util.del_file = del_file
    util.copy = copy
    util.sym_link = sym_link


def main(argv):
    if argv[:1] == ["--batch"]:
        with open(argv[1]) as handle:
            for line in handle:
                line = line.rstrip("\n")
                if not line:
                    continue
                print("## " + line)
                emit(one(line.split("\t")))
        return 0
    if len(argv) < 5:
        sys.stderr.write(
            "usage: tz.py <cfg-json> <distro> <zone-exists> <localtime> "
            "<systemd> | --batch <cases>\n"
        )
        return 2
    emit(one(argv))
    return 0


def emit(record):
    print(
        json.dumps(
            record, indent=1, sort_keys=True, separators=(",", ": ")
        )
    )


def one(fields):
    cfg = json.loads(fields[0])
    name, zone_exists, localtime, systemd = (
        fields[1],
        fields[2] == "1",
        fields[3],
        fields[4] == "1",
    )

    del CALLS[:]
    try:
        distro = distros.fetch(name)(name, {}, None)
    except Exception:
        return {"error": "unknown distro"}
    distros.os = shim_os(zone_exists, localtime)
    distros.Distro.uses_systemd = lambda self: systemd
    install_stubs()

    cloud = types.SimpleNamespace(distro=distro)

    out = {}
    # `handle` is the only way to exercise the config extraction, and the
    # `timezone` it resolves is not otherwise visible, so `get_cfg_option_str`
    # is replayed here. Note the `str()`: a null, a zero and a false all come
    # back as non-empty strings and so are *not* skipped.
    if "timezone" not in cfg:
        resolved = False
    else:
        value = cfg["timezone"]
        resolved = value if isinstance(value, str) else str(value)
    out["tz"] = resolved if resolved else None

    try:
        cc_timezone.handle("timezone", cfg, cloud, [])
        out["calls"] = list(CALLS)
    except Exception as error:
        out["error"] = str(error)
    return out


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
