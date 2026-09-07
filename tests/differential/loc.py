"""Dump `cc_locale.handle`'s decisions, for differential testing.

Usage: `loc.py <cfg-json> <distro> <system-locale> <conf-exists> <locale-gen>
<update-locale>`

`<system-locale>` is `-` for "the conf file does not set `LANG`". The last
three are `0` or `1`. Matches `dump-cc-locale`.

Everything the decision reads is supplied as an argument and injected, because
carrying the plan out would run `locale-gen` on the machine doing the
comparison. `os` is shimmed into `cloudinit.distros.debian` alone rather than
patched globally, so that `json` and `logging` keep the real one.

Only the debian `apply_locale` is ported, so only its distros are swept.

Not named after the module it imports.
"""

import json
import os as real_os
import sys
import types

from cloudinit import distros, subp, util
from cloudinit.config import cc_locale
from cloudinit.distros import debian

CALLS = []


def install_stubs(system_locale, conf_exists, has_locale_gen, has_update_locale):
    def which(program):
        if program == "locale-gen":
            return "/usr/bin/locale-gen" if has_locale_gen else None
        if program == "update-locale":
            return "/usr/sbin/update-locale" if has_update_locale else None
        return None

    def record_subp(argv, *args, **kwargs):
        CALLS.append({"argv": list(argv), "op": "subp"})

    subp.which = which
    subp.subp = record_subp
    # `apply_locale` reads the system locale through this, and `get_locale`
    # calls it by module-global name.
    debian.read_system_locale = lambda *a, **k: system_locale
    debian.os = types.SimpleNamespace(
        path=types.SimpleNamespace(
            exists=lambda p: conf_exists, join=real_os.path.join
        )
    )


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
    if len(argv) < 6:
        sys.stderr.write(
            "usage: loc.py <cfg-json> <distro> <system-locale> <conf-exists> "
            "<locale-gen> <update-locale> | --batch <cases>\n"
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
    name = fields[1]
    system_locale = "" if fields[2] == "-" else fields[2]

    del CALLS[:]
    try:
        distro = distros.fetch(name)(name, {}, None)
    except Exception:
        return {"error": "unknown distro"}
    distro.install_packages = lambda packages: CALLS.append(
        {"op": "install_packages", "packages": list(packages)}
    )
    install_stubs(
        system_locale, fields[3] == "1", fields[4] == "1", fields[5] == "1"
    )

    cloud = Cloud(distro)

    # `get_cfg_option_str(cfg, "locale", cloud.get_locale())` -- note the
    # default is evaluated whether or not the key is present.
    fallback = cloud.get_locale()
    if "locale" not in cfg:
        locale = fallback
    else:
        value = cfg["locale"]
        locale = value if isinstance(value, str) else str(value)

    out = {"locale": locale}
    skipped = util.is_false(locale)
    out["skipped"] = skipped

    try:
        cc_locale.handle("locale", cfg, cloud, [])
        out["calls"] = [] if skipped else list(CALLS)
    except Exception as error:
        out["error"] = str(error)
    return out


# `Cloud.get_locale` -> `DataSource.get_locale`, which swallows the
# `NotImplementedError` the abstract `Distro.get_locale` raises.
class Cloud:
    default_locale = "en_US.UTF-8"

    def __init__(self, distro):
        self.distro = distro

    def get_locale(self):
        locale = self.default_locale
        try:
            locale = self.distro.get_locale()
        except NotImplementedError:
            pass
        return locale


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
