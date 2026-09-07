#!/usr/bin/env python3
"""Reference side of the ssh_util differential.

Mirrors crates/ci-ssh/examples/dump-sshutil.rs. The first four modes are pure.

The fifth, "install", runs the real filesystem half against a fixture tree. It
does that by patching exactly two things: the prefix every absolute path is
resolved against, and the passwd/group databases. Every decision -- which
candidate wins, which mode a new directory gets, which permission check fails
-- is still upstream's own code, which is the whole point.
"""
import json
import os
import stat as statmod
import sys

from cloudinit import ssh_util


def unescape(s):
    return s.replace("\\n", "\n").replace("\\t", "\t")


def line_json(line):
    return {
        "source": line.source,
        "keytype": line.keytype or "",
        "base64": line.base64 or "",
        "comment": line.comment or "",
        "options": line.options or "",
        "valid": bool(line.valid()),
        "str": str(line),
    }


def tree(root):
    """Everything under root except the fixture's own identity databases."""
    out = []

    def walk(directory):
        try:
            names = sorted(os.listdir(directory))
        except OSError:
            # A mode-000 directory is one of the things being tested; the Rust
            # side's read_dir gives up on it in the same silent way.
            return
        for name in names:
            path = os.path.join(directory, name)
            logical = "/" + os.path.relpath(path, root)
            if logical.startswith("/etc"):
                continue
            st = os.lstat(path)
            if statmod.S_ISLNK(st.st_mode):
                kind, mode = "link", ""
            elif statmod.S_ISDIR(st.st_mode):
                kind, mode = "dir", "%o" % statmod.S_IMODE(st.st_mode)
            else:
                kind, mode = "file", "%o" % statmod.S_IMODE(st.st_mode)
            content = ""
            if kind == "file":
                try:
                    with open(path, "rb") as fh:
                        content = fh.read().decode("utf-8", "replace")
                except OSError:
                    # As with listdir above: a mode-000 file is a case, and the
                    # Rust side reports it as empty rather than failing.
                    pass
            out.append(
                {
                    "content": content,
                    "kind": kind,
                    "mode": mode,
                    "path": logical,
                }
            )
            if kind == "dir":
                walk(path)

    walk(root)
    return out


def reroot(root):
    """Point ssh_util's world at the fixture.

    Every replacement is idempotent -- a path that already lives under root is
    returned unchanged -- because CPython's own os.makedirs and cloud-init's
    write_file call back into the functions being patched with paths that have
    already been rewritten once.
    """
    import contextlib
    import grp
    import pwd

    from cloudinit import util

    def real(path):
        if path == root or path.startswith(root + "/"):
            return path
        return os.path.join(root, path.lstrip("/"))

    entries = []
    with open(os.path.join(root, "etc/passwd")) as fh:
        for line in fh:
            f = line.rstrip("\n").split(":")
            if len(f) >= 7:
                entries.append(
                    pwd.struct_passwd(
                        (f[0], f[1], int(f[2]), int(f[3]), f[4], f[5], f[6])
                    )
                )
    groups = []
    with open(os.path.join(root, "etc/group")) as fh:
        for line in fh:
            f = line.rstrip("\n").split(":")
            if len(f) >= 4:
                members = [m for m in f[3].split(",") if m]
                groups.append(
                    grp.struct_group((f[0], f[1], int(f[2]), members))
                )

    def getpwnam(name):
        for ent in entries:
            if ent.pw_name == name:
                return ent
        raise KeyError(name)

    def getpwuid(uid):
        for ent in entries:
            if ent.pw_uid == uid:
                return ent
        raise KeyError(uid)

    def getgrgid(gid):
        for group in groups:
            if group.gr_gid == gid:
                return group
        raise KeyError(gid)

    def getgrnam(name):
        for group in groups:
            if group.gr_name == name:
                return group
        raise KeyError(name)

    pwd.getpwnam = getpwnam
    pwd.getpwuid = getpwuid
    grp.getgrgid = getgrgid
    grp.getgrnam = getgrnam
    grp.getgrall = lambda: list(groups)

    for name in ("islink", "isfile", "isdir", "exists", "lexists"):
        original = getattr(os.path, name)
        setattr(
            os.path,
            name,
            (lambda fn: lambda p, *a, **kw: fn(real(p), *a, **kw))(original),
        )

    makedirs = os.makedirs
    os.makedirs = lambda p, *a, **kw: makedirs(real(p), *a, **kw)

    load_text_file = util.load_text_file
    util.load_text_file = lambda p, *a, **kw: load_text_file(real(p), *a, **kw)
    write_file = util.write_file
    util.write_file = lambda p, *a, **kw: write_file(real(p), *a, **kw)

    util.get_permissions = lambda p: statmod.S_IMODE(os.stat(real(p)).st_mode)
    util.get_owner = lambda p: getpwuid(os.stat(real(p)).st_uid).pw_name
    util.get_group = lambda p: getgrgid(os.stat(real(p)).st_gid).gr_name

    def chownbyid(path, uid=None, gid=None):
        if uid in [None, -1] and gid in [None, -1]:
            return
        os.chown(real(path), uid, gid)

    util.chownbyid = chownbyid
    ssh_util.util = util

    @contextlib.contextmanager
    def no_guard(*_args, **_kwargs):
        yield

    util.SeLinuxGuard = no_guard


def mode_install(root, username, keys):
    reroot(root)
    try:
        if keys:
            ssh_util.setup_user_keys(keys, username)
        chosen = ssh_util.extract_authorized_keys(username)[0]
        return {"chosen": chosen, "error": False, "tree": tree(root)}
    except Exception:
        return {"chosen": "", "error": True, "tree": tree(root)}


def main():
    argv = sys.argv[1:]
    mode = argv[0] if argv else ""

    def arg(n):
        return argv[n] if len(argv) > n else ""

    parser = ssh_util.AuthKeyLineParser()

    if mode == "parse":
        out = line_json(parser.parse(unescape(arg(1)), options=arg(2)))
    elif mode == "update":
        old = [parser.parse(line) for line in unescape(arg(1)).splitlines()]
        new = [
            parser.parse(line, options=arg(3))
            for line in unescape(arg(2)).splitlines()
            if line != ""
        ]
        out = {"content": ssh_util.update_authorized_keys(old, new)}
    elif mode == "paths":
        out = {
            "paths": ssh_util.render_authorizedkeysfile_paths(
                arg(1), arg(2), arg(3)
            )
        }
    elif mode == "sshdcfg":
        lines = ssh_util.parse_ssh_config_lines(unescape(arg(1)).splitlines())
        out = {
            "lines": [
                {
                    "key": line.key,
                    "raw_key": line._key,
                    "value": line.value,
                    "str": str(line),
                }
                for line in lines
            ],
            "map": {line.key: line.value for line in lines if line.key},
        }
    elif mode == "updatecfg":
        lines = ssh_util.parse_ssh_config_lines(unescape(arg(1)).splitlines())
        updates = {}
        for spec in unescape(arg(2)).splitlines():
            if not spec:
                continue
            key, _, value = spec.partition("=")
            updates[key] = value
        changed = ssh_util.update_ssh_config_lines(lines=lines, updates=updates)
        out = {
            "changed": changed,
            "content": "\n".join(str(line) for line in lines) + "\n",
        }
    elif mode == "install":
        keys = [k for k in unescape(arg(3)).splitlines() if k != ""]
        out = mode_install(arg(1).rstrip("/"), arg(2), keys)
    else:
        sys.stderr.write("unknown mode: %s\n" % mode)
        return 2

    print(json.dumps(out, indent=1, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
