#!/bin/sh
# Runs one ds-identify implementation against a fixture root and prints
# everything it produced, so the packaged shell script and the Rust port can be
# compared byte for byte.
#
# Usage: dsidentify.sh <impl> <workdir> <root> [args...]
#
# The fixture root is handed over through PATH_ROOT, which every other PATH_*
# in the script defaults to. PATH_RUN* are pointed at a scratch directory that
# is wiped first, because ds-identify short-circuits on a previous result.
#
# Note that `is_disabled` reads a hardcoded /etc/cloud/cloud-init.disabled
# rather than a PATH_ROOT-relative one (docs/COMPAT.md B55), so that path
# cannot be exercised from a fixture. The port reproduces the bug, so the two
# still agree.

set -u

impl=$1
work=$2
root=$3
shift 3

run="$work/run"
rm -rf "$run"
mkdir -p "$run/cloud-init"

PATH_ROOT="$root"
PATH_RUN="$run"
PATH_RUN_CI="$run/cloud-init"
PATH_RUN_CI_CFG="$run/cloud-init/cloud.cfg"
PATH_RUN_DI_RESULT="$run/cloud-init/.ds-identify.result"
DI_LOG="$work/log"
DEBUG_LEVEL="${DEBUG_LEVEL:-2}"
export PATH_ROOT PATH_RUN PATH_RUN_CI PATH_RUN_CI_CFG PATH_RUN_DI_RESULT
export DI_LOG DEBUG_LEVEL

rm -f "$DI_LOG"

# DI_MAIN=print_info writes the collected info to stdout, and the only two
# fields in it that cannot match between two processes are the pids.
{
    "$impl" "$@"
    echo "$?" >"$work/rc"
} | sed -e 's/^pid=[0-9]* ppid=[0-9]*$/pid=<pid> ppid=<ppid>/'
rc=$(cat "$work/rc")

echo "rc=$rc"
echo "--- cloud.cfg"
if [ -f "$PATH_RUN_CI_CFG" ]; then
    cat "$PATH_RUN_CI_CFG"
fi
echo "--- result"
if [ -f "$PATH_RUN_DI_RESULT" ]; then
    cat "$PATH_RUN_DI_RESULT"
fi
echo "--- log"
if [ -f "$DI_LOG" ]; then
    # The uptime and the pids are the only two things in the log that cannot
    # match between two processes.
    sed -e 's/\[up [^]]*\]/[up]/' \
        -e 's/^pid=[0-9]* ppid=[0-9]*$/pid=<pid> ppid=<ppid>/' "$DI_LOG"
fi

exit "$rc"
