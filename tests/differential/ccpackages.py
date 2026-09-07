#!/usr/bin/env python3
"""`cc_package_update_upgrade_install.handle` reduced to the commands it picks.

Usage: ccpackages.py <cfg-json> <system-info-json> <state-json>

Running this module for real installs packages on the machine that runs it,
and possibly reboots it. So `subp.subp` is replaced by a recorder and the
ordered list of commands is the comparison — the same arrangement `ccssh.py`
and `ccsetpw.py` use.

Everything above `subp` is upstream's own code: the distro is a real `ubuntu`
(or whatever `<system-info-json>` names), so `Distro.install_packages`,
`_extract_package_by_manager`, `Apt.run_package_command` and
`util.expand_package_list` all really run.

`<state-json>` is what the module reads off the running system:

* `apt` / `snap` — whether `subp.which` finds each manager.
* `all_packages` — `apt-cache pkgnames`, or `null` to skip the availability
  check entirely (which is what an unreadable cache amounts to).
* `snap_hold` — `refresh.hold` out of `snap get system -d`; `"forever"` is the
  one value that stops `snap refresh`.
* `reboot_marker` — which of `REBOOT_FILES` exists, or `null` for neither.

`_fire_reboot` is recorded and returns instead of sleeping for a minute and
raising; the decision to reboot is what is being compared, not the panic that
follows a reboot which did not happen.

Package ORDER is deliberately not compared: upstream builds the argv out of a
`set`, so it differs on every run (upstream bug B72, port deviation 130). The
recorder sorts the package operands of an `install` command; every other word
of every command keeps its position.
"""

import json
import re
import sys

from cloudinit import distros
from cloudinit.config import cc_package_update_upgrade_install as mod
from cloudinit.distros.package_management import apt as apt_mod
from cloudinit.distros.package_management import snap as snap_mod


class Recorder:
    """Every stub writes here, so the order between them is preserved."""

    def __init__(self, snap_hold=None):
        self.calls = []
        self.semaphore = None
        self.snap_hold = snap_hold

    def subp(self, args=None, **kwargs):
        argv = list(args or [])
        # The operands of an `install` are a set upstream; sort them so the
        # comparison is about *which* packages, not about this boot's hash
        # seed. Everything before the subcommand keeps its position.
        for index, word in enumerate(argv):
            if word in ("install", "dist-upgrade", "upgrade"):
                head, tail = argv[: index + 1], argv[index + 1 :]
                argv = head + sorted(tail)
                break
        self.calls.append(
            {
                "op": "subp",
                "argv": argv,
                "env": dict(kwargs.get("update_env") or {}),
                "capture": bool(kwargs.get("capture", True)),
                "semaphore": self.semaphore,
            }
        )
        self.semaphore = None
        if argv == ["snap", "get", "system", "-d"]:
            # `Snap.upgrade_packages` reads `refresh.hold` out of this, and
            # only catches ProcessExecutionError — so the stub has to answer
            # with JSON or the module dies on a JSONDecodeError.
            hold = {} if self.snap_hold is None else {"hold": self.snap_hold}
            return Result(json.dumps({"refresh": hold}))
        return Result()

    def fire_reboot(self, *args, **kwargs):
        self.calls.append({"op": "reboot"})


class Result:
    stderr = ""
    return_code = 0

    def __init__(self, stdout=""):
        self.stdout = stdout


class Runner:
    """`helpers.Runners`, which only `update_package_sources` goes through."""

    def __init__(self, recorder):
        self._recorder = recorder

    def run(self, name, functor, args, freq=None):
        self._recorder.semaphore = [name, freq]
        result = functor(*args) if isinstance(args, (list, tuple)) else functor(args)
        self._recorder.semaphore = None
        return result


class Cloud:
    def __init__(self, distro):
        self.distro = distro


def install(recorder, state):
    apt_available = bool(state.get("apt"))
    snap_available = bool(state.get("snap"))
    all_packages = state.get("all_packages")
    marker = state.get("reboot_marker")

    apt_mod.subp.subp = recorder.subp
    snap_mod.subp.subp = recorder.subp
    apt_mod.Apt.available = lambda self: apt_available
    snap_mod.Snap.available = lambda self: snap_available
    # The real one opens `/var/lib/dpkg/lock-frontend`, which an unprivileged
    # harness cannot. Waiting for a lock is not a decision, so it is skipped.
    apt_mod.Apt._wait_for_apt_command = (
        lambda self, subp_kwargs, timeout=None: recorder.subp(**subp_kwargs)
    )
    if all_packages is None:
        # No cache to consult: nothing is ever reported unavailable.
        apt_mod.Apt.get_unavailable_packages = lambda self, pkglist: []
    else:
        known = set(all_packages)
        apt_mod.Apt.get_all_packages = lambda self: known

    mod._fire_reboot = recorder.fire_reboot
    # Only the two reboot markers are answered from the fixture; anything else
    # keeps the real answer, because this name is shared with the rest of the
    # process.
    real_isfile = mod.os.path.isfile
    mod.os.path.isfile = lambda path: (
        path == marker if path in mod.REBOOT_FILES else real_isfile(path)
    )
    mod.flush_loggers = lambda logger: None


def sort_set_reprs(text):
    """`PackageInstallerError` renders a `set`, so the names inside the braces
    come out in hash order (bug B72). Sort them, the way the port does."""

    def sorted_members(match):
        members = re.findall(r"'[^']*'", match.group(0))
        return "{%s}" % ", ".join(sorted(members))

    return re.sub(r"\{'[^{}]*'\}", sorted_members, text)


def sort_snap_installs(calls):
    """`snap install` is one command per package, so B72's set order shows up
    as the order of the commands rather than of the words inside one. Each run
    of adjacent snap installs is sorted; nothing else moves."""
    out = []
    run = []

    def flush():
        run.sort(key=lambda call: call["argv"])
        out.extend(run)
        del run[:]

    for call in calls:
        if call.get("op") == "subp" and call["argv"][:2] == ["snap", "install"]:
            run.append(call)
        else:
            flush()
            out.append(call)
    flush()
    return out


def main(argv):
    cfg = json.loads(argv[1])
    system_info = json.loads(argv[2])
    state = json.loads(argv[3])

    recorder = Recorder(state.get("snap_hold"))
    install(recorder, state)

    name = system_info.get("distro", "ubuntu")
    distro_cfg = dict(system_info.get("distro_cfg") or {})
    try:
        distro = distros.fetch(name)(name, distro_cfg, None)
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        # `get_apt_wrapper` raises out of `Distro.__init__`, so this is not a
        # module failure at all: cloud-init cannot build its distro object and
        # nothing runs. It is reported as its own kind of outcome, and as a
        # non-zero exit, to keep it apart from the exceptions `handle` catches.
        print(json.dumps({"error": str(exc)}, indent=1, sort_keys=True))
        return 1
    runner = Runner(recorder)
    distro._runner = runner
    # Each manager captured `self._runner` in `Distro.__init__`, which ran
    # before the line above, so they need it too.
    for manager in distro.package_managers:
        manager.runner = runner
    # `DebianDistro.__init__` already built its `Apt` from the cfg above, so
    # the wrapper and the apt-get argv are whatever the fixture asked for.

    out = recorder.calls
    try:
        mod.handle("package_update_upgrade_install", cfg, Cloud(distro), [])
    except Exception as exc:  # noqa: BLE001 - the error text is the output
        out.append({"op": "raise", "error": sort_set_reprs(str(exc))})
    print(
        json.dumps(
            sort_snap_installs(out) if isinstance(out, list) else out,
            indent=1,
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
