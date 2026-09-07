#!/usr/bin/env python3
"""`cc_set_passwords.handle` reduced to the calls it decides to make.

Usage: ccsetpw.py <cfg-json> <state-json> <default-user-json> <args-json>

Running this module for real changes the passwords on the machine that runs
it, expires them, and restarts `sshd`. So the five escapes it has are stubbed
onto one shared list and recorded in order, and that ordered list is the
comparison.

`Distro.chpasswd` and `Distro.expire_passwd` record their arguments and stop
there. Both can raise for real — `":".join` refuses a non-string, and `subp`
refuses an argv it cannot encode — but the port raises those while *carrying
out* the plan, not while making it, so a stub that raised here would be
comparing two different phases. Those texts are pinned by unit tests instead.

`<state-json>` is what the module reads off the running system:

* `service` — `distro.get_option("ssh_svcname", "ssh")`.
* `systemd` — `distro.uses_systemd()`.
* `updated` — what `update_ssh_config` returns, i.e. whether the file moved.
* `active` — `systemctl show --property ActiveState --value <service>`.

`<default-user-json>` stands in for `distro.get_default_user()`; `<args-json>`
for the extra arguments a `- [set_passwords, ...]` module section carries.
"""

import json
import sys

from cloudinit.config import cc_set_passwords


class Recorder:
    """Every stub writes here, so the order between them is preserved."""

    def __init__(self, state):
        self.calls = []
        self.updated = bool(state.get("updated"))
        self.counter = 0
        # Everything the module parses happens before the first `distro` call,
        # and `handle_ssh_pwauth` is the last thing it does. So this flag says
        # which of the two an escaping exception was: a plan that could not be
        # made, or a step in a plan that was.
        self.reached_pwauth = False

    def rand_user_password(self, pwlen=20):
        # Not random: the comparison is about who got one and where it was
        # announced, and a real draw cannot be compared at all.
        self.counter += 1
        return "<random %d>" % self.counter

    def chpasswd(self, plist_in, hashed):
        self.calls.append(
            {
                "op": "chpasswd",
                "hashed": bool(hashed),
                "entries": [list(pair) for pair in plist_in],
            }
        )

    def expire_passwd(self, user):
        self.calls.append({"op": "expire_passwd", "user": user})

    def multi_log(self, text, **kwargs):
        self.calls.append({"op": "announce_random", "text": text})

    def update_ssh_config(self, updates):
        self.calls.append(
            {"op": "update_ssh_config", "value": updates["PasswordAuthentication"]}
        )
        return self.updated

    def manage_service(self, action, service, *extra_args, **kwargs):
        assert action == "restart", action
        self.calls.append(
            {
                "op": "restart_ssh",
                "service": service,
                "ignore_dependencies": "--job-mode=ignore-dependencies" in extra_args,
            }
        )


class Distro:
    def __init__(self, state, default_user, recorder):
        self._recorder = recorder
        self._service = state.get("service", "ssh")
        self._systemd = bool(state.get("systemd"))
        self._active = state.get("active", "")
        self._default_user = default_user

    def get_option(self, name, default=None):
        return self._service if name == "ssh_svcname" else default

    def uses_systemd(self):
        return self._systemd

    def get_default_user(self):
        return self._default_user

    def chpasswd(self, plist_in, hashed):
        return self._recorder.chpasswd(plist_in, hashed)

    def expire_passwd(self, user):
        return self._recorder.expire_passwd(user)

    def manage_service(self, action, service, *extra_args, **kwargs):
        return self._recorder.manage_service(action, service, *extra_args, **kwargs)


class Cloud:
    def __init__(self, distro):
        self.distro = distro


class Subp:
    """Stands in for `subp.subp`, which only the systemd probe reaches."""

    def __init__(self, active):
        self._active = active

    def __call__(self, cmd, **kwargs):
        assert cmd[:2] == ["systemctl", "show"], cmd
        return Result(self._active)


class Result:
    def __init__(self, stdout):
        self.stdout = stdout


def install(recorder, state):
    cc_set_passwords.rand_user_password = recorder.rand_user_password
    cc_set_passwords.update_ssh_config = recorder.update_ssh_config
    cc_set_passwords.log_util.multi_log = recorder.multi_log
    cc_set_passwords.subp.subp = Subp(state.get("active", ""))

    real = cc_set_passwords.handle_ssh_pwauth

    def handle_ssh_pwauth(pw_auth, distro):
        recorder.reached_pwauth = True
        return real(pw_auth, distro)

    cc_set_passwords.handle_ssh_pwauth = handle_ssh_pwauth


def main(argv):
    cfg = json.loads(argv[1])
    state = json.loads(argv[2])
    default_user = json.loads(argv[3])
    args = json.loads(argv[4])

    recorder = Recorder(state)
    install(recorder, state)
    cloud = Cloud(Distro(state, default_user, recorder))
    out = recorder.calls
    try:
        cc_set_passwords.handle("set_passwords", cfg, cloud, args)
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        if recorder.reached_pwauth:
            # Raised while carrying the plan out, so the steps before it stand.
            out.append({"op": "abort", "error": str(exc)})
        else:
            out = {"error": str(exc)}
    print(json.dumps(out, indent=1, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
