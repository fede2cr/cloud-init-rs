#!/usr/bin/env python3
"""Dump what `cloudinit.dmi` and `util.is_container` see on this host.

Paired with the `dump-dmi` example in ci-datasource. Both read the real
machine, so the output only agrees if the port reads it the same way.

With `--syspath DIR` the sysfs reader is pointed at a prepared directory
instead, which is the only way to exercise it on a host whose DMI is absent.

Usage: dmi.py [--syspath DIR key ...] [substitution-string ...]
"""
import sys

sys.path.insert(0, "/usr/lib/python3/dist-packages")

from cloudinit import dmi, util  # noqa: E402


def main(argv):
    if argv[:1] == ["--syspath"]:
        dmi.DMI_SYS_PATH = argv[1]
        for key in argv[2:]:
            value = dmi._read_dmi_syspath(key)
            print("syspath %s=%s" % (key, "<none>" if value is None else value))
        return 0
    print("container=%s" % util.is_container())
    for key in dmi.DMIDECODE_TO_KERNEL:
        value = dmi.read_dmi_data(key)
        print("%s=%s" % (key, "<none>" if value is None else value))
    for src in argv:
        print("sub %s -> %s" % (src, dmi.sub_dmi_vars(src)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
