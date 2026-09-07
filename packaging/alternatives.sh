#!/bin/sh
# Register both cloud-init implementations with update-alternatives, and switch
# between them.
#
#   sudo sh packaging/alternatives.sh register    # register both, keep Python
#   sudo sh packaging/alternatives.sh rust        # select the port
#   sudo sh packaging/alternatives.sh python      # select the distribution's
#   sudo sh packaging/alternatives.sh unregister  # undo everything
#   sh packaging/alternatives.sh status
#
# This implements PLAN §6.5. It is a script rather than the
# `cloud-init-alternatives` package described there because the registration it
# performs is the same either way, and a script can be reverted by hand on a
# machine that has lost its package manager.
#
# The cloud-init-rs package itself is untouched by this: it installs only under
# /usr/libexec/cloud-init-rs/ and /usr/share/, owns none of the paths below, and
# ships no maintainer script that would run any of this. Switching the default
# is an administrator's deliberate act, never an install's side effect.
set -eu

RS=/usr/libexec/cloud-init-rs

# The five paths that decide which implementation a boot uses. The unit files
# are not in the list on purpose: in 26.1 only cloud-init-main.service names a
# binary, and it names /usr/bin/cloud-init, so switching the master link is
# enough. The local, network and final units all shell out to a socket under
# /run/cloud-init/share.
#
#   <alternatives name>|<system path>|<basename under $RS>
LINKS='cloud-init|/usr/bin/cloud-init|cloud-init
cloud-id|/usr/bin/cloud-id|cloud-id
cloud-init-per|/usr/bin/cloud-init-per|cloud-init-per
ds-identify|/usr/lib/cloud-init/ds-identify|ds-identify
cloud-init-generator|/usr/lib/systemd/system-generators/cloud-init-generator|cloud-init-generator'

# Priorities. Python stays the automatic choice, so an admin who runs
# `update-alternatives --auto cloud-init` gets the distribution back.
PRIO_PYTHON=100
PRIO_RUST=50

# Where the distribution's files go once update-alternatives owns their paths.
SUFFIX=.cloud-init-python

each() { printf '%s\n' "$LINKS"; }
field() { printf '%s' "$1" | cut -d'|' -f"$2"; }

need_root() {
    [ "$(id -u)" -eq 0 ] || {
        echo "$0: must run as root" >&2
        exit 1
    }
}

# --- divert ----------------------------------------------------------------
#
# update-alternatives refuses to manage a path that a package owns, and dpkg
# would fight it on the next cloud-init-base upgrade. Diverting with --rename
# moves each file aside once and tells dpkg to keep writing to the new name
# forever, which is what makes the switch survive an upgrade and what makes
# `unregister` able to put everything back exactly as it was.

divert() {
    each | while read -r spec; do
        path=$(field "$spec" 2)
        # Checked before existence, because a second run finds the path already
        # renamed away and would otherwise call that a missing package.
        if dpkg-divert --list "$path" | grep -q .; then
            continue
        fi
        [ -e "$path" ] || {
            echo "$0: $path is missing; is cloud-init-base installed?" >&2
            exit 1
        }
        dpkg-divert --package cloud-init-rs --quiet \
            --divert "$path$SUFFIX" --rename --add "$path"
    done
}

undivert() {
    each | while read -r spec; do
        path=$(field "$spec" 2)
        dpkg-divert --list "$path" | grep -q . || continue
        # The alternatives symlink has to be gone first, or --rename finds the
        # destination occupied and refuses.
        if [ -L "$path" ]; then rm -f "$path"; fi
        dpkg-divert --package cloud-init-rs --quiet \
            --divert "$path$SUFFIX" --rename --remove "$path"
    done
}

# --- register --------------------------------------------------------------

install_one() {
    # $1 = target directory convention: "python" or "rust"
    master=/usr/bin/cloud-init
    case $1 in
    python)
        target=$master$SUFFIX
        prio=$PRIO_PYTHON
        ;;
    rust)
        target=$RS/cloud-init
        prio=$PRIO_RUST
        ;;
    *) exit 1 ;;
    esac
    [ -x "$target" ] || {
        echo "$0: $target is missing or not executable" >&2
        exit 1
    }

    set -- --install "$master" cloud-init "$target" "$prio"
    for spec in $(each | tail -n +2); do
        name=$(field "$spec" 1)
        path=$(field "$spec" 2)
        base=$(field "$spec" 3)
        case $prio in
        "$PRIO_PYTHON") slave=$path$SUFFIX ;;
        *) slave=$RS/$base ;;
        esac
        [ -x "$slave" ] || {
            echo "$0: $slave is missing or not executable" >&2
            exit 1
        }
        set -- "$@" --slave "$path" "$name" "$slave"
    done
    update-alternatives "$@"
}

register() {
    need_root
    divert
    install_one python
    install_one rust
}

# --- entry point -----------------------------------------------------------

case ${1:-status} in
register)
    register
    echo "registered; run '$0 rust' to switch"
    ;;
rust)
    need_root
    update-alternatives --list cloud-init >/dev/null 2>&1 || register
    update-alternatives --set cloud-init "$RS/cloud-init"
    ;;
python)
    need_root
    update-alternatives --set cloud-init "/usr/bin/cloud-init$SUFFIX"
    ;;
unregister)
    need_root
    update-alternatives --remove-all cloud-init >/dev/null 2>&1 || true
    undivert
    echo "unregistered; /usr/bin/cloud-init is the distribution's again"
    ;;
status)
    update-alternatives --query cloud-init 2>&1 | sed -n '1,6p'
    echo
    each | while read -r spec; do
        path=$(field "$spec" 2)
        printf '%-56s -> %s\n' "$path" "$(readlink -f "$path" 2>/dev/null || echo MISSING)"
    done
    ;;
*)
    echo "usage: $0 {register|rust|python|unregister|status}" >&2
    exit 2
    ;;
esac
