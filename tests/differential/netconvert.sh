#!/bin/sh
# One net-convert run, reduced to comparable text.
#
# Usage: netconvert.sh <impl> <workdir> <config-file> [distro]
#
# Prints the exit code, the tool's own stderr (with the Python logging lines
# stripped, since the port does not reproduce the logging format), and the full
# output tree including file modes and contents. The whole rendered file is
# compared, not a summary of it: the renderer's job is the exact bytes.

set -eu

impl=$1
work=$2
config=$3
distro=${4:-ubuntu}
# "quiet" hides stderr entirely, for the malformed-config cases where upstream
# prints a Python traceback and the port prints a one-line message. The exit
# code and the (empty) output tree still have to agree.
stderr_mode=${5:-show}

out="$work/out"
rm -rf "$out"

set +e
"$impl" devel net-convert \
    -p "$config" -k yaml -d "$out" -D "$distro" -O netplan \
    >"$work/stdout" 2>"$work/stderr"
rc=$?
set -e

printf 'rc=%s\n' "$rc"

printf -- '--- stdout\n'
cat "$work/stdout"

printf -- '--- stderr\n'
if [ "$stderr_mode" = quiet ]; then
    printf '<suppressed>\n'
else
    # Drop `2026-01-01 00:00:00,000 - netplan.py[WARNING]: ...` lines, and the
    # tracebacks upstream prints for a bad config, which carry file paths and
    # line numbers from the Python source.
    sed -e '/^[0-9][0-9]*-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9],[0-9]* - /d' \
        -e '/^Traceback (most recent call last):/,$d' \
        -e "s#$work#<work>#g" \
        -e "s#$config#<config>#g" \
        "$work/stderr"
fi

printf -- '--- tree\n'
if [ -d "$out" ]; then
    find "$out" -mindepth 1 -printf '%M %y %P\n' | sort
fi

printf -- '--- files\n'
if [ -d "$out" ]; then
    find "$out" -type f -printf '%P\n' | sort | while read -r rel; do
        printf '== %s\n' "$rel"
        cat "$out/$rel"
    done
fi
