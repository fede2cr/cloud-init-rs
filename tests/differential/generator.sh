#!/bin/sh
# Runs one cloud-init-generator implementation against a fixture and prints
# every decision it made, so the packaged shell script and the Rust port can be
# compared byte for byte.
#
# Usage: generator.sh <impl> <workdir> [token...]
#
# The generator has no configuration at all: /run/cloud-init, the ds-identify
# path and the systemd target are baked into it. Rather than add environment
# overrides the original does not have (which would let anything able to set a
# variable in PID 1's environment redirect the decision), the fixture is put
# where the program already looks, using an unprivileged mount namespace:
#
#   unshare -Urm  ->  mount --bind <fixture>/run  /run/cloud-init
#                     mount -t tmpfs none         /usr/lib/cloud-init
#
# Both mounts are private to the child, so the host's real /run/cloud-init is
# untouched no matter what either implementation writes. Inside the namespace
# we are uid 0, so DAC permission checks are not usable to provoke failures;
# where a failure is wanted the fixture uses a wrong file *type* instead (a
# directory where a file is expected), which fails for root too.
#
# Tokens, applied in order, all optional:
#   ds=<n>      ds-identify stub exits <n>
#   ds=missing  no ds-identify at all
#   ds=noexec   ds-identify present but not executable
#   ds=noisy    ds-identify exits 0 after writing to stdout and stderr
#   link=good      pre-existing correct symlink
#   link=stale     pre-existing symlink to a different target
#   link=dangling  pre-existing symlink whose target does not exist
#   link=file      a regular file where the symlink goes
#   wants=yes      create multi-user.target.wants but leave it empty
#   flag=enabled|disabled|both  pre-existing /run/cloud-init flag files
#   log=dir     make the log path a directory, so `: > $LOG_F` fails
#   run=fresh   /run/cloud-init does not exist yet

set -u

impl=$1
work=$2
shift 2

fix="$work/fix"
rm -rf "$fix"
mkdir -p "$fix/run" "$fix/usr" "$fix/early" "$fix/normal" "$fix/late"

wants="$fix/early/multi-user.target.wants"
link="$wants/cloud-init.target"
target=/lib/systemd/system/cloud-init.target

ds_mode=0
run_mode=live

for tok in "$@"; do
    case "$tok" in
        ds=*) ds_mode=${tok#ds=} ;;
        link=good) mkdir -p "$wants" && ln -sf "$target" "$link" ;;
        link=stale) mkdir -p "$wants" && ln -sf /lib/systemd/system/basic.target "$link" ;;
        link=dangling) mkdir -p "$wants" && ln -sf /nonexistent.target "$link" ;;
        link=file) mkdir -p "$wants" && printf 'not a link\n' >"$link" ;;
        wants=yes) mkdir -p "$wants" ;;
        flag=enabled) : >"$fix/run/enabled" ;;
        flag=disabled) : >"$fix/run/disabled" ;;
        flag=both) : >"$fix/run/enabled"; : >"$fix/run/disabled" ;;
        log=dir) mkdir -p "$fix/run/cloud-init-generator.log" ;;
        run=fresh) run_mode=fresh; rmdir "$fix/run" ;;
        *) echo "generator.sh: unknown token $tok" >&2; exit 99 ;;
    esac
done

# The stub stands in for ds-identify. Its own behaviour is compared separately
# by dsidentify.sh; here all that matters is the exit code it hands back and
# whether the generator lets its output through untouched.
case "$ds_mode" in
    missing) ;;
    noexec)
        printf '#!/bin/sh\nexit 0\n' >"$fix/usr/ds-identify"
        chmod 0644 "$fix/usr/ds-identify"
        ;;
    noisy)
        printf '#!/bin/sh\necho stdout-from-ds\necho stderr-from-ds >&2\nexit 0\n' \
            >"$fix/usr/ds-identify"
        chmod 0755 "$fix/usr/ds-identify"
        ;;
    *)
        printf '#!/bin/sh\nexit %s\n' "$ds_mode" >"$fix/usr/ds-identify"
        chmod 0755 "$fix/usr/ds-identify"
        ;;
esac

# `run=fresh` hides the host's /run/cloud-init by covering its parent, which
# also proves the mkdir -p path. Everything else binds the fixture directly.
if [ "$run_mode" = fresh ]; then
    mount_run="mount -t tmpfs none /run"
else
    mount_run="mount --bind '$fix/run' /run/cloud-init"
fi

unshare -Urm sh -c "
    set -e
    $mount_run
    mount -t tmpfs none /usr/lib/cloud-init
    if [ -f '$fix/usr/ds-identify' ]; then
        cp -a '$fix/usr/ds-identify' /usr/lib/cloud-init/ds-identify
    fi
    set +e
    '$impl' '$fix/normal' '$fix/early' '$fix/late'
    echo \"rc=\$?\"
    echo '--- run'
    for f in disabled enabled cloud-init-generator.log; do
        if [ -d \"/run/cloud-init/\$f\" ]; then echo \"\$f: directory\"
        elif [ -e \"/run/cloud-init/\$f\" ]; then echo \"\$f: present\"
        fi
    done
    echo '--- log'
    if [ -f /run/cloud-init/cloud-init-generator.log ]; then
        cat /run/cloud-init/cloud-init-generator.log
    fi
" 2>&1 |
    # argv[0] is the only thing in the output that cannot match between two
    # implementations: the script logs its own path, the port logs its own.
    # The shell's own exec diagnostics go too: when ds-identify cannot be run,
    # dash prints "not found"/"Permission denied" on the generator's stderr and
    # the port stays quiet (docs/COMPAT.md, generator deviations).
    sed -e "s|^[^ ]* normal=|<argv0> normal=|" -e "s|$fix|<fix>|g" \
        -e '/ds-identify: not found$/d' \
        -e '/ds-identify: Permission denied$/d'

echo "--- tree"
# The symlink and its target are the decision that actually reaches systemd.
if [ -d "$wants" ]; then
    if [ -L "$link" ]; then
        printf 'link -> %s\n' "$(readlink "$link")"
    elif [ -f "$link" ]; then
        printf 'link: regular file\n'
    else
        printf 'wants dir, no link\n'
    fi
else
    printf 'no wants dir\n'
fi
