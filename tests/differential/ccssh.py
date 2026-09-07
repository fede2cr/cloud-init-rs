#!/usr/bin/env python3
"""`cc_ssh.handle` reduced to the calls it decides to make.

Usage: ccssh.py <cfg-json> <state-json> <default-user-json> <cloud-keys-json>

Every one of this module's effects lands on the machine that runs it: it
deletes everything matching `/etc/ssh/ssh_host_*key*`, writes new private
keys, shells out to `ssh-keygen`, and rewrites root's `authorized_keys`.
Running it for real under the harness would leave the developer's box
unreachable. So the seven calls it makes are stubbed onto one shared list and
recorded in order, and that ordered list is the comparison.

`<state-json>` is what the module reads off the running system before it
decides anything:

* `stale` — what `glob("/etc/ssh/ssh_host_*key*")` returns.
* `existing` — the paths `os.path.exists` answers true for, which is how the
  generator decides a key type is already covered.
* `fips` — `util.fips_enabled()`.
* `redhat` — `distro.osfamily == "redhat"`.

`<default-user-json>` stands in for `distro.get_default_user()`;
`<cloud-keys-json>` for `cloud.get_public_ssh_keys()`.
"""

import json
import re
import sys

from cloudinit import ssh_util, subp, util
from cloudinit.config import cc_ssh

# `KEY_GEN_TPL % (private, public)`, read back apart.
KEY_GEN_RE = re.compile(
    r'^o=\$\(ssh-keygen -yf "(?P<private>.*)"\) '
    r'&& echo "\$o" root@localhost > "(?P<public>.*)"$'
)

# What the fake `subp` hands back for a key generation, so that the branch
# deciding whether to echo it has something to echo.
KEYGEN_OUTPUT = b"<keygen output>"


def sort_key(value):
    """`sorted(set(keys))`, with the ties `str` alone would leave.

    `1` and `"1"` are different set elements that `str` cannot tell apart, and
    the set's own iteration order is randomised per process, so `repr` breaks
    the tie the same way on both sides.
    """
    return (str(value), repr(value))


class Recorder:
    """Every stub writes here, so the order between them is preserved."""

    def __init__(self, state):
        self.calls = []
        self.existing = set(state.get("existing") or [])
        self.stale = list(state.get("stale") or [])
        self.fips = bool(state.get("fips"))
        self.redhat = bool(state.get("redhat"))

    # -- the deletion pass ------------------------------------------------

    def glob(self, pattern):
        assert pattern == "/etc/ssh/ssh_host_*key*", pattern
        return self.stale

    def del_file(self, path):
        self.calls.append({"op": "delete_file", "path": path})

    # -- supplied keys ----------------------------------------------------

    def write_file(self, path, content, mode):
        # The real `write_file` encodes before it writes, and a key that is
        # not text never reaches the disk.
        util.encode_text(content)
        self.calls.append(
            {
                "op": "write_key",
                "path": path,
                "mode": format(mode, "o"),
                "value": content,
            }
        )

    def append_ssh_config(self, lines, fname=None):
        self.calls.append(
            {"op": "append_ssh_config", "lines": [list(line) for line in lines]}
        )

    # -- generated keys ---------------------------------------------------

    def exists(self, path):
        return path in self.existing

    def subp(self, cmd, capture=False, update_env=None, **kwargs):
        argv = list(cmd)
        if argv[:1] == ["sh"]:
            matched = KEY_GEN_RE.match(argv[2])
            self.calls.append(
                {
                    "op": "keygen_public",
                    "private": matched.group("private"),
                    "public": matched.group("public"),
                }
            )
        else:
            # `quiet` and `redhat_perms` are decisions made *after* this call
            # returns, so the two stubs below amend the entry in place.
            self.calls.append(
                {
                    "op": "keygen",
                    "keytype": argv[2],
                    "keyfile": argv[6],
                    "quiet": True,
                    "redhat_perms": False,
                }
            )
            # A key type that is not text makes the real `subp` refuse the
            # argv, which is the one exception the caller catches; the attempt
            # is recorded, and nothing after it happens.
            subp.raise_on_invalid_command(argv)
        return (KEYGEN_OUTPUT, b"")

    def stdout_write(self, text):
        if text == util.decode_binary(KEYGEN_OUTPUT):
            self.calls[-1]["quiet"] = False

    def set_redhat_keyfile_perms(self, keyfile):
        self.calls[-1]["redhat_perms"] = True

    # -- publishing -------------------------------------------------------

    def get_public_host_keys(self, blacklist=None):
        self.calls.append({"op": "publish_host_keys", "blacklist": list(blacklist)})
        return []

    # -- credentials ------------------------------------------------------

    def setup_user_keys(self, keys, username, options=None):
        self.calls.append(
            {
                "op": "setup_user_keys",
                "user": username,
                "keys": sorted(keys, key=sort_key),
                "options": options or "",
            }
        )


class Guard:
    """`util.SeLinuxGuard`, which has nothing to guard here."""

    def __init__(self, *args, **kwargs):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False


class Datasource:
    def __init__(self, recorder):
        self._recorder = recorder

    def publish_host_keys(self, hostkeys):
        pass


class Distro:
    def __init__(self, default_user, recorder):
        self._default_user = default_user
        self.osfamily = "redhat" if recorder.redhat else "debian"

    def get_default_user(self):
        return self._default_user


class Cloud:
    def __init__(self, distro, datasource, keys):
        self.distro = distro
        self.datasource = datasource
        self._keys = keys

    def get_public_ssh_keys(self):
        return self._keys


def install(recorder):
    """Point every escape from the module at the recorder."""
    cc_ssh.glob.glob = recorder.glob
    cc_ssh.os.path.exists = recorder.exists
    cc_ssh.util.del_file = recorder.del_file
    cc_ssh.util.write_file = recorder.write_file
    cc_ssh.util.ensure_dir = lambda path, **kw: None
    cc_ssh.util.fips_enabled = lambda: recorder.fips
    cc_ssh.util.SeLinuxGuard = Guard
    cc_ssh.ssh_util.append_ssh_config = recorder.append_ssh_config
    cc_ssh.ssh_util.setup_user_keys = recorder.setup_user_keys
    cc_ssh.subp.subp = recorder.subp
    cc_ssh.set_redhat_keyfile_perms = recorder.set_redhat_keyfile_perms
    cc_ssh.get_public_host_keys = recorder.get_public_host_keys
    cc_ssh.sys.stdout = type("Out", (), {"write": staticmethod(recorder.stdout_write)})()


def main(argv):
    cfg = json.loads(argv[1])
    state = json.loads(argv[2])
    default_user = json.loads(argv[3])
    cloud_keys = json.loads(argv[4])

    recorder = Recorder(state)
    real_stdout = sys.stdout
    install(recorder)
    distro = Distro(default_user, recorder)
    cloud = Cloud(distro, Datasource(recorder), cloud_keys)
    out = recorder.calls
    try:
        cc_ssh.handle("ssh", cfg, cloud, [])
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        out = {"error": str(exc)}
    finally:
        # The interpreter flushes `sys.stdout` on the way out, and the stand-in
        # cannot be flushed: leaving it in place turns every run into exit 120.
        sys.stdout = real_stdout
    print(json.dumps(out, indent=1, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
