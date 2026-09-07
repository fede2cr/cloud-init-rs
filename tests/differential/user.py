#!/usr/bin/env python3
"""Dump what Distro.add_user / write_sudo_rules / write_doas_rules / create_user
would have done.

Nothing is allowed to touch the system: subp, is_user, is_group, write_file and
append_file are all replaced with recorders. What comes back is the argument
list cloud-init *would* have run, which is the part worth comparing.

The "create" mode goes further and records the whole ordered sequence of calls
create_user makes -- group creation, useradd, chpasswd, lock/unlock, sudo, doas
and keys. That order is the thing being checked: it is what decides whether an
account ends up with a usable password or a locked one.

Usage:
    user.py argv <name> <config-json> <snappy:0|1>
    user.py sudo <user> <rules-json>
    user.py doas <user> <rules-json>
    user.py create <name> <config-json> <exists:0|1> <blank-shadow:0|1> <snappy:0|1>
"""
import json
import sys

import cloudinit.distros as distros
import cloudinit.ssh_util as ssh_util
import cloudinit.subp as subp
import cloudinit.util as util


def main():
    mode = sys.argv[1]
    out = {}
    calls = []

    def fake_subp(args, *a, **kw):
        calls.append({"argv": list(args), "log": list(kw.get("logstring") or args)})
        return ('{"username": "snapped"}', "")

    subp.subp = fake_subp
    distros.subp.subp = fake_subp
    util.is_user = lambda name: False
    util.is_group = lambda name: False
    distros.util.is_user = util.is_user
    distros.util.is_group = util.is_group

    written = []
    util.write_file = lambda path, contents, *a, **kw: written.append(contents)
    util.append_file = lambda path, contents, *a, **kw: written.append(contents)
    distros.util.write_file = util.write_file
    distros.util.append_file = util.append_file

    from cloudinit.distros.ubuntu import Distro

    d = Distro("ubuntu", {}, None)

    try:
        if mode == "argv":
            name = sys.argv[2]
            config = json.loads(sys.argv[3])
            snappy = sys.argv[4] == "1"
            util.system_is_snappy = lambda: snappy
            distros.util.system_is_snappy = util.system_is_snappy
            d.add_user(name, **config)
            # The last call is the useradd; anything before it is groupadd.
            out["calls"] = calls
        elif mode == "sudo":
            user = sys.argv[2]
            rules = json.loads(sys.argv[3])
            d.ensure_sudo_dir = lambda *a, **kw: None
            d.write_sudo_rules(user, rules, sudo_file="/nonexistent/ci-sudoers")
            # The header line carries a timestamp, so only the body is compared.
            out["content"] = written[-1].split("\n", 1)[1] if written else None
        elif mode == "doas":
            user = sys.argv[2]
            rules = json.loads(sys.argv[3])
            d.write_doas_rules(user, rules, doas_file="/nonexistent/ci-doas")
            out["content"] = written[-1].split("\n", 1)[1] if written else None
            out["valid"] = [d.is_doas_rule_valid(user, r) for r in rules]
        elif mode == "create":
            out["steps"] = mode_create(d, calls)
        else:
            raise SystemExit("unknown mode %s" % mode)
    except Exception as exc:  # noqa: BLE001
        out = {"error": str(exc)}

    print(json.dumps(out, indent=1, sort_keys=True))


def mode_create(d, calls):
    """Record the sequence create_user emits, without running any of it."""
    name = sys.argv[2]
    config = json.loads(sys.argv[3])
    exists = sys.argv[4] == "1"
    blank_shadow = sys.argv[5] == "1"
    snappy = sys.argv[6] == "1"

    util.system_is_snappy = lambda: snappy
    distros.util.system_is_snappy = util.system_is_snappy
    util.is_user = lambda n: exists
    distros.util.is_user = util.is_user
    d._shadow_file_has_empty_user_password = lambda n: blank_shadow

    # groupadd and useradd stay on the subp recorder so their argument lists
    # are compared too; everything below writes files or keys, so it is
    # recorded at the call rather than run.
    def record(op, **fields):
        fields["op"] = op
        calls.append(fields)

    d.set_passwd = lambda user, passwd, hashed=False: record(
        "set_passwd", user=user, passwd=str(passwd), hashed=hashed
    )
    d.lock_passwd = lambda user: record("lock_passwd", user=user)
    d.unlock_passwd = lambda user: record("unlock_passwd", user=user)
    # write_doas_rules iterates its rules first, so the recorder does too --
    # that is what turns a bare string into a list of characters.
    d.write_doas_rules = lambda user, rules, **kw: record(
        "write_doas_rules", user=user, rules=list(rules)
    )
    d.write_sudo_rules = lambda user, rules, **kw: record(
        "write_sudo_rules", user=user, rules=rules
    )

    def fake_keys(keys, user, options=None):
        record(
            "setup_user_keys",
            user=user,
            keys=sorted(str(k) for k in keys),
            options=options or "",
        )

    ssh_util.setup_user_keys = fake_keys
    distros.ssh_util.setup_user_keys = fake_keys

    d.create_user(name, **config)
    for call in calls:
        call.setdefault("op", "run")
    return calls


main()
