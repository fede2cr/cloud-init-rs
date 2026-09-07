"""Dump one distro's facts as JSON, for differential testing.

Usage: `distro.py <name>` or `distro.py --names`.
"""

import json
import sys

from cloudinit import distros

NAMES = [name for members in distros.OSFAMILIES.values() for name in members]

# The class that owns each `_write_hostname`, mapped to the `HostnameWriter`
# variant that reproduces it. Three classes share one variant because their
# bodies are identical. Also imported by `gen_distro_table.py`, so the table
# and the check that guards it cannot drift apart.
WRITERS = {
    "alpine.Distro": "ConfFile",
    "aosc.Distro": "Aosc",
    "arch.Distro": "ConfFile",
    "bsd.BSD": "Bsd",
    "debian.Distro": "ConfFile",
    "gentoo.Distro": "Gentoo",
    "openbsd.Distro": "OpenBsd",
    "opensuse.Distro": "OpenSuse",
    "photon.Distro": "Photon",
    "rhel.Distro": "Rhel",
}

# The same arrangement for `_read_hostname`. The owners are nearly but not
# quite the same set as WRITERS': gentoo writes `hostname=".."` but reads a
# plain `HostnameConf` like debian, so it shares the ConfFile variant here.
READERS = {
    "alpine.Distro": "ConfFile",
    "aosc.Distro": "Aosc",
    "arch.Distro": "ConfFile",
    "bsd.BSD": "Bsd",
    "debian.Distro": "ConfFile",
    "gentoo.Distro": "ConfFile",
    "openbsd.Distro": "OpenBsd",
    "opensuse.Distro": "OpenSuse",
    "photon.Distro": "Photon",
    "rhel.Distro": "Rhel",
}

# And for `_read_system_hostname`, which only chooses WHICH file to read.
# `openbsd` has no override, so it inherits `bsd`'s.
SYSTEM_READERS = {
    "alpine.Distro": "ConfFile",
    "aosc.Distro": "ConfFile",
    "arch.Distro": "ConfFile",
    "bsd.BSD": "ConfFile",
    "debian.Distro": "ConfFile",
    "gentoo.Distro": "ConfFile",
    "opensuse.Distro": "SystemdOrConf",
    "photon.Distro": "Systemd",
    "rhel.Distro": "SystemdOrConf",
}

# The owner of `set_timezone`. Five bodies across nine classes: most just call
# `distros.set_etc_timezone`, which writes `/etc/timezone` and then links or
# copies `/etc/localtime`. `rhel` and `opensuse` share a body that differs
# only in the sysconfig key it writes when there is no systemd, so they stay
# separate variants.
TIMEZONE_WRITERS = {
    "alpine.Distro": "EtcTimezone",
    "aosc.Distro": "Aosc",
    "arch.Distro": "EtcTimezone",
    "bsd.BSD": "Bsd",
    "debian.Distro": "EtcTimezone",
    "gentoo.Distro": "EtcTimezone",
    "opensuse.Distro": "OpenSuse",
    "photon.Distro": "EtcTimezone",
    "rhel.Distro": "Rhel",
}

# The owner of `apply_locale`. Unusually for this file the variants are one
# per class: no two of these eleven bodies are the same, because each distro
# has its own idea of which file the locale lives in and which command
# regenerates it.
LOCALE_WRITERS = {
    "alpine.Distro": "Alpine",
    "aosc.Distro": "Aosc",
    "arch.Distro": "Arch",
    "debian.Distro": "Debian",
    "freebsd.Distro": "FreeBsd",
    "gentoo.Distro": "Gentoo",
    "netbsd.NetBSD": "NetBsd",
    "opensuse.Distro": "OpenSuse",
    "photon.Distro": "Photon",
    "raspberry_pi_os.Distro": "RaspberryPiOs",
    "rhel.Distro": "Rhel",
}

# The owner of `get_locale`. `distros.Distro` is the abstract base, whose body
# is `raise NotImplementedError()` -- so seventeen distros cannot answer the
# question at all, and `cc_locale` on them depends entirely on the config
# naming a locale.
LOCALE_READERS = {
    "alpine.Distro": "Alpine",
    "debian.Distro": "Debian",
    "distros.Distro": "Unsupported",
    "rhel.Distro": "Rhel",
}

# Read off a constructed object, not the class, because that is what a boot
# and `net-convert` both see: several `__init__`s rewrite `renderer_configs`
# wholesale, and `osfamily` is only ever an instance attribute.
ATTRS = [
    "ci_sudoers_fn",
    "default_owner",
    "dhclient_lease_directory",
    "dhclient_lease_file_regex",
    "doas_fn",
    "hostname_conf_fn",
    "hosts_fn",
    "init_cmd",
    "osfamily",
    "pip_package_name",
    "prefer_fqdn",
    "renderer_configs",
    "resolve_conf_fn",
    "shadow_empty_locked_passwd_patterns",
    "shadow_extrausers_fn",
    "shadow_fn",
    "shutdown_options_map",
    "tz_zone_dir",
    "usr_lib_exec",
]

# Attributes only some classes define. `getattr(.., None)` rather than a hard
# lookup, because their absence is itself a fact worth recording: a distro
# with no `clock_conf_fn` is one whose `set_timezone` never needed one.
OPTIONAL_ATTRS = [
    "clock_conf_fn",
    "default_locale",
    "locale_conf_fn",
    "systemd_locale_conf_fn",
    "tz_local_fn",
]


def hostname_writer(distro):
    """Which `_write_hostname` a distro gets, as a `HostnameWriter` variant.

    Not an attribute, so it cannot go in ATTRS, but it is the one piece of
    hostname behaviour a distro selects rather than inherits from its family --
    and the two do not agree, so it has to be read off the MRO.

    A `KeyError` here is the point: it means upstream moved a `_write_hostname`
    to a class the port has never seen, and the port is now silently doing
    something else.
    """
    return _owner(distro, "_write_hostname", WRITERS)


def hostname_reader(distro):
    """Which `_read_hostname` a distro gets, as a `HostnameReader` variant."""
    return _owner(distro, "_read_hostname", READERS)


def system_hostname_reader(distro):
    """Which `_read_system_hostname` a distro gets."""
    return _owner(distro, "_read_system_hostname", SYSTEM_READERS)


def timezone_writer(distro):
    """Which `set_timezone` a distro gets, as a `TimezoneWriter` variant."""
    return _owner(distro, "set_timezone", TIMEZONE_WRITERS)


def locale_writer(distro):
    """Which `apply_locale` a distro gets, as a `LocaleWriter` variant."""
    return _owner(distro, "apply_locale", LOCALE_WRITERS)


def locale_reader(distro):
    """Which `get_locale` a distro gets, as a `LocaleReader` variant."""
    return _owner(distro, "get_locale", LOCALE_READERS)


def _owner(distro, method, variants):
    for cls in type(distro).__mro__:
        if method in cls.__dict__:
            owner = cls.__module__.rsplit(".", 1)[-1] + "." + cls.__name__
            return variants[owner]
    return None


def waits_for_network(distro):
    """Whether this distro's `wait_for_network` does anything.

    The base `Distro.wait_for_network` is a documented no-op, so a plain
    `hasattr` says nothing. Only an override means the distro actually waits,
    and like `_write_hostname` that is a property of the MRO rather than of
    the family, so it has to be read off the class.
    """
    return type(distro).wait_for_network is not distros.Distro.wait_for_network


def dump(value):
    return json.dumps(value, indent=1, sort_keys=True, separators=(",", ": "))


def family_key(name):
    for family, members in distros.OSFAMILIES.items():
        if name in members:
            return family
    return None


def main(argv):
    if not argv:
        sys.stderr.write("usage: distro.py <name>|--names\n")
        return 2
    if argv[0] == "--names":
        print(dump(NAMES))
        return 0
    try:
        distro = distros.fetch(argv[0])(argv[0], {}, None)
    except Exception:
        # Either the name is not a distro at all, or it is one OSFAMILIES
        # lists but no module implements. Both are the same answer here.
        print("null")
        return 0
    out = {attr: getattr(distro, attr) for attr in ATTRS}
    out["family_key"] = family_key(argv[0])
    out["hostname_writer"] = hostname_writer(distro)
    out["hostname_reader"] = hostname_reader(distro)
    out["system_hostname_reader"] = system_hostname_reader(distro)
    out["timezone_writer"] = timezone_writer(distro)
    out["locale_writer"] = locale_writer(distro)
    out["locale_reader"] = locale_reader(distro)
    for attr in OPTIONAL_ATTRS:
        out[attr] = getattr(distro, attr, None)
    out["systemd_hostname_conf_fn"] = getattr(
        distro, "systemd_hostname_conf_fn", None
    )
    out["localhost_ip"] = distro._get_localhost_ip()
    out["waits_for_network"] = waits_for_network(distro)
    out["package_managers"] = [m.name for m in distro.package_managers]
    print(dump(out))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
