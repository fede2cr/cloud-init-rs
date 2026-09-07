"""Dumps a module section at each stage of resolution, matching `dump-modules`.

`Modules` is built with `__new__` rather than its constructor: the real one
takes an `Init`, and an `Init` would go looking for a datasource. Only two
attributes are actually read on the paths under test — the cached config and
`init.distro.name` — so those are supplied directly.

`_run_modules` is replaced, not called. Running the real modules would
configure the machine this test runs on.
"""

import json
import sys
import types

from cloudinit.config import modules as modlib


def module_name(mod):
    """`cc_bootcmd` out of `<module 'cloudinit.config.cc_bootcmd' ...>`."""
    return mod.__name__.rsplit(".", 1)[-1]


def details_json(details):
    # `run_section` builds plain lists rather than `ModuleDetails`, so index
    # rather than attribute-access.
    mod, name, freq, args = details[0], details[1], details[2], details[3]
    return {
        "module": module_name(mod),
        "name": name,
        "frequency": freq,
        "run_args": args,
    }


def main(argv):
    cfg_path, section, distro = argv[0], argv[1], argv[2]
    with open(cfg_path) as handle:
        cfg = json.load(handle)

    mods = modlib.Modules.__new__(modlib.Modules)
    mods._cached_cfg = cfg
    mods.init = types.SimpleNamespace(
        distro=types.SimpleNamespace(name=distro)
    )
    mods.reporter = None

    captured = []

    def fake_run_modules(self, mostly_mods):
        captured.extend(mostly_mods)
        return ([], [])

    modlib.Modules._run_modules = fake_run_modules

    raw = mods._read_modules(section)
    fixed = mods._fixup_modules(raw)
    mods.run_section(section)

    out = {
        "raw": raw,
        "fixed": [details_json(d) for d in fixed],
        "active": [details_json(d) for d in captured],
    }
    print(json.dumps(out, indent=1, sort_keys=True, separators=(",", ": ")))


if __name__ == "__main__":
    main(sys.argv[1:])
