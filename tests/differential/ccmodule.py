"""Runs one `cc_*` module against a config and dumps what landed on disk.

Matches `dump-cc`. The `Cloud` object is faked: only `paths`, `distro` and
`get_instance_id` are read on these paths, and building a real one would go
looking for a datasource.

Every path the fixture names comes from the harness, which templates them so
that they all sit inside the scratch directory. The dump is the tree under that
directory, so an escape shows up as a missing file.

One key in the fixture is not config: `_datasource` describes the fake
datasource the module sees, overriding any of the defaults below. Setting it to
`null` means no datasource at all.

The optional fourth argument names scratch-relative paths whose *content* is
reported as `<masked>` instead of itself, for the files that legitimately differ
between two runs — a timestamp, an uptime. Their presence and mode are still
compared.
"""

import base64
import importlib
import json
import os
import stat
import sys
import types

from cloudinit import helpers, sources, type_utils

# What the fixture's `_datasource` starts from. The instance id is the one
# modules that resolve `get_ipath` land under:
# `<cloud_dir>/instances/i-test/`.
DATASOURCE_DEFAULTS = {
    "class_name": "DataSourceNone",
    "dsname": "None",
    "instance_id": "i-test",
    "metadata": {},
    "sys_cfg": {},
}

# The fixture key describing the fake datasource.
DATASOURCE_KEY = "_datasource"

# The fixture key asking for a real distro whose absolute paths have been moved
# under the scratch directory. `ci-distro`'s `Distro` is a static table row and
# the port passes it a `root` instead; there is no such parameter upstream, so
# the equivalent here is to rewrite the handful of attributes that name files.
ROOT_KEY = "_root"

# Stands in for the content of a file the harness cannot compare.
MASKED = "<masked>"


def fake_datasource(spec):
    """The parts of a datasource a ported module reads, and nothing else.

    A real one would go looking for a cloud. The class is built by hand
    because `str(datasource)` is its *class* name, which is what
    `cc_final_message` prints.
    """
    fields = dict(DATASOURCE_DEFAULTS)
    fields.update(spec)
    cls = type(
        str(fields["class_name"]),
        (object,),
        {
            "__str__": lambda self: type_utils.obj_name(self),
            # The real one, which reads nothing but `self.metadata`. Modules
            # that resolve a hostname go through it.
            "get_hostname": sources.DataSource.get_hostname,
        },
    )
    ds = cls()
    ds.dsname = fields["dsname"]
    ds.metadata = fields["metadata"]
    ds.sys_cfg = fields["sys_cfg"]
    ds.get_instance_id = lambda: fields["instance_id"]
    return ds


def rooted_distro(scratch):
    """A real ubuntu distro that writes under `scratch` instead of `/etc`."""
    from cloudinit import distros

    distro = distros.fetch("ubuntu")("ubuntu", {}, None)
    for attr in ("hosts_fn", "hostname_conf_fn", "systemd_hostname_conf_fn"):
        current = getattr(distro, attr, None)
        if current is not None:
            setattr(distro, attr, os.path.join(scratch, current.lstrip("/")))
    # `hostname(1)` renames the machine running the test. The port skips it
    # for any root but `/` for the same reason.
    distro._apply_hostname = lambda hostname: None
    return distro


def tree(root, masked):
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        # A symlink to a directory is a `dirname` here and is not descended
        # into, so it would otherwise be invisible. Report it and prune it,
        # which is what the Rust dumper's `symlink_metadata` walk does.
        for name in list(dirnames):
            path = os.path.join(dirpath, name)
            if os.path.islink(path):
                dirnames.remove(name)
                found.append(
                    {
                        "path": os.path.relpath(path, root),
                        "mode": "0000",
                        "content": base64.b64encode(b"<symlink>").decode(),
                    }
                )
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            relative = os.path.relpath(path, root)
            if os.path.islink(path):
                found.append(
                    {
                        "path": relative,
                        "mode": "0000",
                        "content": base64.b64encode(b"<symlink>").decode(),
                    }
                )
                continue
            mode = stat.S_IMODE(os.stat(path).st_mode)
            if relative in masked:
                content = MASKED
            else:
                try:
                    with open(path, "rb") as handle:
                        content = base64.b64encode(handle.read()).decode()
                except OSError:
                    # A mode that hides the file from the dumper; that the file
                    # ended up that way is itself part of what is compared.
                    content = None
            found.append({"path": relative, "mode": "%04o" % mode, "content": content})
    found.sort(key=lambda entry: entry["path"])
    return found


def main(argv):
    module_name, cfg_path, scratch = argv[0], argv[1], argv[2]
    masked = [path for path in (argv[3].split(",") if len(argv) > 3 else []) if path]
    with open(cfg_path) as handle:
        cfg = json.load(handle)

    module = importlib.import_module("cloudinit.config." + module_name)

    spec = cfg.pop(DATASOURCE_KEY, {})
    rooted = cfg.pop(ROOT_KEY, False)
    datasource = None if spec is None else fake_datasource(spec)
    paths = helpers.Paths(
        cfg.get("system_info", {}).get("paths", {}), ds=datasource
    )

    def get_template_filename(name):
        fn = paths.template_tpl % (name)
        return fn if os.path.isfile(fn) else None

    cloud = types.SimpleNamespace(
        paths=paths,
        datasource=datasource,
        distro=(
            rooted_distro(scratch)
            if rooted
            else types.SimpleNamespace(default_owner="root:root", name="ubuntu")
        ),
        get_instance_id=(
            datasource.get_instance_id if datasource else lambda: None
        ),
        get_ipath=paths.get_ipath,
        get_ipath_cur=paths.get_ipath_cur,
        get_cpath=paths.get_cpath,
        get_template_filename=get_template_filename,
        get_hostname=(
            datasource.get_hostname
            if datasource
            else lambda fqdn=False, metadata_only=False: None
        ),
    )

    failed = False
    try:
        module.handle(module_name[len("cc_") :], cfg, cloud, [])
    except Exception:
        failed = True

    out = {"failed": failed, "tree": tree(scratch, masked)}
    print(json.dumps(out, indent=1, sort_keys=True, separators=(",", ": ")))
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main(sys.argv[1:])
