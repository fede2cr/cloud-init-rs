#!/usr/bin/env python3
"""`cc_apt_configure`'s decisions, reduced to the ones a fixture can drive.

Usage: ccaptconfigure.py <what> <cfg-json> [<env-json>]

The module is upstream's largest and almost none of it can be run for real on
the machine doing the comparing -- it imports gpg keys off the network,
rewrites /etc/apt and shells out to add-apt-repository. So this is split into
subcommands, one per decision, each of them running upstream's own function
with its inputs supplied as JSON and its effects captured:

    convert   convert_to_v3_apt_format
    aptconf   apply_apt_config
    sources   generate_sources_list (and so disable_suites)
    entries   add_apt_sources
    mirrors   find_apt_mirror_info

Two things are stubbed on both sides, for the same reason `ccpackages.py`
stubs `subp`:

* `util.rand_dict_key` is random, and the key it picks ends up in the
  converted config. Both sides use a counter instead.
* `GPG` would ask keyserver.ubuntu.com for a key, which makes the answer
  depend on the network. `<env-json>.gpg` maps a key id to the armour the
  stub hands back; `dearmor` wraps its input in a marker so the harness can
  see that it happened without either side agreeing on what real binary
  OpenPGP bytes look like.
"""

import json
import sys

from cloudinit import util
from cloudinit.config import cc_apt_configure as mod


class StubGPG:
    def __init__(self, keys):
        self.keys = keys

    def __enter__(self):
        return self

    def __exit__(self, *_):
        return False

    def getkeybyid(self, keyid, keyserver=mod.DEFAULT_KEYSERVER):
        return self.keys.get(keyid)

    def dearmor(self, key):
        if "BAD" in key:
            raise mod.subp.ProcessExecutionError(stderr="Failed to dearmor key")
        return ("<dearmored>%s</dearmored>" % key).encode()

    def list_keys(self, key_file, human_output=False):
        return ""

    def delete_key(self, key):
        pass


class Distro:
    def __init__(self, name, package_mirrors, arch, calls=None, mirror_filter=None):
        self.name = name
        self._package_mirrors = package_mirrors
        self._arch = arch
        self._calls = calls
        self._mirror_filter = mirror_filter

    def update_package_sources(self, *args, **kwargs):
        if self._calls is not None:
            self._calls.append({"op": "update_package_sources"})

    def get_primary_arch(self):
        return self._arch

    def get_option(self, name, default=None):
        if name == "package_mirrors":
            return self._package_mirrors
        return default

    def get_package_mirror_info(self, arch=None, data_source=None):
        from cloudinit import distros

        arch_info = distros._get_arch_package_mirror_info(
            self._package_mirrors, arch or self._arch
        )
        # `mirror_filter` is a *default argument* of
        # `_get_package_mirror_info`, bound to the real `util.search_for_mirror`
        # when the module was imported. Patching the module attribute does not
        # reach it, and the real one does live DNS -- so it has to be passed.
        return distros._get_package_mirror_info(
            data_source=data_source,
            mirror_info=arch_info,
            mirror_filter=self._mirror_filter or distros.util.search_for_mirror,
        )


class DataSource:
    def __init__(self, distro, env):
        self.distro = distro
        self.availability_zone = env.get("availability_zone")
        self.region = env.get("region")
        self.platform_type = env.get("platform_type", "azure")

    def get_package_mirror_info(self):
        return self.distro.get_package_mirror_info(data_source=self)


class Cloud:
    def __init__(self, distro, datasource=None, templates=None):
        self.distro = distro
        self.datasource = datasource
        self._templates = templates or {}

    def get_template_filename(self, name):
        # The real one returns a path or None after warning. The fixture keeps
        # the templates in memory, so the "path" is the name and the loader
        # below knows how to read it back.
        return name if name in self._templates else None


def counter():
    """`util.rand_dict_key`, made repeatable."""
    state = {"n": 0}

    def rand_dict_key(dictionary, postfix=None):
        state["n"] += 1
        return "key%d_%s" % (state["n"], postfix)

    return rand_dict_key


def raised(error):
    return {"error": str(error)}


def do_convert(cfg, env):
    mod.util.rand_dict_key = counter()
    try:
        return mod.convert_to_v3_apt_format(cfg)
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        return raised(exc)


def do_aptconf(cfg, env):
    calls = []
    install_file_recorder(calls)
    # `<env-json>` is the list of drop-in paths that already exist, which is
    # what decides whether a removal is planned.
    present = set(env or [])
    mod.os.path.isfile = lambda path: path in present
    mod.apply_apt_config(cfg, mod.APT_PROXY_FN, mod.APT_CONFIG_FN)
    return calls


def do_sources(cfg, env):
    calls = []
    install_file_recorder(calls)
    templates = env.get("templates") or {}
    files = env.get("files") or {}
    # `get_template_filename` above hands back the template's *name*, so the
    # loader has to answer for both namespaces.
    contents = dict(files)
    contents.update(templates)
    mod.util.load_text_file = lambda path, **kw: contents[path]
    mod.os.path.isfile = lambda path: path in contents
    mod.features.APT_DEB822_SOURCE_LIST_FILE = bool(env.get("deb822", True))
    # `get_apt_cfg` prefers apt_pkg and falls back to `apt-config dump`, so
    # its answer is a property of the machine running the comparison. Pin it
    # to the documented defaults, which is what the port is compared against.
    mod.get_apt_cfg = lambda: {
        "sourcelist": "/%s/%s"
        % (
            mod.DEFAULT_APT_CFG["Dir::Etc"],
            mod.DEFAULT_APT_CFG["Dir::Etc::sourcelist"],
        ),
        "sourceparts": "/%s/%s/"
        % (
            mod.DEFAULT_APT_CFG["Dir::Etc"],
            mod.DEFAULT_APT_CFG["Dir::Etc::sourceparts"],
        ),
    }
    distro = Distro(env.get("distro", "ubuntu"), [], env.get("arch", "amd64"))
    cloud = Cloud(distro, templates=templates)
    try:
        mod.generate_sources_list(
            cfg,
            env.get("release", "noble"),
            env.get("mirrors") or {},
            cloud,
            env.get("keys") or {},
        )
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        return raised(exc)
    return calls


def do_entries(cfg, env):
    calls = []
    install_step_recorder(calls)
    install_subp_recorder(calls)
    distro = Distro(
        env.get("distro", "ubuntu"), [], env.get("arch", "amd64"), calls
    )
    cloud = Cloud(distro)
    import re

    try:
        matcher = re.compile(env.get("aa_repo_match", mod.ADD_APT_REPO_MATCH)).search
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        return raised(exc)
    try:
        mod.add_apt_sources(
            cfg.get("sources"),
            cloud,
            StubGPG(env.get("gpg") or {}),
            template_params=dict(env.get("params") or {}),
            aa_repo_match=matcher,
        )
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        calls.append(raised(exc))
    return calls


def do_mirrors(cfg, env):
    resolvable = set(env.get("resolvable") or [])
    search_for_mirror = lambda candidates: next(  # noqa: E731
        (c for c in (candidates or []) if c in resolvable), None
    )
    mod.util.search_for_mirror = search_for_mirror
    from cloudinit import distros

    distros.util.search_for_mirror = search_for_mirror
    mod.util.get_hostname_fqdn = lambda cfg, cloud, metadata_only=False: Fqdn(
        env.get("fqdn", "host.example.com")
    )
    arch = env.get("arch", "amd64")
    distro = Distro(
        env.get("distro", "ubuntu"),
        env.get("package_mirrors") or [],
        arch,
        mirror_filter=search_for_mirror,
    )
    cloud = Cloud(distro, DataSource(distro, env))
    try:
        return mod.find_apt_mirror_info(cfg, cloud, arch=arch)
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        return raised(exc)


def install_file_recorder(calls):
    """`util.write_file` and `util.del_file`, captured rather than performed."""

    def write_file(path, content, mode=0o644, omode="wb", **kwargs):
        if isinstance(content, bytes):
            content = content.decode("utf-8", "replace")
        call = {"op": "write", "path": path, "content": content, "mode": mode}
        if omode == "a":
            call["op"] = "write_source"
            call["append"] = True
        calls.append(call)

    mod.util.write_file = write_file
    mod.util.del_file = lambda path: calls.append({"op": "remove", "path": path})
    mod.os.path.isfile = lambda path: False


def install_step_recorder(calls):
    """The same writes, classified the way `add_apt_sources` means them.

    A write under one of the two gpg directories is a key import; anything
    else is the sources entry itself, and its `omode` is what `append: false`
    in the config turns into.
    """

    def write_file(path, content, mode=0o644, omode="wb", **kwargs):
        if isinstance(content, bytes):
            content = content.decode("utf-8", "replace")
        if path.startswith(mod.CLOUD_INIT_GPG_DIR) or path.startswith(
            mod.APT_TRUSTED_GPG_DIR
        ):
            calls.append({"op": "write_key", "path": path, "content": content})
        else:
            calls.append(
                {
                    "op": "write_source",
                    "path": path,
                    "content": content,
                    "append": omode == "a",
                }
            )

    mod.util.write_file = write_file
    mod.util.del_file = lambda path: calls.append({"op": "remove", "path": path})
    mod.os.path.isfile = lambda path: False


def install_subp_recorder(calls):
    def subp(args=None, **kwargs):
        argv = list(args or [])
        if argv[:1] == ["add-apt-repository"]:
            calls.append({"op": "add_apt_repository", "source": argv[-1]})
        else:
            calls.append({"op": "subp", "argv": argv})
        return Result()

    mod.subp.subp = subp


class Result:
    stdout = ""
    stderr = ""
    return_code = 0


class Fqdn:
    """What `util.get_hostname_fqdn` returns; only `.fqdn` is read here."""

    def __init__(self, fqdn):
        self.hostname = fqdn.split(".")[0]
        self.fqdn = fqdn
        self.is_default = False


def main(argv):
    what = argv[1]
    cfg = json.loads(argv[2]) if len(argv) > 2 else {}
    env = json.loads(argv[3]) if len(argv) > 3 else {}

    handlers = {
        "convert": do_convert,
        "aptconf": do_aptconf,
        "sources": do_sources,
        "entries": do_entries,
        "mirrors": do_mirrors,
    }
    if what not in handlers:
        sys.stderr.write("unknown subcommand %s\n" % what)
        return 2
    out = handlers[what](cfg, env)
    print(json.dumps(out, indent=1, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
