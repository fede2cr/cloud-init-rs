#!/usr/bin/env python3
"""`cc_users_groups.handle` reduced to the calls it decides to make.

Usage: usersgroups.py <cfg-json> <default-user-json> <cloud-keys-json>

The module itself does nothing: everything it decides gets carried out by
`cloud.distro.create_group` and `cloud.distro.create_user`, and both of those
would add accounts to whatever machine ran the harness. So both are recorded
instead, in order, which is the whole of the module's observable behaviour —
the two refusals it can raise, the warning it can log, and the exact config
each user is created with.

`<default-user-json>` stands in for `distro.get_default_user()`;
`<cloud-keys-json>` for `cloud.get_public_ssh_keys()`.
"""

import json
import sys

from cloudinit.config import cc_users_groups


def key(name):
    """The name as a mapping key, which is where it came from.

    `_normalize_users` uses whatever the config held as a dict key, so
    `users: [[1, 2]]` really does ask for a user named by the integer 1. The
    port's maps are JSON objects and cannot hold one, so both sides spell it
    the way `json.dumps` spells a non-string key (COMPAT.md deviation 116).
    """
    return name if isinstance(name, str) else next(iter(json.loads(json.dumps({name: 0}))))


class Distro:
    def __init__(self, default_user, calls):
        self._default_user = default_user
        self._calls = calls

    def get_default_user(self):
        return self._default_user

    def create_group(self, name, members=None):
        self._calls.append(
            {"op": "create_group", "name": key(name), "members": list(members or [])}
        )

    def create_user(self, name, **kwargs):
        self._calls.append(
            {"op": "create_user", "name": key(name), "config": kwargs}
        )


class Cloud:
    def __init__(self, distro, keys):
        self.distro = distro
        self._keys = keys

    def get_public_ssh_keys(self):
        return self._keys


def main(argv):
    cfg = json.loads(argv[1])
    default_user = json.loads(argv[2])
    cloud_keys = json.loads(argv[3])
    calls = []
    cloud = Cloud(Distro(default_user, calls), cloud_keys)
    try:
        cc_users_groups.handle("users_groups", cfg, cloud, [])
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        print(
            json.dumps(
                {"error": str(exc)}, indent=1, sort_keys=True
            )
        )
        return 0
    print(json.dumps(calls, indent=1, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
