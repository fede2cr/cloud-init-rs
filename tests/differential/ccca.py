#!/usr/bin/env python3
"""`cc_ca_certs.handle` reduced to the actions it decides to take.

Usage: ccca.py plan <cfg-json> <distro>
       ccca.py deselect <file>

Running this module for real empties the trust store of the machine that runs
it and then asks `update-ca-certificates` to rebuild it, so `plan` stubs the
four escapes onto one shared list and records them in order.

`disable_system_ca_certs` is stubbed whole rather than at its file calls: it
decides nothing beyond "this path, if it is configured", and letting it run
would make the recorded plan depend on whether the machine running the
comparison happens to have `/etc/ca-certificates.conf`. Its rewrite is
compared separately by the `deselect` mode, which runs the real function
against a scratch file -- including the two early exits, for a file that is
missing and one that is empty.
"""

import json
import sys

from cloudinit.config import cc_ca_certs


class Recorder:
    """Every stub writes here, so the order between them is preserved."""

    def __init__(self):
        self.calls = []

    def delete_dir_contents(self, dirname):
        self.calls.append({"op": "remove_dir_contents", "path": dirname})

    def disable_system_ca_certs(self, distro_cfg):
        path = distro_cfg["ca_cert_config"]
        if not path:
            return
        self.calls.append({"op": "disable_system_ca_certs", "path": path})

    def write_file(self, filename, content, mode=0o644, **kwargs):
        self.calls.append(
            {"op": "write_cert", "path": filename, "contents": content}
        )

    def subp(self, cmd, **kwargs):
        if list(cmd)[:1] == ["debconf-set-selections"]:
            self.calls.append(
                {"op": "debconf_set_selections", "data": kwargs["data"]}
            )
        else:
            self.calls.append({"op": "update_ca_certs", "argv": list(cmd)})


class Distro:
    def __init__(self, name):
        self.name = name


class Cloud:
    def __init__(self, distro):
        self.distro = distro


def do_plan(cfg, distro_name):
    recorder = Recorder()
    cc_ca_certs.util.delete_dir_contents = recorder.delete_dir_contents
    cc_ca_certs.util.write_file = recorder.write_file
    cc_ca_certs.subp.subp = recorder.subp
    cc_ca_certs.disable_system_ca_certs = recorder.disable_system_ca_certs

    out = recorder.calls
    try:
        cc_ca_certs.handle("ca_certs", cfg, Cloud(Distro(distro_name)), [])
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        out = {"error": str(exc)}
    print(json.dumps(out, indent=1, sort_keys=True))
    return 0


def do_deselect(path):
    cc_ca_certs.disable_system_ca_certs({"ca_cert_config": path})
    try:
        with open(path, "r") as handle:
            content = handle.read()
    except OSError:
        content = None
    print(
        json.dumps({"content": content}, indent=1, sort_keys=True)
    )
    return 0


def main(argv):
    mode = argv[1] if len(argv) > 1 else ""
    if mode == "plan":
        return do_plan(json.loads(argv[2]), argv[3])
    if mode == "deselect":
        return do_deselect(argv[2])
    sys.stderr.write("ccca.py: unknown mode %r\n" % mode)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
