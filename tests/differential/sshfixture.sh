#!/bin/sh
# Builds the fixture tree the ssh_util "install" cases run against.
#
#   $1  root directory, wiped and recreated
#   $2  sshd_config body; empty means no sshd_config at all
#   $3  extra `sh` commands, run with the root as the working directory
#   $4  identity flavour, default "own"
#
# Both sides of the pair get their own copy, because both mutate it.
#
# The flavour decides who the fixture *says* owns the files this script creates,
# which are always really owned by whoever runs it. That is the only way an
# unprivileged harness can reach the branches of `check_permissions` that ask
# about a path owned by root, by another user, or by a group the user is in:
#
#   own       the runner is alice, so every path is owner-owned
#   root      the runner is called root, so the "owned by root" escape applies
#   group     the runner is carol, and alice is a member of carol's group
#   other     the runner is carol, and alice is in no group of hers
#
# In every flavour but "own", alice's own uid is 4242, so a chown to her fails
# with EPERM -- which both sides then have to handle identically too.
set -eu

ROOT=$1
rm -rf "$ROOT"
mkdir -p "$ROOT/etc/ssh" "$ROOT/home/alice"
chmod 755 "$ROOT/home" "$ROOT/home/alice"

U=$(id -u)
G=$(id -g)

case "${4:-own}" in
own)
    # alice is listed before root on purpose: run as root the two share uid 0,
    # and the reverse lookup has to settle on the name the cases are written
    # against.
    printf 'alice:x:%s:%s:Alice:/home/alice:/bin/sh\nroot:x:0:0:root:/root:/bin/sh\n' \
        "$U" "$G" >"$ROOT/etc/passwd"
    printf 'alice:x:%s:\nroot:x:0:\nsudo:x:4242:alice\n' "$G" >"$ROOT/etc/group"
    ;;
root)
    printf 'root:x:%s:%s:root:/root:/bin/sh\nalice:x:4242:4242:Alice:/home/alice:/bin/sh\n' \
        "$U" "$G" >"$ROOT/etc/passwd"
    printf 'rootgrp:x:%s:\nalicegrp:x:4242:alice\n' "$G" >"$ROOT/etc/group"
    ;;
group)
    printf 'carol:x:%s:%s:Carol:/home/carol:/bin/sh\nalice:x:4242:4242:Alice:/home/alice:/bin/sh\nroot:x:0:0:root:/root:/bin/sh\n' \
        "$U" "$G" >"$ROOT/etc/passwd"
    printf 'carolgrp:x:%s:alice\nalicegrp:x:4242:\nroot:x:0:\n' "$G" >"$ROOT/etc/group"
    ;;
other)
    printf 'carol:x:%s:%s:Carol:/home/carol:/bin/sh\nalice:x:4242:4242:Alice:/home/alice:/bin/sh\nroot:x:0:0:root:/root:/bin/sh\n' \
        "$U" "$G" >"$ROOT/etc/passwd"
    printf 'carolgrp:x:%s:\nalicegrp:x:4242:\nroot:x:0:\n' "$G" >"$ROOT/etc/group"
    ;;
*)
    echo "unknown flavour: ${4:-}" >&2
    exit 2
    ;;
esac

if [ -n "$2" ]; then
    # %b, not %s: the cases carry their newlines as backslash-n, the way every
    # other multi-line argument in this harness does.
    printf '%b' "$2" >"$ROOT/etc/ssh/sshd_config"
fi
if [ -n "$3" ]; then
    (cd "$ROOT" && eval "$3")
fi
exit 0
