#!/usr/bin/env python3
"""`ug_util.normalize_users_groups` + `extract_default` for one config.

Usage: ugutil.py <cfg-json> <default-user-json>

`<default-user-json>` stands in for `distro.get_default_user()`, which is the
`system_info.default_user` block; pass `null` for a distro that ships none.
The distro object is a stub because nothing else in `ug_util` touches it.
"""

import json
import sys

from cloudinit.distros import ug_util


class Distro:
    def __init__(self, default_user):
        self._default_user = default_user

    def get_default_user(self):
        return self._default_user


def main(argv):
    cfg = json.loads(argv[1])
    default_user = json.loads(argv[2])
    distro = Distro(default_user)
    try:
        users, groups = ug_util.normalize_users_groups(cfg, distro)
        name, config = ug_util.extract_default(users)
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        print(json.dumps({"error": str(exc)}, indent=1, sort_keys=True))
        return 0
    out = {
        "users": users,
        "groups": groups,
        "default_name": name,
        "default_config": config,
    }
    print(json.dumps(out, indent=1, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
