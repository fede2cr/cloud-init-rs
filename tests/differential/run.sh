#!/bin/sh
# Differential test harness (PLAN.md §6.1).
#
# Runs the same read-only command against the packaged Python cloud-init and the
# Rust port, and fails on any stdout/stderr/exit-code divergence. Only read-only
# commands belong here: this script is expected to be safe to run on a live host.
#
# Usage: tests/differential/run.sh [path-to-rust-target-dir]
#
# Selecting a subset
# ------------------
# The whole suite is the gate and nothing is ever deleted from it, but while
# working on one module there is no reason to re-run the other fifty sections.
#
#   ONLY='locale timezone' tests/differential/run.sh     # just those sections
#   SKIP='ec2 ds-identify' tests/differential/run.sh     # everything else
#
# Both take space-separated *substrings* matched against the section names
# printed by `--list`. A deselected section is reported as `skip` with its name
# so a partial run can never be mistaken for a clean full run: the summary line
# says `SUBSET` instead of `all sections`, and CI must run it with neither
# variable set.
#
#   tests/differential/run.sh --list                     # section names only

set -eu

HARNESS_DIR=$(cd "$(dirname "$0")" && pwd)

if [ "${1-}" = "--list" ]; then
    grep '^sec ' "$0" | sed 's/^sec "//; s/"$//'
    exit 0
fi

ONLY="${ONLY:-}"
SKIP="${SKIP:-}"

# Build a copy carrying only the selected sections and run that. Filtering here
# rather than inside `run_pair` matters: a section is not just its comparisons,
# it is also the fixtures, the HTTP servers and the mount namespaces it stands
# up, and skipping only the comparisons would leave all of that running (and
# would leave the section's own post-processing reading files `run_pair` never
# wrote). The copy is written BESIDE this script because many sections resolve
# their Python counterpart through `dirname $0`.
#
# `set -u` is load-bearing: if a selected section turns out to depend on a
# variable an earlier one defined, the subset run dies immediately with an
# unbound-variable error instead of quietly comparing the wrong thing.
if [ -z "${DIFF_SUBSET-}" ] && [ -n "$ONLY$SKIP" ]; then
    selected=
    dropped=0
    for name in $(grep '^sec ' "$0" | sed 's/^sec "//; s/"$//'); do
        want=1
        if [ -n "$ONLY" ]; then
            want=0
            for w in $ONLY; do
                case "$name" in *"$w"*) want=1 ;; esac
            done
        fi
        for w in $SKIP; do
            case "$name" in *"$w"*) want=0 ;; esac
        done
        if [ "$want" = 1 ]; then
            selected="$selected $name "
        else
            dropped=$((dropped + 1))
        fi
    done
    if [ -z "$selected" ]; then
        echo "no section matches ONLY='$ONLY' SKIP='$SKIP'" >&2
        exit 2
    fi

    subset="$HARNESS_DIR/.subset.sh"
    trap 'rm -f "$subset"' EXIT
    awk -v selected="$selected" '
        /^sec "/ {
            name = $0
            sub(/^sec "/, "", name)
            sub(/"[ \t]*$/, "", name)
            insection = 1
            keep = index(selected, " " name " ") > 0
            if (keep) printf "printf %s %s\n", "'\''== %s\\n'\''", "\"" name "\""
        }
        /^#!!END-OF-SECTIONS$/ { insection = 0; keep = 1 }
        (!insection || keep) { print }
    ' "$0" >"$subset"

    printf 'running %s of %s sections\n' \
        "$(grep -c '^sec ' "$subset")" "$(grep -c '^sec ' "$0")"
    DIFF_SUBSET="$dropped" export DIFF_SUBSET
    sh "$subset" "$@"
    status=$?
    exit $status
fi

TARGET="${1:-target/debug}"
PY_CLOUD_INIT="${PY_CLOUD_INIT:-/usr/bin/cloud-init}"
PY_CLOUD_ID="${PY_CLOUD_ID:-/usr/bin/cloud-id}"
# Shared by boot-stages, force and nocloud, so it cannot live inside a section:
# a SKIP= run that drops the definer leaves the others with `set -u` errors.
CI_RS="$(cd "$TARGET" && pwd)/cloud-init"

# The shipped `log_base`, plus a file handler pointed at the work tree. Shared
# by boot-stage-logging and ec2-metadata-crawl, so it is section-free too.
LOG_INI='[loggers]
keys=root,cloudinit

[handlers]
keys=consoleHandler,cloudLogHandler

[formatters]
keys=simpleFormatter,arg0Formatter

[logger_root]
level=DEBUG
handlers=consoleHandler,cloudLogHandler

[logger_cloudinit]
level=DEBUG
qualname=cloudinit
handlers=
propagate=1

[handler_consoleHandler]
class=StreamHandler
level=WARNING
formatter=arg0Formatter
args=(sys.stderr,)

[formatter_arg0Formatter]
format=%(asctime)s - %(filename)s[%(levelname)s]: %(message)s

[formatter_simpleFormatter]
format=[CLOUDINIT] %(filename)s[%(levelname)s]: %(message)s

[handler_cloudLogHandler]
class=FileHandler
level=DEBUG
formatter=arg0Formatter'

if [ ! -x "$PY_CLOUD_INIT" ]; then
    echo "SKIP: python cloud-init not found at $PY_CLOUD_INIT" >&2
    exit 77
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0

# Names the block that follows, for `--list` and for subset selection. It is
# deliberately inert at run time.
sec() {
    :
}

run_pair() {
    label=$1
    py_cmd=$2
    rs_cmd=$3

    # stderr is deliberately not compared: upstream prefixes messages with the
    # Python logging format (timestamp + module.py), which the port does not fake.
    if sh -c "$py_cmd" >"$WORK/py.out" 2>"$WORK/py.err"; then
        echo 0 >"$WORK/py.rc"
    else
        echo $? >"$WORK/py.rc"
    fi
    if sh -c "$rs_cmd" >"$WORK/rs.out" 2>"$WORK/rs.err"; then
        echo 0 >"$WORK/rs.rc"
    else
        echo $? >"$WORK/rs.rc"
    fi

    if cmp -s "$WORK/py.out" "$WORK/rs.out" &&
        cmp -s "$WORK/py.rc" "$WORK/rs.rc"; then
        pass=$((pass + 1))
        printf 'ok   %s\n' "$label"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n' "$label"
        printf '  exit: python=%s rust=%s\n' \
            "$(cat "$WORK/py.rc")" "$(cat "$WORK/rs.rc")"
        diff -u "$WORK/py.out" "$WORK/rs.out" | sed 's/^/  /' || true
    fi
}

# One comparison over a whole matrix: `$4` is a file of tab-separated argument
# lists, each side runs it in a single process, and the two transcripts are
# compared once. A per-case `run_pair` spends about a third of a second per case
# on Python interpreter startup alone, so a few thousand cases cost ten minutes
# of doing nothing; the same matrix in batch mode costs a couple of seconds.
#
# The cost is granularity: one failing case fails the whole matrix. The `## `
# marker each side prints before every record is what keeps that readable --
# the diff names the exact argument list that diverged -- and each dumper still
# takes a single case on the command line for following it up.
run_batch() {
    label=$1
    py_cmd=$2
    rs_cmd=$3
    cases=$4

    printf '%s cases: %s\n' "$(wc -l <"$cases")" "$label"
    run_pair "$label" "$py_cmd '$cases'" "$rs_cmd '$cases'"

    # A case the dumpers cannot parse is refused *identically* by both sides,
    # so it agrees with itself and passes while testing nothing. Every dumper
    # says `must be an object` when that happens, and no real record contains
    # the phrase, so one grep over the transcript catches a quoting mistake in
    # whichever section built the file.
    if grep -q 'must be an object' "$WORK/py.out" 2>/dev/null; then
        fail=$((fail + 1))
        printf 'FAIL %s -- unparsable case(s) in %s\n' "$label" "$cases"
        grep -B1 'must be an object' "$WORK/py.out" | grep '^## ' |
            head -5 | sed 's/^/  /'
    fi
}

cat >"$WORK/instance-data.json" <<'EOF'
{
  "base64_encoded_keys": [],
  "ds": {"meta_data": {"instance-id": "i-abc123", "local-hostname": "host1"}},
  "sensitive_keys": [],
  "v1": {
    "_beta_keys": ["subplatform"],
    "availability_zone": "cn-north-1a",
    "cloud_name": "aws",
    "distro": "ubuntu",
    "instance_id": "i-abc123",
    "local_hostname": "host1",
    "machine": "x86_64",
    "platform": "ec2",
    "python_version": "3.13.0",
    "region": "cn-north-1",
    "subplatform": "metadata (http://169.254.169.254)"
  }
}
EOF
ID="$WORK/instance-data.json"

printf '## template: jinja\nhostname={{ v1.local_hostname }} id={{ ds.meta_data.instance_id }}\n' \
    >"$WORK/user-data"

# --- cloud-init status -------------------------------------------------------
sec "cloud-init-status"
for opts in "" "--long" "--format=json" "--format=yaml" "--format=tabular"; do
    run_pair "cloud-init status $opts" \
        "$PY_CLOUD_INIT status $opts" \
        "$TARGET/cloud-init status $opts"
done

# --- cloud-init query --------------------------------------------------------
sec "cloud-init-query"
for opts in \
    "--all" \
    "v1" \
    "v1.cloud_name" \
    "v1.region" \
    "cloud_name" \
    "ds.meta_data.instance_id" \
    "-l v1" \
    "-l ds" \
    "v1.nope" \
    "nope" \
    "v1.cloud_name.deeper"; do
    run_pair "cloud-init query $opts" \
        "$PY_CLOUD_INIT query -i '$ID' $opts" \
        "$TARGET/cloud-init query -i '$ID' $opts"
done

run_pair "cloud-init query --format" \
    "$PY_CLOUD_INIT query -i '$ID' --format='{{ v1.cloud_name }}/{{ v1.region }}'" \
    "$TARGET/cloud-init query -i '$ID' --format='{{ v1.cloud_name }}/{{ v1.region }}'"

run_pair "cloud-init query --format missing var" \
    "$PY_CLOUD_INIT query -i '$ID' --format='{{ v1.nope }}'" \
    "$TARGET/cloud-init query -i '$ID' --format='{{ v1.nope }}'"

run_pair "cloud-init query no options" \
    "$PY_CLOUD_INIT query -i '$ID' >/dev/null" \
    "$TARGET/cloud-init query -i '$ID' >/dev/null"

# --- cloud-init devel render -------------------------------------------------
sec "cloud-init-devel-render"
run_pair "cloud-init devel render" \
    "$PY_CLOUD_INIT devel render -i '$ID' '$WORK/user-data'" \
    "$TARGET/cloud-init devel render -i '$ID' '$WORK/user-data'"

# --- cloud-init analyze ------------------------------------------------------
sec "cloud-init-analyze"
# A synthetic log exercising both separator dialects, the version banner, the
# asctime and syslog timestamp forms, and two boot records.
cat >"$WORK/cloud-init.log" <<'EOF'
2017-05-22 18:02:01,088 - util.py[DEBUG]: Cloud-init v. 0.7.9 running 'init-local' at Mon, 22 May 2017 18:02:01 +0000. Up 3.0 seconds.
2017-05-22 18:02:01,088 - handlers.py[DEBUG]: start: init-local/check-cache: attempting to read from cache [check]
2017-05-22 18:02:01,240 - handlers.py[DEBUG]: finish: init-local/check-cache: SUCCESS: no cache found
2017-05-22 18:02:01,297 - handlers.py[DEBUG]: start: init-local/search-Ec2Local: searching for local datasources
2017-05-22 18:02:02,731 - handlers.py[DEBUG]: finish: init-local/search-Ec2Local: SUCCESS: found local data from DataSourceEc2Local
2017-05-22 18:02:02,808 - handlers.py[DEBUG]: finish: init-local: SUCCESS: searching for local datasources
May 22 18:02:03 ip-10-0-0-1 [CLOUDINIT] util.py[DEBUG]: Cloud-init v. 0.7.9 running 'init' at Mon, 22 May 2017 18:02:03 +0000. Up 5.0 seconds.
May 22 18:02:03 ip-10-0-0-1 [CLOUDINIT] handlers.py[DEBUG]: start: init-network/check-cache: attempting to read from cache [trust]
May 22 18:02:04 ip-10-0-0-1 [CLOUDINIT] handlers.py[DEBUG]: finish: init-network/check-cache: SUCCESS: restored from cache
May 22 18:02:04 ip-10-0-0-1 [CLOUDINIT] handlers.py[DEBUG]: finish: init-network: SUCCESS: searching for network datasources
2017-05-22 18:02:05,001 - util.py[DEBUG]: Cloud-init v. 0.7.9 running 'modules:config' at Mon, 22 May 2017 18:02:05 +0000. Up 7.0 seconds.
2017-05-22 18:02:05,002 - handlers.py[DEBUG]: start: modules-config/config-snappy: running config-snappy with frequency once-per-instance
2017-05-22 18:02:05,500 - handlers.py[DEBUG]: finish: modules-config/config-snappy: SUCCESS: config-snappy ran successfully
2017-05-22 18:02:05,600 - handlers.py[DEBUG]: finish: modules-config: SUCCESS: running modules for config
this line does not parse at all
2017-05-22 18:02:06,000 - util.py[DEBUG]: Cloud-init v. 0.7.9 running 'modules:final' at Mon, 22 May 2017 18:02:06 +0000. Up 8.0 seconds.
2017-05-22 18:02:06,100 - handlers.py[DEBUG]: start: modules-final/config-scripts-user: running config-scripts-user with frequency once-per-instance
2017-05-22 18:02:06,900 - handlers.py[DEBUG]: finish: modules-final/config-scripts-user: SUCCESS: config-scripts-user ran successfully
2017-05-22 18:02:07,000 - handlers.py[DEBUG]: finish: modules-final: SUCCESS: running modules for final
2017-05-22 19:00:00,000 - util.py[DEBUG]: Cloud-init v. 0.7.9 running 'init-local' at Mon, 22 May 2017 19:00:00 +0000. Up 3.0 seconds.
2017-05-22 19:00:00,100 - handlers.py[DEBUG]: start: init-local/check-cache: attempting to read from cache [check]
2017-05-22 19:00:00,400 - handlers.py[DEBUG]: finish: init-local/check-cache: SUCCESS: no cache found
2017-05-22 19:00:00,900 - handlers.py[DEBUG]: finish: init-local: SUCCESS: searching for local datasources
EOF
LOG="$WORK/cloud-init.log"

for sub in dump blame show boot; do
    run_pair "cloud-init analyze $sub" \
        "$PY_CLOUD_INIT analyze $sub -i '$LOG'" \
        "$TARGET/cloud-init analyze $sub -i '$LOG'"
done

run_pair "cloud-init analyze show --format" \
    "$PY_CLOUD_INIT analyze show -i '$LOG' -f '%n|%e|%d|%D|%c'" \
    "$TARGET/cloud-init analyze show -i '$LOG' -f '%n|%e|%d|%D|%c'"

run_pair "cloud-init analyze show bad format key" \
    "$PY_CLOUD_INIT analyze show -i '$LOG' -f '%z'" \
    "$TARGET/cloud-init analyze show -i '$LOG' -f '%z'"

run_pair "cloud-init analyze dump from stdin" \
    "$PY_CLOUD_INIT analyze dump -i - <'$LOG'" \
    "$TARGET/cloud-init analyze dump -i - <'$LOG'"

run_pair "cloud-init analyze blame from JSON events" \
    "$PY_CLOUD_INIT analyze dump -i '$LOG' | $PY_CLOUD_INIT analyze blame -i -" \
    "$TARGET/cloud-init analyze dump -i '$LOG' | $TARGET/cloud-init analyze blame -i -"

run_pair "cloud-init analyze blame missing file" \
    "$PY_CLOUD_INIT analyze blame -i '$WORK/nope.log'" \
    "$TARGET/cloud-init analyze blame -i '$WORK/nope.log'"

: >"$WORK/empty.log"
run_pair "cloud-init analyze blame empty file" \
    "$PY_CLOUD_INIT analyze blame -i '$WORK/empty.log'" \
    "$TARGET/cloud-init analyze blame -i '$WORK/empty.log'"

if [ -r /var/log/cloud-init.log ]; then
    for sub in dump blame show boot; do
        run_pair "cloud-init analyze $sub (host log)" \
            "$PY_CLOUD_INIT analyze $sub -i /var/log/cloud-init.log" \
            "$TARGET/cloud-init analyze $sub -i /var/log/cloud-init.log"
    done
fi

# --- cloud-init devel make-mime ----------------------------------------------
sec "cloud-init-devel-make-mime"
# The MIME boundary is a random 19-digit token on both sides, so it is masked
# before comparison. Everything else must match byte for byte.
printf '#cloud-config\nruncmd: [echo hi]\n' >"$WORK/c.yaml"
printf '#!/bin/sh\necho hi\n' >"$WORK/s.sh"
: >"$WORK/empty.txt"
printf '#cloud-config\n%s\n' "$(printf 'x%.0s' $(seq 200))" >"$WORK/big.txt"
MASK="sed 's/=\{15\}[0-9]\{19\}==/BOUNDARY/g'"

for opts in \
    "-l" \
    "" \
    "-a $WORK/c.yaml:cloud-config" \
    "-a $WORK/c.yaml:cloud-config -a $WORK/s.sh:x-shellscript" \
    "-a $WORK/empty.txt:cloud-config" \
    "-a $WORK/big.txt:cloud-config" \
    "-a $WORK/c.yaml:bogus-type" \
    "-a $WORK/c.yaml:bogus-type -f" \
    "-a $WORK/nope.yaml:cloud-config" \
    "-a nocolon"; do
    run_pair "cloud-init devel make-mime $opts" \
        "$PY_CLOUD_INIT devel make-mime $opts | $MASK" \
        "$TARGET/cloud-init devel make-mime $opts | $MASK"
done

# --- collect-logs ------------------------------------------------------------
sec "collect-logs"
# Only the refusal path is exercised: the real collection needs root, writes a
# tarball, and shells out to journalctl, none of which belong in a harness that
# must be safe to run on a live host. Root-path parity is covered by the unit
# tests in crates/cloud-init/src/cmd/collect_logs.rs and was verified by hand
# against the Python implementation inside a user namespace.
for opts in "" "-t $WORK/logs.tar.gz" "-r" "-u" "-r -t $WORK/logs.tar.gz"; do
    run_pair "cloud-init collect-logs $opts" \
        "$PY_CLOUD_INIT collect-logs $opts" \
        "$TARGET/cloud-init collect-logs $opts"
done

# --- schema -------------------------------------------------------------------
sec "schema"
# Fixtures live under $WORK and are referenced by absolute path, which both
# implementations echo back verbatim.
SCHEMA_DIR="$WORK/schema"
mkdir -p "$SCHEMA_DIR"
printf '#cloud-config\nruncmd:\n  - echo hi\n' >"$SCHEMA_DIR/good.yaml"
printf '#cloud-config\nruncmd: 5\nbogus_key_here: 1\n' >"$SCHEMA_DIR/bad.yaml"
: >"$SCHEMA_DIR/empty.yaml"
printf 'runcmd:\n  - echo hi\n' >"$SCHEMA_DIR/noheader.yaml"
printf '#cloud-config\n' >"$SCHEMA_DIR/headeronly.yaml"
printf '#!/bin/sh\necho hi\n' >"$SCHEMA_DIR/script.sh"
printf '#cloud-config\napt_reboot_if_required: true\n' >"$SCHEMA_DIR/deprecated.yaml"
printf '#cloud-config\nusers:\n  - name: u\n    expiredate: nope\n' \
    >"$SCHEMA_DIR/baddate.yaml"
printf '#cloud-config\npackages: [git, curl]\nssh_pwauth: true\n' \
    >"$SCHEMA_DIR/multi.yaml"
# YAML 1.1 scalar resolution: `yes` is a boolean and `0600` is octal to PyYAML,
# and `<<` merges. Getting any of these wrong changes a setting's type.
printf '#cloud-config\nssh_pwauth: yes\npackage_update: no\nssh_deletekeys: off\n' \
    >"$SCHEMA_DIR/yaml11.yaml"
printf '#cloud-config\nwrite_files:\n  - path: /a\n    permissions: 0600\n  - path: /b\n    permissions: 0o600\n' \
    >"$SCHEMA_DIR/octal.yaml"
printf '#cloud-config\n_base: &b\n  owner: root\nwrite_files:\n  - <<: *b\n    path: /a\n' \
    >"$SCHEMA_DIR/merge.yaml"

# --annotate is exercised only where upstream survives it: it dies with an
# unhandled KeyError on root-level errors and on errors nested under a list item
# (docs/COMPAT.md B12, B13), so headeronly.yaml is checked without it.
for f in good.yaml bad.yaml empty.yaml noheader.yaml script.sh deprecated.yaml \
    baddate.yaml multi.yaml yaml11.yaml octal.yaml merge.yaml; do
    for opts in "" "--annotate"; do
        run_pair "cloud-init schema -c $f $opts" \
            "$PY_CLOUD_INIT schema -c '$SCHEMA_DIR/$f' $opts" \
            "$TARGET/cloud-init schema -c '$SCHEMA_DIR/$f' $opts"
    done
done
for extra in "-c $SCHEMA_DIR/headeronly.yaml" "-c $SCHEMA_DIR/missing.yaml" ""; do
    run_pair "cloud-init schema $extra" \
        "$PY_CLOUD_INIT schema $extra" \
        "$TARGET/cloud-init schema $extra"
done

# --- clean --------------------------------------------------------------------
sec "clean"
# `clean` deletes files, so every case is redirected at a throwaway cloud_dir
# under $WORK via CLOUD_CFG. The flags that act *before* the cloud_dir check --
# -l and -c -- are never exercised here: they reach absolute paths like
# /var/log/cloud-init.log and /etc/netplan, which a harness that must be safe to
# run on a live host cannot touch. Those paths are covered by the unit tests in
# crates/cloud-init/src/cmd/clean.rs.
CLEAN_DIR="$WORK/clean"
mkdir -p "$CLEAN_DIR"
printf 'system_info:\n  paths:\n    cloud_dir: %s/absent\n    run_dir: %s/run\n' \
    "$CLEAN_DIR" "$CLEAN_DIR" >"$CLEAN_DIR/missing.cfg"
for opts in "" "-s"; do
    run_pair "cloud-init clean $opts (already cleaned)" \
        "CLOUD_CFG=$CLEAN_DIR/missing.cfg $PY_CLOUD_INIT clean $opts" \
        "CLOUD_CFG=$CLEAN_DIR/missing.cfg $TARGET/cloud-init clean $opts"
done

# A populated cloud_dir, one copy per implementation, so both start from the
# same tree. stdout and the exit code go through run_pair; the resulting trees
# are compared afterwards, which is where this case earns its keep.
for opts in "" "-s"; do
    for impl in py rs; do
        root="$CLEAN_DIR/$impl"
        rm -rf "$root"
        mkdir -p "$root/cloud/instances/i-1" "$root/cloud/seed/nocloud" "$root/run"
        printf 'x\n' >"$root/cloud/instances/i-1/datasource"
        printf 'x\n' >"$root/cloud/seed/nocloud/meta-data"
        printf 'x\n' >"$root/cloud/data"
        ln -s instances/i-1 "$root/cloud/instance"
        printf 'system_info:\n  paths:\n    cloud_dir: %s/cloud\n    run_dir: %s/run\n' \
            "$root" "$root" >"$root/cloud.cfg"
    done
    run_pair "cloud-init clean $opts (populated)" \
        "CLOUD_CFG=$CLEAN_DIR/py/cloud.cfg $PY_CLOUD_INIT clean $opts" \
        "CLOUD_CFG=$CLEAN_DIR/rs/cloud.cfg $TARGET/cloud-init clean $opts"

    label="cloud-init clean $opts (resulting tree)"
    if diff -r "$CLEAN_DIR/py/cloud" "$CLEAN_DIR/rs/cloud" >"$WORK/tree.diff" 2>&1; then
        pass=$((pass + 1))
        printf 'ok   %s\n' "$label"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n' "$label"
        sed 's/^/  /' "$WORK/tree.diff"
    fi
done

# --- user-data processing ----------------------------------------------------
sec "user-data-processing"
# Not a CLI comparison: user-data walking is a library layer, so both sides are
# driven through a dumper that prints the resulting part list as JSON. The Rust
# side is an example rather than a shipped binary, so skip if it wasn't built.
UD_PY="$(cd "$(dirname "$0")" && pwd)/userdata.py"
UD_RS="$TARGET/examples/dump-userdata"
if [ -x "$UD_RS" ] && python3 -c 'import cloudinit.user_data' 2>/dev/null; then
    UD_RS="$(cd "$(dirname "$UD_RS")" && pwd)/dump-userdata"
    UD="$WORK/userdata"
    mkdir -p "$UD"

    printf '#cloud-config\nruncmd: [echo hi]\n' >"$UD/cloud-config"
    printf '#!/bin/sh\necho hi\n' >"$UD/shellscript"
    printf 'just some plain text\n' >"$UD/unrecognised"
    printf '' >"$UD/empty"
    printf '#cloud-config\nlaunch-index: 3\nruncmd: []\n' >"$UD/launch-index"
    printf '#cloud-config-archive\n- |\n  runcmd: []\n' >"$UD/archive-untyped"
    printf '#cloud-config-archive\nnot: a list\n' >"$UD/archive-scalar"
    gzip -c "$UD/cloud-config" >"$UD/gzipped"
    cat >"$UD/archive" <<'EOF'
#cloud-config-archive
- type: text/cloud-config
  content: |
    #cloud-config
    runcmd: []
- filename: run.sh
  content: |
    #!/bin/sh
    echo hi
- |
  #!/bin/sh
  echo bare
EOF
    "$TARGET/cloud-init" devel make-mime \
        -a "$UD/cloud-config:cloud-config" \
        -a "$UD/shellscript:x-shellscript" >"$UD/multipart"

    # Cases the hand-written parser is most likely to get wrong: nesting,
    # transfer encodings, and gzip parts whose launch-index is only visible
    # after decompression.
    python3 - "$UD" <<'EOF'
import base64, gzip, os, sys

out = sys.argv[1]


def write(name, text):
    with open(os.path.join(out, name), "w") as fh:
        fh.write(text)


def b64(data):
    return base64.encodebytes(data).decode()


write(
    "nested",
    'MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary="OUT"\n\n'
    '--OUT\nContent-Type: multipart/mixed; boundary="IN"\n\n'
    "--IN\nContent-Type: text/cloud-config\n\n#cloud-config\nruncmd: []\n"
    "--IN--\n"
    "--OUT\nContent-Type: text/x-shellscript\n\n#!/bin/sh\necho outer\n"
    "--OUT--\n",
)
write(
    "quoted-printable",
    'MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary="B"\n\n'
    "--B\nContent-Type: text/cloud-config\n"
    "Content-Transfer-Encoding: quoted-printable\n\n"
    "#cloud-config\nrunc=\nmd: [echo =3D]\n"
    "--B--\n",
)
write(
    "part-headers",
    'MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary="B"\n\n'
    "--B\nContent-Type: text/x-shellscript\n"
    'Content-Disposition: attachment; filename="mine.sh"\n'
    "Launch-Index: 7\n\n#cloud-config\nruncmd: []\n"
    "--B\nContent-Type: text/plain\n\n#cloud-boothook\necho boot\n"
    "--B--\n",
)
write(
    "gzip-part",
    'MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary="B"\n\n'
    "--B\nContent-Type: application/x-gzip\n"
    "Content-Transfer-Encoding: base64\n\n"
    + b64(gzip.compress(b"#cloud-config\nlaunch-index: 4\nruncmd: []\n"))
    + "--B--\n",
)
write(
    "gzip-corrupt",
    'MIME-Version: 1.0\nContent-Type: multipart/mixed; boundary="B"\n\n'
    "--B\nContent-Type: application/x-gzip\n\nnot actually gzip\n--B--\n",
)
write(
    "archive-headers",
    "#cloud-config-archive\n"
    '- content: "#cloud-config\\nlaunch-index: 9\\nruncmd: []\\n"\n'
    "  X-Custom: hello\n"
    "- type: text/plain\n"
    "  content: nothing special\n",
)
# The archive builds its parts by hand, so the charset and transfer encoding
# it picks are visible in `user-data.txt.i`: us-ascii/7bit for plain text,
# utf-8/base64 once a byte is not ASCII, and neither for a non-text type.
write(
    "archive-non-ascii",
    "#cloud-config-archive\n"
    '- content: "#cloud-config\\nruncmd: [ \'echo caf\u00e9\' ]\\n"\n',
)
write(
    "archive-binary",
    "#cloud-config-archive\n- type: application/octet-stream\n  content: abc\n",
)
# `launch-index` is added once by _explode_archive and again when the part is
# attached, so the header appears twice.
write(
    "archive-launch-index",
    "#cloud-config-archive\n"
    '- content: "#cloud-config\\nruncmd: []\\n"\n'
    "  filename: foo.sh\n"
    "  launch-index: 4\n",
)
# `#include` targets: the fetched document replaces the include part, so these
# also cover a fetched document that is itself an include.
write("include-file", "#include\n%s/cloud-config\n" % out)
write("include-file-url", "#include\nfile://%s/cloud-config\n" % out)
write("include-missing", "#include\nfile:///nonexistent/seed\n")
write("include-comments", "#include\n# a note\n\n%s/cloud-config\n" % out)
write("include-two", "#include\n%s/cloud-config\n%s/shellscript\n" % (out, out))
write("include-nested", "#include\n%s/include-file\n" % out)
write("include-archive", "#include\n%s/archive\n" % out)
write("include-empty-target", "#include\n%s/empty\n" % out)
write("include-uppercase", "#INCLUDE\n%s/cloud-config\n" % out)
write("include-url-on-marker-line", "#include %s/cloud-config\n" % out)
write("include-once-then-include", "#include-once %s/cloud-config\n#include %s/shellscript\n" % (out, out))
# `splitlines`, not `split("\n")`: these are two URLs upstream.
write("include-sep-cr", "#include\n%s/cloud-config\r%s/shellscript\n" % (out, out))
write("include-sep-vt", "#include\n%s/cloud-config\v%s/shellscript\n" % (out, out))
write("include-sep-nel", "#include\n%s/cloud-config\u0085%s/shellscript\n" % (out, out))
EOF

    for fixture in "$UD"/*; do
        name=$(basename "$fixture")
        # Python must not run from inside the cloudinit package directory.
        run_pair "user-data $name" \
            "cd /tmp && python3 '$UD_PY' <'$fixture'" \
            "cd /tmp && '$UD_RS' <'$fixture'"
        # The same walk, but comparing the accumulated MIME message rather than
        # the part list: this is what `Init.update` writes to `user-data.txt.i`,
        # so it has to match byte for byte once the random boundary is masked.
        run_pair "user-data $name (mime)" \
            "cd /tmp && python3 '$UD_PY' --mime <'$fixture'" \
            "cd /tmp && '$UD_RS' --mime <'$fixture'"
    done

    # `#include-once` writes what it fetched under the instance data directory
    # and reads it back on the next walk. The cache file is named after the MD5
    # of the URL, so the second pass only agrees if both sides hash it alike.
    ONCE="$WORK/include-once"
    mkdir -p "$ONCE/py" "$ONCE/rs"
    printf '#include-once\n%s/cloud-config\n' "$UD" >"$ONCE/blob"
    for walk in first second; do
        run_pair "user-data include-once ($walk walk)" \
            "cd /tmp && python3 '$UD_PY' '$ONCE/py' <'$ONCE/blob'" \
            "cd /tmp && '$UD_RS' '$ONCE/rs' <'$ONCE/blob'"
    done
    label="user-data include-once (url cache)"
    if diff -r "$ONCE/py" "$ONCE/rs" >"$WORK/once.diff" 2>&1; then
        pass=$((pass + 1))
        printf 'ok   %s\n' "$label"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n' "$label"
        sed 's/^/  /' "$WORK/once.diff"
    fi

    # Against a real server, because the port's HTTP client is hand-written
    # rather than a `requests` equivalent.
    HTTPD="$WORK/httpd"
    mkdir -p "$HTTPD/root/sub" "$HTTPD/fixtures"
    printf '#cloud-config\nruncmd: [served]\n' >"$HTTPD/root/seed"
    printf '#!/bin/sh\necho served\n' >"$HTTPD/root/sub/script"
    python3 "$(dirname "$UD_PY")/httpd.py" "$HTTPD/root" >"$HTTPD/port" 2>/dev/null &
    httpd_pid=$!
    port=""
    tries=0
    while [ -z "$port" ] && [ "$tries" -lt 50 ]; do
        port=$(cat "$HTTPD/port" 2>/dev/null || true)
        [ -n "$port" ] || sleep 0.1
        tries=$((tries + 1))
    done
    if [ -n "$port" ]; then
        printf '#include\nhttp://127.0.0.1:%s/seed\n' "$port" >"$HTTPD/fixtures/include-http"
        printf '#include\n127.0.0.1:%s/seed\n' "$port" >"$HTTPD/fixtures/include-http-no-scheme"
        printf '#include\nhttp://127.0.0.1:%s/sub/script\n' "$port" \
            >"$HTTPD/fixtures/include-http-script"
        # A directory without its trailing slash: served as a 301.
        printf '#include\nhttp://127.0.0.1:%s/sub\n' "$port" \
            >"$HTTPD/fixtures/include-http-redirect"
        for fixture in "$HTTPD"/fixtures/*; do
            run_pair "user-data $(basename "$fixture")" \
                "cd /tmp && python3 '$UD_PY' <'$fixture'" \
                "cd /tmp && '$UD_RS' <'$fixture'"
        done
    else
        printf 'skip user-data include over http (no server)\n'
    fi
    kill "$httpd_pid" 2>/dev/null || true
    wait "$httpd_pid" 2>/dev/null || true

    # Over TLS. Upstream reaches OpenSSL through `requests`; the port reaches
    # the same OpenSSL through the `openssl` crate. Both are pointed at a
    # throwaway CA that lives and dies with the work directory.
    CERTS="$WORK/certs"
    HTTPS="$WORK/httpsd"
    if sh "$(dirname "$UD_PY")/mkcerts.sh" "$CERTS" 2>/dev/null; then
        mkdir -p "$HTTPS/root" "$HTTPS/withcert/data/ssl" "$HTTPS/nocert"
        printf '#cloud-config\nruncmd: [tls]\n' >"$HTTPS/root/seed"
        cp "$CERTS/clientpair.pem" "$HTTPS/withcert/data/ssl/cert.pem"
        # Both sides read their CA out of the environment; each ignores the
        # other's variable.
        TRUST="SSL_CERT_FILE='$CERTS/ca.pem' REQUESTS_CA_BUNDLE='$CERTS/ca.pem'"

        start_https() { # cert key [ca]
            rm -f "$HTTPS/port"
            # shellcheck disable=SC2086
            python3 "$(dirname "$UD_PY")/httpsd.py" "$HTTPS/root" "$1" "$2" ${3-} \
                >"$HTTPS/port" 2>/dev/null &
            https_pid=$!
            https_port=""
            tries=0
            while [ -z "$https_port" ] && [ "$tries" -lt 50 ]; do
                https_port=$(cat "$HTTPS/port" 2>/dev/null || true)
                [ -n "$https_port" ] || sleep 0.1
                tries=$((tries + 1))
            done
        }

        https_case() { # label env url [cloud_dir]
            printf '#include\n%s\n' "$3" >"$HTTPS/blob"
            run_pair "user-data $1" \
                "cd /tmp && $2 python3 '$UD_PY' ${4-} <'$HTTPS/blob'" \
                "cd /tmp && $2 '$UD_RS' ${4-} <'$HTTPS/blob'"
        }

        # A certificate that matches, signed by a CA the client is given.
        start_https "$CERTS/server.pem" "$CERTS/server.key" "$CERTS/ca.pem"
        if [ -n "$https_port" ]; then
            base="https://127.0.0.1:$https_port"
            https_case "include-https" "$TRUST" "$base/seed"
            # The same server, with nothing told to trust the CA.
            https_case "include-https-unknown-ca" "" "$base/seed"
            # `fetch_ssl_details` finds a client certificate under the
            # instance data directory; the server echoes the name it saw.
            https_case "include-https-client-cert" "$TRUST" "$base/whoami" \
                "'$HTTPS/withcert'"
            https_case "include-https-no-client-cert" "$TRUST" "$base/whoami" \
                "'$HTTPS/nocert'"
        else
            printf 'skip user-data include over https (no server)\n'
        fi
        kill "$https_pid" 2>/dev/null || true
        wait "$https_pid" 2>/dev/null || true

        # Signed by the trusted CA, but issued for somebody else.
        start_https "$CERTS/wrongname.pem" "$CERTS/wrongname.key"
        if [ -n "$https_port" ]; then
            https_case "include-https-wrong-name" "$TRUST" \
                "https://127.0.0.1:$https_port/seed"
        fi
        kill "$https_pid" 2>/dev/null || true
        wait "$https_pid" 2>/dev/null || true

        # The right name, self-signed: what a machine-in-the-middle looks like.
        start_https "$CERTS/untrusted.pem" "$CERTS/untrusted.key"
        if [ -n "$https_port" ]; then
            https_case "include-https-self-signed" "$TRUST" \
                "https://127.0.0.1:$https_port/seed"
        fi
        kill "$https_pid" 2>/dev/null || true
        wait "$https_pid" 2>/dev/null || true

        # Trusted issuer, right name, but the issuer has no `keyUsage`: only a
        # client verifying strictly notices.
        start_https "$CERTS/sloppy.pem" "$CERTS/sloppy.key"
        if [ -n "$https_port" ]; then
            https_case "include-https-sloppy-ca" \
                "SSL_CERT_FILE='$CERTS/sloppyca.pem' REQUESTS_CA_BUNDLE='$CERTS/sloppyca.pem'" \
                "https://127.0.0.1:$https_port/seed"
        fi
        kill "$https_pid" 2>/dev/null || true
        wait "$https_pid" 2>/dev/null || true
    else
        printf 'skip user-data include over https (no openssl)\n'
    fi
fi

# --- part handlers -----------------------------------------------------------
sec "part-handlers"
# Compares what each handler writes to disk: path, mode and content. Boot hooks
# are written but never executed on either side, so this stays safe to run on a
# live host.
HD_PY="$(cd "$(dirname "$0")" && pwd)/handlers.py"
HD_RS="$TARGET/examples/dump-handlers"
if [ -x "$HD_RS" ] && python3 -c 'import cloudinit.handlers.jinja_template' 2>/dev/null; then
    HD_RS="$(cd "$(dirname "$HD_RS")" && pwd)/dump-handlers"
    HD="$WORK/handlers"
    mkdir -p "$HD"

    printf '#!/bin/sh\necho hello\n' >"$HD/shellscript"
    printf '#cloud-boothook\n#!/bin/sh\necho boot\n' >"$HD/boothook"
    printf '#cloud-boothook   \n\n\n#!/bin/sh\n' >"$HD/boothook-blanks"
    printf '#cloud-boothook' >"$HD/boothook-bare"
    printf '#!/bin/sh\r\necho crlf\r\n' >"$HD/crlf"
    printf '## template: jinja\n#!/bin/sh\necho {{ v1.greeting }}\n' >"$HD/jinja"
    printf '## template: jinja\n#!/bin/sh\necho {{ nope }}\n' >"$HD/jinja-missing"
    printf '## template: jinja\nplain {{ v1.greeting }}\n' >"$HD/jinja-unknown"
    printf '## template: jinja\n' >"$HD/jinja-empty"
    gzip -c "$HD/shellscript" >"$HD/gzipped"
    cat >"$HD/archive" <<'EOF'
#cloud-config-archive
- type: text/x-shellscript
  filename: arch.sh
  content: |
    #!/bin/sh
    echo arch
- type: text/cloud-boothook
  content: |
    #cloud-boothook
    #!/bin/sh
EOF

    # Filenames needing sanitising, per-frequency scripts, and a jinja template
    # that renders into a different handler's type.
    python3 - "$HD" <<'EOF'
import os, sys
from email.mime.multipart import MIMEMultipart
from email.mime.text import MIMEText

out = sys.argv[1]


def write(name, text):
    with open(os.path.join(out, name), "w") as fh:
        fh.write(text)


def multipart(parts):
    msg = MIMEMultipart()
    for subtype, body, filename in parts:
        part = MIMEText(body, subtype)
        if filename is not None:
            part.add_header(
                "Content-Disposition", "attachment", filename=filename
            )
        msg.attach(part)
    return msg.as_string()


write(
    "byfreq",
    multipart(
        [
            ("x-shellscript-per-boot", "#!/bin/sh\necho boot\n", "b.sh"),
            ("x-shellscript-per-instance", "#!/bin/sh\necho inst\n", "i.sh"),
            ("x-shellscript-per-once", "#!/bin/sh\necho once\n", "o.sh"),
        ]
    ),
)
write(
    "dirtyname",
    multipart([("x-shellscript", "#!/bin/sh\necho x\n", "../../ev il/x?.sh")]),
)
# A non-ASCII filename is RFC 2231 encoded by the sender and mostly stripped by
# clean_filename, which is exactly the interaction worth pinning down.
write(
    "utf8name",
    multipart([("x-shellscript", "#!/bin/sh\necho u\n", "\u00e9\u00e0.sh")]),
)
write("no-filename", multipart([("x-shellscript", "#!/bin/sh\necho n\n", None)]))
write(
    "jinja-to-boothook",
    "## template: jinja\n#cloud-boothook\n#!/bin/sh\necho {{ v1.greeting }}\n",
)

# Cloud-config parts are folded into one document at CONTENT_END, so what is
# being compared is the merged file: its header comments, the merge strategy
# that produced it, and which rejected parts are named in it.
write("cloudconfig", "#cloud-config\nfoo: bar\nlist: [1, 2]\n")
write("cloudconfig-empty", "#cloud-config\n")
write("cloudconfig-comment-only", "#cloud-config\n# nothing but comments\n")
write("cloudconfig-list", "#cloud-config\n- a\n- b\n")
write("cloudconfig-scalar", "#cloud-config\njust a string\n")
write("cloudconfig-unparseable", "#cloud-config\nfoo: [unclosed\n")
write("cloudconfig-unicode", "#cloud-config\ncaf\u00e9: \u65e5\u672c\n")


def cc(body, subtype="cloud-config", filename=None, headers=()):
    part = MIMEText(body, subtype)
    if filename is not None:
        part.add_header("Content-Disposition", "attachment", filename=filename)
    for name, value in headers:
        part.add_header(name, value)
    return part


def multi(parts):
    msg = MIMEMultipart()
    for part in parts:
        msg.attach(part)
    return msg.as_string()


# Dictionaries replace by default here, rather than merging as they do
# everywhere else, so the second part's `foo` wins outright.
write(
    "cloudconfig-two",
    multi(
        [
            cc("#cloud-config\nfoo: {a: 1}\nlist: [1]\nonly1: yes\n", filename="a.yaml"),
            cc("#cloud-config\nfoo: {b: 2}\nlist: [2]\nonly2: yes\n", filename="b.yaml"),
        ]
    ),
)
write(
    "cloudconfig-merge-how",
    multi(
        [
            cc("#cloud-config\nfoo: {a: 1}\nlist: [1]\n"),
            cc(
                "#cloud-config\n"
                "merge_how: 'dict(recurse_dict,no_replace)+list(append)'\n"
                "foo: {b: 2}\nlist: [2]\n"
            ),
        ]
    ),
)
write(
    "cloudconfig-merge-type-key",
    multi(
        [
            cc("#cloud-config\nfoo: {a: 1}\n"),
            cc("#cloud-config\nmerge_type: 'dict(recurse_dict)'\nfoo: {b: 2}\n"),
        ]
    ),
)
write(
    "cloudconfig-merge-header",
    multi(
        [
            cc("#cloud-config\nfoo: {a: 1}\nlist: [1]\n"),
            cc(
                "#cloud-config\nfoo: {b: 2}\nlist: [2]\n",
                headers=[("Merge-Type", "dict(recurse_dict)+list(append)")],
            ),
        ]
    ),
)
write(
    "cloudconfig-x-merge-header",
    multi(
        [
            cc("#cloud-config\nfoo: {a: 1}\n"),
            cc(
                "#cloud-config\nfoo: {b: 2}\n",
                headers=[("X-Merge-Type", "dict(recurse_dict)")],
            ),
        ]
    ),
)
# Merge-Type wins over X-Merge-Type regardless of the order they appear in.
write(
    "cloudconfig-both-merge-headers",
    multi(
        [
            cc("#cloud-config\nfoo: {a: 1}\n"),
            cc(
                "#cloud-config\nfoo: {b: 2}\n",
                headers=[
                    ("X-Merge-Type", "dict(no_replace)"),
                    ("Merge-Type", "dict(recurse_dict)"),
                ],
            ),
        ]
    ),
)
write(
    "cloudconfig-bad-merge-header",
    multi([cc("#cloud-config\nfoo: 1\n", headers=[("Merge-Type", "not a merger")])]),
)
# Handlers read their headers out of a plain dict, so a header spelled in any
# other case is simply not found.
write(
    "cloudconfig-lower-merge-header",
    multi(
        [
            cc("#cloud-config\nfoo: {a: 1}\n"),
            cc(
                "#cloud-config\nfoo: {b: 2}\n",
                headers=[("merge-type", "dict(recurse_dict)")],
            ),
        ]
    ),
)
write(
    "cloudconfig-unknown-merger",
    multi([cc("#cloud-config\nfoo: 1\n", headers=[("Merge-Type", "nosuch()")])]),
)
# A part the handler rejects is named in the file it writes, but only when the
# failure was a ValueError; anything else is dropped without trace.
write(
    "cloudconfig-mixed-errors",
    multi(
        [
            cc("#cloud-config\ngood: 1\n", filename="good.yaml"),
            cc("#cloud-config\n", filename="empty.yaml"),
            cc("#cloud-config\nfoo: [unclosed\n", filename="broken.yaml"),
            cc("#cloud-config\nmore: 2\n", filename="   spaced.yaml   "),
        ]
    ),
)
write(
    "cloudconfig-jsonp",
    multi(
        [
            cc("#cloud-config\nfoo: bar\nlist: [1]\n"),
            cc(
                '#cloud-config-jsonp\n[{"op": "add", "path": "/added", "value": 1},'
                ' {"op": "add", "path": "/list/-", "value": 2}]',
                subtype="cloud-config-jsonp",
            ),
        ]
    ),
)
write(
    "cloudconfig-jsonp-only",
    '#cloud-config-jsonp\n[{"op": "add", "path": "/added", "value": [1, 2]}]',
)
write("cloudconfig-jsonp-malformed", "#cloud-config-jsonp\nnot json at all")
write(
    "cloudconfig-jsonp-conflict",
    '#cloud-config-jsonp\n[{"op": "remove", "path": "/nope"}]',
)
write(
    "cloudconfig-jsonp-then-config",
    multi(
        [
            cc(
                '#cloud-config-jsonp\n[{"op": "add", "path": "/first", "value": 1}]',
                subtype="cloud-config-jsonp",
            ),
            cc("#cloud-config\nsecond: 2\n"),
        ]
    ),
)
write(
    "cloudconfig-jinja",
    "## template: jinja\n#cloud-config\ngreeting: {{ v1.greeting }}\n",
)
# The sub-handler is picked from the rendered payload, but the content type
# handed to it is still text/jinja2, so this patch is parsed as YAML.
write(
    "cloudconfig-jinja-jsonp",
    "## template: jinja\n#cloud-config-jsonp\n"
    '[{"op": "add", "path": "/{{ v1.greeting }}", "value": 1}]',
)
write(
    "cloudconfig-archive",
    "#cloud-config-archive\n"
    "- type: text/cloud-config\n"
    "  filename: one.yaml\n"
    "  content: |\n"
    "    foo: {a: 1}\n"
    "- type: text/cloud-config\n"
    "  Merge-Type: 'dict(recurse_dict)'\n"
    "  content: |\n"
    "    foo: {b: 2}\n",
)
# The same archive with the key in the case the documentation uses, which the
# handler's dict lookup misses.
write(
    "cloudconfig-archive-lower",
    "#cloud-config-archive\n"
    "- type: text/cloud-config\n"
    "  content: |\n"
    "    foo: {a: 1}\n"
    "- type: text/cloud-config\n"
    "  merge-type: 'dict(recurse_dict)'\n"
    "  content: |\n"
    "    foo: {b: 2}\n",
)
write(
    "cloudconfig-archive-jsonp",
    "#cloud-config-archive\n"
    "- type: text/cloud-config\n"
    "  content: |\n"
    "    foo: 1\n"
    "- type: text/cloud-config-jsonp\n"
    '  content: \'[{"op": "replace", "path": "/foo", "value": 2}]\'\n',
)
# Every other fixture also writes this file, empty, which is worth pinning too.
write("cloudconfig-with-script", multi([cc("#cloud-config\nfoo: 1\n"),
      cc("#!/bin/sh\necho hi\n", subtype="x-shellscript", filename="s.sh")]))
EOF

    for fixture in "$HD"/*; do
        name=$(basename "$fixture")
        rm -rf "$HD.py" "$HD.rs"
        mkdir -p "$HD.py" "$HD.rs"
        run_pair "handlers $name" \
            "cd /tmp && python3 '$HD_PY' '$HD.py' <'$fixture'" \
            "cd /tmp && '$HD_RS' '$HD.rs' <'$fixture'"
    done
fi

# --- yaml emission -----------------------------------------------------------
sec "yaml-emission"
# `status --format=yaml` above only exercises whatever state this machine is in,
# which on a disabled or unbooted host is a flat map of short strings — exactly
# the shape where any emitter agrees. These fixtures pin the parts that actually
# differ: indent 4, folding at column 80, and the quoting rules.
YF_PY="$(cd "$(dirname "$0")" && pwd)/yamlfmt.py"
YF_RS="$TARGET/examples/dump-yaml"
if [ -x "$YF_RS" ] && python3 -c 'import cloudinit.safeyaml' 2>/dev/null; then
    YF_RS="$(cd "$(dirname "$YF_RS")" && pwd)/dump-yaml"
    YF="$WORK/yamlfmt"
    mkdir -p "$YF"

    long='DataSourceAzure [seed=/dev/sr0] failed to identify the instance because the metadata service did not respond within the configured timeout window'

    python3 - "$YF" "$long" <<'PYEOF'
import json, os, sys

out, long = sys.argv[1], sys.argv[2]
cases = {
    # A machine that has actually booted, which is what CI runners look like.
    "booted": {
        "boot_status_code": "enabled-by-generator",
        "datasource": "azure",
        "detail": long,
        "errors": [long, "short one"],
        "extended_status": "degraded done",
        "init": {"errors": [], "finished": 1756757172.1, "start": 1756757170.9},
        "last_update": "Tue, 01 Sep 2026 19:26:12 +0000",
        "recoverable_errors": {"ERROR": [], "WARNING": ["Used fallback datasource"]},
        "status": "done",
    },
    "empties": {"a": [], "b": {}, "c": "", "d": None},
    "typed_strings": {k: k for k in
        ["yes", "no", "on", "off", "true", "null", "~", "0600", "1.5", "1e3",
         ".inf", ".nan", "2020-01-02", "12:30:00", "<<", "="]},
    "indicators": {k: v for k, v in enumerate(
        ["- x", "#x", "k: v", "? x", "[x]", "{x}", "*x", "&x", "!x", "|x", ">x",
         "'x", '"x', "%x", "@x", "`x", "---x", "...x", "x #y", "x:y"])},
    "whitespace": {"a": " lead", "b": "trail ", "c": "a  b", "d": "a\nb",
                   "e": "a\n\nb", "f": "a \nb", "g": "a\n b", "h": "\nlead"},
    "unicode": {"a": "caf\u00e9", "b": "\u65e5\u672c\u8a9e", "c": "emoji \U0001f389",
                "d": "ctrl\x01char", "e": "tab\there"},
    "numbers": {"a": 0, "b": -1, "c": 1.5, "d": 1756757172.0, "e": 0.1,
                "f": 123456789012345},
    "nesting": {"a": {"b": {"c": {"d": ["e", ["f", "g"], {"h": "i"}]}}}},
    "folding": {"in_seq": [long], "in_map": {"inner": {"msg": long}},
                "quoted": "it's " + long, "unicode": "caf\u00e9 " + long,
                "unbroken": "https://example.com/" + "x" * 120},
    "odd_keys": {"": "empty key", "k" * 200: "long key", "a\nb": "multiline key"},
}
for name, value in cases.items():
    with open(os.path.join(out, name), "w") as handle:
        json.dump(value, handle)
PYEOF

    for fixture in "$YF"/*; do
        [ -f "$fixture" ] || continue
        name="$(basename "$fixture")"
        run_pair "yamlfmt $name" \
            "cd /tmp && python3 '$YF_PY' <'$fixture'" \
            "cd /tmp && '$YF_RS' <'$fixture'"
    done
fi

# --- hyper-v kvp telemetry ---------------------------------------------------
sec "hyper-v-kvp-telemetry"
# The pool file is a flat array of 2560-byte records: a 512-byte key and a
# 2048-byte value, both NUL-padded by `struct.pack`, which truncates in *bytes*
# what the caller budgeted in *characters*. So the records are compared as hex:
# a record cut mid-codepoint is not valid UTF-8, and reproducing that cut is
# half the point (upstream bug 46).
#
# The pool path is always a scratch file. Nothing here goes near the real
# /var/lib/hyperv/.kvp_pool_1, which the host's kvp daemon reads.
KVP_PY="$(cd "$(dirname "$0")" && pwd)/kvp.py"
KVP_RS="$TARGET/examples/dump-kvp"
if [ -x "$KVP_RS" ] &&
    python3 -c 'import cloudinit.reporting.handlers' 2>/dev/null; then
    KVP_RS="$(cd "$(dirname "$KVP_RS")" && pwd)/dump-kvp"
    KVP="$WORK/kvp"
    mkdir -p "$KVP"

    python3 - "$KVP" <<'PYEOF'
import json, os, sys

out = sys.argv[1]
ts = 1700000100.0
# A three-byte character, so 2048 lands mid-codepoint; a two-byte one divides
# 2048 evenly and would hide the bug.
wide = "\u4e00"
narrow = "\u00e9"
emoji = "\U0001f389"


def event(**kw):
    base = {"op": "event", "name": "n", "type": "finish", "timestamp": ts,
            "result": "SUCCESS", "duration": 1.5, "description": ""}
    base.update(kw)
    return base


cases = {
    # --- _encode_kvp_item: the padding and the byte-width truncation ---------
    "item_short": {"op": "item", "key": "k", "value": "v"},
    "item_empty": {"op": "item", "key": "", "value": ""},
    "item_key_at_limit": {"op": "item", "key": "k" * 512, "value": "v"},
    "item_key_overflow": {"op": "item", "key": "k" * 600, "value": "v"},
    "item_key_overflow_wide": {"op": "item", "key": wide * 300, "value": "v"},
    "item_value_at_limit": {"op": "item", "key": "k", "value": "v" * 2048},
    "item_value_overflow": {"op": "item", "key": "k", "value": "v" * 3000},
    "item_value_overflow_wide": {"op": "item", "key": "k", "value": wide * 2000},
    "item_value_overflow_narrow": {"op": "item", "key": "k",
                                   "value": narrow * 2000},
    "item_nul_inside": {"op": "item", "key": "a\x00b", "value": "c\x00d"},

    # --- write_key: the 1023-*character* cut feeding the byte-width one ------
    "write_key_short": {"op": "write_key", "key": "PROVISIONING_REPORT",
                        "value": "ready"},
    "write_key_1023": {"op": "write_key", "key": "k", "value": "x" * 1023},
    "write_key_1024": {"op": "write_key", "key": "k", "value": "x" * 1024},
    "write_key_long": {"op": "write_key", "key": "k", "value": "x" * 4000},
    "write_key_wide": {"op": "write_key", "key": "k", "value": wide * 1500},
    "write_key_narrow": {"op": "write_key", "key": "k", "value": narrow * 1500},

    # --- _encode_event: the metadata block and its field order --------------
    "event_finish": event(name="init-local/check-cache", description="hi"),
    "event_start": {"op": "event", "name": "init-local", "type": "start",
                    "timestamp": ts, "description": "starting"},
    "event_no_duration": {"op": "event", "name": "n", "type": "finish",
                          "timestamp": ts, "result": "FAIL",
                          "description": "boom"},
    "event_whole_duration": event(duration=2.0),
    "event_tiny_duration": event(duration=0.000123),
    "event_epoch_zero": event(timestamp=0.0),
    "event_fractional_ts": event(timestamp=1700000100.123456),
    "event_pipe_in_name": event(name="a|b|c"),
    "event_empty_desc": event(description=""),
    "event_control_chars": event(description="a\nb\tc\rd\x01e\"f\\g\x7fh"),
    "event_unicode_desc": event(description="caf%s \u65e5\u672c\u8a9e %s"
                                % (narrow, emoji)),

    # --- _break_down: the numbered slices -----------------------------------
    "event_break_ascii": event(description="a" * 3000),
    "event_break_boundary": event(description="b" * 1024),
    "event_break_escaped": event(description="\n" * 1200),
    "event_break_emoji": event(description=emoji * 300),
    "event_break_wide": event(description=wide * 1200),
    "event_break_long_name": event(name="N" * 400, description="c" * 2000),
    "event_break_mixed": event(description=("x" * 400 + "\n" + emoji) * 40),
}
for name, value in cases.items():
    with open(os.path.join(out, name), "w") as handle:
        json.dump(value, handle)
PYEOF

    for fixture in "$KVP"/*; do
        [ -f "$fixture" ] || continue
        name="$(basename "$fixture")"
        run_pair "kvp $name" \
            "cd /tmp && python3 '$KVP_PY' <'$fixture'" \
            "cd /tmp && '$KVP_RS' <'$fixture'"
    done
fi

# --- dmi ---------------------------------------------------------------------
sec "dmi"
# `read_dmi_data` reads the live machine, so the first case only proves the two
# implementations agree about *this* host -- which on a container or a VM
# without DMI is a short answer. The `--syspath` cases carry the weight: they
# point both readers at a prepared directory holding the shapes a real
# `/sys/class/dmi/id` produces (padded, empty, uninitialised 0xff with and
# without a newline, and a field that is not UTF-8).
DMI_PY="$(cd "$(dirname "$0")" && pwd)/dmi.py"
DMI_RS="$TARGET/examples/dump-dmi"
if [ -x "$DMI_RS" ]; then
    DMI_RS="$(cd "$(dirname "$DMI_RS")" && pwd)/dump-dmi"
    DMI_SUBS="http://sea/__dmi.system-serial-number__/ \
__dmi.baseboard-product-name__ __dmi.no-such-key__ __dmi.a_b__ plain__ \
__dmi.system-uuid____dmi.bios-vendor__"

    run_pair "dmi host" \
        "cd /tmp && python3 '$DMI_PY' $DMI_SUBS" \
        "cd /tmp && '$DMI_RS' $DMI_SUBS"

    DMI_DIR="$WORK/dmi"
    mkdir -p "$DMI_DIR"
    printf 'Acme Corp\n' >"$DMI_DIR/sys_vendor"
    printf '  padded  \n' >"$DMI_DIR/board_name"
    : >"$DMI_DIR/product_name"
    printf '\377\377\377\377\377\377\377\377\377\377\377\377\377\377\377\377\n' \
        >"$DMI_DIR/product_uuid"
    printf '\377\377\377\377\377\377\377\377\377\377\377\377\377\377\377\377' \
        >"$DMI_DIR/product_serial"
    printf 'caf\351 \200\n' >"$DMI_DIR/bios_vendor"
    printf 'ok\n' >"$DMI_DIR/board_asset_tag"
    chmod 0 "$DMI_DIR/board_asset_tag" 2>/dev/null || true
    DMI_KEYS="system-manufacturer baseboard-product-name system-product-name \
system-uuid system-serial-number bios-vendor chassis-asset-tag processor-family \
baseboard-asset-tag"

    run_pair "dmi sysfs" \
        "cd /tmp && python3 '$DMI_PY' --syspath '$DMI_DIR' $DMI_KEYS" \
        "cd /tmp && '$DMI_RS' --syspath '$DMI_DIR' $DMI_KEYS"
fi

# --- json patch --------------------------------------------------------------
sec "json-patch"
# `#cloud-config-jsonp` is applied by the `jsonpatch` library, which departs
# from RFC 6902 in several places. These cases pin the departures as well as the
# happy path; failures are compared by class, since the port does not reproduce
# upstream's messages.
JP_PY="$(cd "$(dirname "$0")" && pwd)/jsonp.py"
JP_RS="$TARGET/examples/dump-jsonpatch"
if [ -x "$JP_RS" ] && python3 -c 'import jsonpatch' 2>/dev/null; then
    JP_RS="$(cd "$(dirname "$JP_RS")" && pwd)/dump-jsonpatch"
    JP="$WORK/jsonpatch"
    mkdir -p "$JP"

    python3 - "$JP" <<'PYEOF'
import json, os, sys

out = sys.argv[1]
doc = {
    "a": 1,
    "s": "text",
    "list": [1, 2, 3],
    "nested": {"x": {"y": "z"}},
    "mixed": [{"k": "v"}, [1], "str"],
    "f": 1.5,
    "t": True,
    "n": None,
}
cases = {
    # The six operations, on both container kinds.
    "add-key": '[{"op": "add", "path": "/new", "value": {"k": [1, 2]}}]',
    "add-existing": '[{"op": "add", "path": "/a", "value": 2}]',
    "add-nested": '[{"op": "add", "path": "/nested/x/w", "value": 1}]',
    "add-append": '[{"op": "add", "path": "/list/-", "value": 4}]',
    "add-insert": '[{"op": "add", "path": "/list/0", "value": 0}]',
    "add-at-end": '[{"op": "add", "path": "/list/3", "value": 4}]',
    "add-past-end": '[{"op": "add", "path": "/list/9", "value": 4}]',
    "remove-key": '[{"op": "remove", "path": "/a"}]',
    "remove-nested": '[{"op": "remove", "path": "/nested/x/y"}]',
    "remove-index": '[{"op": "remove", "path": "/list/1"}]',
    "remove-missing": '[{"op": "remove", "path": "/nope"}]',
    "remove-past-end": '[{"op": "remove", "path": "/list/9"}]',
    "remove-dash": '[{"op": "remove", "path": "/list/-"}]',
    "replace-key": '[{"op": "replace", "path": "/a", "value": [1]}]',
    "replace-index": '[{"op": "replace", "path": "/list/2", "value": "x"}]',
    "replace-missing": '[{"op": "replace", "path": "/nope", "value": 1}]',
    "replace-past-end": '[{"op": "replace", "path": "/list/9", "value": 1}]',
    "replace-dash": '[{"op": "replace", "path": "/list/-", "value": 1}]',
    "move-key": '[{"op": "move", "from": "/a", "path": "/b"}]',
    "move-into-list": '[{"op": "move", "from": "/a", "path": "/list/1"}]',
    "move-onto-self": '[{"op": "move", "from": "/a", "path": "/a"}]',
    "move-into-child": '[{"op": "move", "from": "/nested", "path": "/nested/x/w"}]',
    "move-within-list": '[{"op": "move", "from": "/list/0", "path": "/list/2"}]',
    "move-missing": '[{"op": "move", "from": "/nope", "path": "/b"}]',
    "copy-key": '[{"op": "copy", "from": "/nested", "path": "/clone"}]',
    "copy-index": '[{"op": "copy", "from": "/list/0", "path": "/list/-"}]',
    "copy-missing": '[{"op": "copy", "from": "/nope", "path": "/b"}]',
    "test-pass": '[{"op": "test", "path": "/a", "value": 1}]',
    "test-fail": '[{"op": "test", "path": "/a", "value": 2}]',
    "test-deep": '[{"op": "test", "path": "/nested", "value": {"x": {"y": "z"}}}]',
    "test-dash": '[{"op": "test", "path": "/list/-", "value": 3}]',
    # bool is an int in Python, and an int compares equal to a float.
    "test-bool-int": '[{"op": "test", "path": "/a", "value": true}]',
    "test-int-float": '[{"op": "test", "path": "/a", "value": 1.0}]',
    "test-str-int": '[{"op": "test", "path": "/s", "value": 1}]',
    "test-null": '[{"op": "test", "path": "/n", "value": null}]',
    # The root, which an empty pointer addresses.
    "root-add": '[{"op": "add", "path": "", "value": {"only": 1}}]',
    "root-replace": '[{"op": "replace", "path": "", "value": [1, 2]}]',
    "root-remove": '[{"op": "remove", "path": ""}]',
    "root-move": '[{"op": "move", "from": "/nested", "path": ""}]',
    # Pointer syntax.
    "escape-tilde": '[{"op": "add", "path": "/~0k", "value": 1}]',
    "escape-slash": '[{"op": "add", "path": "/~1k", "value": 1}]',
    "escape-invalid": '[{"op": "add", "path": "/a~2b", "value": 1}]',
    "escape-trailing": '[{"op": "add", "path": "/ab~", "value": 1}]',
    "no-leading-slash": '[{"op": "add", "path": "a", "value": 1}]',
    "empty-segment": '[{"op": "add", "path": "/", "value": 1}]',
    # jsonpointer's index regex anchors only one of its branches, so a leading
    # zero is accepted and anything else after it is a ValueError.
    "index-leading-zero": '[{"op": "add", "path": "/list/01", "value": 9}]',
    "index-zero": '[{"op": "add", "path": "/list/0", "value": 9}]',
    "index-not-numeric": '[{"op": "add", "path": "/list/0abc", "value": 9}]',
    "index-negative": '[{"op": "add", "path": "/list/-1", "value": 9}]',
    "index-huge": '[{"op": "add", "path": "/list/99999999999999999999", "value": 9}]',
    # Walking into something that is not a container.
    "through-int": '[{"op": "add", "path": "/a/b", "value": 1}]',
    "through-str": '[{"op": "add", "path": "/s/b", "value": 1}]',
    "through-str-index": '[{"op": "add", "path": "/s/0", "value": 1}]',
    "through-bool": '[{"op": "add", "path": "/t/b", "value": 1}]',
    "through-null": '[{"op": "add", "path": "/n/b", "value": 1}]',
    "through-missing": '[{"op": "add", "path": "/nope/b", "value": 1}]',
    # Malformed patches.
    "not-json": "not json",
    "not-a-list": '{"op": "add", "path": "/a", "value": 1}',
    "null-patch": "null",
    "empty-patch": "[]",
    "op-missing": '[{"path": "/a", "value": 1}]',
    "op-unknown": '[{"op": "bogus", "path": "/a", "value": 1}]',
    "op-not-string": '[{"op": 123, "path": "/a"}]',
    "op-not-a-dict": '["nope"]',
    "path-missing": '[{"op": "add", "value": 1}]',
    "path-not-string": '[{"op": "add", "path": 1, "value": 1}]',
    "value-missing": '[{"op": "add", "path": "/a"}]',
    "from-missing": '[{"op": "move", "path": "/a"}]',
    # json.loads is given an object_pairs_hook that folds duplicate keys into a
    # list instead of letting the last one win.
    "duplicate-value": '[{"op": "add", "path": "/k", "value": 1, "value": 2}]',
    "duplicate-nested": '[{"op": "add", "path": "/k", "value": {"x": 1, "x": 2}}]',
    "duplicate-path": '[{"op": "add", "path": "/a", "path": "/b", "value": 1}]',
    # Several operations, including one that fails after others have applied.
    "sequence": '[{"op": "add", "path": "/k", "value": 1},'
                ' {"op": "move", "from": "/k", "path": "/j"},'
                ' {"op": "test", "path": "/j", "value": 1},'
                ' {"op": "remove", "path": "/j"}]',
    "sequence-aborts": '[{"op": "add", "path": "/k", "value": 1},'
                       ' {"op": "remove", "path": "/nope"}]',
    # Whitespace and encoding around the patch body.
    "padded": '\n\n  [{"op": "add", "path": "/k", "value": 1}]  \n',
    "unicode": '[{"op": "add", "path": "/caf\u00e9", "value": "\u65e5"}]',
    "trailing-garbage": '[{"op": "add", "path": "/k", "value": 1}] junk',
}
for name, patch in cases.items():
    with open(os.path.join(out, name), "w") as handle:
        json.dump({"doc": doc, "patch": patch}, handle)
PYEOF

    for fixture in "$JP"/*; do
        [ -f "$fixture" ] || continue
        name="$(basename "$fixture")"
        run_pair "jsonpatch $name" \
            "cd /tmp && python3 '$JP_PY' <'$fixture'" \
            "cd /tmp && '$JP_RS' <'$fixture'"
    done
fi

# --- distro facts ------------------------------------------------------------
sec "distro-facts"
# `cloudinit.distros` is a table, and the port's copy of it is generated rather
# than transcribed. These cases prove the generated copy still matches the
# packaged Python, one distro at a time, so a drifting field names itself.
#
# Every attribute is read off a *constructed* Distro, not the class: several
# `__init__`s replace `renderer_configs` wholesale (azurelinux swaps sysconfig
# for netplan+networkd) and `osfamily` exists nowhere else.
DIST_PY="$(cd "$(dirname "$0")" && pwd)/distro.py"
DIST_RS="$TARGET/examples/dump-distro"
if [ -x "$DIST_RS" ] && python3 -c 'import cloudinit.distros' 2>/dev/null; then
    DIST_RS="$(cd "$(dirname "$DIST_RS")" && pwd)/dump-distro"

    run_pair "distro --names" \
        "cd /tmp && python3 '$DIST_PY' --names" \
        "cd /tmp && '$DIST_RS' --names"

    # The `--names` output is one quoted name per line, indent 1.
    dist_names="$(cd /tmp && python3 "$DIST_PY" --names |
        sed -n 's/^ *"\(.*\)",\{0,1\}$/\1/p')"
    # Plus one name that is not a distro at all, which has to look the same as
    # `dragonfly` -- a name OSFAMILIES lists but no module implements.
    for distro in $dist_names nosuchdistro; do
        run_pair "distro $distro" \
            "cd /tmp && python3 '$DIST_PY' '$distro'" \
            "cd /tmp && '$DIST_RS' '$distro'"
    done
fi

# --- hostname resolution -----------------------------------------------------
sec "hostname-resolution"
# `util.get_hostname_fqdn` picks between three sources -- the config, the
# datasource's metadata, and the running system -- and the *order* is the whole
# behaviour. A machine that comes up named `localhost` forever, or named after
# the image instead of the tenant's request, is this function getting the
# precedence wrong, and nothing about it looks like a failure at the time.
HOST_PY="$(cd "$(dirname "$0")" && pwd)/hostname.py"
HOST_RS="$TARGET/examples/dump-hostname"
if [ -x "$HOST_RS" ] && python3 -c 'import cloudinit.util' 2>/dev/null; then
    HOST_RS="$(cd "$(dirname "$HOST_RS")" && pwd)/dump-hostname"
    HOST_D="$WORK/hostname"
    mkdir -p "$HOST_D/proc/sys/kernel" "$HOST_D/etc"

    # Three fixtures for the fallback path: a plain system name, one that
    # `/etc/hosts` can extend to an FQDN, and the `localhost` that
    # `is_default` exists to catch.
    for host_case in plain hosts localhost; do
        mkdir -p "$HOST_D/$host_case/proc/sys/kernel" "$HOST_D/$host_case/etc"
    done
    printf 'imagehost\n' > "$HOST_D/plain/proc/sys/kernel/hostname"
    : > "$HOST_D/plain/etc/hosts"
    printf 'imagehost\n' > "$HOST_D/hosts/proc/sys/kernel/hostname"
    printf '# a comment\n127.0.0.1 localhost\n127.0.1.1 imagehost.example.com imagehost\n' \
        > "$HOST_D/hosts/etc/hosts"
    printf 'localhost\n' > "$HOST_D/localhost/proc/sys/kernel/hostname"
    : > "$HOST_D/localhost/etc/hosts"

    host_n=0
    for host_case in plain hosts localhost; do
        for host_cfg in \
            '{}' \
            '{"hostname": "cfghost"}' \
            '{"hostname": "cfghost.example.org"}' \
            '{"hostname": ".leadingdot"}' \
            '{"fqdn": "fq.example.org"}' \
            '{"fqdn": "fq.example.org", "hostname": "cfghost"}' \
            '{"fqdn": "bare"}' \
            '{"fqdn": 42}' \
            '{"hostname": ""}'; do
            for host_md in \
                '{}' \
                '{"local-hostname": "mdhost"}' \
                '{"local-hostname": "mdhost.example.net"}' \
                '{"local-hostname": "10.0.0.4"}' \
                '{"local-hostname": ""}' \
                '{"local-hostname": null}' \
                '{"local-hostname": "localhost"}'; do
                host_n=$((host_n + 1))
                run_pair "hostname $host_case #$host_n" \
                    "cd /tmp && python3 '$HOST_PY' '$HOST_D/$host_case' '$host_cfg' '$host_md'" \
                    "cd /tmp && '$HOST_RS' '$HOST_D/$host_case' '$host_cfg' '$host_md'"
            done
        done
    done
fi

# --- users and groups normalisation ------------------------------------------
sec "users-and-groups-normalisation"
# `ug_util.normalize_users_groups` decides who exists on the machine and which
# groups they land in, which is to say it decides who can become root. Its
# input is the most permissive surface cloud-init has -- five spellings of
# `users:`, four of `groups:`, plus the pre-22.2 `user:` key that quietly
# outranks the image's own default account -- so the cases below are mostly
# shapes, not values. Nothing here touches the system, so unlike the module
# that consumes it this can be compared directly.
UG_PY="$(cd "$(dirname "$0")" && pwd)/ugutil.py"
UG_RS="$TARGET/examples/dump-ug"
if [ -x "$UG_RS" ] && python3 -c 'import cloudinit.distros.ug_util' 2>/dev/null; then
    UG_RS="$(cd "$(dirname "$UG_RS")" && pwd)/dump-ug"
    ug_n=0
    for ug_du in \
        'null' \
        '{"name": "ubuntu", "lock_passwd": true, "gecos": "Ubuntu", "groups": ["adm", "sudo"], "sudo": ["ALL=(ALL) NOPASSWD:ALL"], "shell": "/bin/bash"}' \
        '{"name": "ec2-user", "groups": "wheel"}' \
        '{"groups": ["sudo"]}' \
        '"ubuntu"'; do
        for ug_cfg in \
            '{}' \
            '{"users": []}' \
            '{"users": ["alice", "bob"]}' \
            '{"users": "alice,bob,alice"}' \
            '{"users": "alice, bob"}' \
            '{"users": ["default"]}' \
            '{"users": ["default", "alice"]}' \
            '{"users": [{"name": "default"}, {"name": "alice", "sudo": null}]}' \
            '{"users": {"alice": true, "bob": false, "carol": "yes", "dave": 2}}' \
            '{"users": {"alice": {"shell": "/bin/sh"}, "default": {}}}' \
            '{"users": [{"name": "default", "shell": "/bin/zsh"}, {"name": "ubuntu", "groups": "docker"}]}' \
            '{"users": [{"name": "ubuntu", "groups": ["docker", "adm"]}, {"name": "default"}]}' \
            '{"users": [{"name": "default", "groups": []}]}' \
            '{"users": [{"name": "default", "groups": ""}]}' \
            '{"users": [{"name": "default", "groups": "a,a,b"}]}' \
            '{"users": [{"name": "default", "groups": {"a": 1, "b": 2}}]}' \
            '{"users": [{"name": "default", "groups": null}]}' \
            '{"users": [{"name": "default"}, {"name": "default", "shell": "/x"}]}' \
            '{"users": [{"ssh-authorized-keys": ["k1"], "name": "alice"}]}' \
            '{"users": [{"name": "alice", "shell": "/bin/sh"}, {"name": "alice", "shell": "/bin/zsh", "uid": 1005}]}' \
            '{"users": [{"sudo": "ALL"}]}' \
            '{"users": [["alice", "bob"], "carol"]}' \
            '{"users": [{"name": 5}]}' \
            '{"users": [{"name": null}]}' \
            '{"users": [{"name": ""}]}' \
            '{"users": [{"": "x", "name": "a"}]}' \
            '{"users": [{"a-b-c": "x", "name": "a"}]}' \
            '{"users": [null]}' \
            '{"users": [""]}' \
            '{"users": [{}]}' \
            '{"users": 5}' \
            '{"users": null}' \
            '{"users": ""}' \
            '{"users": ","}' \
            '{"users": {"alice": [1]}}' \
            '{"user": "azureuser"}' \
            '{"user": "azureuser", "users": []}' \
            '{"user": "azureuser", "users": ["alice"]}' \
            '{"user": "azureuser", "users": "alice"}' \
            '{"user": "azureuser", "users": {"alice": true}}' \
            '{"user": {"name": "az", "groups": ["sudo"]}}' \
            '{"user": {"name": "default"}}' \
            '{"user": {}}' \
            '{"user": null}' \
            '{"user": false}' \
            '{"user": 5}' \
            '{"user": ["a"]}' \
            '{"groups": "admin,dev"}' \
            '{"groups": "a, b"}' \
            '{"groups": ["admin", {"dev": ["bob", "alice"]}]}' \
            '{"groups": {"dev": "bob,alice"}}' \
            '{"groups": [{"dev": ["bob"]}, {"dev": "alice"}]}' \
            '{"groups": [{"a": ["x"], "b": "y"}]}' \
            '{"groups": ["a", "a"]}' \
            '{"groups": {"a": {"b": 1}}}' \
            '{"groups": [["a"]]}' \
            '{"groups": []}' \
            '{"groups": {}}' \
            '{"groups": null}' \
            '{"groups": ""}' \
            '{"groups": ","}' \
            '{"groups": [{"dev": 5}]}' \
            '{"groups": 5}' \
            '{"groups": [5]}' \
            '{"groups": {"dev": 5}}' \
            '{"groups": {"dev": ["b", 1]}}' \
            '{"groups": {"a": null}}' \
            '{"groups": {"a": true}}' \
            '{"groups": {"dev": []}, "users": ["alice"]}'; do
            ug_n=$((ug_n + 1))
            run_pair "ug #$ug_n" \
                "cd /tmp && python3 '$UG_PY' '$ug_cfg' '$ug_du'" \
                "cd /tmp && '$UG_RS' '$ug_cfg' '$ug_du'"
        done
    done
fi

# --- useradd, groupadd, sudoers and doas -------------------------------------
sec "useradd-groupadd-sudoers-and-doas"
# Where `ug_util` decides who exists, `Distro.add_user` decides what they can
# do: the option list below is the difference between an account that can log
# in and one that cannot, and between a sudoers file that grants one command
# and one that grants root. Nothing is executed on either side -- the Python
# helper replaces `subp`, `is_user`, `is_group` and the file writers with
# recorders -- so what is compared is the argument vector cloud-init *would*
# have run.
#
# Two rules here are easy to get wrong and are covered deliberately. Only
# string values become options, so `gecos: 5` vanishes while `uid: 5` does not;
# and the doas rule pattern is anchored at both ends, so `permit alice garbage`
# is invalid even though it names the right user.
USER_PY="$(cd "$(dirname "$0")" && pwd)/user.py"
USER_RS="$TARGET/examples/dump-user"
if [ -x "$USER_RS" ] && python3 -c 'import cloudinit.distros' 2>/dev/null; then
    USER_RS="$(cd "$(dirname "$USER_RS")" && pwd)/dump-user"
    user_n=0
    for user_snappy in 0 1; do
        for user_cfg in \
            '{}' \
            '{"gecos": "Alice"}' \
            '{"gecos": ""}' \
            '{"gecos": 5}' \
            '{"gecos": true}' \
            '{"gecos": null}' \
            '{"uid": 1005}' \
            '{"uid": "1005"}' \
            '{"uid": 0}' \
            '{"uid": -1}' \
            '{"uid": 1.5}' \
            '{"uid": null}' \
            '{"uid": true}' \
            '{"uid": false}' \
            '{"uid": ""}' \
            '{"shell": "/bin/zsh"}' \
            '{"shell": ""}' \
            '{"homedir": "/srv/alice"}' \
            '{"primary_group": "staff"}' \
            '{"primary_group": ""}' \
            '{"primary_group": 5}' \
            '{"groups": "sudo"}' \
            '{"groups": "sudo,docker"}' \
            '{"groups": " sudo , docker "}' \
            '{"groups": ""}' \
            '{"groups": ","}' \
            '{"groups": "sudo,,docker"}' \
            '{"groups": ["sudo", "docker"]}' \
            '{"groups": [" sudo "]}' \
            '{"groups": []}' \
            '{"groups": {}}' \
            '{"groups": {"sudo": null}}' \
            '{"groups": {"sudo": null, "docker": true}}' \
            '{"groups": {"sudo": false}}' \
            '{"groups": 5}' \
            '{"groups": true}' \
            '{"groups": false}' \
            '{"groups": null}' \
            '{"groups": [5]}' \
            '{"groups": ["sudo", 5]}' \
            '{"groups": [null]}' \
            '{"groups": "sudo", "primary_group": "staff"}' \
            '{"groups": "sudo", "create_groups": false}' \
            '{"groups": "sudo", "create_groups": true}' \
            '{"groups": "sudo", "create_groups": null}' \
            '{"system": true}' \
            '{"system": false}' \
            '{"system": "yes"}' \
            '{"no_create_home": true}' \
            '{"no_create_home": false}' \
            '{"no_user_group": true}' \
            '{"no_log_init": true}' \
            '{"passwd": "$6$hash"}' \
            '{"passwd": ""}' \
            '{"passwd": null}' \
            '{"expiredate": "2030-01-01"}' \
            '{"expiredate": 20300101}' \
            '{"inactive": "5"}' \
            '{"inactive": 5}' \
            '{"selinux_user": "staff_u"}' \
            '{"lock_passwd": false}' \
            '{"sudo": "ALL=(ALL) NOPASSWD:ALL"}' \
            '{"ssh_authorized_keys": ["ssh-rsa AAAA"]}' \
            '{"default": false}' \
            '{"shell": "/bin/zsh", "gecos": "Alice", "homedir": "/srv/a", "system": true, "uid": 7, "groups": "a,b"}' \
            '{"zzz_unknown": "x", "aaa_unknown": "y", "gecos": "g"}'; do
            user_n=$((user_n + 1))
            run_pair "useradd #$user_n" \
                "cd /tmp && python3 '$USER_PY' argv alice '$user_cfg' $user_snappy" \
                "cd /tmp && '$USER_RS' argv alice '$user_cfg' $user_snappy"
        done
    done

    sudo_n=0
    for sudo_rules in \
        '"ALL=(ALL) NOPASSWD:ALL"' \
        '""' \
        '["ALL=(ALL) NOPASSWD:ALL"]' \
        '["ALL=(ALL) NOPASSWD:ALL", "ALL=(ALL) /bin/ls"]' \
        '[]' \
        '5' \
        'null' \
        'true' \
        '{"a": "b"}' \
        '[5]' \
        '[null]'; do
        sudo_n=$((sudo_n + 1))
        run_pair "sudoers #$sudo_n" \
            "cd /tmp && python3 '$USER_PY' sudo alice '$sudo_rules'" \
            "cd /tmp && '$USER_RS' sudo alice '$sudo_rules'"
    done

    doas_n=0
    for doas_rules in \
        '["permit alice"]' \
        '["permit nopass alice"]' \
        '["permit nopass alice as root"]' \
        '["permit persist keepenv alice"]' \
        '["permit setenv { PATH } alice"]' \
        '["permit setenv {PATH} alice"]' \
        '["permit setenv  {PATH} alice"]' \
        '["permit setenv {} alice"]' \
        '["deny alice"]' \
        '["permit alice cmd /bin/ls"]' \
        '["permit alice cmd /bin/ls args -l -a"]' \
        '["permit alice cmd /bin/ls garbage"]' \
        '["permit alice as root cmd /bin/ls"]' \
        '["permit alice as root as wheel"]' \
        '["permit alice as root extra"]' \
        '["permit alice garbage"]' \
        '["permit alice nopass"]' \
        '["permit bob"]' \
        '["nonsense"]' \
        '[]' \
        '["permit alice", "permit bob"]' \
        '["permit alice", "permit alice as root"]' \
        '["permit"]' \
        '["permit alice-dash"]' \
        '["permit  alice"]' \
        '["  permit alice"]' \
        '["permit alice  "]' \
        '["PERMIT alice"]' \
        '["permitalice"]' \
        '["permit al.ice"]' \
        '["permit 123"]' \
        '["permit _alice"]' \
        '["permit nolog nopass persist keepenv alice"]' \
        '["deny nopass alice cmd /bin/su"]' \
        '["permit nopass persist alice as root cmd /usr/bin/id args -u"]' \
        '[5]'; do
        doas_n=$((doas_n + 1))
        run_pair "doas #$doas_n" \
            "cd /tmp && python3 '$USER_PY' doas alice '$doas_rules'" \
            "cd /tmp && '$USER_RS' doas alice '$doas_rules'"
    done

    # --- the whole of create_user ---
    # What is compared here is the *sequence* of calls, not any one of them.
    # create_user decides, from five flags, whether the account ends up locked,
    # unlocked, or unlocked with a blank password -- and the last of those is a
    # machine anyone can walk into. None of it is visible afterwards, so it is
    # checked one case at a time against the recorder in user.py.
    #
    # The password cases run under all four combinations of "does the user
    # already exist" and "is its shadow password blank", because those two are
    # what decide which arm of the lock/unlock chain is taken. Everything else
    # is orthogonal to them and runs once.
    create_n=0
    create_case() {  # $1 config  $2 exists  $3 blank  $4 snappy
        create_n=$((create_n + 1))
        run_pair "create_user #$create_n" \
            "cd /tmp && python3 '$USER_PY' create alice '$1' $2 $3 $4" \
            "cd /tmp && '$USER_RS' create alice '$1' $2 $3 $4"
    }

    for create_exists in 0 1; do
        for create_blank in 0 1; do
            for create_cfg in \
                '{}' \
                '{"lock_passwd": false}' \
                '{"lock_passwd": true}' \
                '{"lock_passwd": 0}' \
                '{"lock_passwd": "false"}' \
                '{"lock_passwd": ""}' \
                '{"lock_passwd": null}' \
                '{"passwd": "$6$abc"}' \
                '{"passwd": ""}' \
                '{"passwd": 5}' \
                '{"passwd": "$6$abc", "lock_passwd": false}' \
                '{"passwd": "", "lock_passwd": false}' \
                '{"passwd": null, "lock_passwd": false}' \
                '{"plain_text_passwd": "hunter2"}' \
                '{"plain_text_passwd": ""}' \
                '{"plain_text_passwd": 5}' \
                '{"plain_text_passwd": "", "lock_passwd": false}' \
                '{"plain_text_passwd": false, "lock_passwd": false}' \
                '{"hashed_passwd": "$6$x"}' \
                '{"hashed_passwd": "", "lock_passwd": false}' \
                '{"hashed_passwd": null, "lock_passwd": false}' \
                '{"plain_text_passwd": "a", "hashed_passwd": "$6$b"}' \
                '{"plain_text_passwd": "", "hashed_passwd": "", "lock_passwd": false}' \
                '{"plain_text_passwd": "a", "hashed_passwd": "", "lock_passwd": false}' \
                '{"passwd": "$6$a", "plain_text_passwd": "b", "lock_passwd": false}' \
                '{"groups": "sudo,adm"}' \
                '{"sudo": "ALL=(ALL) NOPASSWD:ALL"}' \
                '{"ssh_authorized_keys": ["ssh-rsa A a"]}'; do
                create_case "$create_cfg" "$create_exists" "$create_blank" 0
            done
        done
    done

    # Everything below is decided without consulting the machine, so one state
    # is enough; a new user is the state that reaches the most code.
    for create_cfg in \
        '{"groups": ["sudo", " adm "]}' \
        '{"groups": {"sudo": null}}' \
        '{"groups": ""}' \
        '{"groups": 5}' \
        '{"groups": [5]}' \
        '{"groups": "adm", "primary_group": "alice"}' \
        '{"groups": "adm", "primary_group": 5}' \
        '{"groups": "adm", "create_groups": false}' \
        '{"shell": "/bin/bash", "gecos": "Alice"}' \
        '{"system": true}' \
        '{"no_create_home": true}' \
        '{"uid": 1001}' \
        '{"sudo": ["ALL=(ALL) ALL", "X"]}' \
        '{"sudo": false}' \
        '{"sudo": null}' \
        '{"sudo": 0}' \
        '{"sudo": 5}' \
        '{"sudo": ""}' \
        '{"sudo": []}' \
        '{"sudo": {}}' \
        '{"sudo": {"a": 1}}' \
        '{"doas": ["permit nopass alice"]}' \
        '{"doas": ["permit nopass bob"]}' \
        '{"doas": ["permit nopass alice", "permit alice as root"]}' \
        '{"doas": "permit alice"}' \
        '{"doas": {"permit nopass alice": 1}}' \
        '{"doas": 5}' \
        '{"doas": []}' \
        '{"doas": null}' \
        '{"doas": [5]}' \
        '{"ssh_authorized_keys": ["ssh-rsa B b", "ssh-rsa A a"]}' \
        '{"ssh_authorized_keys": ["k", "k"]}' \
        '{"ssh_authorized_keys": "ssh-rsa AAA a"}' \
        '{"ssh_authorized_keys": {"a": "ssh-rsa A a"}}' \
        '{"ssh_authorized_keys": {}}' \
        '{"ssh_authorized_keys": null}' \
        '{"ssh_authorized_keys": 5}' \
        '{"ssh_authorized_keys": true}' \
        '{"ssh_authorized_keys": []}' \
        '{"ssh_authorized_keys": [["a"]]}' \
        '{"ssh_authorized_keys": [5, "a"]}' \
        '{"ssh_redirect_user": "ubuntu"}' \
        '{"ssh_redirect_user": "ubuntu", "cloud_public_ssh_keys": ["ssh-rsa A a"]}' \
        '{"ssh_redirect_user": "ubuntu", "cloud_public_ssh_keys": []}' \
        '{"ssh_redirect_user": "ubuntu", "cloud_public_ssh_keys": "abc"}' \
        '{"ssh_redirect_user": true, "cloud_public_ssh_keys": ["k"]}' \
        '{"ssh_redirect_user": null, "cloud_public_ssh_keys": ["k"]}' \
        '{"ssh_redirect_user": "", "cloud_public_ssh_keys": ["k"]}' \
        '{"ssh_authorized_keys": ["a"], "ssh_redirect_user": "ubuntu", "cloud_public_ssh_keys": ["b"]}' \
        '{"snapuser": "me@example.com"}' \
        '{"snapuser": "me@example.com", "known": true}' \
        '{"snapuser": "", "sudo": "ALL"}' \
        '{"snapuser": null}' \
        '{"snapuser": 5}' \
        '{"known": true}' \
        '{"groups": "sudo", "shell": "/bin/bash", "lock_passwd": false, "passwd": "$6$x", "sudo": ["ALL=(ALL) NOPASSWD:ALL"], "ssh_authorized_keys": ["ssh-rsa A a"]}'; do
        create_case "$create_cfg" 0 0 0
    done

    # An existing user gets no useradd and no groups, whatever its config says.
    for create_cfg in \
        '{"groups": "sudo,adm"}' \
        '{"groups": "adm", "primary_group": 5}' \
        '{"shell": "/bin/bash", "uid": 1001}' \
        '{"snapuser": "me@example.com"}' \
        '{"ssh_authorized_keys": ["a"], "sudo": "ALL"}'; do
        create_case "$create_cfg" 1 0 0
    done

    # Snappy changes both the useradd and the groupadd.
    for create_cfg in \
        '{"groups": "sudo"}' \
        '{"groups": "sudo", "primary_group": "alice"}' \
        '{}' \
        '{"system": true}'; do
        create_case "$create_cfg" 0 0 1
    done
fi

# --- cc_users_groups ---------------------------------------------------------
sec "cc-users-groups"
# The module that decides who can log in. It carries out none of that itself --
# every decision ends in a `distro.create_group` or `distro.create_user` call --
# so both sides record the ordered call list instead of running it, which is
# also the only way to compare it without adding accounts to this machine.
#
# Three of its behaviours are refusals that abort the whole module rather than
# skipping one user, and the fourth is a silent drop: a `ssh_redirect_user` with
# no default user to redirect to is warned about and forgotten. Getting that arm
# wrong either strands the tenant or hands them an unrestricted account.
#
# The default-user block is varied over the cases that can see it; everything
# else runs once against one.
UGM_PY="$(cd "$(dirname "$0")" && pwd)/usersgroups.py"
UGM_RS="$TARGET/examples/dump-users-groups"
if [ -x "$UGM_RS" ] && python3 -c 'import cloudinit.config.cc_users_groups' 2>/dev/null; then
    UGM_RS="$(cd "$(dirname "$UGM_RS")" && pwd)/dump-users-groups"
    ugm_n=0
    ugm_case() {  # $1 config  $2 default-user  $3 cloud keys
        ugm_n=$((ugm_n + 1))
        run_pair "cc_users_groups #$ugm_n" \
            "cd /tmp && python3 '$UGM_PY' '$1' '$2' '$3'" \
            "cd /tmp && '$UGM_RS' '$1' '$2' '$3'"
    }

    # Anything that can name, merge with or redirect to the default user runs
    # against all three shapes of it, including none at all.
    for ugm_du in \
        'null' \
        '{"name": "ubuntu"}' \
        '{"name": "ubuntu", "groups": ["adm", "sudo"], "lock_passwd": true}'; do
        for ugm_cfg in \
            '{}' \
            '{"users": ["default"]}' \
            '{"users": ["default", "alice"]}' \
            '{"users": [{"name": "default", "shell": "/bin/zsh"}]}' \
            '{"users": [{"shell": "/bin/zsh"}]}' \
            '{"users": {"default": {"shell": "/bin/zsh"}}}' \
            '{"users": ["default", "default"]}' \
            '{"users": ["default"], "groups": ["staff"]}' \
            '{"user": "alice"}' \
            '{"user": {"name": "alice", "groups": "adm"}}' \
            '{"user": "alice", "users": ["bob"]}' \
            '{"user": "alice", "users": {"bob": true}}' \
            '{"user": "alice", "users": "bob"}' \
            '{"users": [{"name": "root", "ssh_redirect_user": true}]}' \
            '{"users": [{"name": "root", "ssh_redirect_user": "default"}]}' \
            '{"users": ["default", {"name": "root", "ssh_redirect_user": true}]}' \
            '{"users": ["default", {"name": "root", "ssh_redirect_user": "default"}]}' \
            '{"users": ["default", {"name": "root", "ssh-redirect-user": true}]}' \
            '{"users": ["default", {"name": "a", "ssh_redirect_user": true}, {"name": "b", "ssh_redirect_user": true}]}' \
            '{"users": [{"name": "a", "ssh_redirect_user": true}, {"name": "b", "ssh_redirect_user": 2}]}'; do
            ugm_case "$ugm_cfg" "$ugm_du" '["ssh-rsa AAAA cloud"]'
        done
    done

    # Everything below is decided without looking at the default user.
    for ugm_cfg in \
        '{"users": []}' \
        '{"users": null}' \
        '{"users": "alice"}' \
        '{"users": "alice, bob"}' \
        '{"users": ["alice", "bob"]}' \
        '{"users": ["", "alice"]}' \
        '{"users": [0, "alice"]}' \
        '{"users": [null]}' \
        '{"users": [true]}' \
        '{"users": [[]]}' \
        '{"users": [[1, 2]]}' \
        '{"users": [[[1]]]}' \
        '{"users": 5}' \
        '{"users": [5]}' \
        '{"users": {"alice": true, "bob": false}}' \
        '{"users": {"alice": {"shell": "/bin/sh"}}}' \
        '{"users": {"alice": 5}}' \
        '{"users": {"alice": []}}' \
        '{"users": [{"name": null}]}' \
        '{"users": [{"name": 5}]}' \
        '{"users": [{"name": true}]}' \
        '{"users": [{"name": []}]}' \
        '{"users": [{"name": {}}]}' \
        '{"user": 5}' \
        '{"user": []}' \
        '{"user": false}' \
        '{"user": ""}' \
        '{"user": {}}' \
        '{"user": {"name": 5}}' \
        '{"groups": []}' \
        '{"groups": ""}' \
        '{"groups": null}' \
        '{"groups": 5}' \
        '{"groups": [5]}' \
        '{"groups": "staff"}' \
        '{"groups": "staff, ops"}' \
        '{"groups": ["staff", "ops"]}' \
        '{"groups": ["a", "a"]}' \
        '{"groups": [{"staff": ["alice", "bob"]}]}' \
        '{"groups": [{"staff": "alice"}]}' \
        '{"groups": [{"staff": 5}]}' \
        '{"groups": [{"staff": [1, 2]}]}' \
        '{"groups": [{"a": ["x"]}, {"a": ["y"]}]}' \
        '{"groups": {"staff": ["bob", "alice", "bob"]}}' \
        '{"groups": {"staff": "alice,bob"}}' \
        '{"groups": {"staff": []}}' \
        '{"groups": ["staff"], "users": ["alice"]}' \
        '{"users": ["alice"], "groups": ["staff"]}' \
        '{"users": [{"name": "alice", "system": true}]}' \
        '{"users": [{"name": "alice", "no_create_home": true}]}' \
        '{"users": [{"name": "alice", "system": true, "ssh_authorized_keys": ["k"]}]}' \
        '{"users": [{"name": "alice", "system": true, "ssh_import_id": ["lp:x"]}]}' \
        '{"users": [{"name": "alice", "system": true, "ssh_redirect_user": true}]}' \
        '{"users": [{"name": "alice", "no_create_home": true, "ssh_authorized_keys": ["k"]}]}' \
        '{"users": [{"name": "alice", "system": true, "no_create_home": true, "ssh_authorized_keys": ["k"], "ssh_import_id": ["lp:x"]}]}' \
        '{"users": [{"name": "alice", "system": false, "ssh_authorized_keys": ["k"]}]}' \
        '{"users": [{"name": "alice", "system": 0, "ssh_authorized_keys": ["k"]}]}' \
        '{"users": [{"name": "alice", "system": "", "ssh_authorized_keys": ["k"]}]}' \
        '{"users": [{"name": "alice", "system": "yes", "ssh_import_id": "lp:x"}]}' \
        '{"users": [{"name": "alice", "system": true, "ssh_authorized_keys": []}]}' \
        '{"users": [{"name": "alice", "system": true, "ssh_authorized_keys": null}]}' \
        '{"users": [{"name": "alice", "system": true, "ssh_import_id": 0}]}' \
        '{"users": [{"name": "alice", "system": true, "ssh_redirect_user": false}]}' \
        '{"users": [{"name": "alice", "ssh-authorized-keys": ["k"], "system": true}]}' \
        '{"users": [{"name": "alice", "no_create_home": "0"}]}' \
        '{"users": [{"name": "alice", "passwd": "$6$x$y", "lock_passwd": false}]}' \
        '{"users": [{"name": "alice", "sudo": ["ALL=(ALL) NOPASSWD:ALL"], "groups": ["adm"]}]}' \
        '{"users": [{"name": "alice", "doas": ["permit nopass alice"]}]}' \
        '{"users": [{"name": "alice", "snapuser": "a@b.c"}]}' \
        '{"users": [{"name": "alice", "primary_group": 5}]}' \
        '{"users": [{"name": "alice", "ssh_authorized_keys": "one\ntwo"}]}' \
        '{"users": [{"name": "élève", "ssh_authorized_keys": ["k"]}]}'; do
        ugm_case "$ugm_cfg" '{"name": "ubuntu"}' '["ssh-rsa AAAA cloud"]'
    done

    # A redirect that is not `true` or `default` is refused -- except that
    # Python compares by value, so 1 is true and 1.0 is too.
    for ugm_redirect in 'true' '"default"' 'false' 'null' '1' '1.0' '2' '0' \
        '""' '"ubuntu"' '"Default"' '[]' '["default"]' '{}' '{"a": 1}'; do
        ugm_case \
            "{\"users\": [\"default\", {\"name\": \"root\", \"ssh_redirect_user\": $ugm_redirect}]}" \
            '{"name": "ubuntu"}' '["ssh-rsa AAAA cloud"]'
    done

    # A redirected user is handed the cloud's keys, not its own.
    for ugm_keys in '[]' '["ssh-rsa AAAA one"]' '["ssh-rsa AAAA one", "ssh-ed25519 BBBB two"]'; do
        ugm_case '{"users": ["default", {"name": "root", "ssh_redirect_user": true}]}' \
            '{"name": "ubuntu"}' "$ugm_keys"
        ugm_case '{"users": ["default", {"name": "root", "ssh_redirect_user": true, "ssh_import_id": ["lp:x"]}]}' \
            '{"name": "ubuntu"}' "$ugm_keys"
    done
fi

# --- cc_ssh ------------------------------------------------------------------
sec "cc-ssh"
# The module that makes the machine reachable, and the one whose effects are
# least reversible: it deletes every host key the image shipped, generates new
# ones, and rewrites root's `authorized_keys`. Both sides therefore record the
# ordered list of calls rather than making them -- running this for real under
# the harness would leave this machine with no host keys and a rewritten root
# account.
#
# What the ordering buys is the part that matters. The deletion happens before
# anything else, so a config that aborts the module halfway leaves a machine
# with no host keys at all; that is only visible if the two sides agree on
# *where* the abort happens, not just that one happened.
#
# The state argument is what the module reads off the running system first:
# which stale keys the glob found, which key files already exist, whether FIPS
# is on, and whether this is a redhat-family distro. Those four decide three of
# the seven call kinds between them.
CCSSH_PY="$(cd "$(dirname "$0")" && pwd)/ccssh.py"
CCSSH_RS="$TARGET/examples/dump-cc-ssh"
if [ -x "$CCSSH_RS" ] && python3 -c 'import cloudinit.config.cc_ssh' 2>/dev/null; then
    CCSSH_RS="$(cd "$(dirname "$CCSSH_RS")" && pwd)/dump-cc-ssh"
    ccssh_n=0
    ccssh_case() {  # $1 config  $2 state  $3 default-user  $4 cloud keys
        ccssh_n=$((ccssh_n + 1))
        run_pair "cc_ssh #$ccssh_n" \
            "cd /tmp && python3 '$CCSSH_PY' '$1' '$2' '$3' '$4'" \
            "cd /tmp && '$CCSSH_RS' '$1' '$2' '$3' '$4'"
    }

    CCSSH_PLAIN='{"stale": [], "existing": [], "fips": false, "redhat": false}'

    # Everything the host-key half decides is decided against the system state,
    # so those cases run against all five shapes of it.
    for ccssh_state in \
        "$CCSSH_PLAIN" \
        '{"stale": ["/etc/ssh/ssh_host_rsa_key", "/etc/ssh/ssh_host_rsa_key.pub"], "existing": [], "fips": false, "redhat": false}' \
        '{"stale": [], "existing": ["/etc/ssh/ssh_host_rsa_key"], "fips": false, "redhat": false}' \
        '{"stale": [], "existing": [], "fips": true, "redhat": false}' \
        '{"stale": [], "existing": [], "fips": false, "redhat": true}'; do
        for ccssh_cfg in \
            '{}' \
            '{"ssh_deletekeys": false}' \
            '{"ssh_deletekeys": 0}' \
            '{"ssh_deletekeys": "false"}' \
            '{"ssh_deletekeys": []}' \
            '{"ssh_deletekeys": null}' \
            '{"ssh_genkeytypes": []}' \
            '{"ssh_genkeytypes": ["rsa"]}' \
            '{"ssh_genkeytypes": ["ed25519"]}' \
            '{"ssh_genkeytypes": ["rsa", "rsa"]}' \
            '{"ssh_genkeytypes": "rsa"}' \
            '{"ssh_genkeytypes": 5}' \
            '{"ssh_genkeytypes": [5]}' \
            '{"ssh_genkeytypes": [null]}' \
            '{"ssh_genkeytypes": [true]}' \
            '{"ssh_genkeytypes": [[1]]}' \
            '{"ssh_genkeytypes": [{"a": 1}]}' \
            '{"ssh_genkeytypes": null}' \
            '{"ssh_quiet_keygen": true}' \
            '{"ssh_quiet_keygen": "yes"}' \
            '{"ssh_quiet_keygen": "no"}' \
            '{"ssh_keys": {}}' \
            '{"ssh_keys": {"rsa_private": "PRIV"}}' \
            '{"ssh_keys": {"rsa_private": "PRIV", "rsa_public": "PUB"}}' \
            '{"ssh_keys": {"rsa_certificate": "CERT"}}' \
            '{"ssh_keys": {"ed25519_private": "P", "ecdsa_private": "Q", "rsa_private": "R"}}' \
            '{"ssh_keys": {"ecdsa-sk_private": "x"}}' \
            '{"ssh_keys": {"nonsense": "x"}}' \
            '{"ssh_keys": {"rsa_private": 5}}' \
            '{"ssh_keys": []}' \
            '{"ssh_keys": 5}'; do
            ccssh_case "$ccssh_cfg" "$ccssh_state" '{"name": "ubuntu"}' '["ssh-rsa AAAA cloud"]'
        done
    done

    # The rest is decided from the config alone.
    for ccssh_cfg in \
        '{"ssh_deletekeys": ""}' \
        '{"ssh_keys": {"rsa_certificate": "C1", "ed25519_certificate": "C2"}}' \
        '{"ssh_keys": {"ed25519-sk_certificate": "x"}}' \
        '{"ssh_keys": {"rsa_private": null}}' \
        '{"ssh_keys": {"rsa_private": ["a"]}}' \
        '{"ssh_keys": "text"}' \
        '{"ssh_keys": null}' \
        '{"ssh_publish_hostkeys": {}}' \
        '{"ssh_publish_hostkeys": {"enabled": false}}' \
        '{"ssh_publish_hostkeys": {"enabled": "no"}}' \
        '{"ssh_publish_hostkeys": {"enabled": 0}}' \
        '{"ssh_publish_hostkeys": {"blacklist": ["dsa"]}}' \
        '{"ssh_publish_hostkeys": {"blacklist": "dsa"}}' \
        '{"ssh_publish_hostkeys": {"blacklist": 5}}' \
        '{"ssh_publish_hostkeys": {"blacklist": [], "enabled": true}}' \
        '{"ssh_publish_hostkeys": ["blacklist"]}' \
        '{"ssh_publish_hostkeys": ["enabled"]}' \
        '{"ssh_publish_hostkeys": ["other"]}' \
        '{"ssh_publish_hostkeys": "blacklist here"}' \
        '{"ssh_publish_hostkeys": "enabled here"}' \
        '{"ssh_publish_hostkeys": "nothing"}' \
        '{"ssh_publish_hostkeys": 5}' \
        '{"ssh_publish_hostkeys": null}' \
        '{"ssh_publish_hostkeys": true}' \
        '{"disable_root": false}' \
        '{"disable_root": "no"}' \
        '{"disable_root_opts": "OPTS"}' \
        '{"disable_root_opts": "user=$USER disable=$DISABLE_USER"}' \
        '{"disable_root_opts": 5}' \
        '{"disable_root_opts": null}' \
        '{"disable_root": false, "disable_root_opts": "x=$USER"}' \
        '{"allow_public_ssh_keys": false}' \
        '{"allow_public_ssh_keys": "no"}' \
        '{"ssh_authorized_keys": ["cfgkey"]}' \
        '{"ssh_authorized_keys": "abc"}' \
        '{"ssh_authorized_keys": {"k": 1}}' \
        '{"ssh_authorized_keys": 5}' \
        '{"ssh_authorized_keys": null}' \
        '{"ssh_authorized_keys": [["a"]]}' \
        '{"ssh_authorized_keys": [{"a": 1}]}' \
        '{"ssh_authorized_keys": [1, true, 1.0, "1"]}' \
        '{"ssh_authorized_keys": [null, false, 0]}' \
        '{"users": ["default"]}' \
        '{"users": []}' \
        '{"users": 5}' \
        '{"users": "alice,bob"}' \
        '{"users": [{"name": "alice", "ssh_authorized_keys": ["k"]}]}' \
        '{"users": [{"name": "alice", "default": true}]}' \
        '{"user": "alice"}' \
        '{"user": {"name": "alice"}}' \
        '{"users": ["default"], "disable_root": false}' \
        '{"users": ["default"], "ssh_authorized_keys": ["cfgkey"]}' \
        '{"ssh_keys": {"rsa_private": "P"}, "ssh_publish_hostkeys": {"enabled": false}, "disable_root": false}' \
        '{"ssh_deletekeys": false, "ssh_genkeytypes": ["ecdsa"], "ssh_quiet_keygen": true}'; do
        ccssh_case "$ccssh_cfg" "$CCSSH_PLAIN" '{"name": "ubuntu"}' '["ssh-rsa AAAA cloud"]'
    done

    # `set(keys)` is what reaches `authorized_keys`, so duplicates collapse and
    # the order they land in is the sorted one, not the configured one. The
    # default user decides whether a second file is written at all.
    for ccssh_du in 'null' '{"name": "ubuntu"}'; do
        for ccssh_keys in \
            '[]' \
            '["ssh-rsa AAAA one"]' \
            '["ssh-rsa BBBB two", "ssh-rsa AAAA one"]' \
            '["ssh-rsa AAAA one", "ssh-rsa AAAA one"]'; do
            for ccssh_cfg in \
                '{}' \
                '{"users": ["default"]}' \
                '{"disable_root": false}' \
                '{"allow_public_ssh_keys": false}' \
                '{"ssh_authorized_keys": ["ssh-rsa AAAA one", "ssh-ed25519 CCCC three"]}'; do
                ccssh_case "$ccssh_cfg" "$CCSSH_PLAIN" "$ccssh_du" "$ccssh_keys"
            done
        done
    done
fi

# --- cc_set_passwords --------------------------------------------------------
sec "cc-set-passwords"
# The other half of "who can log in". Two things make this module worth a large
# block. The first is that almost nothing in it is fatal: a `chpasswd` that
# fails, an `expire` that fails, are collected and the module carries on, so
# `PasswordAuthentication` still gets written and only the *last* collected
# error is re-raised at the very end. The second is that `chpasswd:` is read
# with `in` and `[]` rather than `.get`, so a string or a list there does not
# fail on sight -- it fails only if one of `users`, `list` or `expire` happens
# to be a substring or an element of it, and then with whichever of four
# TypeErrors Python picks. Both halves are easy to get subtly wrong and neither
# shows up until a machine is unreachable.
#
# The state argument is what the module reads off the running system: the ssh
# service name, whether systemd is in charge, whether writing sshd_config would
# actually change it, and what `systemctl show ActiveState` said. The last
# three decide, between them, whether sshd is restarted -- and with which
# job mode, since restarting with dependencies honoured would deadlock against
# this module's own `Before=sshd.service`.
CCSETPW_PY="$(cd "$(dirname "$0")" && pwd)/ccsetpw.py"
CCSETPW_RS="$TARGET/examples/dump-cc-set-passwords"
if [ -x "$CCSETPW_RS" ] && python3 -c 'import cloudinit.config.cc_set_passwords' 2>/dev/null; then
    CCSETPW_RS="$(cd "$(dirname "$CCSETPW_RS")" && pwd)/dump-cc-set-passwords"
    ccsetpw_n=0
    ccsetpw_case() {  # $1 config  $2 state  $3 default-user  $4 args
        ccsetpw_n=$((ccsetpw_n + 1))
        run_pair "cc_set_passwords #$ccsetpw_n" \
            "cd /tmp && python3 '$CCSETPW_PY' '$1' '$2' '$3' '$4'" \
            "cd /tmp && '$CCSETPW_RS' '$1' '$2' '$3' '$4'"
    }

    CCSETPW_PLAIN='{"service": "ssh", "systemd": true, "updated": true, "active": "active"}'

    # The password half is decided from the config alone, but the command line
    # overrides it by deleting `chpasswd["list"]` outright -- which is itself
    # one of the ways this module raises -- and the default user decides
    # whether a bare `password:` reaches anyone at all.
    for ccsetpw_args in '[]' '["fromcli"]'; do
        for ccsetpw_du in 'null' '{"name": "ubuntu"}'; do
            for ccsetpw_cfg in \
                '{}' \
                '{"password": "hunter2"}' \
                '{"password": "hunter2", "users": ["default"]}' \
                '{"password": null, "users": ["default"]}' \
                '{"password": 123, "users": ["default"]}' \
                '{"password": true, "users": ["default"]}' \
                '{"password": "", "users": ["default"]}' \
                '{"password": "hunter2", "users": [{"name": "bob"}]}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": "p1"}]}}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": "p1", "type": "text"}]}}' \
                '{"chpasswd": {"users": [{"name": "bob", "type": "RANDOM"}]}}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": "p1", "type": "hash"}]}}' \
                '{"chpasswd": {"users": [{"name": "a", "type": "RANDOM"}, {"name": "b", "password": "x", "type": "text"}, {"name": "c", "password": "y"}]}}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": "p", "type": "weird"}]}}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": "p", "type": null}]}}' \
                '{"chpasswd": {"users": [{"name": 5, "password": "p", "type": "text"}]}}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": 5, "type": "text"}]}}' \
                '{"chpasswd": {"users": [{"password": "p"}]}}' \
                '{"chpasswd": {"users": [5]}}' \
                '{"chpasswd": {"users": "bob"}}' \
                '{"chpasswd": {"users": null}}' \
                '{"chpasswd": {"list": ["bob:p1", "alice:p2"]}}' \
                '{"chpasswd": {"list": "bob:p1\nalice:p2\n"}}' \
                '{"chpasswd": {"list": ["bob:R", "alice:RANDOM"]}}' \
                '{"chpasswd": {"list": ["bob:$6$salt$hashhash"]}}' \
                '{"chpasswd": {"list": ["bob:$1$salt$hash"]}}' \
                '{"chpasswd": {"list": ["bob:$2a$salt$hash"]}}' \
                '{"chpasswd": {"list": ["bob:$2y$salt$hash"]}}' \
                '{"chpasswd": {"list": ["bob:$5$salt$hash"]}}' \
                '{"chpasswd": {"list": ["bob:$3$salt$hash"]}}' \
                '{"chpasswd": {"list": ["bob:$6$salt$hash:extra"]}}' \
                '{"chpasswd": {"list": ["bob:$6$onlyone"]}}' \
                '{"chpasswd": {"list": ["nocolon"]}}' \
                '{"chpasswd": {"list": []}}' \
                '{"chpasswd": {"list": ""}}' \
                '{"chpasswd": {"list": null}}' \
                '{"chpasswd": {"list": 5}}' \
                '{"chpasswd": {"list": ["bob:p"], "expire": false}}' \
                '{"chpasswd": {"list": ["bob:p"], "expire": "no"}}' \
                '{"chpasswd": {"list": ["bob:$6$a$b"], "expire": true}}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": "p"}], "expire": false}}' \
                '{"chpasswd": {"users": [{"name": "bob", "password": "p"}], "list": ["alice:q"]}}' \
                '{"chpasswd": "a list here"}' \
                '{"chpasswd": ["users"]}' \
                '{"chpasswd": ["list"]}' \
                '{"chpasswd": ["expire"]}' \
                '{"chpasswd": "expire"}' \
                '{"chpasswd": 5}' \
                '{"chpasswd": null}' \
                '{"password": "p", "users": ["default"], "ssh_pwauth": true, "chpasswd": {"expire": false}}'; do
                ccsetpw_case "$ccsetpw_cfg" "$CCSETPW_PLAIN" "$ccsetpw_du" "$ccsetpw_args"
            done
        done
    done

    # The sshd half is decided against the system state, so those cases run
    # against every shape of it. `active`/`activating`/`reloading` are the
    # three that get the deadlock-dodging job mode; the match is
    # case-insensitive, which is why one of them is capitalised here.
    for ccsetpw_state in \
        "$CCSETPW_PLAIN" \
        '{"service": "sshd", "systemd": true, "updated": true, "active": "inactive"}' \
        '{"service": "ssh", "systemd": false, "updated": true, "active": ""}' \
        '{"service": "ssh", "systemd": true, "updated": false, "active": "active"}' \
        '{"service": "sshd.service", "systemd": true, "updated": true, "active": "Activating"}' \
        '{"service": "ssh", "systemd": true, "updated": true, "active": "reloading"}'; do
        for ccsetpw_cfg in \
            '{"ssh_pwauth": true}' \
            '{"ssh_pwauth": false}' \
            '{"ssh_pwauth": "yes"}' \
            '{"ssh_pwauth": "no"}' \
            '{"ssh_pwauth": "unchanged"}' \
            '{"ssh_pwauth": "UNCHANGED"}' \
            '{"ssh_pwauth": "maybe"}' \
            '{"ssh_pwauth": null}' \
            '{"ssh_pwauth": 1}' \
            '{"ssh_pwauth": 0}' \
            '{"ssh_pwauth": 2}' \
            '{"ssh_pwauth": 0.0}' \
            '{"ssh_pwauth": "0.0"}' \
            '{"ssh_pwauth": []}' \
            '{"ssh_pwauth": {}}' \
            '{"ssh_pwauth": 1.5}' \
            '{"chpasswd": {"users": [{"name": "bob", "password": "p"}]}, "ssh_pwauth": 2}'; do
            ccsetpw_case "$ccsetpw_cfg" "$ccsetpw_state" '{"name": "ubuntu"}' '[]'
        done
    done
fi

# --- cc_package_update_upgrade_install ---------------------------------------
sec "cc-package-update-upgrade-install"
# The module that makes `packages:` in custom data mean something. What is
# compared is the ordered list of commands it decides to run, because running
# them for real installs software on the machine doing the comparing.
#
# The interesting surface is not the four booleans -- though those are here,
# with the `bool()`-not-`is_true` truthiness that makes the STRING "false"
# turn updates ON -- but the `packages:` list, which accepts four shapes at
# once: a bare name, a `[name, version]` pair, a `{manager: [...]}` mapping
# that routes entries to apt or snap, and anything else, which is a ValueError
# raised from inside the install rather than while parsing. A mapping under an
# unknown manager key is dropped with a warning, but its entries are still
# validated on the way past, so a malformed entry under a manager that does
# not exist still fails the whole module.
#
# Package ORDER is not compared: upstream builds the apt-get argv out of a
# `set`, so it is different on every boot (bug B72). Both sides sort the
# operands and nothing else.
CCPKG_PY="$(cd "$(dirname "$0")" && pwd)/ccpackages.py"
CCPKG_RS="$TARGET/examples/dump-cc-packages"
if [ -x "$CCPKG_RS" ] && python3 -c 'import cloudinit.config.cc_package_update_upgrade_install' 2>/dev/null; then
    CCPKG_RS="$(cd "$(dirname "$CCPKG_RS")" && pwd)/dump-cc-packages"
    ccpkg_n=0
    ccpkg_case() {  # $1 config  $2 system-info  $3 state
        ccpkg_n=$((ccpkg_n + 1))
        run_pair "cc_packages #$ccpkg_n" \
            "cd /tmp && python3 '$CCPKG_PY' '$1' '$2' '$3'" \
            "cd /tmp && '$CCPKG_RS' '$1' '$2' '$3'"
    }

    CCPKG_UBUNTU='{"distro": "ubuntu"}'
    CCPKG_BOTH='{"apt": true, "snap": true, "all_packages": null, "reboot_marker": null}'

    # The four booleans and the shapes of `packages:`, against a machine that
    # has both managers and no reboot marker.
    for ccpkg_cfg in \
        '{}' \
        '{"package_update": true}' \
        '{"apt_update": true}' \
        '{"package_update": false, "apt_update": true}' \
        '{"package_update": "false"}' \
        '{"package_update": "no"}' \
        '{"package_update": 0}' \
        '{"package_update": []}' \
        '{"package_update": null}' \
        '{"package_upgrade": true}' \
        '{"apt_upgrade": true}' \
        '{"package_upgrade": true, "package_update": true}' \
        '{"packages": ["git"]}' \
        '{"packages": ["git", "curl", "vim"]}' \
        '{"packages": "git"}' \
        '{"packages": []}' \
        '{"packages": null}' \
        '{"packages": 5}' \
        '{"packages": [["git", "1:2.3-4"]]}' \
        '{"packages": [["git", ""]]}' \
        '{"packages": [["git", null]]}' \
        '{"packages": [["git", 0]]}' \
        '{"packages": [["git", "1.0", "extra"]]}' \
        '{"packages": [["git"]]}' \
        '{"packages": [5]}' \
        '{"packages": [null]}' \
        '{"packages": [{"apt": ["git"]}]}' \
        '{"packages": [{"snap": ["hello"]}]}' \
        '{"packages": [{"apt": ["git"], "snap": ["hello"]}]}' \
        '{"packages": [{"snap": ["hello=latest/edge"]}]}' \
        '{"packages": [{"apt": [["git", "1.0"]]}]}' \
        '{"packages": [{"apt": ["git"]}, "curl"]}' \
        '{"packages": ["curl", {"apt": ["git"]}]}' \
        '{"packages": [{"dnf": ["git"]}]}' \
        '{"packages": [{"dnf": [5]}]}' \
        '{"packages": [{"apt": [5]}]}' \
        '{"packages": [{"apt": []}]}' \
        '{"packages": [{}]}' \
        '{"packages": ["git", "git"]}' \
        '{"packages": ["git-"]}' \
        '{"packages": ["ubuntu-desktop^"]}' \
        '{"packages": ["git/stable"]}' \
        '{"package_update": true, "package_upgrade": true, "packages": ["git"]}' \
        '{"apt_reboot_if_required": true}' \
        '{"apt_reboot_if_required": true, "packages": ["git"]}' \
        '{"package_reboot_if_required": true, "package_upgrade": true}' \
        '{"apt_get_wrapper": {"enabled": true}, "packages": ["git"]}'; do
        ccpkg_case "$ccpkg_cfg" "$CCPKG_UBUNTU" "$CCPKG_BOTH"
    done

    # The same decisions against every shape of the machine underneath: one
    # manager missing, an apt cache that knows only some of the names, and
    # each of the two reboot markers.
    for ccpkg_state in \
        "$CCPKG_BOTH" \
        '{"apt": true, "snap": false, "all_packages": null, "reboot_marker": null}' \
        '{"apt": false, "snap": true, "all_packages": null, "reboot_marker": null}' \
        '{"apt": false, "snap": false, "all_packages": null, "reboot_marker": null}' \
        '{"apt": true, "snap": true, "all_packages": ["git", "curl"], "reboot_marker": null}' \
        '{"apt": true, "snap": true, "all_packages": [], "reboot_marker": null}' \
        '{"apt": true, "snap": true, "all_packages": null, "reboot_marker": "/var/run/reboot-required"}' \
        '{"apt": true, "snap": true, "all_packages": null, "reboot_marker": "/run/reboot-needed"}' \
        '{"apt": true, "snap": true, "all_packages": null, "reboot_marker": null, "snap_hold": "forever"}' \
        '{"apt": true, "snap": true, "all_packages": null, "reboot_marker": null, "snap_hold": "2030-01-01T00:00:00Z"}'; do
        for ccpkg_cfg in \
            '{"packages": ["git"]}' \
            '{"packages": ["git", "nosuchpackage"]}' \
            '{"packages": [{"snap": ["hello"]}]}' \
            '{"packages": [{"apt": ["git"], "snap": ["hello"]}]}' \
            '{"package_update": true}' \
            '{"package_upgrade": true}' \
            '{"package_upgrade": true, "apt_reboot_if_required": true}' \
            '{"packages": ["git"], "package_reboot_if_required": true}' \
            '{"apt_reboot_if_required": true}'; do
            ccpkg_case "$ccpkg_cfg" "$CCPKG_UBUNTU" "$ccpkg_state"
        done
    done

    # `apt_get_wrapper` lives in system_info, not in the cloud config, and its
    # default is `auto`: the wrapper is used only if `which` finds it. The
    # non-list, non-string command is a TypeError raised out of the distro's
    # own constructor, before the module gets a say.
    for ccpkg_si in \
        "$CCPKG_UBUNTU" \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {"enabled": false}}}' \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {"enabled": true, "command": ["nice", "-n", "10"]}}}' \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {"enabled": true, "command": "nice"}}}' \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {"enabled": "auto", "command": ["sh"]}}}' \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {"enabled": "auto", "command": ["nosuchwrapper"]}}}' \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {"enabled": true, "command": []}}}' \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {"enabled": "yes", "command": ["sh"]}}}' \
        '{"distro": "ubuntu", "distro_cfg": {"apt_get_wrapper": {}}}' \
        '{"distro": "debian"}' \
        '{"distro": "raspberry-pi-os"}'; do
        for ccpkg_cfg in \
            '{"packages": ["git"]}' \
            '{"package_update": true}' \
            '{"package_upgrade": true}'; do
            ccpkg_case "$ccpkg_cfg" "$ccpkg_si" "$CCPKG_BOTH"
        done
    done
fi

# --- cc_apt_configure --------------------------------------------------------
sec "cc-apt-configure"
# Upstream's largest module, and the one that decides which archive a machine
# installs from. Almost none of it can be run for real on the machine doing the
# comparing -- it imports gpg keys off the network, rewrites /etc/apt and shells
# out to add-apt-repository -- so the pair is split into subcommands, one per
# decision, with the effects captured rather than performed.
#
# Two stubs on both sides: `util.rand_dict_key` becomes a counter, because the
# key it invents ends up in the converted config, and `GPG` answers out of the
# fixture, because the real one would ask keyserver.ubuntu.com.
CCAPT_PY="$(cd "$(dirname "$0")" && pwd)/ccaptconfigure.py"
CCAPT_RS="$TARGET/examples/dump-cc-apt-configure"
if [ -x "$CCAPT_RS" ] && python3 -c 'import cloudinit.config.cc_apt_configure' 2>/dev/null; then
    CCAPT_RS="$(cd "$(dirname "$CCAPT_RS")" && pwd)/dump-cc-apt-configure"
    ccapt_n=0
    ccapt_case() {  # $1 subcommand  $2 config  $3 env
        ccapt_n=$((ccapt_n + 1))
        run_pair "cc_apt_configure #$ccapt_n ($1)" \
            "cd /tmp && python3 '$CCAPT_PY' '$1' '$2' '$3'" \
            "cd /tmp && '$CCAPT_RS' '$1' '$2' '$3'"
    }

    # convert_to_v3_apt_format: three historical config shapes collapsing into
    # one. The v1 list of source strings becomes a mapping under invented keys,
    # the flat v2 keys move under `apt`, and the deprecated proxy spellings are
    # renamed -- two of them to the wrong destination (bug B76).
    for ccapt_cfg in \
        '{}' \
        '{"apt": {}}' \
        '{"apt": {"proxy": "http://p:3128"}}' \
        '{"apt_mirror": "http://m/"}' \
        '{"apt_mirror_search": ["http://a/", "http://b/"]}' \
        '{"apt_mirror_search_dns": true}' \
        '{"apt_proxy": "http://p:3128"}' \
        '{"apt_http_proxy": "http://p:3128"}' \
        '{"apt_ftp_proxy": "http://f:21"}' \
        '{"apt_https_proxy": "http://s:443"}' \
        '{"apt_ftp_proxy": "http://f:21", "apt_https_proxy": "http://s:443"}' \
        '{"apt_preserve_sources_list": true}' \
        '{"apt_custom_sources_list": "deb $MIRROR $RELEASE main"}' \
        '{"add_apt_repo_match": "^x:"}' \
        '{"apt_sources": [{"source": "deb $MIRROR $RELEASE main"}]}' \
        '{"apt_sources": [{"source": "ppa:foo/bar"}]}' \
        '{"apt_sources": [{"source": "deb x", "filename": "given.list"}]}' \
        '{"apt_sources": [{"source": "deb a"}, {"source": "deb b"}]}' \
        '{"apt_sources": {"x.list": {"source": "deb x"}}}' \
        '{"apt_sources": "nope"}' \
        '{"apt_mirror": "http://m/", "apt": {"primary": [{"arches": ["default"], "uri": "http://m/"}]}}' \
        '{"apt_mirror": "http://m/", "apt": {"primary": [{"arches": ["default"], "uri": "http://other/"}]}}' \
        '{"apt_proxy": "http://p:3128", "apt": {"proxy": "http://p:3128"}}' \
        '{"apt": {"primary": [{"arches": ["default"], "uri": "http://m/"}]}}' \
        '{"apt": {"sources": {"x.list": {"source": "deb x"}}}}' \
        '{"apt": "nope"}' \
        '{"apt": []}'; do
        ccapt_case convert "$ccapt_cfg" '{}'
    done

    # apply_apt_config: the proxy and free-form drop-ins, and the removal of a
    # previous boot's file when nothing asks for one. `$2` is the list of paths
    # that already exist, which is what decides whether a removal is planned.
    CCAPT_BOTH='["/etc/apt/apt.conf.d/90cloud-init-aptproxy", "/etc/apt/apt.conf.d/94cloud-init-config"]'
    for ccapt_exists in '[]' "$CCAPT_BOTH"; do
        for ccapt_cfg in \
            '{}' \
            '{"proxy": "http://p:3128"}' \
            '{"http_proxy": "http://p:3128"}' \
            '{"ftp_proxy": "http://f:21"}' \
            '{"https_proxy": "http://s:443"}' \
            '{"proxy": "http://a/", "http_proxy": "http://b/"}' \
            '{"proxy": "http://a/", "ftp_proxy": "http://b/", "https_proxy": "http://c/"}' \
            '{"proxy": ""}' \
            '{"proxy": null}' \
            '{"proxy": false}' \
            '{"proxy": 0}' \
            '{"proxy": 8080}' \
            '{"conf": "APT::Get::Assume-Yes \"true\";"}' \
            '{"conf": ""}' \
            '{"conf": null}' \
            '{"conf": "a\nb\n"}' \
            '{"proxy": "http://p:3128", "conf": "X \"1\";"}'; do
            ccapt_case aptconf "$ccapt_cfg" "$ccapt_exists"
        done
    done

    # generate_sources_list, and so disable_suites: which file the render lands
    # in depends on the rendered *content*, not only on the feature flag, and a
    # deb822 body and a one-line body take different code paths through the
    # suite disabling.
    CCAPT_D822='Types: deb\nURIs: $MIRROR\nSuites: $RELEASE $RELEASE-updates $RELEASE-backports\nComponents: main\n\nTypes: deb\nURIs: $SECURITY\nSuites: $RELEASE-security\nComponents: main\n'
    CCAPT_ONE='deb $MIRROR $RELEASE main\ndeb $MIRROR $RELEASE-updates main\ndeb $SECURITY $RELEASE-security main\n'
    CCAPT_MIRRORS='{"PRIMARY": "http://m/", "SECURITY": "http://s/", "MIRROR": "http://m/"}'
    for ccapt_deb822 in true false; do
        CCAPT_ENV="{\"release\": \"noble\", \"distro\": \"ubuntu\", \"deb822\": $ccapt_deb822, \"mirrors\": $CCAPT_MIRRORS, \"templates\": {\"sources.list.ubuntu.deb822\": \"$CCAPT_D822\", \"sources.list.ubuntu\": \"$CCAPT_ONE\", \"sources.list\": \"$CCAPT_ONE\"}, \"files\": {\"/etc/apt/sources.list\": \"deb http://old/ noble main\\n\"}}"
        for ccapt_cfg in \
            '{}' \
            '{"disable_suites": []}' \
            '{"disable_suites": ["$RELEASE"]}' \
            '{"disable_suites": ["$RELEASE-updates"]}' \
            '{"disable_suites": ["$RELEASE-security"]}' \
            '{"disable_suites": ["updates"]}' \
            '{"disable_suites": ["security"]}' \
            '{"disable_suites": ["backports"]}' \
            '{"disable_suites": ["proposed"]}' \
            '{"disable_suites": ["$RELEASE", "$RELEASE-updates", "$RELEASE-backports", "$RELEASE-security"]}' \
            '{"disable_suites": ["noble"]}' \
            '{"disable_suites": ["nosuchsuite"]}' \
            '{"sources_list": "deb $MIRROR $RELEASE main\n"}' \
            '{"sources_list": "deb $MIRROR $RELEASE main\ndeb $MIRROR $RELEASE-updates main\n", "disable_suites": ["$RELEASE-updates"]}' \
            '{"sources_list": "Types: deb\nURIs: $MIRROR\nSuites: $RELEASE\nComponents: main\n"}' \
            '{"sources_list": ""}' \
            '{"sources_list": "deb $MIRROR\n"}' \
            '{"sources_list": "deb [arch=amd64] $MIRROR\n"}' \
            '{"sources_list": "no substitutions at all\n"}'; do
            ccapt_case sources "$ccapt_cfg" "$CCAPT_ENV"
        done
        # No template at all: the module warns and writes nothing.
        ccapt_case sources '{}' "{\"release\": \"noble\", \"distro\": \"ubuntu\", \"deb822\": $ccapt_deb822, \"mirrors\": $CCAPT_MIRRORS}"
    done

    # add_apt_sources: the key import and the entry it signs. A `ppa:` source
    # goes to add-apt-repository instead of a file, and a dearmour failure
    # leaves /dev/null in the rendered line rather than failing the module.
    CCAPT_KEY='-----BEGIN PGP PUBLIC KEY BLOCK-----\nkeybytes\n-----END PGP PUBLIC KEY BLOCK-----'
    CCAPT_EENV="{\"gpg\": {\"ABCD1234\": \"$CCAPT_KEY\"}, \"params\": {\"MIRROR\": \"http://m/\", \"RELEASE\": \"noble\"}}"
    for ccapt_cfg in \
        '{"sources": {}}' \
        '{"sources": {"x.list": {"source": "deb $MIRROR $RELEASE main"}}}' \
        '{"sources": {"x.list": {"source": "deb $MIRROR $RELEASE main", "append": false}}}' \
        '{"sources": {"x.list": {"source": "deb $MIRROR $RELEASE main", "append": true}}}' \
        '{"sources": {"x": {"source": "deb $MIRROR $RELEASE main"}}}' \
        '{"sources": {"x.list": {"source": "deb $MIRROR $RELEASE main", "filename": "other.list"}}}' \
        '{"sources": {"x.list": {"source": "ppa:foo/bar"}}}' \
        '{"sources": {"x.list": {"source": "cloud-archive:queens"}}}' \
        '{"sources": {"x.list": {"source": "deb [signed-by=$KEY_FILE] $MIRROR $RELEASE main", "keyid": "ABCD1234"}}}' \
        '{"sources": {"x.list": {"source": "deb $MIRROR $RELEASE main", "keyid": "ABCD1234"}}}' \
        '{"sources": {"x.list": {"source": "deb [signed-by=$KEY_FILE] $MIRROR $RELEASE main", "keyid": "ABCD1234", "keyserver": "pgp.example.com"}}}' \
        "{\"sources\": {\"x.list\": {\"source\": \"deb \$MIRROR \$RELEASE main\", \"key\": \"$CCAPT_KEY\"}}}" \
        "{\"sources\": {\"x.list\": {\"source\": \"deb [signed-by=\$KEY_FILE] \$MIRROR \$RELEASE main\", \"key\": \"$CCAPT_KEY\"}}}" \
        '{"sources": {"x.list": {"source": "deb [signed-by=$KEY_FILE] $MIRROR $RELEASE main", "key": "BAD KEY"}}}' \
        '{"sources": {"a.list": {"source": "deb a"}, "b.list": {"source": "deb b"}}}' \
        '{"sources": {"x.list": {}}}' \
        '{"sources": "nope"}' \
        '{"sources": []}' \
        '{"sources": null}'; do
        ccapt_case entries "$ccapt_cfg" "$CCAPT_EENV"
    done

    # find_apt_mirror_info: uri beats search beats search_dns, and a config
    # that names none of them falls through to the datasource's own
    # package_mirrors -- which on an Azure image is the only thing that keeps
    # an arm64 instance off http://ports.ubuntu.com/ubuntu-ports.
    CCAPT_PM='[{"arches": ["i386", "amd64", "arm64"], "failsafe": {"primary": "http://archive.ubuntu.com/ubuntu", "security": "http://security.ubuntu.com/ubuntu"}, "search": {"primary": ["http://azure.archive.ubuntu.com/ubuntu/"], "security": ["http://azure.archive.ubuntu.com/ubuntu/"]}}, {"arches": ["default"], "failsafe": {"primary": "http://ports.ubuntu.com/ubuntu-ports", "security": "http://ports.ubuntu.com/ubuntu-ports"}}]'
    CCAPT_RESOLVE='["http://azure.archive.ubuntu.com/ubuntu/", "http://ubuntu-mirror.example.com/ubuntu/", "http://found.example/ubuntu/"]'
    CCAPT_EC2PM='[{"arches": ["default"], "failsafe": {"primary": "http://archive.ubuntu.com/ubuntu"}, "search": {"primary": ["http://%(ec2_region)s.ec2.archive.ubuntu.com/ubuntu/", "http://%(availability_zone)s.clouds.archive.ubuntu.com/ubuntu/", "http://%(region)s.clouds.archive.ubuntu.com/ubuntu/"]}}]'
    for ccapt_arch in amd64 arm64 s390x; do
        for ccapt_menv in \
            "{\"arch\": \"$ccapt_arch\", \"resolvable\": $CCAPT_RESOLVE}" \
            "{\"arch\": \"$ccapt_arch\", \"resolvable\": $CCAPT_RESOLVE, \"package_mirrors\": $CCAPT_PM}" \
            "{\"arch\": \"$ccapt_arch\", \"resolvable\": [], \"package_mirrors\": $CCAPT_PM}" \
            "{\"arch\": \"$ccapt_arch\", \"resolvable\": $CCAPT_RESOLVE, \"package_mirrors\": $CCAPT_EC2PM, \"availability_zone\": \"us-east-1b\", \"platform_type\": \"ec2\"}" \
            "{\"arch\": \"$ccapt_arch\", \"resolvable\": $CCAPT_RESOLVE, \"package_mirrors\": $CCAPT_EC2PM, \"availability_zone\": \"us-east-1b\", \"platform_type\": \"azure\"}" \
            "{\"arch\": \"$ccapt_arch\", \"resolvable\": $CCAPT_RESOLVE, \"package_mirrors\": $CCAPT_EC2PM, \"region\": \"westus3\"}"; do
            for ccapt_cfg in \
                '{}' \
                '{"primary": [{"arches": ["default"], "uri": "http://p/"}]}' \
                '{"primary": [{"arches": ["default"], "uri": "http://p/"}], "security": [{"arches": ["default"], "uri": "http://s/"}]}' \
                '{"security": [{"arches": ["default"], "uri": "http://s/"}]}' \
                '{"primary": [{"arches": ["default"], "search": ["http://nope/", "http://found.example/ubuntu/"]}]}' \
                '{"primary": [{"arches": ["default"], "search": ["http://nope/"]}]}' \
                '{"primary": [{"arches": ["default"], "search": []}]}' \
                '{"primary": [{"arches": ["default"], "search_dns": true}]}' \
                '{"primary": [{"arches": ["default"], "search_dns": false}]}' \
                '{"primary": [{"arches": ["arm64"], "uri": "http://arm/"}]}' \
                '{"primary": [{"arches": ["default"], "uri": "http://p/", "search": ["http://found.example/ubuntu/"]}]}'; do
                ccapt_case mirrors "$ccapt_cfg" "$ccapt_menv"
            done
        done
    done
fi

# --- authorized_keys and sshd_config -----------------------------------------
sec "authorized-keys-and-sshd-config"
# The authorized-keys parser is the file that decides who can log in, and its
# most important property is not that it understands every line but that it
# preserves the ones it does not: an entry cloud-init cannot parse has to
# survive cloud-init rewriting the file around it. So the comparison below
# checks the whole decomposition of each line -- options, keytype, base64,
# comment, validity -- *and* the text it renders back to.
#
# The options scanner is the sharp edge. It is a hand-written quoting loop with
# two quirks that no reasonable person would guess: a backslash escapes only a
# double quote, and the character before the end of the string is never tested
# for quoting, so an unbalanced quote swallows the line. Getting that wrong in
# the permissive direction attaches someone else's restrictions to a key, or
# drops them.
SSHU_PY="$(cd "$(dirname "$0")" && pwd)/sshutil.py"
SSHU_RS="$TARGET/examples/dump-sshutil"
if [ -x "$SSHU_RS" ] && python3 -c 'import cloudinit.ssh_util' 2>/dev/null; then
    SSHU_RS="$(cd "$(dirname "$SSHU_RS")" && pwd)/dump-sshutil"
    sshu_key="AAAAB3NzaC1yc2EAAAADAQABAAABgQ"
    sshu_n=0
    for sshu_opts in '' 'forced' 'command="echo no"'; do
        for sshu_line in \
            "ssh-rsa $sshu_key alice@host" \
            "ssh-rsa $sshu_key" \
            "ssh-rsa   $sshu_key   a b c " \
            "  ssh-rsa $sshu_key trimmed  " \
            "ssh-ed25519 $sshu_key ed" \
            "ssh-dss $sshu_key nope" \
            "rsa $sshu_key bare" \
            "sk-ssh-ed25519@openssh.com $sshu_key fido" \
            "ecdsa-sha2-nistp521-cert-v01@openssh.com $sshu_key cert" \
            "SSH-RSA $sshu_key uppercase" \
            "ssh-xmss@openssh.com $sshu_key x" \
            '# a comment' \
            '#' \
            '' \
            '   ' \
            'garbage' \
            'one two' \
            "no-pty ssh-rsa $sshu_key bob" \
            "no-pty,no-X11-forwarding ssh-rsa $sshu_key bob" \
            "command=\"echo hi there\" ssh-rsa $sshu_key bob" \
            "command=\"echo \\\"quoted\\\"\" ssh-rsa $sshu_key bob" \
            "command=\"unbalanced ssh-rsa $sshu_key bob" \
            "command=\"a\\\\\" ssh-rsa $sshu_key bob" \
            "from=\"1.2.3.4,5.6.7.8\" ssh-rsa $sshu_key bob" \
            "environment=\"A=b c\" no-pty ssh-rsa $sshu_key" \
            '"' \
            '""' \
            "\"\" ssh-rsa $sshu_key" \
            "a\"b\"c ssh-rsa $sshu_key" \
            'opts' \
            'opts ssh-rsa' \
            "opts ssh-rsa $sshu_key" \
            'opts\tssh-rsa\t'"$sshu_key"'\tcomment' \
            "ssh-rsa $sshu_key comment with spaces" \
            'ssh-rsa ' \
            "ssh-rsa $sshu_key alice@host\r" \
            "ssh-rsa $sshu_key alice@host\n" \
            "caf\xc3\xa9 ssh-rsa $sshu_key unicode-opts"; do
            sshu_n=$((sshu_n + 1))
            run_pair "authkey parse #$sshu_n" \
                "cd /tmp && python3 '$SSHU_PY' parse '$sshu_line' '$sshu_opts'" \
                "cd /tmp && '$SSHU_RS' parse '$sshu_line' '$sshu_opts'"
        done
    done

    # The merge replaces a matching key *in place*, so a second boot leaves the
    # file the same length and leaves hand-written lines where the user put
    # them.
    sshu_upd=0
    for sshu_old in \
        '' \
        '\n' \
        '# mine\n' \
        "ssh-rsa $sshu_key old\n" \
        "# top\nssh-rsa $sshu_key old\nssh-rsa BBBB other\n# bottom\n" \
        "ssh-rsa BBBB other\nssh-rsa $sshu_key old\n" \
        "no-pty ssh-rsa $sshu_key withopts\n" \
        "garbage\nssh-rsa $sshu_key old\n" \
        "ssh-rsa $sshu_key dup\nssh-rsa $sshu_key dup2\n"; do
        for sshu_new in \
            '' \
            "ssh-rsa $sshu_key new\n" \
            'ssh-rsa CCCC fresh\n' \
            "ssh-rsa $sshu_key new\nssh-rsa CCCC fresh\n" \
            'garbage\n' \
            "ssh-rsa $sshu_key one\nssh-rsa $sshu_key two\n" \
            'ssh-ed25519 DDDD ed\n'; do
            for sshu_opts in '' 'forced-opts'; do
                sshu_upd=$((sshu_upd + 1))
                run_pair "authkey update #$sshu_upd" \
                    "cd /tmp && python3 '$SSHU_PY' update '$sshu_old' '$sshu_new' '$sshu_opts'" \
                    "cd /tmp && '$SSHU_RS' update '$sshu_old' '$sshu_new' '$sshu_opts'"
            done
        done
    done

    # `%h` is substituted before `%%`, so `%%h` is not a literal `%h`. That is
    # upstream's ordering and sshd's own, and it is the reason these are
    # compared rather than reasoned about.
    sshu_path=0
    for sshu_value in \
        '' \
        '%h/.ssh/authorized_keys' \
        '.ssh/authorized_keys' \
        '/etc/ssh/keys/%u' \
        '%h/.ssh/ak %u.keys /etc/ssh/%u' \
        '/x/%%h' \
        '%%' \
        '%%u' \
        '%u%h%%' \
        '  spaced   out  ' \
        '\tone\ttwo\t' \
        'relative/path' \
        '%h'; do
        for sshu_home in '/home/alice' '/home/alice/' '' '/'; do
            sshu_path=$((sshu_path + 1))
            run_pair "authkeysfile paths #$sshu_path" \
                "cd /tmp && python3 '$SSHU_PY' paths '$sshu_value' '$sshu_home' alice" \
                "cd /tmp && '$SSHU_RS' paths '$sshu_value' '$sshu_home' alice"
        done
    done

    # sshd_config splits on whitespace first and on `=` only when that found
    # nothing, so `Port = 22` has the value `= 22`; and a repeated keyword
    # takes the *last* value here where sshd itself takes the first.
    sshu_cfg=0
    for sshu_conf in \
        '' \
        '\n' \
        '# comment\n' \
        'PermitRootLogin no\n' \
        'permitrootlogin NO\n' \
        '  PermitRootLogin   no  \n' \
        'Port=22\n' \
        'Port = 22\n' \
        'Port =22\n' \
        'Port= 22\n' \
        'Compression\n' \
        'Compression\nPort 22\n' \
        'StrictModes yes\nStrictModes no\n' \
        'AuthorizedKeysFile %h/.ssh/authorized_keys .ssh/ak\n' \
        '#Port 22\n\n   \nMatch User bob\n  PermitRootLogin yes\n' \
        'key=\n' \
        '=value\n' \
        '=\n' \
        'Include /etc/ssh/sshd_config.d/*.conf\n' \
        'a\tb\n'; do
        sshu_cfg=$((sshu_cfg + 1))
        run_pair "sshd_config #$sshu_cfg" \
            "cd /tmp && python3 '$SSHU_PY' sshdcfg '$sshu_conf'" \
            "cd /tmp && '$SSHU_RS' sshdcfg '$sshu_conf'"
    done

    # --- update_ssh_config_lines ---
    #
    # The pure half of the rewriting family. What is compared is both halves of
    # its answer: which keys it *reports* changed, because that is what decides
    # whether the file gets written at all and sshd restarted, and the file it
    # would leave behind. A keyword already set to the wanted value must come
    # back as no change, or every boot rewrites the config.
    #
    # Keywords are matched case-insensitively but appended with the case the
    # caller used, so the "port" and "PORT" variants are here to pin down which
    # spelling survives.
    sshu_upd_cfg=0
    for sshu_conf in \
        '' \
        '# comment\n' \
        'Port 22\n' \
        'port 22\n' \
        'PORT 22\n' \
        'Port 22\nPort 2222\n' \
        '  Port   22  \n' \
        'Port=22\n' \
        'Port = 22\n' \
        '=22\n' \
        'Compression\n' \
        'nokeyvaluepair\n' \
        '# top\nPort 22\n# bottom\n' \
        'PasswordAuthentication yes\n' \
        'passwordauthentication yes\nPasswordAuthentication no\n' \
        'Match User bob\n    PasswordAuthentication yes\n' \
        'Include /etc/ssh/sshd_config.d/*.conf\nPort 22\n'; do
        for sshu_updates in \
            '' \
            'Port=2222' \
            'Port=22' \
            'port=2222' \
            'Port=' \
            '=2222' \
            'Port=2222\nport=3333' \
            'PasswordAuthentication=no\nKbdInteractiveAuthentication=no' \
            'KbdInteractiveAuthentication=no\nPasswordAuthentication=no' \
            'NewOption=value' \
            'Port=22 22'; do
            sshu_upd_cfg=$((sshu_upd_cfg + 1))
            run_pair "update_ssh_config #$sshu_upd_cfg" \
                "cd /tmp && python3 '$SSHU_PY' updatecfg '$sshu_conf' '$sshu_updates'" \
                "cd /tmp && '$SSHU_RS' updatecfg '$sshu_conf' '$sshu_updates'"
        done
    done

    # --- installing them ---
    #
    # Both sides run against a fixture tree built by sshfixture.sh, which also
    # supplies the passwd and group databases. The "flavour" decides who that
    # database claims owns the tree, which is the only way an unprivileged
    # harness can reach the branches of check_permissions that ask about a path
    # owned by root, by a stranger, or by a group the user happens to be in.
    # Everything else -- which candidate path wins, which mode a new directory
    # gets, whether a chown failure sinks the candidate -- is real.
    SSHU_FIX="$(cd "$(dirname "$0")" && pwd)/sshfixture.sh"
    SSHU_KEY='ssh-rsa AAAAB3NzaC1yc2E alice@host'
    sshu_inst=0

    # The arguments travel in the environment rather than spliced into the
    # command text: several of them are shell fragments carrying quotes of
    # their own, and `run_pair` hands its arguments to `sh -c`.
    sshu_install() {
        sshu_inst=$((sshu_inst + 1))
        SSHU_CFG=$2
        SSHU_EXTRA=$3
        SSHU_KEYS=$4
        SSHU_FLAV=$5
        export SSHU_CFG SSHU_EXTRA SSHU_KEYS SSHU_FLAV
        # Chase the mode-000 directories the cases leave behind, or the next
        # `rm -rf` cannot clean up after them.
        chmod -R u+rwX "$WORK/sshu-py" "$WORK/sshu-rs" 2>/dev/null || true
        run_pair "ssh install #$sshu_inst ($1)" \
            "sh '$SSHU_FIX' '$WORK/sshu-py' \"\$SSHU_CFG\" \"\$SSHU_EXTRA\" \"\$SSHU_FLAV\" &&
             cd /tmp &&
             python3 '$SSHU_PY' install '$WORK/sshu-py' alice \"\$SSHU_KEYS\" |
             sed 's|$WORK/sshu-py|ROOT|g'" \
            "sh '$SSHU_FIX' '$WORK/sshu-rs' \"\$SSHU_CFG\" \"\$SSHU_EXTRA\" \"\$SSHU_FLAV\" &&
             cd /tmp &&
             '$SSHU_RS' install '$WORK/sshu-rs' alice \"\$SSHU_KEYS\" |
             sed 's|$WORK/sshu-rs|ROOT|g'"
    }

    for sshu_flav in own root group other; do
        for sshu_strict in yes no; do
            sshu_conf="AuthorizedKeysFile %h/.ssh/ak\nStrictModes $sshu_strict\n"

            sshu_install "$sshu_flav/$sshu_strict fresh" "$sshu_conf" '' \
                "$SSHU_KEY" "$sshu_flav"
            sshu_install "$sshu_flav/$sshu_strict no keys" "$sshu_conf" '' \
                '' "$sshu_flav"

            for sshu_mode in 700 770 755 707 000; do
                sshu_install "$sshu_flav/$sshu_strict ssh dir $sshu_mode" \
                    "$sshu_conf" \
                    "mkdir -p home/alice/.ssh && chmod $sshu_mode home/alice/.ssh" \
                    "$SSHU_KEY" "$sshu_flav"
            done

            for sshu_mode in 600 640 604 000; do
                sshu_install "$sshu_flav/$sshu_strict key file $sshu_mode" \
                    "$sshu_conf" \
                    "mkdir -p home/alice/.ssh && printf '' > home/alice/.ssh/ak &&
                     chmod $sshu_mode home/alice/.ssh/ak" \
                    "$SSHU_KEY" "$sshu_flav"
            done

            sshu_install "$sshu_flav/$sshu_strict nested" \
                "AuthorizedKeysFile %h/keys/sub/ak\nStrictModes $sshu_strict\n" \
                '' "$SSHU_KEY" "$sshu_flav"
            sshu_install "$sshu_flav/$sshu_strict home 000" "$sshu_conf" \
                'chmod 000 home/alice' "$SSHU_KEY" "$sshu_flav"
            sshu_install "$sshu_flav/$sshu_strict outside home" \
                "AuthorizedKeysFile /var/keys/%u\nStrictModes $sshu_strict\n" \
                '' "$SSHU_KEY" "$sshu_flav"
            sshu_install "$sshu_flav/$sshu_strict three candidates" \
                "AuthorizedKeysFile %h/a/ak %h/b/ak %h/.ssh/ak\nStrictModes $sshu_strict\n" \
                'printf x > home/alice/a' "$SSHU_KEY" "$sshu_flav"
        done
    done

    # The shapes that do not depend on who owns what.
    sshu_install "no sshd_config"        '' '' "$SSHU_KEY"                       own
    sshu_install "two keys"              '' '' "$SSHU_KEY\nssh-ed25519 BBBB bob" own
    sshu_install "invalid key"           '' '' 'not-a-key'                       own
    sshu_install "key with options"      '' '' \
        'no-port-forwarding ssh-rsa AAAAB3NzaC1yc2E alice' own
    sshu_install "relative AuthorizedKeysFile" 'AuthorizedKeysFile .ssh/ak\n' '' \
        "$SSHU_KEY" own
    sshu_install "global AuthorizedKeysFile" 'AuthorizedKeysFile /etc/ssh/global_keys\n' \
        '' "$SSHU_KEY" own
    sshu_install "AuthorizedKeysFile with no value" 'AuthorizedKeysFile\n' '' \
        "$SSHU_KEY" own
    sshu_install "escaped percent" 'AuthorizedKeysFile %%h/ak\n' '' "$SSHU_KEY" own
    sshu_install "equals form" 'AuthorizedKeysFile=%h/.ssh/ak\n' '' "$SSHU_KEY" own
    sshu_install "junk lines in config" \
        'lonely\n=empty\nAuthorizedKeysFile %h/.ssh/ak\n' '' "$SSHU_KEY" own
    sshu_install "unreadable sshd_config" 'AuthorizedKeysFile %h/.ssh/ak\n' \
        'chmod 000 etc/ssh/sshd_config' "$SSHU_KEY" own
    sshu_install "file where a dir must go" 'AuthorizedKeysFile %h/keys/ak\n' \
        'printf x > home/alice/keys' "$SSHU_KEY" own
    sshu_install "symlinked dir in path" 'AuthorizedKeysFile %h/keys/ak\n' \
        'mkdir -p elsewhere && ln -s ../elsewhere home/alice/keys' "$SSHU_KEY" own
    sshu_install "key file is a symlink" 'AuthorizedKeysFile %h/.ssh/ak\n' \
        'mkdir -p home/alice/.ssh && printf t > t &&
         ln -s ../../../t home/alice/.ssh/ak' "$SSHU_KEY" own
    sshu_install "key file is a directory" 'AuthorizedKeysFile %h/.ssh/ak\n' \
        'mkdir -p home/alice/.ssh/ak' "$SSHU_KEY" own
    sshu_install "ssh dir is a file" 'AuthorizedKeysFile %h/.ssh/ak\n' \
        'printf x > home/alice/.ssh' "$SSHU_KEY" own
    sshu_install "existing keys are kept" '' \
        "mkdir -p home/alice/.ssh &&
         printf '# mine\nssh-rsa BBBB bob\n' > home/alice/.ssh/authorized_keys" \
        "$SSHU_KEY" own
    sshu_install "the same key is replaced" '' \
        "mkdir -p home/alice/.ssh &&
         printf 'ssh-rsa AAAAB3NzaC1yc2E old\n' > home/alice/.ssh/authorized_keys" \
        "$SSHU_KEY" own
    sshu_install "unparseable lines survive" '' \
        "mkdir -p home/alice/.ssh &&
         printf 'garbage here\n\n  \nssh-rsa\n' > home/alice/.ssh/authorized_keys" \
        "$SSHU_KEY" own
    sshu_install "existing mode is preserved" '' \
        "mkdir -p home/alice/.ssh && printf '' > home/alice/.ssh/authorized_keys &&
         chmod 640 home/alice/.ssh/authorized_keys" "$SSHU_KEY" own
    sshu_install "no home directory" 'AuthorizedKeysFile %h/.ssh/ak\n' \
        'rm -rf home' "$SSHU_KEY" own

    chmod -R u+rwX "$WORK/sshu-py" "$WORK/sshu-rs" 2>/dev/null || true
fi

# --- network renderer selection ----------------------------------------------
sec "network-renderer-selection"
# `renderers.available()` probes the machine it runs on, so there is no fixture
# to write: the only comparison worth making is whether the two implementations
# agree about *this* host. They either pick the same renderer for a boot here
# or one of them would misconfigure the network.
REND_PY="$(cd "$(dirname "$0")" && pwd)/renderers.py"
REND_RS="$TARGET/examples/dump-renderers"
if [ -x "$REND_RS" ] && python3 -c 'import cloudinit.net.renderers' 2>/dev/null
then
    REND_RS="$(cd "$(dirname "$REND_RS")" && pwd)/dump-renderers"

    # No argument at all is `priority=None`, i.e. DEFAULT_PRIORITY, which is
    # what a boot with no `network: renderers:` setting uses.
    run_pair "renderers (default)" \
        "cd /tmp && python3 '$REND_PY'" \
        "cd /tmp && '$REND_RS'"

    # An empty argument is the empty *list*, which is not the same thing: it
    # searches nothing and reports having searched nothing.
    for priority in "" "netplan" "eni,netplan" "sysconfig" "networkd" \
        "network-manager" "freebsd,netbsd,openbsd" "nosuch" \
        "netplan,nosuch,alsono" "networkd,netplan"; do
        run_pair "renderers [$priority]" \
            "cd /tmp && python3 '$REND_PY' '$priority'" \
            "cd /tmp && '$REND_RS' '$priority'"
    done
fi

# --- network activator selection ---------------------------------------------
sec "network-activator-selection"
# Which activator a boot would use, probed the same way and for the same reason
# as the renderers above. Only selection is compared: actually bringing an
# interface up would reconfigure the machine running the test.
ACT_PY="$(cd "$(dirname "$0")" && pwd)/activators.py"
ACT_RS="$TARGET/examples/dump-activators"
if [ -x "$ACT_RS" ] && python3 -c 'import cloudinit.net.activators' 2>/dev/null
then
    ACT_RS="$(cd "$(dirname "$ACT_RS")" && pwd)/dump-activators"

    run_pair "activators (default)" \
        "cd /tmp && python3 '$ACT_PY'" \
        "cd /tmp && '$ACT_RS'"

    for priority in "" "netplan" "eni,netplan" "networkd" "ifconfig" \
        "network-manager" "networkd,netplan" "nosuch" \
        "netplan,nosuch,alsono" "ifconfig,eni"; do
        run_pair "activators [$priority]" \
            "cd /tmp && python3 '$ACT_PY' '$priority'" \
            "cd /tmp && '$ACT_RS' '$priority'"
    done
fi

# --- fallback network config -------------------------------------------------
sec "fallback-network-config"
# The config a boot with no datasource renders, generated from whatever NICs
# this host has. No fixture for the same reason as the renderers above.
FALLBACK_PY="$(cd "$(dirname "$0")" && pwd)/fallback.py"
FALLBACK_RS="$TARGET/examples/dump-fallback"
if [ -x "$FALLBACK_RS" ] && python3 -c 'import cloudinit.net' 2>/dev/null
then
    FALLBACK_RS="$(cd "$(dirname "$FALLBACK_RS")" && pwd)/dump-fallback"
    run_pair "fallback network config" \
        "cd /tmp && python3 '$FALLBACK_PY'" \
        "cd /tmp && '$FALLBACK_RS'"
fi

# --- dhcpcd lease parsing ----------------------------------------------------
sec "dhcpcd-lease-parsing"
# The fixture carries both halves of what a real run reads: the text
# `dhcpcd --dumplease` prints and the binary lease the unknown options live in.
# Option 245 is the one that matters most here — it is how a machine on Azure
# learns which wireserver to report ready to.
DHCP_PY="$(cd "$(dirname "$0")" && pwd)/dhcp.py"
DHCP_RS="$TARGET/examples/dump-dhcp"
if [ -x "$DHCP_RS" ] && python3 -c 'import cloudinit.net.dhcp' 2>/dev/null; then
    DHCP_RS="$(cd "$(dirname "$DHCP_RS")" && pwd)/dump-dhcp"
    DHCP_D="$WORK/dhcp"
    mkdir -p "$DHCP_D"

    dhcp_pair() {
        run_pair "dhcp lease $1" \
            "cd /tmp && python3 '$DHCP_PY' '$DHCP_D/$1.json'" \
            "cd /tmp && '$DHCP_RS' '$DHCP_D/$1.json'"
    }

    python3 "$(dirname "$0")/dhcpfix.py" "$DHCP_D"
    # `trailing-option` is deliberately not compared: upstream crashes on it
    # (bug B64), and the port refuses the option instead.
    for case in plain routes wireserver other-options no-options quoted \
        odd-routes empty-routes blank-line no-assignments empty-dump \
        collide; do
        dhcp_pair "$case"
    done
fi

# --- shlex and load_shell_content --------------------------------------------
sec "shlex-and-load-shell-content"
# Shared machinery: the klibc reader below is built on it, and it is the one
# piece of Python string handling in the port that had to be written from the
# interpreter's behaviour rather than from a spec. The case file is an
# exhaustive sweep of every one-, two- and three-character string over the
# alphabet that matters, so a disagreement has nowhere to hide.
SHLEX_PY="$(cd "$(dirname "$0")" && pwd)/shellwords.py"
SHLEX_RS="$TARGET/examples/dump-shlex"
if [ -x "$SHLEX_RS" ] && python3 -c 'import cloudinit.util' 2>/dev/null; then
    SHLEX_RS="$(cd "$(dirname "$SHLEX_RS")" && pwd)/dump-shlex"
    SHLEX_F="$WORK/shlex-cases.txt"
    python3 - "$SHLEX_F" <<'PYEOF'
import base64, itertools, random, sys

cases = [
    "a b  c", '"a b" c', "'a b' c", "a#b c", "a #b c", "#whole line",
    "a\\ b", '"a\\"b"', "'a\\'", '"a$b`c\\d"', 'a"b"c', "a\nb",
    "K=v # trailing\nJ=w", "K=", 'K="a b" # c', "", "   ", "a\\\\b",
    '"a\\nb"', '""', "''", "novalue", "K==v", "=v", "K=v=w",
    "DEVICE=eth0\nPROTO=dhcp\n",
    "DEVICE=eth0\n#c\nIPV4ADDR=10.0.0.5\nDOMAINSEARCH='a.com b.com'\n",
]
alpha = " \t\n'\"\\#=ab"
for n in (1, 2, 3):
    cases += ["".join(c) for c in itertools.product(alpha, repeat=n)]
random.seed(7)
cases += [
    "".join(random.choice(alpha) for _ in range(random.randint(4, 9)))
    for _ in range(2000)
]

seen = set()
with open(sys.argv[1], "w") as fp:
    for case in cases:
        if case in seen:
            continue
        seen.add(case)
        fp.write(base64.b64encode(case.encode()).decode() + "\n")
PYEOF
    run_pair "shlex split and load_shell_content" \
        "cd /tmp && python3 '$SHLEX_PY' '$SHLEX_F'" \
        "cd /tmp && '$SHLEX_RS' '$SHLEX_F'"
fi

# --- klibc initramfs network config ------------------------------------------
sec "klibc-initramfs-network-config"
# What a netbooted or iSCSI-booted machine finds in /run. The fixture stands in
# for /run on both sides; the mac addresses still come from this host's real
# /sys/class/net, because that lookup is not injectable upstream.
KLIBC_PY="$(cd "$(dirname "$0")" && pwd)/klibc.py"
KLIBC_RS="$TARGET/examples/dump-klibc"
if [ -x "$KLIBC_RS" ] && python3 -c 'import cloudinit.net.cmdline' 2>/dev/null
then
    KLIBC_RS="$(cd "$(dirname "$KLIBC_RS")" && pwd)/dump-klibc"
    KLIBC_D="$WORK/klibc"
    NIC="$(ls /sys/class/net | grep -v '^lo$' | head -1)"

    mkcase() { mkdir -p "$KLIBC_D/$1"; cat > "$KLIBC_D/$1/$2"; }

    mkcase dhcp net-eth0.conf <<'EOF'
DEVICE=eth0
PROTO=dhcp
IPV4ADDR=10.0.0.5
IPV4BROADCAST=10.0.0.255
IPV4NETMASK=255.255.255.0
IPV4GATEWAY=10.0.0.1
IPV4DNS0=10.0.0.1
IPV4DNS1=0.0.0.0
HOSTNAME=host
ROOTSERVER=10.0.0.1
filename=
UPTIME=1
DOMAINSEARCH=
EOF
    mkcase static net-eth1.conf <<'EOF'
DEVICE=eth1
PROTO=none
IPV4ADDR=192.168.1.10
IPV4BROADCAST=192.168.1.255
IPV4NETMASK=255.255.255.0
IPV4GATEWAY=192.168.1.1
IPV4DNS0=192.168.1.1
IPV4DNS1=8.8.8.8
DOMAINSEARCH='a.example.com b.example.com'
EOF
    mkcase dual net-eth0.conf <<'EOF'
DEVICE=eth0
PROTO=dhcp
IPV4ADDR=10.0.0.5
IPV4NETMASK=255.255.255.0
EOF
    mkcase dual net6-eth0.conf <<'EOF'
DEVICE6=eth0
IPV6PROTO=dhcp6
IPV6ADDR=2001:db8::5
IPV6NETMASK=64
IPV6DNS0=2001:db8::1
IPV6DNS1=::
DOMAINSEARCH=a.com,b.com
EOF
    mkcase filename net-eth0.conf <<'EOF'
DEVICE=eth0
IPV4ADDR=10.0.0.5
filename=pxelinux.0
EOF
    mkcase noproto net-eth0.conf <<'EOF'
DEVICE=eth0
IPV4ADDR=10.0.0.5
EOF
    mkcase off net-eth0.conf <<'EOF'
DEVICE=eth0
PROTO=off
IPV4ADDR=10.0.0.5
EOF
    # PROTO=bootp is what ipconfig writes after a BOOTP configuration that
    # succeeded, and upstream rejects it, taking the local stage with it —
    # see COMPAT.md B69.
    mkcase badproto net-eth0.conf <<'EOF'
DEVICE=eth0
PROTO=bootp
IPV4ADDR=10.0.0.5
EOF
    mkcase nodevice net-eth0.conf <<'EOF'
PROTO=dhcp
IPV4ADDR=10.0.0.5
EOF
    mkcase noequals net-eth0.conf <<'EOF'
DEVICE=eth0
broken
EOF
    mkcase quoted net-eth0.conf <<'EOF'
# klibc never writes a comment, but shlex honours one
DEVICE="eth0"   # trailing
PROTO='dhcp'
IPV4ADDR=10.0.0.5
IPV4DNS0=10.0.0.1
DOMAINSEARCH="x.com y.com"
EOF
    mkdir -p "$KLIBC_D/empty"
    # A name this host really has, so the mac_address lookup has something to
    # find on both sides.
    mkcase realnic net-eth0.conf <<EOF
DEVICE=$NIC
PROTO=dhcp
IPV4ADDR=10.0.0.5
EOF

    for case in dhcp static dual filename noproto off badproto nodevice \
        noequals quoted empty realnic; do
        for kcmdline in "root=/dev/sda1 ro ip=dhcp" "root=/dev/sda1 ro" \
            "ip6=auto quiet"; do
            run_pair "klibc $case [$kcmdline]" \
                "cd /tmp && python3 '$KLIBC_PY' '$KLIBC_D/$case' '$kcmdline'" \
                "cd /tmp && '$KLIBC_RS' '$KLIBC_D/$case' '$kcmdline'"
        done
    done
fi

# --- XML round trip and the Azure password redaction -------------------------
sec "xml-round-trip-and-the-azure-password-redaction"
# `write_files` caches `ovf-env.xml` after replacing the admin password, which
# means re-serialising a parsed document. `ET.tostring` is not a pretty printer:
# it regenerates namespace prefixes, drops declarations nobody used, keeps a
# short list of well-known prefixes, writes `<a />` with a space and escapes tab
# and newline in attributes as zero-padded character references. Every one of
# those is load bearing for a file that gets read back on the next boot, so the
# whole transform is compared rather than just the password.
XML_PY="$(cd "$(dirname "$0")" && pwd)/ettree.py"
XML_RS="$TARGET/examples/dump-xml"
if [ -x "$XML_RS" ]; then
    XML_RS="$(cd "$(dirname "$XML_RS")" && pwd)/dump-xml"
    XML_F="$WORK/xml-cases.txt"
    XML_OVF="$(cd "$(dirname "$0")/../.." && pwd)/test-files/ovf-env.xml"
    python3 - "$XML_F" "$XML_OVF" <<'PYEOF'
import base64, itertools, os, sys

# Both sides must accept every case, or the comparison is between a parse
# failure and a parse failure's wording. So the documents are composed rather
# than fuzzed: well-formed by construction, hostile only in content.
cases = [
    "<a/>", "<a></a>", "<a> </a>", "<a>x</a>", "<a><b/></a>",
    "<a>x<b/>y<c/>z</a>", "<a>\n <b>1</b>\n <b>2</b>\n</a>",
    "<a b='1' c='2'/>", "<a z='1' a='2' m='3'/>",
    "<?xml version='1.0'?><a/>", "<!-- lead --><a><!-- in --><b/></a>",
    "<a><?pi body?><b/></a>", "<a><![CDATA[<raw> & ]]></a>",
    "<a>&amp;&lt;&gt;&quot;&apos;</a>", "<a k='&amp;&lt;&gt;&quot;&apos;'/>",
    "<a k='&#10;&#9;&#13;'/>", "<a>&#10;&#9;&#13;</a>",
    "<a>caf\u00e9 \u4e2d\u6587 \U0001f600</a>", "<a k='caf\u00e9'/>",
    "<a>&#233;&#20013;</a>",
]

# Namespace prefix regeneration, over URIs ElementTree knows and does not.
uris = [
    "urn:alpha",
    "urn:beta",
    "http://www.w3.org/2001/XMLSchema-instance",
    "http://www.w3.org/2001/XMLSchema",
    "http://www.w3.org/XML/1998/namespace",
    "http://purl.org/dc/elements/1.1/",
    "http://schemas.dmtf.org/ovf/environment/1",
    "http://schemas.microsoft.com/windowsazure",
]
prefixes = ["p", "q", "oe", "wa", "ns0", "ns1", "xsi"]
for first, second in itertools.combinations(range(len(uris)), 2):
    for pfx, qfx in itertools.combinations(prefixes, 2):
        decls = 'xmlns:%s="%s" xmlns:%s="%s"' % (pfx, uris[first], qfx,
                                                 uris[second])
        cases.append("<%s:R %s><%s:C>1</%s:C></%s:R>" % (pfx, decls, qfx, qfx,
                                                         pfx))
        # The second declaration is never used, so it must not be written out.
        cases.append("<%s:R %s><%s:C/></%s:R>" % (pfx, decls, pfx, pfx))
        # A prefixed attribute pulls its namespace in too.
        cases.append('<R %s %s:k="v" plain="w"><C/></R>' % (decls, qfx))

# A default namespace has no prefix on the way in and gains one on the way out.
for uri in uris:
    cases.append('<R xmlns="%s"><C>1</C></R>' % uri)
    cases.append('<R xmlns="%s" xmlns:p="urn:beta"><p:C k="v"/></R>' % uri)

# The redaction itself: which tags it hits, and what it leaves alone.
bodies = [
    "<UserPassword>hunter2</UserPassword>",
    "<UserPassword>REDACTED</UserPassword>",
    "<UserPassword/>",
    "<UserPassword></UserPassword>",
    "<UserPassword> </UserPassword>",
    "<MyUserPasswordX>hunter2</MyUserPasswordX>",
    "<UserPasswords><UserPassword>a</UserPassword></UserPasswords>",
    "<Password>hunter2</Password>",
    "<UserPassword k='v'>hunter2</UserPassword>",
    "<UserPassword>a&amp;b&lt;c</UserPassword>",
    "<UserPassword>caf\u00e9</UserPassword>",
    "<UserPassword><Nested>x</Nested></UserPassword>",
]
for body in bodies:
    cases.append("<R>%s</R>" % body)
    cases.append('<R xmlns="urn:alpha">%s</R>' % body)
    cases.append('<w:R xmlns:w="http://schemas.microsoft.com/windowsazure">'
                 '%s</w:R>' % body.replace("<", "<w:").replace("<w:/", "</w:"))

# The real thing, captured off an Azure Gen2 VM.
if os.path.exists(sys.argv[2]):
    with open(sys.argv[2]) as fp:
        ovf = fp.read()
    cases.append(ovf)
    cases.append(ovf.replace("ns0:", "oe:").replace("ns1:", "wa:"))

# The minimal OVF `crawl_metadata` synthesises when only IMDS answered. It is
# the one cached document that has never been through a parser before
# `write_files` sees it, so its round trip is the one that has to hold.
cases.append(
    '<ns0:Environment xmlns:ns0="http://schemas.dmtf.org/ovf/environment/1"\n'
    ' xmlns:ns1="http://schemas.microsoft.com/windowsazure"\n'
    ' xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">\n'
    "  <ns1:ProvisioningSection>\n    <ns1:Version>1.0</ns1:Version>\n"
    "    <ns1:LinuxProvisioningConfigurationSet>\n"
    "      <ns1:ConfigurationSetType>LinuxProvisioningConfiguration\n"
    "      </ns1:ConfigurationSetType>\n"
    "      <ns1:UserName>azureuser</ns1:UserName>\n"
    "      <ns1:DisableSshPasswordAuthentication>true"
    "</ns1:DisableSshPasswordAuthentication>\n"
    "      <ns1:HostName>vm-1</ns1:HostName>\n"
    "    </ns1:LinuxProvisioningConfigurationSet>\n"
    "  </ns1:ProvisioningSection>\n  <ns1:PlatformSettingsSection>\n"
    "    <ns1:Version>1.0</ns1:Version>\n    <ns1:PlatformSettings>\n"
    "      <ns1:ProvisionGuestAgent>true</ns1:ProvisionGuestAgent>\n"
    "    </ns1:PlatformSettings>\n  </ns1:PlatformSettingsSection>\n"
    "</ns0:Environment>\n"
)

seen = set()
with open(sys.argv[1], "w") as fp:
    for case in cases:
        if case in seen:
            continue
        seen.add(case)
        fp.write(base64.b64encode(case.encode()).decode() + "\n")
PYEOF
    run_pair "xml tostring and password redaction" \
        "cd /tmp && python3 '$XML_PY' '$XML_F'" \
        "cd /tmp && '$XML_RS' '$XML_F'"
fi

# --- cloud-id ----------------------------------------------------------------
sec "cloud-id"
if [ -x "$PY_CLOUD_ID" ]; then
    for opts in "" "-l" "-j"; do
        run_pair "cloud-id $opts" \
            "$PY_CLOUD_ID -i '$ID' $opts" \
            "$TARGET/cloud-id -i '$ID' $opts"
    done
fi

# --- boot stages -------------------------------------------------------------
sec "boot-stages"

compare_stage_artifact() {
    if diff -u "$stages/py.$2" "$stages/rs.$2" >"$WORK/stage.diff" 2>&1; then
        pass=$((pass + 1))
        printf 'ok   %s\n' "$1"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n' "$1"
        sed 's/^/  /' "$WORK/stage.diff"
    fi
}

# Each implementation gets its own relocated root so the stages can run
# unprivileged and leave the live host alone. `datasource_list: []` makes the
# outcome deterministic: no datasource can ever be found. `$3` is appended to
# `system_info:` at its own indent, which is where `distro` belongs.
compare_boot_stages() {
    suffix=$2
    stages="$WORK/stages-$1"
    for impl in py rs; do
        root="$stages/$impl"
        mkdir -p "$root/run"
        cat >"$root/cfg.yaml" <<EOF
datasource_list: []
def_log_file: $root/cloud-init.log
log_cfgs: []
system_info:
  paths:
    cloud_dir: $root/var
    run_dir: $root/run
$3
EOF
    done

    for args in "init --local" "init" "modules --mode=config" \
        "modules --mode=final"; do
        run_pair "cloud-init $args$suffix" \
            "cd /tmp && CLOUD_CFG='$stages/py/cfg.yaml' python3 -m cloudinit.cmd.main $args" \
            "cd /tmp && CLOUD_CFG='$stages/rs/cfg.yaml' '$CI_RS' $args"
    done

    # `data/python-version` is written by the interpreter-change cache purge,
    # which the port has no equivalent of (COMPAT.md deviation 48). `run/.impl`
    # is the port's alone by definition: it records *which* implementation is
    # driving the boot, so upstream writing it too would defeat the point
    # (deviation 103).
    for impl in py rs; do
        (cd "$stages/$impl" && find var run -printf '%y %m %p\n') |
            grep -Ev 'python-version|run/\.impl' | sort >"$stages/$impl.tree"
    done
    compare_stage_artifact "cloud-init boot stages$suffix (state tree)" tree

    for impl in py rs; do
        cp "$stages/$impl/var/data/result.json" "$stages/$impl.result"
    done
    compare_stage_artifact "cloud-init boot stages$suffix (result.json)" result

    # `start`, `finished` and `recoverable_errors` are timing- and
    # environment-dependent, so only the stable fields are compared.
    for impl in py rs; do
        python3 - "$stages/$impl/var/data/status.json" \
            >"$stages/$impl.status" <<'PYEOF'
import json
import sys

with open(sys.argv[1]) as handle:
    v1 = json.load(handle)["v1"]
for stage in v1.values():
    if isinstance(stage, dict):
        for field in ("start", "finished", "recoverable_errors"):
            stage.pop(field, None)
print(json.dumps(v1, indent=1, sort_keys=True))
PYEOF
    done
    compare_stage_artifact "cloud-init boot stages$suffix (status.json)" status
}

compare_boot_stages default "" ""

# `distros.fetch` raises before a datasource is ever looked for, so this pins
# the one error that outranks "no datasource found" — and, with it, that the
# port resolves the distro at the same point in the stage that upstream does.
compare_boot_stages nosuchdistro " [nosuchdistro]" "  distro: nosuchdistro"

# --- boot stage reporting ----------------------------------------------------
sec "boot-stage-reporting"
# A `print` handler routes the reporting events to stdout, which run_pair can
# compare once the wall-clock durations are masked. `logging: null` drops the
# default handler so nothing else competes for the stream.
REPORT="$WORK/reporting"
NORM="sed -E 's/\(duration: [0-9.]+s\)/(duration: Xs)/'"
for impl in py rs; do
    root="$REPORT/$impl"
    mkdir -p "$root/run"
    cat >"$root/cfg.yaml" <<EOF
datasource_list: []
def_log_file: $root/cloud-init.log
log_cfgs: []
reporting:
  logging: null
  myprint:
    type: print
system_info:
  paths:
    cloud_dir: $root/var
    run_dir: $root/run
EOF
done

for args in "init --local" "init" "modules --mode=config" "modules --mode=final"; do
    run_pair "cloud-init $args (reporting)" \
        "cd /tmp && CLOUD_CFG='$REPORT/py/cfg.yaml' python3 -m cloudinit.cmd.main $args >'$REPORT/py.raw'; rc=\$?; $NORM '$REPORT/py.raw'; exit \$rc" \
        "cd /tmp && CLOUD_CFG='$REPORT/rs/cfg.yaml' '$CI_RS' $args >'$REPORT/rs.raw'; rc=\$?; $NORM '$REPORT/rs.raw'; exit \$rc"
done

# --- boot stage logging ------------------------------------------------------
sec "boot-stage-logging"
LOGD="$WORK/logging"

for impl in py rs; do
    root="$LOGD/$impl"
    mkdir -p "$root/var/data" "$root/run"
    {
        echo "datasource_list: []"
        echo "def_log_file: $root/cloud-init.log"
        echo "log_cfgs:"
        echo "  - |"
        echo "$LOG_INI" | sed 's/^/    /'
        echo "    args=('$root/cloud-init.log', 'a', 'UTF-8')"
        echo "system_info:"
        echo "  paths:"
        echo "    cloud_dir: $root/var"
        echo "    run_dir: $root/run"
    } >"$root/cfg.yaml"
done

# Only the lines both sides claim to emit; upstream also logs stdin handling,
# output redirection and its own PID, none of which the port implements.
LOG_LINES="grep -E 'Cloud-init v\.|No local datasource found|Exiting without datasource'"
LOG_NORM="sed -E 's/\(duration: [0-9.]+s\)/(duration: Xs)/; s/^[0-9-]+ [0-9:,]+ - //; s/(Cloud-init v\.) \S+ (running .*) at .*/\1 X \2/'"
# `python3 -m cloudinit.cmd.main` cannot be used here: it makes the main module
# `__main__`, which `fileConfig` then disables (COMPAT.md B29).
run_pair "cloud-init init --local (log file)" \
    "cd /tmp && CLOUD_CFG='$LOGD/py/cfg.yaml' /usr/bin/cloud-init init --local >/dev/null 2>&1; rc=\$?; $LOG_LINES '$LOGD/py/cloud-init.log' | $LOG_NORM; exit \$rc" \
    "cd /tmp && CLOUD_CFG='$LOGD/rs/cfg.yaml' '$CI_RS' init --local >/dev/null 2>&1; rc=\$?; $LOG_LINES '$LOGD/rs/cloud-init.log' | $LOG_NORM; exit \$rc"

# --- recoverable errors ------------------------------------------------------
sec "recoverable-errors"
RECOV="$WORK/recoverable"
for impl in py rs; do
    root="$RECOV/$impl"
    mkdir -p "$root/var/data" "$root/run"
    cat >"$root/cfg.yaml" <<EOF
datasource_list: []
def_log_file: $root/cloud-init.log
log_cfgs: []
system_info:
  paths:
    cloud_dir: $root/var
    run_dir: $root/run
EOF
    # A config stage that started and never finished, so the wrapper warns.
    python3 -c "
import json
v = {'v1': {'datasource': None, 'stage': None}}
for m in ('init', 'init-local', 'modules-config', 'modules-final'):
    v['v1'][m] = {'errors': [], 'finished': None, 'start': None,
                  'recoverable_errors': {}}
v['v1']['modules-config']['start'] = 1.0
json.dump(v, open('$root/var/data/status.json', 'w'))
"
done

# Upstream puts the list through `set()`, so the order varies between runs
# (COMPAT.md B26); sorting is what makes this comparable at all.
SHOW_RECOV="python3 -c \"import json,sys
e = json.load(open(sys.argv[1]))['v1']['modules-config']['recoverable_errors']
print(json.dumps({k: sorted(v) for k, v in e.items()}, indent=1, sort_keys=True))\""
run_pair "cloud-init modules --mode=config (recoverable errors)" \
    "cd /tmp && CLOUD_CFG='$RECOV/py/cfg.yaml' /usr/bin/cloud-init modules --mode=config >/dev/null 2>&1; rc=\$?; $SHOW_RECOV '$RECOV/py/var/data/status.json'; exit \$rc" \
    "cd /tmp && CLOUD_CFG='$RECOV/rs/cfg.yaml' '$CI_RS' modules --mode=config >/dev/null 2>&1; rc=\$?; $SHOW_RECOV '$RECOV/rs/var/data/status.json'; exit \$rc"

# --- the cache-trust decision ------------------------------------------------
sec "the-cache-trust-decision"
# `manual_cache_clean`, from the config or from the marker the instance
# directory carries, is what makes the local stage trust a cached datasource
# instead of re-checking it. The `print` reporting handler puts the resulting
# `check-cache` event on stdout, which is the only place it is observable.
TRUST="$WORK/trust"
CACHE_LINE="grep -F 'check-cache'"

trust_case() {
    label=$1
    shift
    for impl in py rs; do
        rm -rf "${TRUST:?}/$impl"
        mkdir -p "$TRUST/$impl/run" "$TRUST/$impl/var/instances/i-0001"
        ln -s "$TRUST/$impl/var/instances/i-0001" "$TRUST/$impl/var/instance"
        cat >"$TRUST/$impl/cfg.yaml" <<EOF
datasource_list: []
def_log_file: $TRUST/$impl/cloud-init.log
log_cfgs: []
reporting:
  logging: null
  myprint:
    type: print
$1
system_info:
  paths:
    cloud_dir: $TRUST/$impl/var
    run_dir: $TRUST/$impl/run
EOF
        [ -n "$2" ] && : >"$TRUST/$impl/var/instances/i-0001/manual-clean"
    done
    run_pair "cloud-init init --local ($label)" \
        "cd /tmp && CLOUD_CFG='$TRUST/py/cfg.yaml' /usr/bin/cloud-init init --local 2>/dev/null | $CACHE_LINE" \
        "cd /tmp && CLOUD_CFG='$TRUST/rs/cfg.yaml' '$CI_RS' init --local 2>/dev/null | $CACHE_LINE"
}

trust_case "cache checked by default" "" ""
trust_case "cache trusted from config" "manual_cache_clean: true" ""
trust_case "cache trusted from marker" "" "marker"
# `no` is truthy to Python but false to `translate_bool`, which is what the
# option is actually read with.
trust_case "cache checked when the flag says no" "manual_cache_clean: no" ""

# --- --force -----------------------------------------------------------------
sec "force"
# `--force` is documented as running "even if no datasource is found", but with
# no datasource there is no instance id, so upstream dies on the next line every
# single time (COMPAT.md B31). Only the stage that ran is read back: upstream
# also files the error under the three that did not (B32, deviation 63).
FORCE="$WORK/force"
SHOW_ERRORS="python3 -c \"import json,sys
print(json.dumps(json.load(open(sys.argv[1]))['v1'][sys.argv[2]]['errors'], indent=1))\""

force_case() {
    stage=$2
    for impl in py rs; do
        rm -rf "${FORCE:?}/$impl"
        mkdir -p "$FORCE/$impl/run"
        cat >"$FORCE/$impl/cfg.yaml" <<EOF
datasource_list: []
def_log_file: $FORCE/$impl/cloud-init.log
log_cfgs: []
system_info:
  paths:
    cloud_dir: $FORCE/$impl/var
    run_dir: $FORCE/$impl/run
EOF
    done
    run_pair "cloud-init --force $1" \
        "cd /tmp && CLOUD_CFG='$FORCE/py/cfg.yaml' /usr/bin/cloud-init --force $1 >/dev/null 2>&1; rc=\$?; $SHOW_ERRORS '$FORCE/py/run/status.json' '$stage'; exit \$rc" \
        "cd /tmp && CLOUD_CFG='$FORCE/rs/cfg.yaml' '$CI_RS' --force $1 >/dev/null 2>&1; rc=\$?; $SHOW_ERRORS '$FORCE/rs/run/status.json' '$stage'; exit \$rc"
}

force_case "init --local" init-local
force_case "init" init
force_case "modules --mode=config" modules-config

# --- merged system config ----------------------------------------------------
sec "merged-system-config"
CFG_RS="$(cd "$TARGET/examples" && pwd)/dump-config"
CFG_PY="$(cd "$(dirname "$0")" && pwd)/config.py"
MERGE="$WORK/merge"
mkdir -p "$MERGE/var/instance" "$MERGE/run"
echo "my_marker: from-instance" >"$MERGE/var/instance/cloud-config.txt"
echo "vendored: from-vendor" >"$MERGE/var/instance/vendor-cloud-config.txt"
echo "vendored: from-vendor2" >"$MERGE/var/instance/vendor2-cloud-config.txt"
cat >"$MERGE/relocated.yaml" <<EOF
datasource_list: []
locale: en_GB
system_info:
  paths:
    cloud_dir: $MERGE/var
    run_dir: $MERGE/run
EOF
# Moving only `cloud_dir` does not trigger upstream's second pass, so the
# instance layer above is not picked up at all (B30).
cat >"$MERGE/cloud-dir-only.yaml" <<EOF
system_info:
  paths:
    cloud_dir: $MERGE/var
EOF
printf 'locale: fr_FR\n' >"$MERGE/first.yaml"
printf 'locale: de_DE\ntimezone: UTC\n' >"$MERGE/second.yaml"
printf 'ntp: [unterminated\n' >"$MERGE/bad.yaml"

compare_merged_config() {
    run_pair "merged config ($1)" \
        "cd /tmp && $2 python3 '$CFG_PY' $3" \
        "cd /tmp && $2 '$CFG_RS' $3"
}

compare_merged_config "system config only" "" ""
compare_merged_config "env config layered on top" "CLOUD_CFG='$MERGE/relocated.yaml'" ""
compare_merged_config "cloud_dir moved alone" "CLOUD_CFG='$MERGE/cloud-dir-only.yaml'" ""
compare_merged_config "env config that does not exist" "CLOUD_CFG='$MERGE/absent.yaml'" ""
compare_merged_config "two --file configs" "CLOUD_CFG='$MERGE/relocated.yaml'" \
    "'$MERGE/first.yaml' '$MERGE/second.yaml'"
compare_merged_config "an unparsable --file config" "CLOUD_CFG='$MERGE/relocated.yaml'" \
    "'$MERGE/bad.yaml' '$MERGE/first.yaml'"
compare_merged_config "a --file config that does not exist" "CLOUD_CFG='$MERGE/relocated.yaml'" \
    "'$MERGE/absent.yaml'"

# --- module sections ---------------------------------------------------------
sec "module-sections"
# `Modules._read_modules` / `_fixup_modules` / `run_section`, up to but not
# including the call into the module. Entry shapes that make upstream raise are
# a documented deviation (COMPAT.md 86) and so are deliberately absent here:
# the port warns and drops where upstream dies, and the harness compares exit
# codes.
MOD_PY="$(cd "$(dirname "$0")" && pwd)/modules.py"
MOD_RS="$TARGET/examples/dump-modules"
if [ -x "$MOD_RS" ]; then
    MOD_RS="$(cd "$(dirname "$MOD_RS")" && pwd)/dump-modules"
    MODS="$WORK/modules"
    mkdir -p "$MODS"

    # Every entry shape, plus canonicalisation, an unknown module, a removed
    # one, a renamed one, and a bogus frequency.
    cat >"$MODS/shapes.json" <<'EOF'
{"bootcmd": [], "runcmd": [], "apk_repos": {}, "keyboard": {},
 "m": ["bootcmd", ["runcmd", "always"], ["ntp", "always", "x", "y"],
       {"name": "ansible", "frequency": "once", "args": ["a"]},
       {"name": "apt_configure"}, {"name": "ca_certs", "args": []},
       {"name": "ntp", "args": null}, {"nothing": 1}, [], {}, null, "",
       "nope", "migrator", "emit_upstart", "rightscale_userdata",
       "refresh_rmc_and_interface", "ubuntu_advantage",
       ["keyboard", "bogusfreq"], "Bootcmd", "apk-configure",
       "cc_power_state_change.py", " spaced ", "cc_", ".py",
       ["cc_bootcmd", "always", 1, {"k": "v"}],
       {"name": " padded ", "frequency": " once "}]}
EOF

    # Distro filtering, `activate_by_schema_keys`, and the override.
    cat >"$MODS/select.json" <<'EOF'
{"unverified_modules": ["apk_configure", "not_here"],
 "ansible": {}, "ca_certs": {},
 "m": ["apk_configure", "ansible", "ca_certs", "ubuntu_pro",
       "power_state_change", "byobu", "apt_configure", "yum_add_repo"]}
EOF

    # Sections that are not lists. A string iterates per character and a
    # mapping iterates its keys; both are reproduced rather than rejected.
    cat >"$MODS/odd.json" <<'EOF'
{"s": "bootcmd", "d": {"runcmd": 1, "bootcmd": 2}, "e": [], "n": null, "z": 0,
 "one": ["bootcmd"]}
EOF

    compare_modules() {
        run_pair "modules $1 ($2, $3)" \
            "cd /tmp && python3 '$MOD_PY' '$MODS/$1.json' '$2' '$3'" \
            "cd /tmp && '$MOD_RS' '$MODS/$1.json' '$2' '$3'"
    }

    for distro in ubuntu alpine debian rhel photon; do
        compare_modules shapes m "$distro"
        compare_modules select m "$distro"
    done
    for section in s d e n z one absent; do
        compare_modules odd "$section" ubuntu
    done
fi

# --- config module bodies ----------------------------------------------------
sec "config-module-bodies"
# The `cc_*` handlers themselves, run for real. Only the modules that touch
# nothing but the paths their own config names can be compared this way: every
# fixture below writes inside the scratch directory and the dump is the tree
# that results, so a module that reached outside shows up as a missing file.
#
# Ownership is the one thing deliberately not exercised. The harness has to run
# unprivileged, and `chown`ing to root would fail on both sides for a reason
# that has nothing to do with the port, so every entry says `-1:-1` — upstream's
# "leave it alone". The chown itself has unit tests.
CC_PY="$(cd "$(dirname "$0")" && pwd)/ccmodule.py"
CC_RS="$TARGET/examples/dump-cc"
if [ -x "$CC_RS" ]; then
    CC_RS="$(cd "$(dirname "$CC_RS")" && pwd)/dump-cc"
    CC="$WORK/ccmodule"
    mkdir -p "$CC"

    # `@@` stands in for the scratch directory. Both sides use the *same* one,
    # reset between the two runs, so that a fixture may write its own absolute
    # path into a file without the two dumps disagreeing about it. An optional
    # fourth argument is a shell snippet run after the reset — that is how the
    # modules whose input is a *directory* rather than a config key get
    # anything to work on. An optional fifth names scratch-relative paths whose
    # content is masked, for the files that cannot agree between two runs.
    compare_cc() {
        cc_module=$1
        cc_label=$2
        cc_body=$3
        cc_prep=${4-}
        cc_mask=${5-}
        printf '%s\n' "$cc_body" | sed "s#@@#$CC/work#g" >"$CC/cfg.json"
        {
            printf 'rm -rf %s\nmkdir -p %s\n' "$CC/work" "$CC/work"
            printf '%s\n' "$cc_prep" | sed "s#@@#$CC/work#g"
        } >"$CC/setup.sh"
        run_pair "$cc_module ($cc_label)" \
            "cd /tmp && sh '$CC/setup.sh' && python3 '$CC_PY' '$cc_module' '$CC/cfg.json' '$CC/work' '$cc_mask'" \
            "cd /tmp && sh '$CC/setup.sh' && '$CC_RS' '$cc_module' '$CC/cfg.json' '$CC/work' '$cc_mask'"
    }

    compare_write_files() {
        compare_cc cc_write_files "$1" "$2"
    }

    # Nothing to do, in each of the three ways there are.
    compare_write_files "absent" '{}'
    compare_write_files "empty" '{"write_files": []}'
    compare_write_files "all deferred" \
        '{"write_files": [{"path": "@@/d", "content": "x", "defer": true},
                          {"path": "@@/e", "content": "x", "defer": "yes"}]}'
    # `defer` is only honoured when `translate_bool` says so: 2 and "maybe" are
    # truthy to Python but false here, so those entries are written.
    compare_write_files "defer is not python truthiness" \
        '{"write_files": [{"path": "@@/a", "content": "a", "defer": 2, "owner": "-1:-1"},
                          {"path": "@@/b", "content": "b", "defer": "maybe", "owner": "-1:-1"},
                          {"path": "@@/c", "content": "c", "defer": 1, "owner": "-1:-1"},
                          {"path": "@@/d", "content": "d", "defer": "on", "owner": "-1:-1"}]}'

    # Content, or the lack of it, and the parent directories that have to
    # appear first.
    compare_write_files "content and parents" \
        '{"write_files": [{"path": "@@/one/two/three/f", "content": "nested\n", "owner": "-1:-1"},
                          {"path": "@@/blank", "owner": "-1:-1"},
                          {"path": "@@/null", "content": null, "owner": "-1:-1"},
                          {"path": "@@/multi", "content": "a\nb\n", "owner": "-1:-1"}]}'

    # `os.path.abspath` is lexical: `..` removes the previous component whether
    # or not it was a directory that exists.
    compare_write_files "paths are normalised lexically" \
        '{"write_files": [{"path": "@@/x/../y//./z", "content": "z\n", "owner": "-1:-1"}]}'

    # `decode_perms`, including the values it refuses and the one it silently
    # skips the chmod for.
    compare_write_files "permissions" \
        '{"write_files": [{"path": "@@/str", "content": "1", "permissions": "0600", "owner": "-1:-1"},
                          {"path": "@@/bare", "content": "1", "permissions": "755", "owner": "-1:-1"},
                          {"path": "@@/int", "content": "1", "permissions": 420, "owner": "-1:-1"},
                          {"path": "@@/bool", "content": "1", "permissions": true, "owner": "-1:-1"},
                          {"path": "@@/float", "content": "1", "permissions": 493.7, "owner": "-1:-1"},
                          {"path": "@@/zero", "content": "1", "permissions": 0, "owner": "-1:-1"},
                          {"path": "@@/null", "content": "1", "permissions": null, "owner": "-1:-1"},
                          {"path": "@@/absent", "content": "1", "owner": "-1:-1"},
                          {"path": "@@/bogus", "content": "1", "permissions": "not-a-mode", "owner": "-1:-1"},
                          {"path": "@@/nine", "content": "1", "permissions": "649", "owner": "-1:-1"},
                          {"path": "@@/list", "content": "1", "permissions": [6, 4, 4], "owner": "-1:-1"}]}'

    # Every spelling of every encoding, plus one that is not a spelling of any.
    compare_write_files "encodings" \
        '{"write_files": [{"path": "@@/b64", "encoding": "b64", "content": "cGxhaW4gcGF5bG9hZAo=", "owner": "-1:-1"},
                          {"path": "@@/base64", "encoding": "BASE64", "content": "cGxhaW4gcGF5bG9hZAo=", "owner": "-1:-1"},
                          {"path": "@@/gzb64", "encoding": "gz+b64", "content": "H4sIAAAAAAAC/0vOzy0oSi0uTk1RKEiszMlPTOECAFg6AQ4TAAAA", "owner": "-1:-1"},
                          {"path": "@@/gzipbase64", "encoding": " gzip+base64 ", "content": "H4sIAAAAAAAC/0vOzy0oSi0uTk1RKEiszMlPTOECAFg6AQ4TAAAA", "owner": "-1:-1"},
                          {"path": "@@/plain", "encoding": "text/plain", "content": "cGxhaW4gcGF5bG9hZAo=", "owner": "-1:-1"},
                          {"path": "@@/empty", "encoding": "", "content": "as-is\n", "owner": "-1:-1"},
                          {"path": "@@/nullenc", "encoding": null, "content": "as-is\n", "owner": "-1:-1"},
                          {"path": "@@/unknown", "encoding": "rot13", "content": "as-is\n", "owner": "-1:-1"}]}'

    # A failure part-way through leaves the earlier entries on disk. Both sides
    # abort the module and exit non-zero.
    compare_write_files "a bad gzip stream aborts the rest" \
        '{"write_files": [{"path": "@@/before", "content": "kept\n", "owner": "-1:-1"},
                          {"path": "@@/bad", "encoding": "gzip", "content": "not gzip", "owner": "-1:-1"},
                          {"path": "@@/after", "content": "never\n", "owner": "-1:-1"}]}'

    # A missing or empty `path` is a warning and a skip, not a failure.
    compare_write_files "entries without a path are skipped" \
        '{"write_files": [{"content": "orphan\n", "owner": "-1:-1"},
                          {"path": "", "content": "orphan\n", "owner": "-1:-1"},
                          {"path": null, "content": "orphan\n", "owner": "-1:-1"},
                          {"path": "@@/kept", "content": "kept\n", "owner": "-1:-1"}]}'

    # `append` decides between truncating and adding, on the same
    # `translate_bool` rules as `defer`.
    compare_write_files "append" \
        '{"write_files": [{"path": "@@/log", "content": "one\n", "owner": "-1:-1"},
                          {"path": "@@/log", "content": "two\n", "append": true, "owner": "-1:-1"},
                          {"path": "@@/log", "content": "three\n", "append": "yes", "owner": "-1:-1"},
                          {"path": "@@/log", "content": "four\n", "append": 2, "owner": "-1:-1"}]}'

    # cc_write_files_deferred: the complement of the filter above, so the two
    # modules between them write every entry exactly once.
    compare_cc cc_write_files_deferred "absent" '{}'
    compare_cc cc_write_files_deferred "nothing deferred" \
        '{"write_files": [{"path": "@@/now", "content": "x", "owner": "-1:-1"}]}'
    compare_cc cc_write_files_deferred "only the deferred entries" \
        '{"write_files": [{"path": "@@/now", "content": "now\n", "owner": "-1:-1"},
                          {"path": "@@/later", "content": "later\n", "defer": true, "owner": "-1:-1"},
                          {"path": "@@/maybe", "content": "maybe\n", "defer": "yes", "owner": "-1:-1"},
                          {"path": "@@/never", "content": "never\n", "defer": 2, "owner": "-1:-1"}]}'

    # cc_runcmd: writes `<instance>/scripts/runcmd` and runs nothing. The
    # instance directory comes from `system_info.paths.cloud_dir`, so it is
    # templated into the scratch directory like any other path.
    cc_runcmd() {
        compare_cc cc_runcmd "$1" \
            "{\"system_info\": {\"paths\": {\"cloud_dir\": \"@@/cloud\", \"run_dir\": \"@@/run\"}}$2}"
    }
    cc_runcmd "absent" ''
    cc_runcmd "empty" ', "runcmd": []'
    cc_runcmd "strings are shell source, lists are quoted argv" \
        ', "runcmd": ["echo one > /dev/null", ["echo", "two"], ["echo", "it'"'"'s", 5, true], null]'
    cc_runcmd "a scalar is not shellifiable" ', "runcmd": "echo hi"'
    cc_runcmd "a mapping item is not shellifiable" ', "runcmd": [{"a": 1}]'

    # cc_bootcmd: the one ported module that really executes. Every fixture
    # runs `sh` against the scratch directory and nothing else.
    cc_bootcmd() {
        compare_cc cc_bootcmd "$1" "{\"bootcmd\": $2}"
    }
    compare_cc cc_bootcmd "absent" '{}'
    cc_bootcmd "empty" '[]'
    cc_bootcmd "commands run in order" \
        '["echo one > @@/out", ["sh", "-c", "echo two >> @@/out"], null]'
    cc_bootcmd "the instance id is in the environment" \
        '["printf %s \"$INSTANCE_ID\" > @@/iid"]'
    cc_bootcmd "a non-zero exit fails the module" \
        '["echo before > @@/out", "exit 3", "echo after >> @@/out"]'
    cc_bootcmd "an unshellifiable entry never reaches the shell" '[{"a": 1}]'
    cc_bootcmd "a scalar is not shellifiable" '"echo hi"'

    # cc_seed_random: appends to a file the config names, then optionally runs
    # a command with RANDOM_SEED_FILE set. `_datasource` is the harness'
    # channel for the datasource half of the seed.
    cc_seed_random() {
        compare_cc cc_seed_random "$1" "{\"random_seed\": {\"file\": \"@@/seed\"${2-}}${3-}}"
    }
    compare_cc cc_seed_random "absent" '{}'
    cc_seed_random "config data only" ', "data": "from-config"'
    cc_seed_random "metadata only" '' \
        ', "_datasource": {"metadata": {"random_seed": "from-md"}}'
    cc_seed_random "config data then metadata" ', "data": "cfg"' \
        ', "_datasource": {"metadata": {"random_seed": "md"}}'
    cc_seed_random "empty data writes nothing" ', "data": ""'
    cc_seed_random "base64" ', "data": "cGxhaW4gcGF5bG9hZAo=", "encoding": "B64"'
    cc_seed_random "gzip" \
        ', "data": "H4sIAAAAAAAC/0vOzy0oSi0uTk1RKEiszMlPTOECAFg6AQ4TAAAA", "encoding": "gz"'
    # The encodings this module does NOT share with write_files.
    cc_seed_random "gz+b64 is not an encoding here" \
        ', "data": "x", "encoding": "gz+b64"'
    cc_seed_random "raw is" ', "data": "as-is\n", "encoding": "RAW"'
    cc_seed_random "a command that does not exist" \
        ', "data": "x", "command": ["definitely-not-a-program"]'
    cc_seed_random "a command that does not exist but is required" \
        ', "data": "x", "command": ["definitely-not-a-program"], "command_required": true'
    cc_seed_random "no command but required" ', "command_required": true'
    cc_seed_random "the command sees RANDOM_SEED_FILE" \
        ', "data": "x", "command": ["/bin/sh", "-c", "printf %s \"$RANDOM_SEED_FILE\" > @@/env"]'
    cc_seed_random "a failing command" \
        ', "data": "x", "command": ["/bin/sh", "-c", "exit 3"]'

    # cc_scripts_*: the input is a directory, not a config key, so the fixtures
    # are built by the prep snippet. Scripts write to files rather than stdout
    # so that `capture=False` cannot interleave with the dump.
    cc_scripts() {
        compare_cc "$1" "$2" \
            '{"system_info": {"paths": {"cloud_dir": "@@/cloud", "run_dir": "@@/run"}}}' \
            "${3-}"
    }
    cc_scripts cc_scripts_per_boot "a missing directory is not an error" ''
    cc_scripts cc_scripts_per_boot "an empty directory" 'mkdir -p @@/cloud/scripts/per-boot'
    cc_scripts cc_scripts_per_boot "sorted, not creation order" '
        d=@@/cloud/scripts/per-boot; mkdir -p $d
        printf "#!/bin/sh\necho second >> @@/out\n" >$d/20-second
        printf "#!/bin/sh\necho first >> @@/out\n" >$d/10-first
        chmod 0755 $d/10-first $d/20-second'
    cc_scripts cc_scripts_per_boot "a file without the execute bit is skipped" '
        d=@@/cloud/scripts/per-boot; mkdir -p $d
        printf "#!/bin/sh\necho ran >> @@/out\n" >$d/inert
        chmod 0644 $d/inert'
    cc_scripts cc_scripts_per_boot "a subdirectory is a special file" '
        d=@@/cloud/scripts/per-boot; mkdir -p $d/subdir
        printf "#!/bin/sh\necho ran >> @@/out\n" >$d/subdir/deep
        chmod 0755 $d/subdir/deep'
    cc_scripts cc_scripts_per_boot "one failure does not stop the rest" '
        d=@@/cloud/scripts/per-boot; mkdir -p $d
        printf "#!/bin/sh\nexit 3\n" >$d/1-bad
        printf "#!/bin/sh\necho ran >> @@/out\n" >$d/2-good
        printf "#!/bin/sh\nexit 4\n" >$d/3-bad
        chmod 0755 $d/1-bad $d/2-good $d/3-bad'
    cc_scripts cc_scripts_per_boot "a script that is not executable code" '
        d=@@/cloud/scripts/per-boot; mkdir -p $d
        printf "not a program\n" >$d/junk
        chmod 0755 $d/junk'
    cc_scripts cc_scripts_per_once "reads cloud_dir, not the instance" '
        d=@@/cloud/scripts/per-once; mkdir -p $d
        printf "#!/bin/sh\necho once >> @@/out\n" >$d/go
        chmod 0755 $d/go'
    cc_scripts cc_scripts_per_instance "reads cloud_dir too" '
        d=@@/cloud/scripts/per-instance; mkdir -p $d
        printf "#!/bin/sh\necho inst >> @@/out\n" >$d/go
        chmod 0755 $d/go'
    # `get_ipath_cur` goes through the `instance` symlink, which a real boot
    # would have made in stage 6.
    cc_scripts cc_scripts_user "runs through the instance link" '
        d=@@/cloud/instances/i-test/scripts; mkdir -p $d
        ln -s instances/i-test @@/cloud/instance
        printf "#!/bin/sh\necho user >> @@/out\n" >$d/go
        chmod 0755 $d/go'
    cc_scripts cc_scripts_user "a dangling instance link is a missing dir" '
        mkdir -p @@/cloud
        ln -s instances/i-test @@/cloud/instance'
    cc_scripts cc_scripts_vendor "runs the vendor subdirectory" '
        d=@@/cloud/instances/i-test/scripts/vendor; mkdir -p $d
        ln -s instances/i-test @@/cloud/instance
        printf "#!/bin/sh\necho vendor >> @@/out\n" >$d/go
        chmod 0755 $d/go'
    compare_cc cc_scripts_vendor "the prefix wraps each script" \
        '{"system_info": {"paths": {"cloud_dir": "@@/cloud", "run_dir": "@@/run"}},
          "vendor_data": {"prefix": ["/bin/sh"]}}' '
        d=@@/cloud/instances/i-test/scripts/vendor; mkdir -p $d
        ln -s instances/i-test @@/cloud/instance
        printf "echo wrapped >> @@/out\n" >$d/go
        chmod 0755 $d/go'
    compare_cc cc_scripts_vendor "a string prefix is one argument" \
        '{"system_info": {"paths": {"cloud_dir": "@@/cloud", "run_dir": "@@/run"}},
          "vendor_data": {"prefix": "/bin/sh"}}' '
        d=@@/cloud/instances/i-test/scripts/vendor; mkdir -p $d
        ln -s instances/i-test @@/cloud/instance
        printf "echo wrapped >> @@/out\n" >$d/go
        chmod 0755 $d/go'
    compare_cc cc_scripts_vendor "a scalar vendor_data is not iterable" \
        '{"system_info": {"paths": {"cloud_dir": "@@/cloud", "run_dir": "@@/run"}},
          "vendor_data": 5}' '
        d=@@/cloud/instances/i-test/scripts/vendor; mkdir -p $d
        ln -s instances/i-test @@/cloud/instance
        printf "#!/bin/sh\necho ran >> @@/out\n" >$d/go
        chmod 0755 $d/go'

    # cc_final_message: what it renders goes to stderr, which `run_pair` does
    # not compare, so the observable half is the `boot-finished` file. Its
    # content is an uptime, a timestamp and a version — none of which two runs
    # can agree on — so it is masked and only its presence and mode compare.
    # The rendering itself is covered by unit tests.
    cc_final_message() {
        compare_cc cc_final_message "$1" \
            "{\"system_info\": {\"paths\": {\"cloud_dir\": \"@@/cloud\"}}${2-}}" \
            'mkdir -p @@/cloud/instance' \
            'cloud/instance/boot-finished'
    }
    cc_final_message "the default message"
    cc_final_message "a configured message" \
        ', "final_message": "up ${uptime} on ${datasource}"'
    cc_final_message "a non-string message is stringified" ', "final_message": 5'
    cc_final_message "an empty message falls back to the default" \
        ', "final_message": "   "'
    # A template that cannot even be parsed. Rendering and writing are
    # independent, so the file still lands.
    cc_final_message "a broken jinja template is not fatal" \
        ', "final_message": "## template: jinja\n{% for x in %}"'
    cc_final_message "a jinja template that fails at run time" \
        ', "final_message": "## template: jinja\n{{ nope.missing }}"'
    # No instance directory: `write_file` does not create it, so the write
    # fails, is logged, and the module still succeeds.
    compare_cc cc_final_message "no instance directory" \
        '{"system_info": {"paths": {"cloud_dir": "@@/cloud"}}}'
    # No datasource at all: upstream reads `cloud.datasource.dsname` after the
    # write, so the file lands and *then* the module dies.
    compare_cc cc_final_message "no datasource" \
        '{"system_info": {"paths": {"cloud_dir": "@@/cloud"}}, "_datasource": null}' \
        'mkdir -p @@/cloud/instance' \
        'cloud/instance/boot-finished'
    # The fallback warning is a log line, so all this can check is that
    # reaching for `sys_cfg` does not upset either side.
    compare_cc cc_final_message "a configured datasource_list" \
        '{"system_info": {"paths": {"cloud_dir": "@@/cloud"}},
          "_datasource": {"sys_cfg": {"datasource_list": ["None"]}}}' \
        'mkdir -p @@/cloud/instance' \
        'cloud/instance/boot-finished'

    # --- cc_update_hostname and cc_update_etc_hosts --------------------------
    # These two are the first cases to use `_root`: they name `/etc/hostname`
    # and `/etc/hosts`, so the port is given the scratch directory as its root
    # and the Python side has the matching distro attributes rewritten. The
    # distro is a real `ubuntu` on both sides, not the usual stand-in, and its
    # `_apply_hostname` is stubbed out — the alternative is renaming whatever
    # machine runs the suite.
    UH_BASE='"_root": true,
        "system_info": {"paths": {"cloud_dir": "@@/var/lib/cloud",
                                  "run_dir": "@@/run",
                                  "templates_dir": "@@/etc/cloud/templates"}}'
    # The templates are read, not written, so the fixture copies the packaged
    # ones in. Both sides then see the same bytes.
    UH_PREP='mkdir -p @@/etc @@/var/lib/cloud/data @@/etc/cloud/templates
        cp /etc/cloud/templates/hosts.*.tmpl @@/etc/cloud/templates/'

    compare_etc_hosts() {
        compare_cc cc_update_etc_hosts "$1" \
            "{$UH_BASE, \"manage_etc_hosts\": $2,
              \"hostname\": \"h1\", \"fqdn\": \"h1.example.com\"}" \
            "$UH_PREP
             $3" \
            "${4-}"
    }

    # Every shape `translate_bool(.., addons=["template"])` has to sort out.
    # `"false"` and `0` are the ones worth having: a string that looks false
    # is falsy, but an empty file is not the same as no file.
    EXISTING='printf "127.0.0.1\tlocalhost\n192.0.2.5\tother\n" > @@/etc/hosts'
    for value in true false '"true"' '"false"' '"template"' '"localhost"' \
                 0 1 '""' null '"nonsense"' '[]' '{}'; do
        compare_etc_hosts "manage_etc_hosts: $value, file present" "$value" "$EXISTING"
        # With no file to read, the template arm still renders and the
        # localhost arm writes a `util.make_header` line — which carries the
        # packaged version, so its content is masked (deviation 107).
        compare_etc_hosts "manage_etc_hosts: $value, no file" "$value" "" 'etc/hosts'
    done

    # The loopback line in the shapes `update_etc_hosts` has to recognise.
    # Only the first of these already names the pair, so only it is left alone.
    for existing in '127.0.1.1\th1.example.com\th1\n' \
                    '127.0.1.1\th1.example.com\n' \
                    '127.0.1.1\th1.example.com\tother\n' \
                    '127.0.1.1\tother.example.com\th1\n' \
                    '127.0.1.1\th1.example.com\th1\t# tail\n' \
                    '127.0.1.1\n' \
                    '  127.0.1.1   h1.example.com   h1   \n' \
                    '# a comment\n\n127.0.1.1 h1.example.com h1\n' \
                    '127.0.0.1\tlocalhost\n::1\tlocalhost\n'; do
        compare_etc_hosts "localhost, existing $existing" '"localhost"' \
            "printf '$existing' > @@/etc/hosts"
    done

    # No template for the family is a `RuntimeError` upstream.
    compare_etc_hosts "template missing" true \
        'rm -f @@/etc/cloud/templates/hosts.debian.tmpl'

    compare_update_hostname() {
        compare_cc cc_update_hostname "$1" "{$UH_BASE, $2}" \
            "$UH_PREP
             $3"
    }

    # The four states `update_hostname` distinguishes, across the config keys
    # that change which name it settles on. "drifted" is the one that matters:
    # the running name no longer matches the record, so nothing is written.
    for cfg in '"hostname": "h1", "fqdn": "h1.example.com"' \
               '"hostname": "h1", "fqdn": "h1.example.com", "preserve_hostname": true' \
               '"hostname": "h1", "fqdn": "h1.example.com", "prefer_fqdn_over_hostname": true' \
               '"hostname": "h1", "fqdn": "h1.example.com", "create_hostname_file": false' \
               '"fqdn": "h1.example.com"' \
               '"hostname": "localhost"' \
               '"_datasource": {"metadata": {"local-hostname": "h1.example.com"}}' \
               '"_datasource": {"metadata": {"local-hostname": "192.0.2.7"}}'; do
        compare_update_hostname "fresh: $cfg" "$cfg" ''
        compare_update_hostname "agreeing: $cfg" "$cfg" \
            'printf "h1\n" > @@/etc/hostname
             printf "h1\n" > @@/var/lib/cloud/data/previous-hostname'
        compare_update_hostname "drifted: $cfg" "$cfg" \
            'printf "renamed\n" > @@/etc/hostname
             printf "h1\n" > @@/var/lib/cloud/data/previous-hostname'
        compare_update_hostname "stale record: $cfg" "$cfg" \
            'printf "old\n" > @@/etc/hostname
             printf "old\n" > @@/var/lib/cloud/data/previous-hostname'
        compare_update_hostname "comments in the file: $cfg" "$cfg" \
            'printf "# keep me\nold # why\n" > @@/etc/hostname
             printf "old\n" > @@/var/lib/cloud/data/previous-hostname'
    done
fi

# --- ca certificates ---------------------------------------------------------
sec "ca-certificates"
#
# `cc_ca_certs` decides who the machine will believe, and `remove_defaults`
# can leave it believing nobody, so both halves are worth pinning: which
# actions are taken and in what order, and the rewrite of the selection file
# that disables the shipped certificates.
#
# The plan is compared rather than carried out. The Python side stubs
# `util.write_file`, `util.delete_dir_contents`, `subp.subp` and
# `disable_system_ca_certs` onto one ordered list; running it for real would
# empty the trust store of the machine doing the comparison.
#
# The distro name is the whole configuration surface here -- it picks the
# paths, the filename template and the update command -- so the matrix walks
# one name from each table entry plus the aliases that behave unlike their
# family: `centos` and `rocky` share rhel's paths but neither removal arm, so
# `remove_defaults` silently does nothing on them.
CCCA_PY="$(cd "$(dirname "$0")" && pwd)/ccca.py"
CCCA_RS="$TARGET/examples/dump-cc-ca-certs"
if [ -x "$CCCA_RS" ] && python3 -c 'import cloudinit.config.cc_ca_certs' 2>/dev/null; then
    CCCA_RS="$(cd "$(dirname "$CCCA_RS")" && pwd)/dump-cc-ca-certs"
    ccca_plan() {  # $1 config  $2 distro
        run_pair "cc_ca_certs plan [$2] $1" \
            "cd /tmp && python3 '$CCCA_PY' plan '$1' '$2'" \
            "cd /tmp && '$CCCA_RS' plan '$1' '$2'"
    }

    for ccca_distro in ubuntu debian raspberry-pi-os alpine aosc rhel fedora \
                       centos rocky almalinux cloudlinux photon opensuse \
                       opensuse-leap sles sle-micro arch unknown; do
        # The empty mapping still refreshes the trust store; the missing key
        # skips the module outright; a non-mapping is the TypeError whose
        # message upstream forgot to format (docs/COMPAT.md bug B80).
        for ccca_cfg in \
            '{}' \
            '{"ca_certs": {}}' \
            '{"ca_certs": []}' \
            '{"ca_certs": "text"}' \
            '{"ca_certs": null}' \
            '{"ca_certs": {"remove_defaults": true}}' \
            '{"ca_certs": {"remove_defaults": false}}' \
            '{"ca_certs": {"remove_defaults": ""}}' \
            '{"ca_certs": {"trusted": []}}' \
            '{"ca_certs": {"trusted": null}}' \
            '{"ca_certs": {"trusted": "PEM"}}' \
            '{"ca_certs": {"trusted": ["one", "two", "three"]}}' \
            '{"ca_certs": {"trusted": [1, true, null, {"k": 1}, ["z"]]}}' \
            '{"ca_certs": {"remove_defaults": true, "trusted": ["one"]}}'; do
            ccca_plan "$ccca_cfg" "$ccca_distro"
        done

        # Both spellings of both keys, including the pair that collide: the
        # underscore one wins and the dashed one is only warned about.
        for ccca_cfg in \
            '{"ca-certs": {"trusted": ["one"]}}' \
            '{"ca-certs": {"remove-defaults": true}}' \
            '{"ca_certs": {"remove-defaults": true, "remove_defaults": false}}' \
            '{"ca-certs": {"trusted": ["dashed"]}, "ca_certs": {"trusted": ["under"]}}'; do
            ccca_plan "$ccca_cfg" "$ccca_distro"
        done
    done

    # The rewrite itself runs for real, against a scratch file each side gets
    # its own copy of. `printf %b` builds the body, so the cases can carry the
    # line endings and the vertical tab that `splitlines` treats as a break
    # but `split("\n")` does not.
    CCCA_WORK="$WORK/ccca"
    mkdir -p "$CCCA_WORK"
    ccca_n=0
    ccca_deselect() {  # $1 label  $2 printf-body, or MISSING
        ccca_n=$((ccca_n + 1))
        ccca_py="$CCCA_WORK/py$ccca_n.conf"
        ccca_rs="$CCCA_WORK/rs$ccca_n.conf"
        if [ "$2" = "MISSING" ]; then
            rm -f "$ccca_py" "$ccca_rs"
        else
            printf '%b' "$2" >"$ccca_py"
            printf '%b' "$2" >"$ccca_rs"
        fi
        run_pair "cc_ca_certs deselect $1" \
            "cd /tmp && python3 '$CCCA_PY' deselect '$ccca_py'" \
            "cd /tmp && '$CCCA_RS' deselect '$ccca_rs'"
    }

    ccca_deselect 'absent file' 'MISSING'
    ccca_deselect 'empty file' ''
    ccca_deselect 'one entry' 'mozilla/one.crt\n'
    ccca_deselect 'comments and blanks' '# c\n\n!off\na.crt\nb.crt\n'
    ccca_deselect 'no trailing newline' 'a.crt'
    ccca_deselect 'blank lines only' '\n\n\n'
    ccca_deselect 'header already present' \
        '# Modified by cloud-init to deselect certs due to user-data\n!a.crt\nb.crt\n'
    ccca_deselect 'leading and trailing spaces' '  spaced.crt \nx\n'
    ccca_deselect 'non-ascii name' '\xc3\xa9.crt\n'
    ccca_deselect 'crlf line endings' 'a.crt\r\nb.crt\r\n'
    ccca_deselect 'vertical tab' 'a.crt\n\x0bvt.crt\n'
    ccca_deselect 'form feed' 'a.crt\n\x0cff.crt\n'
    ccca_deselect 'bang only' '!\n'
    ccca_deselect 'hash only' '#\n'
fi

# --- apt pipelining ----------------------------------------------------------
sec "apt-pipelining"
#
# One config key, one file, and a validator that accepts a surprising set of
# spellings: `false` writes a zero, three words write nothing at all, the
# digits zero to five write themselves, and everything else is a warning. The
# stringification happens before the lowering, so a YAML `true` arrives as
# `True` and a bare `apt_pipelining:` arrives as `None` -- which is one of the
# three words, so a null quietly means "leave it alone".
#
# The module's only escape is a write into the real `/etc/apt/apt.conf.d`, so
# the Python side stubs `util.write_file` and the module logger onto one
# ordered list rather than letting it land.
APTPIPE_PY="$(cd "$(dirname "$0")" && pwd)/aptpipe.py"
APTPIPE_RS="$TARGET/examples/dump-cc-apt-pipelining"
if [ -x "$APTPIPE_RS" ] &&
   python3 -c 'import cloudinit.config.cc_apt_pipelining' 2>/dev/null; then
    APTPIPE_RS="$(cd "$(dirname "$APTPIPE_RS")" && pwd)/dump-cc-apt-pipelining"
    for aptpipe_cfg in \
        '{}' \
        '{"apt_pipelining": null}' \
        '{"apt_pipelining": false}' \
        '{"apt_pipelining": true}' \
        '{"apt_pipelining": "false"}' \
        '{"apt_pipelining": "False"}' \
        '{"apt_pipelining": "FALSE"}' \
        '{"apt_pipelining": "none"}' \
        '{"apt_pipelining": "None"}' \
        '{"apt_pipelining": "unchanged"}' \
        '{"apt_pipelining": "UNCHANGED"}' \
        '{"apt_pipelining": "os"}' \
        '{"apt_pipelining": "OS"}' \
        '{"apt_pipelining": 0}' \
        '{"apt_pipelining": 1}' \
        '{"apt_pipelining": 5}' \
        '{"apt_pipelining": 6}' \
        '{"apt_pipelining": -1}' \
        '{"apt_pipelining": "0"}' \
        '{"apt_pipelining": "5"}' \
        '{"apt_pipelining": "05"}' \
        '{"apt_pipelining": "  3  "}' \
        '{"apt_pipelining": ""}' \
        '{"apt_pipelining": "  "}' \
        '{"apt_pipelining": 3.0}' \
        '{"apt_pipelining": "yes"}' \
        '{"apt_pipelining": [1]}' \
        '{"apt_pipelining": {"depth": 1}}'; do
        printf '%s\n' "$aptpipe_cfg" >>"$WORK/aptpipe.cases"
    done
    run_batch "cc_apt_pipelining" \
        "cd /tmp && python3 '$APTPIPE_PY' --batch" \
        "cd /tmp && '$APTPIPE_RS' --batch" \
        "$WORK/aptpipe.cases"
fi

# --- timezone ----------------------------------------------------------------
sec "timezone"
#
# `cc_timezone` is four lines around `distro.set_timezone`, and all the
# variation is in the distro: five different bodies, one of which branches
# again on whether the machine runs systemd, and a shared helper that relinks
# `/etc/localtime` three different ways depending on what is already there.
#
# The three facts the decision turns on -- does the zone file exist, is
# `/etc/localtime` a link, a file or missing, and is this systemd -- are
# supplied as arguments rather than read, so the matrix covers combinations
# this machine is not in. Nothing is carried out: the Python side stubs the
# four `util` escapes and shims `os` into `cloudinit.distros` alone, because
# doing it for real would relink the clock of the machine doing the
# comparison.
#
# `systemd` is pinned to 1: the other arm reaches `rhel_util`'s sysconfig
# writer, which is not ported (docs/COMPAT.md).
CCTZ_PY="$(cd "$(dirname "$0")" && pwd)/tz.py"
CCTZ_RS="$TARGET/examples/dump-cc-timezone"
if [ -x "$CCTZ_RS" ] && python3 -c 'import cloudinit.config.cc_timezone' 2>/dev/null; then
    CCTZ_RS="$(cd "$(dirname "$CCTZ_RS")" && pwd)/dump-cc-timezone"
    for cctz_distro in ubuntu debian raspberry-pi-os alpine arch gentoo photon \
                       cos mariner aosc rhel centos fedora rocky almalinux \
                       openeuler opensuse sles suse-microos; do
        for cctz_cfg in \
            '{}' \
            '{"timezone": "UTC"}' \
            '{"timezone": "Europe/Madrid"}' \
            '{"timezone": ""}' \
            '{"timezone": null}' \
            '{"timezone": false}' \
            '{"timezone": true}' \
            '{"timezone": 0}' \
            '{"timezone": 3.5}' \
            '{"timezone": []}' \
            '{"timezone": "UTC  "}' \
            '{"timezone": "/etc/shadow"}' \
            '{"timezone": "../../etc/passwd"}'; do
            for cctz_local in symlink regular absent; do
                for cctz_zone in 0 1; do
                    printf '%s\t%s\t%s\t%s\t1\n' \
                        "$cctz_cfg" "$cctz_distro" "$cctz_zone" "$cctz_local" \
                        >>"$WORK/cctz.cases"
                done
            done
        done
    done
    run_batch "cc_timezone" \
        "cd /tmp && python3 '$CCTZ_PY' --batch" \
        "cd /tmp && '$CCTZ_RS' --batch" \
        "$WORK/cctz.cases"
fi

# --- locale ------------------------------------------------------------------
sec "locale"
#
# `cc_locale` has one turn worth pinning: with no `locale` key the fallback is
# `cloud.get_locale()`, which reads `/etc/default/locale` and only then falls
# back to the distro's hardcoded default -- so an unconfigured run usually
# asks for the locale the machine already has. Whether that is a no-op then
# depends on `apply_locale`'s three-term `need_regen`, which the matrix walks
# by supplying the system locale, whether the conf file exists, and whether
# each of the two tools is installed.
#
# Only debian's `apply_locale` is ported, so only its distros are swept; the
# other ten bodies are named gaps (docs/COMPAT.md). Nothing runs: the Python
# side stubs `subp.which`, `subp.subp`, `read_system_locale` and
# `install_packages`, because a real run would regenerate this machine's
# locales.
CCLOC_PY="$(cd "$(dirname "$0")" && pwd)/loc.py"
CCLOC_RS="$TARGET/examples/dump-cc-locale"
if [ -x "$CCLOC_RS" ] && python3 -c 'import cloudinit.config.cc_locale' 2>/dev/null; then
    CCLOC_RS="$(cd "$(dirname "$CCLOC_RS")" && pwd)/dump-cc-locale"
    for ccloc_distro in ubuntu debian; do
        for ccloc_cfg in \
            '{}' \
            '{"locale": "en_GB.UTF-8"}' \
            '{"locale": "C.UTF-8"}' \
            '{"locale": "c"}' \
            '{"locale": "POSIX"}' \
            '{"locale": "EN_gb.utf-8"}' \
            '{"locale": ""}' \
            '{"locale": false}' \
            '{"locale": true}' \
            '{"locale": null}' \
            '{"locale": 0}' \
            '{"locale": []}' \
            '{"locale": "false"}' \
            '{"locale": "off"}' \
            '{"locale": "  Off  "}' \
            '{"locale": "en_US.UTF-8", "locale_configfile": "/etc/other"}' \
            '{"locale": "en_US.UTF-8", "locale_configfile": ""}' \
            '{"locale": "en_US.UTF-8", "locale_configfile": 7}'; do
            for ccloc_sys in - C.UTF-8 en_GB.UTF-8; do
                for ccloc_conf in 0 1; do
                    for ccloc_gen in 0 1; do
                        for ccloc_upd in 0 1; do
                            printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
                                "$ccloc_cfg" "$ccloc_distro" "$ccloc_sys" \
                                "$ccloc_conf" "$ccloc_gen" "$ccloc_upd" \
                                >>"$WORK/ccloc.cases"
                        done
                    done
                done
            done
        done
    done
    run_batch "cc_locale" \
        "cd /tmp && python3 '$CCLOC_PY' --batch" \
        "cd /tmp && '$CCLOC_RS' --batch" \
        "$WORK/ccloc.cases"
fi

# --- mount info --------------------------------------------------------------
sec "mount-info"
#
# `util.parse_mount_info` answers "which device is this path on", and three
# modules that go on to resize or reformat that device depend on the answer.
# It is prefix matching with a twist: the *deepest* mount point covering the
# path wins, which is what sees through a bind mount or a btrfs subvolume to
# the device really underneath. A malformed line abandons the whole file
# rather than being skipped, on the grounds that a confidently wrong device
# here is worse than no answer.
#
# The file contents are passed in, so the matrix can cover shapes this machine
# is not in -- overmounts, extra optional columns, a missing '-' separator --
# plus one case built from the real `/proc/self/mountinfo`, which both sides
# read from the same place.
MI_PY="$(cd "$(dirname "$0")" && pwd)/mountinfo.py"
MI_RS="$TARGET/examples/dump-mountinfo"
if [ -x "$MI_RS" ] && python3 -c 'import cloudinit.util' 2>/dev/null; then
    MI_RS="$(cd "$(dirname "$MI_RS")" && pwd)/dump-mountinfo"
    US="$(printf '\037')"
    mi_root='36 35 98:0 / / rw,relatime shared:1 - ext4 /dev/sda1 rw'
    mi_var='40 36 98:1 / /var rw,relatime shared:2 - ext4 /dev/sdb1 rw'
    mi_cloud="41 40 0:33 /@cloud /var/lib/cloud rw,relatime shared:3 - btrfs /dev/sdc1 rw,subvol=/@cloud"
    mi_seed='42 41 98:3 / /var/lib/cloud/seed/nocloud ro,relatime - iso9660 /dev/sr0 ro'
    mi_home='43 36 98:4 / /home rw - ext4 /dev/sde1 rw'
    mi_a='50 36 98:5 / /mnt rw - ext4 /dev/sdf1 rw'
    mi_b='51 36 98:6 / /mnt ro - vfat /dev/sdg1 ro'
    # Extra optional columns before the '-', which is what makes the
    # separator search necessary in the first place.
    mi_opt='52 36 98:7 / /var rw shared:9 master:4 propagate_from:4 - ext4 /dev/sdh1 rw'
    mi_short='36 35 98:0 / /var rw'
    mi_nodash='36 35 98:0 / / rw,relatime shared:1 ext4 /dev/sda1 rw'
    mi_trail='36 35 98:0 / / rw shared:1 master:2 - ext4'
    mi_full="$mi_root$US$mi_var$US$mi_cloud$US$mi_seed$US$mi_home"
    mi_real="$(tr '\n' "$US" </proc/self/mountinfo | sed "s/$US\$//")"

    : >"$WORK/mi.cases"
    for mi_path in / /var /var/lib /var/lib/cloud /var/lib/cloud/seed/nocloud \
                   /var/lib/cloud/seed/nocloud/user-data /home/user /etc \
                   '///var//lib///'; do
        printf 'mountinfo\t%s\t%s\n' "$mi_path" "$mi_full" >>"$WORK/mi.cases"
    done
    # An empty path: every mount point is a prefix of it, so the shallowest
    # wins by default.
    printf 'mountinfo\t\t%s\n' "$mi_full" >>"$WORK/mi.cases"
    printf 'mountinfo\t/mnt\t%s\n' "$mi_root$US$mi_a$US$mi_b" >>"$WORK/mi.cases"
    printf 'mountinfo\t/mnt\t%s\n' "$mi_root$US$mi_b$US$mi_a" >>"$WORK/mi.cases"
    printf 'mountinfo\t/var\t%s\n' "$mi_root$US$mi_opt" >>"$WORK/mi.cases"
    printf 'mountinfo\t/var\t%s\n' "$mi_root$US$mi_short" >>"$WORK/mi.cases"
    printf 'mountinfo\t/\t%s\n' "$mi_nodash" >>"$WORK/mi.cases"
    printf 'mountinfo\t/\t%s\n' "$mi_trail" >>"$WORK/mi.cases"
    printf 'mountinfo\t/\t\n' >>"$WORK/mi.cases"
    for mi_path in / /var /var/lib/cloud /tmp /proc /nonexistent; do
        printf 'mountinfo\t%s\t%s\n' "$mi_path" "$mi_real" >>"$WORK/mi.cases"
    done
    # `parse_mtab` matches the mount point exactly, so a path *inside* a
    # mount finds nothing -- the whole reason `mountinfo` is preferred.
    printf 'mtab\t/var\t%s\n' '/dev/sdb1 /var ext4 rw,relatime 0 0' >>"$WORK/mi.cases"
    printf 'mtab\t/var/lib/cloud\t%s\n' '/dev/sdb1 /var ext4 rw 0 0' >>"$WORK/mi.cases"
    printf 'mtab\t/\t%s\n' "/dev/sda1 / ext4 rw 0 0$US/dev/sdb1 / xfs rw 0 0" \
        >>"$WORK/mi.cases"
    printf 'mtab\t/nope\t%s\n' '/dev/sda1 / ext4 rw 0 0' >>"$WORK/mi.cases"

    run_batch "parse_mount_info" \
        "cd /tmp && python3 '$MI_PY' --batch" \
        "cd /tmp && '$MI_RS' --batch" \
        "$WORK/mi.cases"
fi

# --- human2bytes -------------------------------------------------------------
sec "human2bytes"
#
# `swap: {size: ...}` in cloud-config reaches `util.human2bytes` as a raw
# string, so this is a tenant-facing parser and its edges are worth pinning.
# The number is handed to Python's `float()`, which accepts rather more than
# it looks like: surrounding whitespace, underscores between digits,
# exponents, and `inf`/`nan`. The last of those is upstream bug B82 -- the
# "is not valid input." guard is bypassed and a different exception escapes.
#
# Cases are base64-encoded so leading and trailing whitespace, tabs and the
# empty string survive the cases file.
H2B_PY="$(cd "$(dirname "$0")" && pwd)/h2b.py"
H2B_RS="$TARGET/examples/dump-human2bytes"
if [ -x "$H2B_RS" ] && python3 -c 'import cloudinit.util' 2>/dev/null; then
    H2B_RS="$(cd "$(dirname "$H2B_RS")" && pwd)/dump-human2bytes"
    python3 - "$WORK/h2b.cases" <<'PYEOF'
import base64
import sys

cases = [
    # Plain sizes, both prefix spellings, both meaning 1024.
    "10", "0", "10B", "10K", "10M", "10G", "10T",
    "10KB", "10MB", "10GB", "10TB",
    "10KiB", "10MiB", "10GiB", "10TiB",
    # The number is a float, not an int.
    "10.5M", "0.5G", ".5M", "5.M", "1e3", "1E3", "1e3M", "2e-1M",
    # Signs and zeroes.
    "+10M", "-0", "-0M", "-1", "-1M", "-0.5G",
    # Whitespace: stripped inside float(), which runs AFTER the suffix match,
    # so a padded suffix still fails while a padded bare number does not.
    " 10", "10 ", "  10  ", "\t10\t", " 10M", "10M ", "  10M  ", " ", "",
    # Underscores are legal only between digits.
    "1_000", "1_0M", "_1", "1_", "1__0", "1._5", "1_.5", "10_",
    # Suffix edge cases: what is left after the strip, and stacked suffixes.
    "B", "iB", "M", "MiB", "MB", "10iB", "10Mi", "10BB", "10MBB",
    # Case matters -- the table is upper-case only.
    "10m", "10k", "10g", "10t", "10mb", "10mib",
    # Not numbers at all.
    "abc", "0x10", "10X", "--1", "1.2.3", "1,000", "one",
    # B82: float() takes these, int() will not.
    "inf", "Inf", "INF", "infinity", "Infinity", "-inf", "-infinity",
    "nan", "NaN", "NAN", "-nan", "infM", "nanM", "1e400", "-1e400",
]

with open(sys.argv[1], "w") as fp:
    for case in cases:
        fp.write(base64.b64encode(case.encode()).decode() + "\n")
PYEOF
    run_batch "human2bytes" \
        "cd /tmp && python3 '$H2B_PY' --batch" \
        "cd /tmp && '$H2B_RS' --batch" \
        "$WORK/h2b.cases"
fi

# --- snap --------------------------------------------------------------------
sec "cc-snap"
#
# Two halves behind one key. `assertions` is a list or a mapping whose values
# are concatenated and acked in one go -- and a mapping contributes its values
# in insertion order, where `commands` just below sorts by key, so the two
# halves of the same module disagree about what a mapping means.
#
# `commands` is where tenant config becomes a root shell: a string entry is
# handed to `/bin/sh`, a list entry is executed directly, and the two are
# chosen per item. `prepend_base_command` then rewrites each entry, with an
# empty list raising IndexError partway through -- so a later bad entry can
# abort the module after the assertions have already been written and acked.
#
# Failing commands are out of scope: upstream reports them as
# `str(ProcessExecutionError)`, a template this port does not reproduce.
CCSNAP_PY="$(cd "$(dirname "$0")" && pwd)/snap.py"
CCSNAP_RS="$TARGET/examples/dump-cc-snap"
if [ -x "$CCSNAP_RS" ] &&
   python3 -c 'import cloudinit.config.cc_snap' 2>/dev/null; then
    CCSNAP_RS="$(cd "$(dirname "$CCSNAP_RS")" && pwd)/dump-cc-snap"
    : >"$WORK/ccsnap.cases"
    for ccsnap_cfg in \
        '{}' \
        '{"snap": {}}' \
        '{"snap": null}' \
        '{"snap": 0}' \
        '{"snap": ""}' \
        '{"snap": "text"}' \
        '{"snap": [1, 2]}' \
        '{"snap": 5}' \
        '{"snap": {"assertions": []}}' \
        '{"snap": {"assertions": null}}' \
        '{"snap": {"assertions": ["solo"]}}' \
        '{"snap": {"assertions": ["one\ntwo\nthree", "solo"]}}' \
        '{"snap": {"assertions": ["", "x"]}}' \
        '{"snap": {"assertions": {"b": "bee", "a": "ay"}}}' \
        '{"snap": {"assertions": {"a": "ay", "b": "bee"}}}' \
        '{"snap": {"assertions": [1]}}' \
        '{"snap": {"assertions": ["ok", null]}}' \
        '{"snap": {"assertions": "oops"}}' \
        '{"snap": {"assertions": 7}}' \
        '{"snap": {"commands": []}}' \
        '{"snap": {"commands": null}}' \
        '{"snap": {"commands": ["snap install hello"]}}' \
        '{"snap": {"commands": ["echo hi"]}}' \
        '{"snap": {"commands": ["snapinstall hello"]}}' \
        '{"snap": {"commands": ["snap"]}}' \
        '{"snap": {"commands": ["echo a", "echo b", "snap install x"]}}' \
        '{"snap": {"commands": [["snap", "install", "hello"]]}}' \
        '{"snap": {"commands": [["install", "hello"]]}}' \
        '{"snap": {"commands": [[null, "echo", "hi"]]}}' \
        '{"snap": {"commands": [[null]]}}' \
        '{"snap": {"commands": [[]]}}' \
        '{"snap": {"commands": [[1, 2]]}}' \
        '{"snap": {"commands": [["snap", 1, null]]}}' \
        '{"snap": {"commands": {"z": "snap install z", "a": "snap install a"}}}' \
        '{"snap": {"commands": {"10": "snap install ten", "9": "snap install nine"}}}' \
        '{"snap": {"commands": {"a": ["snap", "install", "a"]}}}' \
        '{"snap": {"commands": 1}}' \
        '{"snap": {"commands": "oops"}}' \
        '{"snap": {"commands": [1, {"a": 1}]}}' \
        '{"snap": {"commands": [true]}}' \
        '{"snap": {"assertions": ["a"], "commands": ["bad"]}}' \
        '{"snap": {"assertions": ["a"], "commands": [[]]}}' \
        '{"snap": {"assertions": [1], "commands": ["snap install x"]}}' \
        '{"snap": {"assertions": ["a"], "commands": ["snap install x"], "extra": 1}}'; do
        for ccsnap_present in 0 1; do
            printf '%s\t%s\n' "$ccsnap_cfg" "$ccsnap_present" \
                >>"$WORK/ccsnap.cases"
        done
    done
    run_batch "cc_snap" \
        "cd /tmp && python3 '$CCSNAP_PY' --batch" \
        "cd /tmp && '$CCSNAP_RS' --batch" \
        "$WORK/ccsnap.cases"
fi

# --- mounts -------------------------------------------------------------------
sec "cc-mounts"
#
# The module rewrites /etc/fstab from tenant config, so the interesting part is
# what it refuses to write. `sanitize_devname` will not name a device it cannot
# see, and "seeing" one means finding it under /sys/block -- which holds whole
# disks, so a partition only resolves when /sys/block/<disk>/<part> exists.
#
# Both sides are pointed at the same fixture tree: the Rust side roots its
# paths, the Python side patches os.path.exists/os.path.realpath and
# FSTAB_PATH, so cases can name devices this machine does not have.
#
# The config shapes below are mostly the ones the schema forbids but the code
# still runs: an empty entry, a null past the end of mount_default_fields, and
# an entry too short for `line[1]` all end the module with a bare IndexError.
# `mounts` as a string iterates characters; as a mapping, keys.
#
# Only the four passes over `mounts` are compared here; the swap and
# fstab-write halves are the cc-mounts-handle section below.
CCMOUNTS_PY="$(cd "$(dirname "$0")" && pwd)/ccmounts.py"
CCMOUNTS_RS="$TARGET/examples/dump-cc-mounts"
if [ -x "$CCMOUNTS_RS" ] &&
   python3 -c 'import cloudinit.config.cc_mounts' 2>/dev/null; then
    CCMOUNTS_RS="$(cd "$(dirname "$CCMOUNTS_RS")" && pwd)/dump-cc-mounts"
    CCMOUNTS_ROOT="$WORK/mountroot"
    rm -rf "$CCMOUNTS_ROOT"
    # sda has a first partition, xvda has one too, sdb/sdc/sr0 are bare disks,
    # and sdd exists in /dev but not in /sys/block so it is not a block device.
    mkdir -p "$CCMOUNTS_ROOT/dev" \
             "$CCMOUNTS_ROOT/etc" \
             "$CCMOUNTS_ROOT/sys/block/sda/sda1" \
             "$CCMOUNTS_ROOT/sys/block/xvda/xvda1" \
             "$CCMOUNTS_ROOT/sys/block/sdb" \
             "$CCMOUNTS_ROOT/sys/block/sdc" \
             "$CCMOUNTS_ROOT/sys/block/sr0"
    for ccmounts_dev in sda sda1 sdb sdc sdd xvda xvda1 sr0; do
        : >"$CCMOUNTS_ROOT/dev/$ccmounts_dev"
    done
    printf '%s\n' \
        '/dev/root / ext4 defaults 0 1' \
        '' \
        '/dev/sdc /already auto defaults 0 2' \
        '/dev/sdb /old auto defaults,comment=cloudconfig 0 2' \
        >"$CCMOUNTS_ROOT/etc/fstab"

    : >"$WORK/ccmounts.cases"
    for ccmounts_cfg in \
        '{}' \
        '{"mounts": []}' \
        '{"mounts": null}' \
        '{"mounts": 5}' \
        '{"mounts": "ab"}' \
        '{"mounts": {"b": 1, "a": 2}}' \
        '{"mounts": [[]]}' \
        '{"mounts": [["sdb"]]}' \
        '{"mounts": [["sdb", "/mnt"]]}' \
        '{"mounts": [["sdb", "/mnt", "ext4", "noatime", "0", "0"]]}' \
        '{"mounts": [["sda"]]}' \
        '{"mounts": [["sda", "/mnt"]]}' \
        '{"mounts": [["sda1", "/mnt"]]}' \
        '{"mounts": [["sdc", "/c"]]}' \
        '{"mounts": [["sdd", "/d"]]}' \
        '{"mounts": [["sr0", "/cd"]]}' \
        '{"mounts": [["xvda", "/x"]]}' \
        '{"mounts": [["sdz", "/gone"]]}' \
        '{"mounts": [["/dev/sdc", "/c"]]}' \
        '{"mounts": [[null, "/mnt"]]}' \
        '{"mounts": [[1, "/mnt"]]}' \
        '{"mounts": [["sdb", null]]}' \
        '{"mounts": [["a", "b", "c", "d", "e", "f", "g"]]}' \
        '{"mounts": [["a", "b", "c", "d", "e", "f", null]]}' \
        '{"mounts": [["sdb"]], "mount_default_fields": []}' \
        '{"mounts": [["sdb", "/mnt"]], "mount_default_fields": [null, null, "auto", "defaults", "0", "2"]}' \
        '{"mounts": [["sdb", "/one"], ["sdb", "/two"]]}' \
        '{"mounts": [["sdb", "/one"], ["sdb", null]]}' \
        '{"mounts": [["server:/exp", "/nfs", "nfs"]]}' \
        '{"mounts": [["ephemeral", "/mnt"]]}' \
        '{"mounts": [["ephemeral0", "/mnt"]]}' \
        '{"mounts": [["ebs1", "/e"]]}' \
        '{"mounts": [["swap", "none", "swap", "sw", "0", "0"]]}' \
        '{"mounts": [["sda.1", "/p"]]}' \
        '{"mounts": [["sda.1", "/p"]], "device_aliases": {"sda": "/dev/sda"}}' \
        '{"mounts": [["sda.9", "/p"]], "device_aliases": {"sda": "/dev/sda"}}' \
        '{"mounts": [["disk", "/p"]], "device_aliases": {"disk": "/dev/sdb"}}' \
        '{"mounts": [["/dev/root", "/x"]]}' \
        '{"mounts": [["sdc", "/x"]]}' \
        '{"mounts": ["sdb"]}' \
        '{"mounts": [null]}'; do
        for ccmounts_map in \
            '{}' \
            '{"ephemeral0": "sdb", "swap": "sdc", "ebs1": "xvda"}'; do
            for ccmounts_systemd in 0 1; do
                printf '%s\t%s\t%s\t%s\n' \
                    "$CCMOUNTS_ROOT" "$ccmounts_cfg" \
                    "$ccmounts_map" "$ccmounts_systemd" \
                    >>"$WORK/ccmounts.cases"
            done
        done
    done
    run_batch "cc_mounts" \
        "cd /tmp && python3 '$CCMOUNTS_PY' --batch" \
        "cd /tmp && '$CCMOUNTS_RS' --batch" \
        "$WORK/ccmounts.cases"
fi

# --- mounts, whole handle -----------------------------------------------------
sec "cc-mounts-handle"
#
# The rest of the module: the swap plan and the exact bytes written to fstab.
# A fifth case field carries the host facts the swap code reads straight off
# the machine -- the swap directory's fstype, the kernel version, MemTotal and
# the free space from statvfs -- so both sides answer from the same numbers
# instead of from this host.
#
# Commands are recorded, not run. The Python side stubs subp/util so nothing is
# created; the Rust side renders the plan, whose run half refuses to shell out
# unless the root is "/" anyway. That keeps the fallocate-fails-so-try-dd path
# out of reach: it is a run-time branch on both sides, and is unit-tested.
#
# `mount_if_needed` is compared as its three inputs rather than as `mount -a`,
# because whether it fires reads the live mount table.
CCMH_PY="$(cd "$(dirname "$0")" && pwd)/ccmounts.py"
CCMH_RS="$TARGET/examples/dump-cc-mounts"
if [ -x "$CCMH_RS" ] &&
   python3 -c 'import cloudinit.config.cc_mounts' 2>/dev/null; then
    CCMH_RS="$(cd "$(dirname "$CCMH_RS")" && pwd)/dump-cc-mounts"

    # Three trees, differing only in what the swap file's own branch can see:
    # absent, present and listed in /proc/swaps, present and not listed.
    for ccmh_root in "$WORK/mh0" "$WORK/mh1" "$WORK/mh2"; do
        rm -rf "$ccmh_root"
        mkdir -p "$ccmh_root/dev" "$ccmh_root/etc" "$ccmh_root/proc" \
                 "$ccmh_root/sys/block/sda/sda1" \
                 "$ccmh_root/sys/block/sdb" \
                 "$ccmh_root/sys/block/sdc"
        for ccmh_dev in sda sda1 sdb sdc; do
            : >"$ccmh_root/dev/$ccmh_dev"
        done
        printf '%s\n' \
            '/dev/root / ext4 defaults 0 1' \
            '' \
            '/dev/sdc /already auto defaults 0 2' \
            '/dev/sdb /old auto defaults,comment=cloudconfig 0 2' \
            >"$ccmh_root/etc/fstab"
    done
    : >"$WORK/mh1/swap.img"
    printf '%s\n' \
        'Filename				Type		Size	Used	Priority' \
        '/swap.img                               file		1048572	0	-2' \
        >"$WORK/mh1/proc/swaps"
    : >"$WORK/mh2/swap.img"

    : >"$WORK/ccmh.cases"
    for ccmh_root in "$WORK/mh0" "$WORK/mh1" "$WORK/mh2"; do
        for ccmh_cfg in \
            '{}' \
            '{"mounts": [["sdb", "/mnt"]]}' \
            '{"swap": {"size": "1G"}}' \
            '{"swap": {"size": "auto"}}' \
            '{"swap": {"size": "auto", "maxsize": "512M"}}' \
            '{"swap": {"size": "auto", "maxsize": 0}}' \
            '{"swap": {"size": "512M", "maxsize": "256M"}}' \
            '{"swap": {"filename": "/swap.img", "size": "1G"}}' \
            '{"swap": {"filename": "swap.img", "size": "1G"}}' \
            '{"swap": {"filename": "/var/swap/f", "size": "1G"}}' \
            '{"swap": {"size": 0}}' \
            '{"swap": {"filename": "", "size": "1G"}}' \
            '{"swap": "nope"}' \
            '{"swap": {"size": "bogus"}}' \
            '{"mounts": [["swap", "none", "swap", "sw", "0", "0"]], "swap": {"size": "1G"}}' \
            '{"mounts": [["sdb", "/mnt"], ["sdc", "/c"]], "swap": {"size": "auto"}}'; do
            # fstype null makes get_mount_info return None, which upstream
            # subscripts; memtotal null makes read_meminfo raise. There is no
            # `available: null` case: upstream's statvfs either answers or
            # raises, while the port's `stat -f` fallback treats a failure as
            # "no filesystem named" -- deviation 148, so the two cannot agree
            # and the path is unit-tested instead.
            for ccmh_env in \
                '{"fstype": "ext4", "kernel_version": [5, 15], "memtotal": 2147483648, "available": 10737418240}' \
                '{"fstype": "btrfs", "kernel_version": [4, 10], "memtotal": 8589934592, "available": 1073741824}' \
                '{"fstype": "xfs", "kernel_version": [4, 10], "memtotal": 4294967296, "available": 107374182400}' \
                '{"fstype": "xfs", "kernel_version": [5, 4], "memtotal": 4294967296, "available": 107374182400}' \
                '{"fstype": null, "kernel_version": [5, 15], "memtotal": 2147483648, "available": 10737418240}' \
                '{"fstype": "ext4", "kernel_version": [5, 15], "memtotal": null, "available": 10737418240}'; do
                for ccmh_systemd in 0 1; do
                    printf '%s\t%s\t%s\t%s\t%s\n' \
                        "$ccmh_root" "$ccmh_cfg" '{}' \
                        "$ccmh_systemd" "$ccmh_env" \
                        >>"$WORK/ccmh.cases"
                done
            done
        done
    done
    run_batch "cc_mounts handle" \
        "cd /tmp && python3 '$CCMH_PY' --batch" \
        "cd /tmp && '$CCMH_RS' --batch" \
        "$WORK/ccmh.cases"
fi

# --- growpart ----------------------------------------------------------------
sec "cc-growpart"
#
# The whole module against a scripted machine: what growpart printed and with
# what exit code, which paths exist, what stat and lseek said, what is mounted
# where. Nothing runs and no device is touched on either side -- the Python
# half stubs subp and the `os` probes onto the same script -- so what is
# compared is the log, the ordered list of questions the module asked, and the
# (device, action, message) triples it ended with.
#
# A size given as a list is one entry per read, which is how a partition that
# actually grew is spelled; a null in it is a read that finds nothing.
CCGP_PY="$(cd "$(dirname "$0")" && pwd)/ccgrowpart.py"
CCGP_RS="$TARGET/examples/dump-cc-growpart"
if [ -x "$CCGP_RS" ] &&
   python3 -c 'import cloudinit.config.cc_growpart' 2>/dev/null; then
    CCGP_RS="$(cd "$(dirname "$CCGP_RS")" && pwd)/dump-cc-growpart"

    ccgp_case() {
        printf '{"cfg": %s, "host": {%s}}\n' "$1" "$2" >>"$WORK/ccgp.cases"
    }

    # growpart is installed, its help mentions --update, and it grows either
    # partition of /dev/sda.
    ccgp_help='"growpart --help": {"stdout": "growpart disk partition\n   -u | --update  update the kernel\n"}'
    ccgp_dry='"growpart --dry-run /dev/sda 1": {"stdout": "CHANGE: partition=1 start=2048 old: size=2048,end=4096 new: size=4096,end=6144\n"}'
    ccgp_grow='"growpart /dev/sda 1": {"stdout": "CHANGED: partition=1 start=2048 old: size=2048,end=4096 new: size=4096,end=6144\n"}'
    ccgp_dry2='"growpart --dry-run /dev/sda 2": {"stdout": "CHANGE: partition=2\n"}'
    ccgp_grow2='"growpart /dev/sda 2": {"stdout": "CHANGED: partition=2\n"}'
    ccgp_ok="$ccgp_help, $ccgp_dry, $ccgp_grow"
    ccgp_ok2="$ccgp_ok, $ccgp_dry2, $ccgp_grow2"

    # A disk with two partitions, a character device, and the by-uuid links a
    # kernel command line can name. Mounts are per case, so that root can be
    # a partition, /dev/root, a mapped device or a zfs dataset in turn.
    ccgp_dev='"stat": {"/dev/sda1": "0o60660", "/dev/sda2": "0o60660", "/dev/dm-0": "0o60660", "/dev/sr0": "0o20660", "/dev/loop0": "0o100644", "/dev/disk/by-uuid/ab-cd": "0o60660", "/dev/disk/by-partuuid/12-34": "0o60660"}, '
    ccgp_dev=$ccgp_dev'"realpath": {"/sys/class/block/sda1": "/sys/devices/pci0/block/sda/sda1", "/sys/class/block/sda2": "/sys/devices/pci0/block/sda/sda2", "/dev/block/8:0": "/dev/sda", "/dev/disk/by-uuid/ab-cd": "/dev/sda1", "/dev/disk/by-partuuid/12-34": "/dev/sda1"}, '
    ccgp_dev=$ccgp_dev'"exists": ["/sys/class/block/sda1", "/sys/class/block/sda1/partition", "/sys/class/block/sda2", "/sys/class/block/sda2/partition", "/dev/disk/by-partuuid/12-34"], '
    ccgp_dev=$ccgp_dev'"text": {"/sys/class/block/sda1/partition": "1\n", "/sys/class/block/sda2/partition": "2\n", "/sys/devices/pci0/block/sda/dev": "8:0\n"}, '
    ccgp_dev=$ccgp_dev'"sizes": {"/dev/sda1": [2097152, 4194304], "/dev/sda2": [1048576, 2097152], "/dev/disk/by-uuid/ab-cd": [2097152, 4194304], "/dev/disk/by-partuuid/12-34": [2097152, 4194304]}'

    ccgp_m_part='"mounts": {"/": ["/dev/sda1", "ext4", "/"], "/srv": ["/dev/sda2", "xfs", "/srv"]}'
    ccgp_m_root='"mounts": {"/": ["/dev/root", "ext4", "/"]}'
    ccgp_m_dm='"mounts": {"/": ["/dev/dm-0", "ext4", "/"]}'
    ccgp_m_zfs='"mounts": {"/": ["tank/root", "zfs", "/"]}'

    : >"$WORK/ccgp.cases"

    # Which resizer, if any. An empty machine has none of the three.
    ccgp_case '{}' ''
    ccgp_case '{}' "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_part"
    for ccgp_mode in '"off"' 'false' '"false"' '0' 'true' '"auto"' \
                     '"growpart"' '"gpart"' '"growfs"' '"bogus"' '5' 'null'; do
        ccgp_case "{\"growpart\": {\"mode\": $ccgp_mode}}" ''
        ccgp_case "{\"growpart\": {\"mode\": $ccgp_mode}}" \
            "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_part"
    done
    # growpart's help without the flag it is probed for.
    ccgp_case '{}' '"commands": {"growpart --help": {"stdout": "usage: growpart\n"}}'
    # gpart, which answers on stderr and exits 1 doing it.
    ccgp_case '{}' '"commands": {"gpart help": {"exit_code": 1, "stderr": "usage: gpart recover [-f flags] geom\n"}}, '"$ccgp_dev, $ccgp_m_part"
    ccgp_case '{}' '"commands": {"gpart help": {"exit_code": 1, "stderr": "usage: gpart show\n"}}'
    ccgp_case '{}' '"commands": {"gpart help": {"stderr": "usage: gpart recover [-f flags] geom\n"}, "gpart recover /dev/sda": {}, "gpart resize -i 1 /dev/sda": {}}, '"$ccgp_dev, $ccgp_m_part"
    ccgp_case '{}' '"commands": {"gpart help": {"stderr": "usage: gpart recover [-f flags] geom\n"}, "gpart resize -i 1 /dev/sda": {}}, '"$ccgp_dev, $ccgp_m_part"
    # growfs, which is a file on FreeBSD and only ever resizes "/".
    ccgp_case '{}' '"files": ["/etc/rc.d/growfs"], '"$ccgp_dev, $ccgp_m_zfs"
    ccgp_case '{}' '"files": ["/etc/rc.d/growfs"], "commands": {"zpool get -Hpovalue size tank": {"stdout": "8589934592\n"}}, '"$ccgp_dev, $ccgp_m_zfs"
    ccgp_case '{"growpart": {"devices": ["/srv"]}}' \
        '"files": ["/etc/rc.d/growfs"], '"$ccgp_dev, $ccgp_m_zfs"

    # The flag file a distro drops to keep the root filesystem as built.
    ccgp_case '{}' "\"files\": [\"/etc/growroot-disabled\"], \"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_part"
    ccgp_case '{"growpart": {"ignore_growroot_disabled": true}}' \
        "\"files\": [\"/etc/growroot-disabled\"], \"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_part"

    # What can be named in `devices`.
    for ccgp_devs in '[]' 'null' '"/dev/sda1"' '"/ /srv"' '5' '[5]' '["/opt"]' \
                     '["/srv"]' '["/dev/sr0"]' '["/dev/loop0"]' \
                     '["/dev/missing"]' '["/", "/", "/srv"]'; do
        ccgp_case "{\"growpart\": {\"devices\": $ccgp_devs}}" \
            "\"commands\": {$ccgp_ok2}, $ccgp_dev, $ccgp_m_part"
    done

    # /dev/root, which has to be resolved through the kernel command line
    # unless this is a container.
    for ccgp_cmdline in '"root=/dev/sda1 ro"' '"root=sda1 ro"' \
                        '"root=UUID=AB-CD ro"' '"root=LABEL=cloudimg ro"' \
                        '"root=PARTUUID=12-34 ro"' '"root=PARTUUID=99-99 ro"' \
                        '"ro quiet"' '""'; do
        ccgp_case '{}' \
            "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_root, \"cmdline\": $ccgp_cmdline"
        ccgp_case '{}' \
            "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_root, \"cmdline\": $ccgp_cmdline, \"container\": true"
    done
    # A PARTUUID with no link on disk, which blkid may still know.
    ccgp_case '{}' \
        "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_root, \"cmdline\": \"root=PARTUUID=99-99\", \"devs\": {\"PARTUUID=99-99\": [\"/dev/sda1\", \"/dev/sda2\"]}"

    # A mapped device: dmsetup names the partition under it, and only an
    # encrypted one is followed.
    ccgp_dmdeps='"dmsetup deps --options=devname /dev/dm-0": {"stdout": "1 dependencies\t: (sda2)\n"}'
    ccgp_luks='"cryptsetup status /dev/dm-0": {}, "cryptsetup isLuks /dev/sda2": {}'
    ccgp_resize='"cryptsetup --key-file - resize /dev/dm-0": {}, "cryptsetup luksKillSlot --batch-mode /dev/sda2 5": {}'
    ccgp_key='"keydata": "{\"key\": \"MTIzNA==\", \"slot\": 5}"'
    ccgp_case '{}' "\"commands\": {$ccgp_ok2}, $ccgp_dev, $ccgp_m_dm"
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps}, $ccgp_dev, $ccgp_m_dm"
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm"
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps, \"cryptsetup status /dev/dm-0\": {\"exit_code\": 4}}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm"
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps, $ccgp_luks}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm"
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps, $ccgp_luks}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm, $ccgp_key"
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps, $ccgp_luks, $ccgp_resize}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm, $ccgp_key"
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps, $ccgp_luks, $ccgp_resize}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm, \"keydata\": \"not json\""
    ccgp_case '{}' "\"commands\": {$ccgp_ok2, $ccgp_dmdeps, $ccgp_luks, $ccgp_resize}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm, \"keydata\": \"{}\""
    # dmsetup saying something else.
    for ccgp_deps in '"0 dependencies\t: ()\n"' '"2 dependencies\t: (sda2) (sdb1)\n"' \
                     '"1 dependencies : sda2\n"' '""'; do
        ccgp_case '{}' \
            "\"commands\": {$ccgp_ok2, \"dmsetup deps --options=devname /dev/dm-0\": {\"stdout\": $ccgp_deps}}, \"which\": [\"cryptsetup\"], $ccgp_dev, $ccgp_m_dm"
    done

    # What growpart itself said, and what lseek saw afterwards.
    ccgp_case '{}' "\"commands\": {$ccgp_help, \"growpart --dry-run /dev/sda 1\": {\"exit_code\": 1, \"stdout\": \"NOCHANGE: partition 1 is size 4096. it cannot be grown\n\"}}, $ccgp_dev, $ccgp_m_part"
    ccgp_case '{}' "\"commands\": {$ccgp_help, \"growpart --dry-run /dev/sda 1\": {\"exit_code\": 2, \"stderr\": \"failed [sfd_dump:1] sfdisk --unit=S --dump /dev/sda\n\"}}, $ccgp_dev, $ccgp_m_part"
    ccgp_case '{}' "\"commands\": {$ccgp_help, $ccgp_dry, \"growpart /dev/sda 1\": {\"exit_code\": 2, \"stderr\": \"FAILED: failed to resize\n\"}}, $ccgp_dev, $ccgp_m_part"
    # The partition is the same size afterwards, then gone afterwards, then
    # never readable at all.
    ccgp_case '{}' "\"commands\": {$ccgp_ok}, $ccgp_m_part, \"stat\": {\"/dev/sda1\": \"0o60660\"}, \"realpath\": {\"/sys/class/block/sda1\": \"/sys/devices/pci0/block/sda/sda1\", \"/dev/block/8:0\": \"/dev/sda\"}, \"exists\": [\"/sys/class/block/sda1\", \"/sys/class/block/sda1/partition\"], \"text\": {\"/sys/class/block/sda1/partition\": \"1\n\", \"/sys/devices/pci0/block/sda/dev\": \"8:0\n\"}, \"sizes\": {\"/dev/sda1\": 2097152}"
    ccgp_case '{}' "\"commands\": {$ccgp_ok}, $ccgp_m_part, \"stat\": {\"/dev/sda1\": \"0o60660\"}, \"realpath\": {\"/sys/class/block/sda1\": \"/sys/devices/pci0/block/sda/sda1\", \"/dev/block/8:0\": \"/dev/sda\"}, \"exists\": [\"/sys/class/block/sda1\", \"/sys/class/block/sda1/partition\"], \"text\": {\"/sys/class/block/sda1/partition\": \"1\n\", \"/sys/devices/pci0/block/sda/dev\": \"8:0\n\"}, \"sizes\": {\"/dev/sda1\": [2097152, null]}"
    ccgp_case '{}' "\"commands\": {$ccgp_ok}, $ccgp_m_part, \"stat\": {\"/dev/sda1\": \"0o60660\"}, \"realpath\": {\"/sys/class/block/sda1\": \"/sys/devices/pci0/block/sda/sda1\", \"/dev/block/8:0\": \"/dev/sda\"}, \"exists\": [\"/sys/class/block/sda1\", \"/sys/class/block/sda1/partition\"], \"text\": {\"/sys/class/block/sda1/partition\": \"1\n\", \"/sys/devices/pci0/block/sda/dev\": \"8:0\n\"}"
    # growpart's own temp directory, already there and not.
    ccgp_case '{}' "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_part, \"tmpdir\": \"/var/tmp/cloud-init/tmp0\""
    ccgp_case '{}' "\"commands\": {$ccgp_ok}, $ccgp_m_part, \"stat\": {\"/dev/sda1\": \"0o60660\"}, \"realpath\": {\"/sys/class/block/sda1\": \"/sys/devices/pci0/block/sda/sda1\", \"/dev/block/8:0\": \"/dev/sda\"}, \"exists\": [\"/sys/class/block/sda1\", \"/sys/class/block/sda1/partition\", \"/var/tmp/cloud-init/tmpfixture/growpart\"], \"text\": {\"/sys/class/block/sda1/partition\": \"1\n\", \"/sys/devices/pci0/block/sda/dev\": \"8:0\n\"}, \"sizes\": {\"/dev/sda1\": [2097152, 4194304]}"

    # A device that is in /sys but is not a partition, and one that is not in
    # /sys at all.
    ccgp_case '{"growpart": {"devices": ["/dev/sda"]}}' \
        "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_part, \"stat\": {\"/dev/sda\": \"0o60660\"}, \"exists\": [\"/sys/class/block/sda\"]"
    ccgp_case '{"growpart": {"devices": ["/dev/sda"]}}' \
        "\"commands\": {$ccgp_ok}, $ccgp_dev, $ccgp_m_part, \"stat\": {\"/dev/sda\": \"0o60660\"}"
    # A partition whose number, or whose disk's major:minor, cannot be read.
    ccgp_case '{}' \
        "\"commands\": {$ccgp_ok}, $ccgp_m_part, \"stat\": {\"/dev/sda1\": \"0o60660\"}, \"realpath\": {\"/sys/class/block/sda1\": \"/sys/devices/pci0/block/sda/sda1\"}, \"exists\": [\"/sys/class/block/sda1\", \"/sys/class/block/sda1/partition\"]"
    ccgp_case '{}' \
        "\"commands\": {$ccgp_ok}, $ccgp_m_part, \"stat\": {\"/dev/sda1\": \"0o60660\"}, \"realpath\": {\"/sys/class/block/sda1\": \"/sys/devices/pci0/block/sda/sda1\"}, \"exists\": [\"/sys/class/block/sda1\", \"/sys/class/block/sda1/partition\"], \"text\": {\"/sys/class/block/sda1/partition\": \"1\n\"}"

    run_batch "cc_growpart" \
        "cd /tmp && python3 '$CCGP_PY' --batch" \
        "cd /tmp && '$CCGP_RS' --batch" \
        "$WORK/ccgp.cases"
fi

# --- disk_setup --------------------------------------------------------------
sec "cc-disk-setup"
#
# The whole module against a scripted machine: what lsblk, blkid, sfdisk and
# sgdisk printed, which paths exist, which are block devices, what realpath
# resolved to. Nothing runs and no device is touched on either side -- the
# Python half stubs subp, the `os` probes, `pathlib.Path` and the module's own
# `open`, which is the one that would have zeroed a mebibyte of disk.
#
# What is compared is the log and the ordered list of questions the module
# asked. For this module the second is the outcome: every partition it creates
# and every filesystem it makes is a command, so the command list is the
# effect.
CCDS_PY="$(cd "$(dirname "$0")" && pwd)/ccdisksetup.py"
CCDS_RS="$TARGET/examples/dump-cc-disk-setup"
if [ -x "$CCDS_RS" ] &&
   python3 -c 'import cloudinit.config.cc_disk_setup' 2>/dev/null; then
    CCDS_RS="$(cd "$(dirname "$CCDS_RS")" && pwd)/dump-cc-disk-setup"

    ccds_case() {
        printf '{"cfg": %s, "host": {%s}}\n' "$1" "$2" >>"$WORK/ccds.cases"
    }

    # A machine with every tool the module looks for, and one spare disk.
    ccds_which='"which": {"sgdisk": "/usr/sbin/sgdisk", "partprobe": "/usr/sbin/partprobe", "udevadm": "/usr/bin/udevadm", "mkfs.ext4": "/usr/sbin/mkfs.ext4", "mkfs.xfs": "/usr/sbin/mkfs.xfs", "mkswap": "/usr/sbin/mkswap", "wipefs": "/usr/sbin/wipefs", "blkid": "/usr/sbin/blkid"}'
    ccds_paths='"exists": ["/dev/sdb", "/dev/sdb1", "/dev/sdb2", "/dev/disk/by-id/spare", "/dev/nvme0n1"], "block": ["/dev/sdb1", "/dev/sdb2", "/dev/nvme0n1p1"], "realpath": {"/dev/disk/by-id/spare": "/dev/sdb"}'

    # The commands a healthy run makes, in the shapes the module builds them.
    ccds_settle='"udevadm settle": {}, "partprobe /dev/sdb": {}, "partprobe /dev/nvme0n1": {}'
    ccds_blank='"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\n"}, "lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\n"}'
    ccds_size='"blockdev --getsize64 /dev/sdb": {"stdout": "429496729600\n"}, "blockdev --getss /dev/sdb": {"stdout": "512\n"}'
    ccds_empty_gpt='"sgdisk -p /dev/sdb": {"stdout": "Disk /dev/sdb: 838860800 sectors, 400.0 GiB\n\nNumber  Start (sector)    End (sector)  Size       Code  Name\n"}'
    ccds_one_gpt='"sgdisk -p /dev/sdb": {"stdout": "Disk /dev/sdb: 838860800 sectors, 400.0 GiB\n\nNumber  Start (sector)    End (sector)  Size       Code  Name\n   1            2048       838860766   400.0 GiB   8300  Linux filesystem\n"}'
    ccds_two_gpt='"sgdisk -p /dev/sdb": {"stdout": "Number  Start (sector)    End (sector)  Size       Code  Name\n   1            2048       419430400   200.0 GiB   8300  Linux filesystem\n   2       419430401       838860766   200.0 GiB   8200  Linux swap\n"}'
    ccds_mbr_empty='"sfdisk -l /dev/sdb": {"stdout": "Disk /dev/sdb: 400 GiB\nDevice     Boot Start     End Sectors  Size Id Type\n"}'
    ccds_mbr_one='"sfdisk -l /dev/sdb": {"stdout": "Disk /dev/sdb: 400 GiB\nDevice     Boot Start       End   Sectors  Size Id Type\n/dev/sdb1        2048 838860766 838858719  400G 83 Linux\n"}'
    ccds_mkpart='"sgdisk -Z /dev/sdb": {}, "sgdisk -n 1:0:0 /dev/sdb": {}, "sgdisk -n 1:0:+141733920768 /dev/sdb": {}, "sgdisk -n 2:0:0 /dev/sdb": {}, "sgdisk -t 1:8300 /dev/sdb": {}, "sgdisk -t 2:8200 /dev/sdb": {}, "sfdisk --force /dev/sdb": {}, "sfdisk -X gpt --force /dev/sdb": {}'
    ccds_ok="$ccds_settle, $ccds_blank, $ccds_size, $ccds_empty_gpt, $ccds_mbr_empty, $ccds_mkpart"

    : >"$WORK/ccds.cases"

    # --- nothing to do -------------------------------------------------------
    for ccds_cfg in \
        '{}' \
        '{"disk_setup": null}' \
        '{"disk_setup": "sdb"}' \
        '{"disk_setup": []}' \
        '{"disk_setup": {}}' \
        '{"fs_setup": null}' \
        '{"fs_setup": {}}' \
        '{"fs_setup": []}' \
        '{"disk_setup": {"/dev/sdb": "yes"}}' \
        '{"disk_setup": {"/dev/sdb": 5}}' \
        '{"fs_setup": ["/dev/sdb"]}' \
        '{"fs_setup": [null]}' \
        '{"fs_setup": [{}]}'; do
        ccds_case "$ccds_cfg" "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    done

    # --- a layout that says do nothing ---------------------------------------
    for ccds_layout in 'false' 'null' '0' '""' '[]' '{}'; do
        ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": $ccds_layout}}}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    done

    # --- the device itself ---------------------------------------------------
    # A device that is not there, one that arrives only after a settle, one
    # reached through a symlink, and one whose settle fails.
    ccds_case '{"disk_setup": {"/dev/sdz": {"layout": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    ccds_case '{"disk_setup": {"/dev/disk/by-id/spare": {"layout": true, "table_type": "gpt"}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {\"udevadm settle\": {\"exit_code\": 1, \"stderr\": \"settle: timed out\n\"}}"
    # No udevadm at all, which is the Alpine case util.udevadm_settle guards.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt"}}}' \
        "\"which\": {\"sgdisk\": \"/usr/sbin/sgdisk\"}, $ccds_paths, \"commands\": {$ccds_ok}"

    # --- is the device a disk ------------------------------------------------
    for ccds_lsblk in \
        '"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"stdout": "NAME=\"sdb\" TYPE=\"part\" FSTYPE=\"\" LABEL=\"\"\n"}' \
        '"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"stdout": "\n"}' \
        '"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"stdout": "NAME=\"sdb\"\n"}' \
        '"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"exit_code": 1, "stderr": "lsblk: /dev/sdb: not a block device\n"}' \
        '"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" LABEL=\"a=b\"\n"}' \
        '"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"stdout": "NAME=\"sdb\" TYPE\n"}' \
        '"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb --nodeps": {"stdout": "NAME=\"sdb TYPE=\"disk\"\n"}'; do
        ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt"}}}' \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_size, $ccds_empty_gpt, $ccds_mkpart, $ccds_lsblk}"
    done

    # --- remove the table ----------------------------------------------------
    ccds_children='"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\nNAME=\"sdb1\" TYPE=\"part\" FSTYPE=\"ext4\" LABEL=\"data\"\nNAME=\"sdb2\" TYPE=\"part\" FSTYPE=\"swap\" LABEL=\"\"\n"}, "wipefs --all /dev/sdb1": {}, "wipefs --all /dev/sdb2": {}'
    for ccds_layout in '"remove"' '"REMOVE"' '"Remove"'; do
        ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": $ccds_layout}}}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok, $ccds_children}"
    done
    # wipefs fails, and the zeroing of the ends fails.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": "remove"}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok, $ccds_children, \"wipefs --all /dev/sdb2\": {\"exit_code\": 1, \"stderr\": \"busy\n\"}}"
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": "remove"}}}' \
        "$ccds_which, $ccds_paths, \"wipe\": {\"/dev/sdb\": \"[Errno 13] Permission denied: '/dev/sdb'\"}, \"commands\": {$ccds_ok, $ccds_children}"
    # A crypt child is left alone.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": "remove"}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok, \"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb\": {\"stdout\": \"NAME=\\\"sdb\\\" TYPE=\\\"disk\\\" FSTYPE=\\\"\\\" LABEL=\\\"\\\"\nNAME=\\\"dm-0\\\" TYPE=\\\"crypt\\\" FSTYPE=\\\"ext4\\\" LABEL=\\\"\\\"\n\"}}"

    # --- does the layout already match ---------------------------------------
    # true against an empty table, true against one partition, a two-entry
    # layout against two partitions, and the type comparisons in between.
    for ccds_layout in \
        'true' \
        '[50, 50]' \
        '[[50, 82], [50, 83]]' \
        '[[50, "8200"], [50, "8300"]]' \
        '[[100, "0FC63DAF-8483-4772-8E79-3D69D8477DE4"]]' \
        '[[100, "zz"]]' \
        '[[100, "12345"]]' \
        '[[100, 83], [100, 83], [100, 83]]' \
        '[[50]]' \
        '[[50, 82, 83]]' \
        '"auto"' \
        '5'; do
        for ccds_found in "$ccds_empty_gpt" "$ccds_one_gpt" "$ccds_two_gpt"; do
            ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": $ccds_layout, \"table_type\": \"gpt\", \"overwrite\": true}}}" \
                "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mkpart, $ccds_found}"
        done
    done

    # sgdisk output that stops short of the six columns the parser indexes.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt"}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mkpart, \"sgdisk -p /dev/sdb\": {\"stdout\": \"Number  Start End\n   1  2048\n\"}}"
    # sgdisk that fails outright.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt"}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mkpart, \"sgdisk -p /dev/sdb\": {\"exit_code\": 2, \"stderr\": \"Problem opening /dev/sdb\n\"}}"

    # --- gpt without sgdisk, which is the sfdisk JSON path --------------------
    ccds_nosg='"which": {"partprobe": "/usr/sbin/partprobe", "udevadm": "/usr/bin/udevadm", "mkfs.ext4": "/usr/sbin/mkfs.ext4", "wipefs": "/usr/sbin/wipefs"}'
    for ccds_json in \
        '{"stdout": "{\"partitiontable\": {\"label\": \"gpt\", \"partitions\": [{\"node\": \"/dev/sdb1\", \"type\": \"0FC63DAF-8483-4772-8E79-3D69D8477DE4\"}]}}\n"}' \
        '{"stdout": "{\"partitiontable\": {\"label\": \"gpt\"}}\n"}' \
        '{"stdout": "{\"partitiontable\": {\"partitions\": []}}\n"}' \
        '{"stdout": "{}\n"}' \
        '{"exit_code": 1, "stderr": "sfdisk: cannot open /dev/sdb\n"}'; do
        ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt", "overwrite": true}}}' \
            "$ccds_nosg, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mkpart, \"sfdisk -l -J /dev/sdb\": $ccds_json}"
    done

    # --- mbr ------------------------------------------------------------------
    for ccds_layout in \
        'true' \
        '[100]' \
        '[33, 66]' \
        '[[33, 82], [66, 83]]' \
        '[[33, "83"]]' \
        '[25, 25, 25, 25]' \
        '[20, 20, 20, 20, 20]' \
        '["50", 50]' \
        '[[1.5, 83], [98.5, 83]]' \
        '["abc"]'; do
        for ccds_found in "$ccds_mbr_empty" "$ccds_mbr_one"; do
            ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": $ccds_layout, \"table_type\": \"mbr\", \"overwrite\": true}}}" \
                "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mkpart, $ccds_found}"
        done
    done

    # An unknown table type, and one that is not a string.
    for ccds_table in '"dos"' 'null' '5'; do
        ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": true, \"table_type\": $ccds_table}}}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    done

    # --- is the disk in use ---------------------------------------------------
    ccds_used='"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\nNAME=\"sdb1\" TYPE=\"part\" FSTYPE=\"ext4\" LABEL=\"data\"\n"}'
    ccds_formatted='"blkid -c /dev/null /dev/sdb": {"stdout": "/dev/sdb: LABEL=\"data\" UUID=\"ab-cd\" TYPE=\"ext4\"\n"}'
    for ccds_over in 'false' 'true'; do
        ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": true, \"table_type\": \"gpt\", \"overwrite\": $ccds_over}}}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_empty_gpt, $ccds_mkpart, $ccds_used}"
        ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": true, \"table_type\": \"gpt\", \"overwrite\": $ccds_over}}}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_empty_gpt, $ccds_mkpart, $ccds_formatted}"
    done
    # blkid answering 2, which is "no filesystem" and not an error.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt"}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_empty_gpt, $ccds_mkpart, \"blkid -c /dev/null /dev/sdb\": {\"exit_code\": 2}}"
    # blkid answering 3, which is.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt"}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_empty_gpt, $ccds_mkpart, \"blkid -c /dev/null /dev/sdb\": {\"exit_code\": 3, \"stderr\": \"blkid: cannot open\n\"}}"

    # --- the size the layout is computed from ---------------------------------
    for ccds_sizes in \
        '"blockdev --getsize64 /dev/sdb": {"stdout": "429496729600\n"}, "blockdev --getss /dev/sdb": {"stdout": "4096\n"}' \
        '"blockdev --getsize64 /dev/sdb": {"stdout": "not a number\n"}, "blockdev --getss /dev/sdb": {"stdout": "512\n"}' \
        '"blockdev --getsize64 /dev/sdb": {"stdout": "429496729600\n"}, "blockdev --getss /dev/sdb": {"stdout": "0\n"}' \
        '"blockdev --getsize64 /dev/sdb": {"exit_code": 1, "stderr": "blockdev: cannot open\n"}'; do
        ccds_case '{"disk_setup": {"/dev/sdb": {"layout": [50, 50], "table_type": "mbr", "overwrite": true}}}' \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_mbr_empty, $ccds_mkpart, $ccds_sizes}"
    done

    # --- partitioning that fails ----------------------------------------------
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "mbr", "overwrite": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mbr_empty, \"sfdisk --force /dev/sdb\": {\"exit_code\": 1, \"stderr\": \"sfdisk: cannot write\n\"}}"
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "gpt", "overwrite": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_empty_gpt, \"sgdisk -Z /dev/sdb\": {\"exit_code\": 1, \"stderr\": \"sgdisk: cannot zap\n\"}}"
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": [[50, 82], [50, 83]], "table_type": "gpt", "overwrite": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_empty_gpt, $ccds_mkpart, \"sgdisk -t 2:8300 /dev/sdb\": {\"exit_code\": 1, \"stderr\": \"sgdisk: bad type\n\"}}"
    # partprobe failing is logged and forgiven; the settle after it is not.
    ccds_case '{"disk_setup": {"/dev/sdb": {"layout": true, "table_type": "mbr", "overwrite": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mbr_empty, $ccds_mkpart, \"partprobe /dev/sdb\": {\"exit_code\": 1, \"stderr\": \"partprobe: device busy\n\"}}"

    # --- the aliases ----------------------------------------------------------
    ccds_case '{"device_aliases": {"spare": "/dev/sdb"}, "disk_setup": {"spare": {"layout": true, "table_type": "gpt", "overwrite": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    ccds_case '{"device_aliases": {"spare": "/dev/sdb"}, "disk_setup": {"spare": {"layout": true}, "/dev/sdb": {"layout": false}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    ccds_case '{"device_aliases": {"spare": "/dev/sdb"}, "disk_setup": {"spare": "no"}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    ccds_case '{"device_aliases": {"spare": null}, "disk_setup": {"spare": {"layout": true}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"
    ccds_case '{"device_aliases": "spare", "disk_setup": {"/dev/sdb": {"layout": false}}}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok}"

    # --- fs_setup: a numbered partition ---------------------------------------
    ccds_fs_blank='"blkid -c /dev/null /dev/sdb1": {"exit_code": 2}, "blkid -c /dev/null /dev/sdb2": {"exit_code": 2}, "blkid -c /dev/null /dev/nvme0n1p1": {"exit_code": 2}'
    ccds_fs_ext4='"blkid -c /dev/null /dev/sdb1": {"stdout": "/dev/sdb1: LABEL=\"data\" UUID=\"ab-cd\" TYPE=\"ext4\"\n"}'
    ccds_mkfs='"/usr/sbin/mkfs.ext4 -L data /dev/sdb1": {}, "/usr/sbin/mkfs.ext4 /dev/sdb1": {}, "/usr/sbin/mkfs.ext4 -L data /dev/nvme0n1p1": {}, "/usr/sbin/mkfs.ext4 -L data -F /dev/sdb": {}, "/usr/sbin/mkfs.ext4 -L data -F /dev/sdb1": {}, "/usr/sbin/mkswap -L swap -f /dev/sdb2": {}'
    ccds_fs_cmds="$ccds_settle, $ccds_fs_blank, $ccds_mkfs, $ccds_blank"

    for ccds_part in '1' '"1"' '2' '"01"' '99' '""' '"none"' '"None"' '"auto"' '"any"' '"AUTO"' '"bogus"' 'null' 'true'; do
        ccds_case "{\"fs_setup\": [{\"device\": \"/dev/sdb\", \"partition\": $ccds_part, \"filesystem\": \"ext4\", \"label\": \"data\"}]}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds}"
    done

    # nvme, where the partition suffix gains a `p`.
    ccds_case '{"fs_setup": [{"device": "/dev/nvme0n1", "partition": 1, "filesystem": "ext4", "label": "data"}]}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds, \"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/nvme0n1p1 --nodeps\": {\"stdout\": \"NAME=\\\"nvme0n1p1\\\" TYPE=\\\"part\\\" FSTYPE=\\\"\\\" LABEL=\\\"\\\"\n\"}}"

    # The partition already carries the filesystem that was asked for.
    for ccds_over in 'false' 'true'; do
        ccds_case "{\"fs_setup\": [{\"device\": \"/dev/sdb\", \"partition\": 1, \"filesystem\": \"ext4\", \"label\": \"data\", \"overwrite\": $ccds_over}]}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds, $ccds_fs_ext4, \"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb1 --nodeps\": {\"stdout\": \"NAME=\\\"sdb1\\\" TYPE=\\\"part\\\" FSTYPE=\\\"ext4\\\" LABEL=\\\"data\\\"\n\"}}"
    done

    # --- fs_setup: automatic device selection ---------------------------------
    ccds_tree_free='"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\nNAME=\"sdb1\" TYPE=\"part\" FSTYPE=\"\" LABEL=\"\"\n"}'
    ccds_tree_match='"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\nNAME=\"sdb1\" TYPE=\"part\" FSTYPE=\"ext4\" LABEL=\"data\"\n"}'
    ccds_tree_other='"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"\" LABEL=\"\"\nNAME=\"sdb1\" TYPE=\"part\" FSTYPE=\"xfs\" LABEL=\"other\"\n"}'
    ccds_tree_full='"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb": {"stdout": "NAME=\"sdb\" TYPE=\"disk\" FSTYPE=\"ext4\" LABEL=\"\"\n"}'
    for ccds_tree in "$ccds_tree_free" "$ccds_tree_match" "$ccds_tree_other" "$ccds_tree_full"; do
        for ccds_fs in \
            '{"device": "/dev/sdb", "partition": "auto", "filesystem": "ext4", "label": "data"}' \
            '{"device": "/dev/sdb", "partition": "any", "filesystem": "ext4", "label": "data"}' \
            '{"device": "/dev/sdb", "partition": "auto", "filesystem": "ext4"}' \
            '{"device": "/dev/sdb", "partition": "auto", "filesystem": "ext4", "label": "data", "replace_fs": "xfs"}' \
            '{"device": "/dev/sdb", "partition": "any", "filesystem": "ext4", "label": "data", "replace_fs": "xfs"}'; do
            ccds_case "{\"fs_setup\": [$ccds_fs]}" \
                "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds, $ccds_tree}"
        done
    done

    # --- fs_setup: what to run ------------------------------------------------
    for ccds_fs in \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4"}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "label": "data", "overwrite": true}' \
        '{"device": "/dev/sdb", "partition": 2, "filesystem": "swap", "label": "swap"}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "reiserfs", "label": "data"}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "vfat", "label": "data"}' \
        '{"device": "/dev/sdb", "partition": 1, "label": "data"}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "label": "data", "extra_opts": ["-m", "0"]}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "label": "data", "extra_opts": "-m0"}' \
        '{"device": "/dev/sdb", "partition": 1, "cmd": "mkfs -t %(filesystem)s -L %(label)s %(device)s", "filesystem": "ext4", "label": "data"}' \
        '{"device": "/dev/sdb", "partition": 1, "cmd": "mkfs %(device)s", "overwrite": true, "extra_opts": ["-m"]}' \
        '{"device": "/dev/sdb", "partition": 1, "cmd": "mkfs %(nope)s"}' \
        '{"device": "/dev/sdb", "partition": 1, "cmd": ["mkfs", "/dev/sdb1"]}' \
        '{"partition": 1, "filesystem": "ext4"}' \
        '{"device": null, "partition": 1, "filesystem": "ext4"}' \
        '{"device": "/dev/sdz", "partition": 1, "filesystem": "ext4"}'; do
        ccds_case "{\"fs_setup\": [$ccds_fs]}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds}, \"shell\": {\"mkfs -t ext4 -L data /dev/sdb1\": {}, \"mkfs /dev/sdb1\": {}}"
    done

    # mkfs itself failing, both ways.
    ccds_case '{"fs_setup": [{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "label": "data"}]}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_fs_blank, $ccds_blank, \"/usr/sbin/mkfs.ext4 -L data /dev/sdb1\": {\"exit_code\": 1, \"stderr\": \"mkfs.ext4: device busy\n\"}}"
    ccds_case '{"fs_setup": [{"device": "/dev/sdb", "partition": 1, "cmd": "mkfs %(device)s"}]}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds}, \"shell\": {\"mkfs /dev/sdb1\": {\"exit_code\": 1, \"stderr\": \"mkfs: no\n\"}}"
    # No mkfs for the filesystem asked for.
    ccds_case '{"fs_setup": [{"device": "/dev/sdb", "partition": 1, "filesystem": "zfs", "label": "data"}]}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds}"

    # --- fs_setup: dotted names and aliases -----------------------------------
    for ccds_fs in \
        '{"device": "/dev/sdb.1", "filesystem": "ext4", "label": "data"}' \
        '{"device": "/dev/sdb.1", "partition": 2, "filesystem": "ext4", "label": "data"}' \
        '{"device": "/dev/sdb.", "filesystem": "ext4", "label": "data"}' \
        '{"device": "spare.1", "filesystem": "ext4", "label": "data"}' \
        '{"device": "spare", "partition": 1, "filesystem": "ext4", "label": "data"}' \
        '{"device": 5, "filesystem": "ext4"}'; do
        ccds_case "{\"device_aliases\": {\"spare\": \"/dev/sdb\"}, \"fs_setup\": [$ccds_fs]}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds}"
    done

    # --- both halves, the way an image actually configures a spare disk -------
    ccds_case '{"device_aliases": {"spare": "/dev/sdb"}, "disk_setup": {"spare": {"table_type": "gpt", "layout": true, "overwrite": true}}, "fs_setup": [{"device": "spare.1", "filesystem": "ext4", "label": "data", "overwrite": true}]}' \
        "$ccds_which, $ccds_paths, \"commands\": {$ccds_ok, $ccds_fs_blank, $ccds_mkfs, \"lsblk --pairs --output NAME,TYPE,FSTYPE,LABEL /dev/sdb1 --nodeps\": {\"stdout\": \"NAME=\\\"sdb1\\\" TYPE=\\\"part\\\" FSTYPE=\\\"\\\" LABEL=\\\"\\\"\n\"}}"

    # --- how an mbr listing is read back --------------------------------------
    # The type label is the last field that is all digits, scanning right to
    # left, and rows whose last word is `extended` or `empty` are skipped.
    for ccds_listing in \
        '"sfdisk -l /dev/sdb": {"stdout": "/dev/sdb1 2048 1000 500 83 Linux\n/dev/sdb2 3048 2000 500 82 Linux swap\n"}' \
        '"sfdisk -l /dev/sdb": {"stdout": "/dev/sdb1 2048 1000 500 5 Extended\n/dev/sdb5 2049 999 400 83 Linux\n"}' \
        '"sfdisk -l /dev/sdb": {"stdout": "/dev/sdb1 0 0 0 0 Empty\n"}' \
        '"sfdisk -l /dev/sdb": {"stdout": "/dev/sdb1 Linux filesystem\n"}' \
        '"sfdisk -l /dev/sdb": {"stdout": "   \n\n/dev/sdb1 1 2 3 83 Linux\n"}' \
        '"sfdisk -l /dev/sdb": {"stdout": "sdb1 1 2 3 83 Linux\n"}' \
        '"sfdisk -l /dev/sdb": {"exit_code": 1, "stderr": "sfdisk: no such device\n"}'; do
        for ccds_layout in 'true' '[100]' '[[100, 83]]' '[[50, 83], [50, 82]]'; do
            ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": $ccds_layout, \"table_type\": \"mbr\", \"overwrite\": true}}}" \
                "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mkpart, $ccds_listing}"
        done
    done

    # --- comparing partition types --------------------------------------------
    # Two-digit codes promote to four, four-digit codes promote to GUIDs, and
    # each length that is neither raises. B87: all three messages name the
    # wrong value.
    for ccds_wanted in '83' '"83"' '"8300"' '"0FC63DAF-8483-4772-8E79-3D69D8477DE4"' '"0fc63daf-8483-4772-8e79-3d69d8477de4"' '"ffff"' '"8"' '"83000"' 'null' 'true'; do
        for ccds_found in \
            '"sgdisk -p /dev/sdb": {"stdout": "Number\n 1 2 3 4 5 8300 Linux\n"}' \
            '"sgdisk -p /dev/sdb": {"stdout": "Number\n 1 2 3 4 5 0FC63DAF-8483-4772-8E79-3D69D8477DE4 Linux\n"}' \
            '"sgdisk -p /dev/sdb": {"stdout": "Number\n 1 2 3 4 5 83 Linux\n"}' \
            '"sgdisk -p /dev/sdb": {"stdout": "Number\n 1 2 3 4 5 ffff Linux\n"}' \
            '"sgdisk -p /dev/sdb": {"stdout": "Number\n 1 2 3 4 5 nope Linux\n"}'; do
            ccds_case "{\"disk_setup\": {\"/dev/sdb\": {\"layout\": [[100, $ccds_wanted]], \"table_type\": \"gpt\", \"overwrite\": true}}}" \
                "$ccds_which, $ccds_paths, \"commands\": {$ccds_settle, $ccds_blank, $ccds_size, $ccds_mkpart, $ccds_found}"
        done
    done

    # --- fs_setup corners -----------------------------------------------------
    for ccds_fs in \
        '{"device": "/dev/sdb", "partition": "auto", "filesystem": "ext4", "label": "data", "replace_fs": null}' \
        '{"device": "/dev/sdb", "partition": "auto", "filesystem": null, "label": "data"}' \
        '{"device": "/dev/sdb", "partition": "any", "filesystem": null}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "label": 5}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "label": ""}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": 5}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "extra_opts": 5}' \
        '{"device": "/dev/sdb", "partition": 1, "filesystem": "ext4", "extra_opts": [5, true]}' \
        '{"device": "/dev/sdb", "partition": "none", "filesystem": "ext4", "label": "data"}' \
        '{"device": "/dev/sdb", "partition": "NONE", "filesystem": "ext4", "label": "data"}'; do
        ccds_case "{\"fs_setup\": [$ccds_fs]}" \
            "$ccds_which, $ccds_paths, \"commands\": {$ccds_fs_cmds, $ccds_tree_free}"
    done

    run_batch "cc_disk_setup" \
        "cd /tmp && python3 '$CCDS_PY' --batch" \
        "cd /tmp && '$CCDS_RS' --batch" \
        "$WORK/ccds.cases"
fi

# --- resizefs ----------------------------------------------------------------
sec "cc-resizefs"
#
# The whole module against a scripted machine: what btrfs, growfs, zpool and
# the resizer itself printed, which paths exist, what stat said, what is
# mounted where and with which options. Nothing runs and no filesystem is
# touched on either side -- the Python half stubs subp, util.fork_cb and the
# `os` probes onto the same script -- so what is compared is the log and the
# ordered list of questions the module asked.
#
# `mounts` is [device, fstype, mount-point, options]: the module asks for the
# first three, and `util.mount_is_read_write` asks again for the fourth.
CCRF_PY="$(cd "$(dirname "$0")" && pwd)/ccresizefs.py"
CCRF_RS="$TARGET/examples/dump-cc-resizefs"
if [ -x "$CCRF_RS" ] &&
   python3 -c 'import cloudinit.config.cc_resizefs' 2>/dev/null; then
    CCRF_RS="$(cd "$(dirname "$CCRF_RS")" && pwd)/dump-cc-resizefs"

    ccrf_case() {
        printf '{"cfg": %s, "args": %s, "host": {%s}}\n' "$1" "$2" "$3" \
            >>"$WORK/ccrf.cases"
    }

    # An ext4 root on a partition that resize2fs grows.
    ccrf_ext='"mounts": {"/": ["/dev/sda1", "ext4", "/", "rw,relatime"]}, "stat": {"/dev/sda1": "0o60660"}'
    ccrf_ext_ok="$ccrf_ext"', "commands": {"resize2fs /dev/sda1": {"stdout": "Filesystem at /dev/sda1 is mounted on /; on-line resizing required\n"}}'

    : >"$WORK/ccrf.cases"

    # --- the switch -----------------------------------------------------------
    # Everything `resize_rootfs` can be. The default is the bool True, and
    # anything configured is str() of it before translate_bool sees it, so 1
    # and "on" are yes and 2 and "maybe" are not.
    for ccrf_v in 'true' 'false' '"true"' '"false"' '"True"' '0' '1' '2' \
                  '"on"' '"off"' '"yes"' '"no"' '"noblock"' '"NoBlock"' \
                  '" noblock "' '"maybe"' 'null' '""' '[]' '{}' '1.0' \
                  '["noblock"]' '{"a": 1}'; do
        ccrf_case "{\"resize_rootfs\": $ccrf_v}" '[]' "$ccrf_ext_ok"
    done
    # No key at all, and the run arguments that override it.
    ccrf_case '{}' '[]' "$ccrf_ext_ok"
    ccrf_case '{"resize_rootfs": false}' '["noblock"]' "$ccrf_ext_ok"
    ccrf_case '{}' '["false"]' "$ccrf_ext_ok"
    ccrf_case '{}' '[false]' "$ccrf_ext_ok"
    ccrf_case '{}' '[true, "noblock"]' "$ccrf_ext_ok"
    ccrf_case '{}' '[null]' "$ccrf_ext_ok"
    # The module's configured name, which the skip line quotes.
    printf '{"name": "cc_resizefs", "cfg": {"resize_rootfs": false}}\n' \
        >>"$WORK/ccrf.cases"

    # --- what is mounted on / -------------------------------------------------
    # Nothing at all, then each filesystem family in turn, then one nobody
    # has a resizer for and one whose name only differs in case.
    ccrf_case '{}' '[]' ''
    for ccrf_fs in 'ext2' 'ext3' 'ext4' 'EXT4' 'xfs' 'btrfs' 'ufs' 'ufs2' \
                   'hammer2' 'bcachefs' 'reiserfs' 'overlay' 'ex' ''; do
        ccrf_case '{}' '[]' \
            "\"mounts\": {\"/\": [\"/dev/sda1\", \"$ccrf_fs\", \"/\", \"rw\"]}, \"stat\": {\"/dev/sda1\": \"0o60660\"}"
    done

    # --- the device the mount table names -------------------------------------
    # /dev/root, which only the kernel command line can resolve, and only
    # outside a container.
    for ccrf_cmdline in '"root=/dev/sda1 ro"' '"root=sda1"' \
                        '"root=UUID=AB-CD"' '"root=LABEL=cloudimg"' \
                        '"root=PARTUUID=12-34"' '"root=PARTUUID=99-99"' \
                        '"ro quiet"'; do
        ccrf_case '{}' '[]' \
            "\"mounts\": {\"/\": [\"/dev/root\", \"ext4\", \"/\", \"rw\"]}, \"stat\": {\"/dev/sda1\": \"0o60660\", \"/dev/disk/by-uuid/ab-cd\": \"0o60660\", \"/dev/disk/by-partuuid/12-34\": \"0o60660\"}, \"exists\": [\"/dev/disk/by-partuuid/12-34\"], \"cmdline\": $ccrf_cmdline, \"commands\": {\"resize2fs /dev/sda1\": {}, \"resize2fs /dev/disk/by-uuid/ab-cd\": {}, \"resize2fs /dev/disk/by-partuuid/12-34\": {}}"
        ccrf_case '{}' '[]' \
            "\"mounts\": {\"/\": [\"/dev/root\", \"ext4\", \"/\", \"rw\"]}, \"stat\": {\"/dev/root\": \"0o60660\"}, \"cmdline\": $ccrf_cmdline, \"container\": true"
    done
    # A PARTUUID with no link on disk, which blkid may still know.
    ccrf_case '{}' '[]' \
        '"mounts": {"/": ["/dev/root", "ext4", "/", "rw"]}, "stat": {"/dev/sdb1": "0o60660"}, "cmdline": "root=PARTUUID=99-99", "devs": {"PARTUUID=99-99": ["/dev/sdb1"]}, "commands": {"resize2fs /dev/sdb1": {}}'
    # /dev/root that is actually there, so the command line is never read.
    ccrf_case '{}' '[]' \
        '"mounts": {"/": ["/dev/root", "ext4", "/", "rw"]}, "exists": ["/dev/root"], "stat": {"/dev/root": "0o60660"}, "commands": {"resize2fs /dev/root": {}}'

    # The three names that are not paths: an overlay root, a FreeBSD gpt
    # label, and a bare gpart name that gets /dev/ prepended.
    ccrf_case '{}' '[]' '"mounts": {"/": ["overlayroot", "ext4", "/", "rw"]}'
    ccrf_case '{}' '[]' '"mounts": {"/": ["gpt/rootfs", "ufs", "/", "rw"]}, "commands": {"growfs -N gpt/rootfs": {"exit_code": 1, "stderr": "growfs: requested size 8.0GB is not larger than the current filesystem size 8.0GB\n"}}'
    ccrf_case '{}' '[]' '"mounts": {"/": ["da0p3", "ufs", "/", "rw"]}, "stat": {"/dev/da0p3": "0o60660"}, "commands": {"growfs -N /dev/da0p3": {}, "growfs -y /": {}}'
    ccrf_case '{}' '[]' '"mounts": {"/": ["da0p3", "ufs", "/", "rw"]}, "exists": ["da0p3"], "stat": {"da0p3": "0o60660"}, "commands": {"growfs -N da0p3": {}, "growfs -y /": {}}'

    # What stat said about it: gone, gone inside a container, a character
    # device (which is fine), and a plain file (which is not).
    ccrf_case '{}' '[]' '"mounts": {"/": ["/dev/sda1", "ext4", "/", "rw"]}'
    ccrf_case '{}' '[]' '"mounts": {"/": ["/dev/sda1", "ext4", "/", "rw"]}, "container": true'
    ccrf_case '{}' '[]' '"mounts": {"/": ["/dev/sr0", "ext4", "/", "rw"]}, "stat": {"/dev/sr0": "0o20660"}, "commands": {"resize2fs /dev/sr0": {}}'
    ccrf_case '{}' '[]' '"mounts": {"/": ["/dev/loop0", "ext4", "/", "rw"]}, "stat": {"/dev/loop0": "0o100644"}'
    ccrf_case '{}' '[]' '"mounts": {"/": ["/dev/loop0", "ext4", "/", "rw"]}, "stat": {"/dev/loop0": "0o100644"}, "container": true'

    # --- zfs ------------------------------------------------------------------
    # The mount table names a dataset, so the pool has to be found first and
    # is what gets resized.
    ccrf_zfs='"mounts": {"/": ["vmzroot/ROOT/freebsd", "zfs", "/", "rw"]}'
    ccrf_status='"zpool status vmzroot": {"stdout": "  pool: vmzroot\n state: ONLINE\nconfig:\n\n\tNAME        STATE\n\tvmzroot     ONLINE\n\t  da0p4     ONLINE\n"}'
    ccrf_case '{}' '[]' "$ccrf_zfs"
    ccrf_case '{}' '[]' "$ccrf_zfs, \"container\": true"
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\"]"
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\"], \"container\": true"
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\", \"/dev/da0p4\"], \"stat\": {\"da0p4\": \"0o60660\", \"/dev/da0p4\": \"0o60660\"}, \"commands\": {$ccrf_status, \"zpool online -e vmzroot da0p4\": {}, \"zpool online -e vmzroot /dev/da0p4\": {}}"
    # zpool status writing to stderr, and failing outright.
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\"], \"commands\": {\"zpool status vmzroot\": {\"stderr\": \"cannot open 'vmzroot': no such pool\n\"}}"
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\"], \"commands\": {\"zpool status vmzroot\": {\"exit_code\": 1, \"stderr\": \"no pool\n\"}}"
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\"], \"container\": true, \"commands\": {\"zpool status vmzroot\": {\"exit_code\": 1, \"stderr\": \"no pool\n\"}}"
    # A status listing with no ONLINE line, and one whose only ONLINE lines
    # are the two the scan skips.
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\"], \"commands\": {\"zpool status vmzroot\": {\"stdout\": \"  pool: vmzroot\n state: DEGRADED\n\"}}"
    ccrf_case '{}' '[]' "$ccrf_zfs, \"exists\": [\"/dev/zfs\"], \"commands\": {\"zpool status vmzroot\": {\"stdout\": \" state: ONLINE\n\tvmzroot     ONLINE\n\"}}"

    # --- ufs ------------------------------------------------------------------
    # growfs -N is the precheck: exit 1 with that exact wording means the
    # filesystem is already as big as its device, anything else is fatal.
    ccrf_ufs='"mounts": {"/": ["/dev/da0p3", "ufs", "/", "rw"]}, "stat": {"/dev/da0p3": "0o60660"}'
    ccrf_case '{}' '[]' "$ccrf_ufs, \"commands\": {\"growfs -N /dev/da0p3\": {\"exit_code\": 1, \"stderr\": \"growfs: requested size 8.0GB is not larger than the current filesystem size 8.0GB\n\"}}"
    ccrf_case '{}' '[]' "$ccrf_ufs, \"commands\": {\"growfs -N /dev/da0p3\": {\"exit_code\": 1, \"stderr\": \"growfs: cannot open /dev/da0p3\n\"}}"
    ccrf_case '{}' '[]' "$ccrf_ufs, \"commands\": {\"growfs -N /dev/da0p3\": {\"exit_code\": 1, \"stderr\": \"growfs: requested size 8.0GB is fine\n\"}}"
    ccrf_case '{}' '[]' "$ccrf_ufs, \"commands\": {\"growfs -N /dev/da0p3\": {}, \"growfs -y /\": {}}"
    ccrf_case '{}' '[]' "$ccrf_ufs, \"commands\": {\"growfs -N /dev/da0p3\": {}, \"growfs -y /\": {\"exit_code\": 1, \"stderr\": \"growfs: failed\n\"}}"

    # --- btrfs ----------------------------------------------------------------
    # Whether the mount is rw decides which subvolume is resized, and the
    # reported version decides whether the resize may be queued.
    ccrf_btrfs='"stat": {"/dev/sda1": "0o60660"}, "commands": {"btrfs --version": {"stdout": "btrfs-progs v6.2\n"}, "btrfs filesystem resize --enqueue max /": {}, "btrfs filesystem resize --enqueue max //.snapshots": {}, "btrfs filesystem resize max /": {}, "btrfs filesystem resize max //.snapshots": {}}'
    for ccrf_opts in '"rw,relatime"' '"ro,relatime"' '"rw"' '"ro"' '""' \
                     '"relatime,rw"'; do
        ccrf_case '{}' '[]' \
            "\"mounts\": {\"/\": [\"/dev/sda1\", \"btrfs\", \"/\", $ccrf_opts]}, $ccrf_btrfs"
        ccrf_case '{}' '[]' \
            "\"mounts\": {\"/\": [\"/dev/sda1\", \"btrfs\", \"/\", $ccrf_opts]}, \"dirs\": [\"//.snapshots\"], $ccrf_btrfs"
    done
    # Every shape `btrfs --version` can print, including the two that make
    # Version.from_str raise.
    for ccrf_ver in '"btrfs-progs v6.2\n"' '"btrfs-progs v5.10\n"' \
                    '"btrfs-progs v5.10.1\n"' '"btrfs-progs v5.9.9.9\n"' \
                    '"btrfs-progs v5.4.1\n"' '"btrfs-progs v4.20\n"' \
                    '"btrfs-progs v6.2\nextra\n"' '"  6.2  \n"' \
                    '"btrfs-progs v1.2.3.4.5\n"' '"btrfs-progs v6.x\n"' \
                    '"btrfs-progs\n"' '"\n"' '""'; do
        ccrf_case '{}' '[]' \
            "\"mounts\": {\"/\": [\"/dev/sda1\", \"btrfs\", \"/\", \"rw\"]}, \"stat\": {\"/dev/sda1\": \"0o60660\"}, \"commands\": {\"btrfs --version\": {\"stdout\": $ccrf_ver}, \"btrfs filesystem resize --enqueue max /\": {}, \"btrfs filesystem resize max /\": {}}"
    done
    # btrfs itself missing, which is fatal before anything is resized.
    ccrf_case '{}' '[]' \
        '"mounts": {"/": ["/dev/sda1", "btrfs", "/", "rw"]}, "stat": {"/dev/sda1": "0o60660"}'

    # --- the resize failing ---------------------------------------------------
    ccrf_case '{}' '[]' \
        "$ccrf_ext"', "commands": {"resize2fs /dev/sda1": {"exit_code": 1, "stderr": "resize2fs: Bad magic number in super-block\n"}}'
    ccrf_case '{"resize_rootfs": "noblock"}' '[]' \
        "$ccrf_ext"', "commands": {"resize2fs /dev/sda1": {"exit_code": 1, "stderr": "resize2fs: Bad magic number in super-block\n"}}'

    run_batch "cc_resizefs" \
        "cd /tmp && python3 '$CCRF_PY' --batch" \
        "cd /tmp && '$CCRF_RS' --batch" \
        "$WORK/ccrf.cases"
fi

# --- rsyslog -----------------------------------------------------------------
sec "cc-rsyslog"
#
# The whole module against a scripted machine: which programs `which` finds,
# which calls fail and how, and which paths refuse to be written. Nothing runs
# and nothing is written on either side -- the Python half stubs `subp.which`,
# `subp.subp`, `util.write_file`, the two distro methods and the two logging
# ones onto the same script -- so what is compared is the log, the ordered
# list of things the module did, and what each file ended up holding.
#
# The distro is Ubuntu on both sides. `DISTRO_OVERRIDES` is keyed on
# `distro.osfamily`, which the BSD classes take from `platform.system()`, so on
# Linux no override can fire for anyone.
CCRS_PY="$(cd "$(dirname "$0")" && pwd)/ccrsyslog.py"
CCRS_RS="$TARGET/examples/dump-cc-rsyslog"
if [ -x "$CCRS_RS" ] &&
   python3 -c 'import cloudinit.config.cc_rsyslog' 2>/dev/null; then
    CCRS_RS="$(cd "$(dirname "$CCRS_RS")" && pwd)/dump-cc-rsyslog"

    ccrs_case() {
        # `${2:-{}}` cannot be written in POSIX sh -- the brace closes the
        # expansion -- and a case that is not valid JSON would agree with
        # itself on both sides and pass. Hence the plain defaults, and the
        # check below.
        ccrs_si=${2:-}
        ccrs_host=${3:-}
        [ -n "$ccrs_si" ] || ccrs_si='{}'
        printf '{"cfg": %s, "system_info": %s, "host": {%s}}\n' \
            "$1" "$ccrs_si" "$ccrs_host" >>"$WORK/ccrs.cases"
    }
    # A config that reaches the end of the module every time.
    ccrs_one='{"rsyslog": {"configs": ["*.* @@syslog:514"]}}'

    : >"$WORK/ccrs.cases"

    # --- the switch -----------------------------------------------------------
    ccrs_case '{}'
    ccrs_case '{"rsyslog_filename": "x.conf"}'
    printf '{"name": "cc_rsyslog", "cfg": {}}\n' >>"$WORK/ccrs.cases"

    # --- what `rsyslog:` can be -----------------------------------------------
    # Only a mapping and a list are meant to work. The rest die on the first
    # `in` test, and a string dies one line later on the item assignment.
    for ccrs_v in 'null' '5' '5.5' 'true' '"hello"' '""' '[]' '{}'; do
        ccrs_case "{\"rsyslog\": $ccrs_v}"
    done
    # The deprecated list form, with and without the two keys that only it
    # reads.
    ccrs_case '{"rsyslog": ["*.* @@a", "*.* @@b"]}'
    ccrs_case '{"rsyslog": ["*.* @@a"], "rsyslog_filename": "99-x.conf", "rsyslog_dir": "/tmp/rs"}'
    ccrs_case '{"rsyslog": ["*.* @@a"], "rsyslog_filename": 5}'
    ccrs_case '{"rsyslog": ["*.* @@a"], "rsyslog_dir": null}'
    ccrs_case '{"rsyslog": [], "rsyslog_filename": "99-x.conf"}'
    ccrs_case '{"rsyslog": [{"content": "x"}, 5, null]}'

    # --- the eight fillup keys, each with a value of the wrong type -----------
    for ccrs_bad in \
        '"configs": "x"' '"configs": {}' '"configs": null' '"configs": true' \
        '"config_dir": 5' '"config_dir": []' '"config_dir": null' \
        '"config_filename": 5' '"config_filename": true' \
        '"remotes": []' '"remotes": "a"' '"remotes": null' \
        '"service_reload_command": 5' '"service_reload_command": {}' \
        '"service_reload_command": null' \
        '"check_exe": 5' '"packages": "rsyslog"' '"packages": null' \
        '"install_rsyslog": 1' '"install_rsyslog": 0' \
        '"install_rsyslog": "yes"' '"install_rsyslog": null'; do
        ccrs_case "{\"rsyslog\": {$ccrs_bad}}"
    done

    # --- configs: what each entry can be --------------------------------------
    for ccrs_entry in \
        '"*.* @@a"' \
        '""' \
        '"already ends in a newline\n"' \
        '{"content": "x"}' \
        '{"content": "x", "filename": "10-x.conf"}' \
        '{"content": "x", "filename": "  10-x.conf  "}' \
        '{"content": "x", "filename": ""}' \
        '{"content": "x", "filename": "   "}' \
        '{"content": "x", "filename": "/etc/rsyslog.conf"}' \
        '{"content": "x", "filename": "sub/dir/x.conf"}' \
        '{"filename": "10-x.conf"}' \
        '{}' \
        '{"content": 5}' \
        '{"content": null}' \
        '{"content": ["a"]}' \
        '{"content": "x", "filename": 5}' \
        '{"content": "x", "filename": null}' \
        '5' 'null' 'true' '["a"]' '{"content": "x", "extra": 1}'; do
        ccrs_case "{\"rsyslog\": {\"configs\": [$ccrs_entry]}}"
    done
    # Two entries naming one file: the first truncates, the second appends.
    ccrs_case '{"rsyslog": {"configs": [{"content": "one", "filename": "a.conf"}, {"content": "two", "filename": "a.conf"}, {"content": "three", "filename": "b.conf"}, {"content": "four", "filename": "a.conf"}]}}'
    # A config_dir that already ends in a slash, and an empty one.
    ccrs_case '{"rsyslog": {"configs": ["x"], "config_dir": "/etc/rsyslog.d/"}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "config_dir": ""}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "config_filename": "/abs.conf"}}'
    # The write itself failing, for the first of two entries and for both.
    ccrs_case '{"rsyslog": {"configs": [{"content": "one", "filename": "a.conf"}, {"content": "two", "filename": "b.conf"}]}}' \
        '{}' '"unwritable": {"/etc/rsyslog.d/a.conf": "[Errno 13] Permission denied"}'
    ccrs_case '{"rsyslog": {"configs": ["x"]}}' \
        '{}' '"unwritable": {"/etc/rsyslog.d/20-cloud-config.conf": "[Errno 30] Read-only file system"}'

    # --- remotes --------------------------------------------------------------
    # Every shape parse_remotes_line has an opinion about.
    for ccrs_remote in \
        '"192.168.1.1"' '"@192.168.1.1"' '"@@192.168.1.1"' \
        '"@@192.168.1.1:514"' '"192.168.1.1:514"' \
        '"*.* @@syslog"' '"kern.* @@syslog:514"' \
        '"[::1]"' '"[::1]:514"' '"@[fe80::1]:514"' '"@@[::1]"' \
        '"host # comment"' '"host#comment"' '"host  ##  comment"' \
        '"a b c"' '""' '" "' '":514"' '"@@:514"' '"[]"' '"@[]:514"' \
        '"[abc"' '"host:0"' '"host:00"' '"host:99999999999999999999"' \
        '"*.* @h # x # y"' '"h  # c"' '"  host  "' '"@"' '"@@"' \
        '"host:"' '"host:abc"' '"a:b:c"' '"[a:b]:1"' \
        '5' 'null' 'true' '[]' '{}' '0'; do
        ccrs_case "{\"rsyslog\": {\"remotes\": {\"maas\": $ccrs_remote}}}"
    done
    # Several at once, so the order and the header/footer are pinned, and one
    # that is only remotes so the appended entry is the only config.
    ccrs_case '{"rsyslog": {"remotes": {"maas": "@@10.0.0.1:514", "juju": "10.0.0.2", "bad": "a b c", "empty": ""}}}'
    ccrs_case '{"rsyslog": {"configs": ["local"], "remotes": {"maas": "@@10.0.0.1"}}}'
    ccrs_case '{"rsyslog": {"remotes": {"empty": ""}}}'
    ccrs_case '{"rsyslog": {"remotes": {}, "configs": ["x"]}}'

    # --- installing the package -----------------------------------------------
    ccrs_case "$ccrs_one"
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": false}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": true}}' \
        '{}' '"present": ["rsyslogd"]'
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": true}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": true, "check_exe": "syslog-ng"}}' \
        '{}' '"present": ["syslog-ng"]'
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": true, "packages": ["rsyslog", "rsyslog-gnutls"]}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": true, "packages": []}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": true, "packages": [5, null]}}'
    # The install failing, which ends the module before anything is written.
    ccrs_case '{"rsyslog": {"configs": ["x"], "install_rsyslog": true}}' \
        '{}' '"failures": {"install_packages ['"'"'rsyslog'"'"']": {"exit_code": 100, "stderr": "E: Unable to locate package rsyslog\n"}}'
    # An empty config still installs: the install happens before the check.
    ccrs_case '{"rsyslog": {"configs": [], "install_rsyslog": true}}'

    # --- reloading ------------------------------------------------------------
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": "auto"}}'
    ccrs_case "$ccrs_one" '{"rsyslog_svcname": "syslog"}'
    ccrs_case "$ccrs_one" '{"rsyslog_svcname": 5}'
    ccrs_case "$ccrs_one" '{"rsyslog_svcname": null}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": ["systemctl", "restart", "rsyslog"]}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": []}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": [5, null]}}'
    # A string that is not "auto" is one argv element, not a command line.
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": "systemctl restart rsyslog"}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": "true"}}'
    # The reload failing, both ways.
    ccrs_case "$ccrs_one" '{}' \
        '"failures": {"manage_service try-reload rsyslog": {"command": "['"'"'systemctl'"'"', '"'"'try-reload-or-restart'"'"', '"'"'rsyslog'"'"']", "exit_code": 5, "stderr": "Failed to try-reload-or-restart rsyslog.service: Unit not found.\n"}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": ["systemctl", "restart", "rsyslog"]}}' \
        '{}' '"failures": {"subp ['"'"'systemctl'"'"', '"'"'restart'"'"', '"'"'rsyslog'"'"']": {"exit_code": 1, "stderr": "no\n"}}'
    ccrs_case '{"rsyslog": {"configs": ["x"], "service_reload_command": "reload-rsyslog"}}' \
        '{}' '"failures": {"subp reload-rsyslog": {"command": "reload-rsyslog", "stderr": "[Errno 2] No such file or directory: b'"'"'reload-rsyslog'"'"'\n"}}'

    run_batch "cc_rsyslog" \
        "cd /tmp && python3 '$CCRS_PY' --batch" \
        "cd /tmp && '$CCRS_RS' --batch" \
        "$WORK/ccrs.cases"
fi

# --- keys_to_console ---------------------------------------------------------
sec "cc-keys-to-console"
#
# The module runs one helper script and puts what it printed on the console.
# Both sides script the helper rather than running it, so what is compared is
# whether it was looked for, what argv it was given, and what reached the
# console.
CCKC_PY="$(cd "$(dirname "$0")" && pwd)/cckeystoconsole.py"
CCKC_RS="$TARGET/examples/dump-cc-keys-to-console"
if [ -x "$CCKC_RS" ] &&
   python3 -c 'import cloudinit.config.cc_keys_to_console' 2>/dev/null; then
    CCKC_RS="$(cd "$(dirname "$CCKC_RS")" && pwd)/dump-cc-keys-to-console"
    CCKC_HELPER=/usr/lib/cloud-init/write-ssh-key-fingerprints

    cckc_case() {
        cckc_ule=${2:-/usr/lib}
        cckc_host=${3:-}
        printf '{"cfg": %s, "usr_lib_exec": "%s", "host": {%s}}\n' \
            "$1" "$cckc_ule" "$cckc_host" >>"$WORK/cckc.cases"
    }
    # The helper present and printing the two blocks it prints for real.
    cckc_out='-----BEGIN SSH HOST KEY KEYS-----\nssh-ed25519 AAAAC3Nz root@h\nssh-rsa AAAAB3Nz root@h\n-----END SSH HOST KEY KEYS-----\n'
    cckc_ok="\"present\": [\"$CCKC_HELPER\"], \"commands\": {\"$CCKC_HELPER  \": \"$cckc_out\"}"

    : >"$WORK/cckc.cases"

    # --- the switch -----------------------------------------------------------
    # `util.is_false` is not the negation of `is_true`: only the four false
    # strings and the bool stop the module.
    for cckc_v in 'true' 'false' '"false"' '"False"' '"FALSE"' '" false "' \
                  '0' '"0"' '1' '"no"' '"off"' '"OFF"' '"yes"' '"maybe"' \
                  'null' '""' '[]' '{}' '2' '1.0' '0.0'; do
        cckc_case "{\"ssh\": {\"emit_keys_to_console\": $cckc_v}}" \
            /usr/lib "$cckc_ok"
    done
    # The key absent at each level, and an `ssh:` that has no `.get`.
    cckc_case '{}' /usr/lib "$cckc_ok"
    cckc_case '{"ssh": {}}' /usr/lib "$cckc_ok"
    for cckc_ssh in 'null' '"yes"' '5' 'true' '[]' '[{"a": 1}]'; do
        cckc_case "{\"ssh\": $cckc_ssh}" /usr/lib "$cckc_ok"
    done
    printf '{"name": "cc_keys_to_console", "cfg": {"ssh": {"emit_keys_to_console": false}}}\n' \
        >>"$WORK/cckc.cases"

    # --- where the helper is looked for ---------------------------------------
    cckc_case '{}'
    cckc_case '{}' /usr/libexec
    cckc_case '{}' ''
    cckc_case '{}' /usr/libexec \
        "\"present\": [\"/usr/libexec/cloud-init/write-ssh-key-fingerprints\"], \"commands\": {\"/usr/libexec/cloud-init/write-ssh-key-fingerprints  \": \"k\n\"}"

    # --- the two blacklists ---------------------------------------------------
    # `get_cfg_option_list` turns a bare string into a one-element list and
    # `str()`s anything that is not one, then `",".join` sees the result.
    for cckc_fp in '["ssh-dss"]' '["ssh-dss", "ecdsa-sha2-nistp256"]' \
                   '"ssh-dss"' '[]' 'null' '5' '["a", 5, null, true]' \
                   '[["a"]]' '{"a": 1}' '""' '[""]' '["a,b"]'; do
        cckc_case "{\"ssh_fp_console_blacklist\": $cckc_fp}" /usr/lib \
            "\"present\": [\"$CCKC_HELPER\"]"
    done
    cckc_case '{"ssh_key_console_blacklist": ["ssh-dss"], "ssh_fp_console_blacklist": ["ssh-rsa"]}' \
        /usr/lib "\"present\": [\"$CCKC_HELPER\"], \"commands\": {\"$CCKC_HELPER ssh-rsa ssh-dss\": \"k\n\"}"

    # --- what the helper printed ----------------------------------------------
    for cckc_stdout in '""' '"\n"' '"   \n\n"' '"one line"' \
                       '"trailing space   "' '"\ttabbed\t"' \
                       '"a\nb\nc\n"' '"\n\nleading"'; do
        cckc_case '{}' /usr/lib \
            "\"present\": [\"$CCKC_HELPER\"], \"commands\": {\"$CCKC_HELPER  \": $cckc_stdout}"
    done
    # The helper failing, which is logged and then re-raised.
    cckc_case '{}' /usr/lib \
        "\"present\": [\"$CCKC_HELPER\"], \"commands\": {\"$CCKC_HELPER  \": {\"exit_code\": 1, \"stderr\": \"ssh-keygen: not found\n\"}}"
    cckc_case '{}' /usr/lib "\"present\": [\"$CCKC_HELPER\"]"

    run_batch "cc_keys_to_console" \
        "cd /tmp && python3 '$CCKC_PY' --batch" \
        "cd /tmp && '$CCKC_RS' --batch" \
        "$WORK/cckc.cases"
fi

# --- ssh_import_id -----------------------------------------------------------
sec "cc-ssh-import-id"
#
# One `ssh-import-id` run per user that asked for one. Both sides are handed
# the same already-normalized user map -- `normalize_users_groups` has its own
# section -- and script `which`, `getpwnam` and the command itself, so what is
# compared is which users were tried, in which order, and with what argv.
CCSI_PY="$(cd "$(dirname "$0")" && pwd)/ccsshimportid.py"
CCSI_RS="$TARGET/examples/dump-cc-ssh-import-id"
if [ -x "$CCSI_RS" ] &&
   python3 -c 'import cloudinit.config.cc_ssh_import_id' 2>/dev/null; then
    CCSI_RS="$(cd "$(dirname "$CCSI_RS")" && pwd)/dump-cc-ssh-import-id"

    ccsi_case() {
        ccsi_users=${2:-}
        ccsi_args=${3:-}
        ccsi_host=${4:-}
        [ -n "$ccsi_users" ] || ccsi_users='{}'
        [ -n "$ccsi_args" ] || ccsi_args='[]'
        printf '{"cfg": %s, "users": %s, "args": %s, "host": {%s}}\n' \
            "$1" "$ccsi_users" "$ccsi_args" "$ccsi_host" \
            >>"$WORK/ccsi.cases"
    }
    # A machine with everything the module needs.
    ccsi_ok='"present": ["ssh-import-id", "sudo"], "users": ["ubuntu", "alice", "bob"]'
    ccsi_default='{"ubuntu": {"default": true}}'

    : >"$WORK/ccsi.cases"

    # --- the switch -----------------------------------------------------------
    # `is_key_in_nested_dict` walks mappings and the mappings directly inside
    # lists, and nothing else.
    for ccsi_cfg in \
        '{}' \
        '{"ssh_import_id": ["lp:alice"]}' \
        '{"users": [{"name": "alice", "ssh_import_id": ["lp:alice"]}]}' \
        '{"users": [[{"ssh_import_id": ["lp:alice"]}]]}' \
        '{"a": {"b": {"ssh_import_id": ["x"]}}}' \
        '{"a": [1, 2, {"ssh_import_id": ["x"]}]}' \
        '{"a": ["ssh_import_id"]}' \
        '{"ssh_import_id": null}' \
        '{"ssh_import_id": []}'; do
        ccsi_case "$ccsi_cfg" "$ccsi_default" '[]' "$ccsi_ok"
    done
    # The binary missing, which is a warning and nothing else.
    ccsi_case '{"ssh_import_id": ["lp:alice"]}' "$ccsi_default" '[]' \
        '"users": ["ubuntu"]'

    # --- the run arguments ----------------------------------------------------
    # `args[0]` is the user and the rest are the ids; the user map is ignored.
    for ccsi_args in \
        '["alice", "lp:alice"]' \
        '["alice", "lp:alice", "gh:alice"]' \
        '["alice"]' \
        '["ghost", "lp:x"]' \
        '[""]' \
        '["", "lp:x"]' \
        '[5, 6]' \
        '[null]' \
        '[true, "lp:x"]'; do
        ccsi_case '{"ssh_import_id": ["ignored"]}' "$ccsi_default" \
            "$ccsi_args" "$ccsi_ok"
    done

    # --- which users are tried ------------------------------------------------
    # The default user takes the top-level key; everyone else takes their own.
    # `user_cfg["default"]` is an index rather than a `.get`, so a map without
    # the key ends the module -- `normalize_users_groups` always sets it, so
    # only a hand-built map gets there.
    for ccsi_users in \
        '{"ubuntu": {"default": true}}' \
        '{"ubuntu": {"default": false}}' \
        '{"ubuntu": {}}' \
        '{"ubuntu": {"default": null}}' \
        '{"ubuntu": {"default": 0}}' \
        '{"ubuntu": {"default": "no"}}' \
        '{"ubuntu": "notadict"}' \
        '{"ubuntu": {"default": true}, "alice": {"default": false, "ssh_import_id": ["lp:alice"]}}' \
        '{"alice": {"default": false, "ssh_import_id": ["lp:alice"]}, "bob": {"default": false, "ssh_import_id": ["gh:bob"]}}' \
        '{"alice": {"default": false, "ssh_import_id": []}}' \
        '{"alice": {"default": false, "ssh_import_id": null}}' \
        '{"alice": {"default": false, "ssh_import_id": ""}}' \
        '{"alice": {"default": false, "ssh_import_id": "lp:alice"}}' \
        '{"alice": {"default": false, "ssh_import_id": "lp:a, gh:b"}}' \
        '{"alice": {"default": false, "ssh_import_id": ["lp:a", "lp:a", "lp:b"]}}' \
        '{"alice": {"default": false, "ssh_import_id": [5, null, true]}}' \
        '{"alice": {"default": false, "ssh_import_id": 5}}' \
        '{"alice": {"default": false, "ssh_import_id": {"lp:a": 1}}}' \
        '{"alice": {"default": false, "ssh_import_id": true}}' \
        '{"alice": {"default": false}}' \
        '{"ghost": {"default": false, "ssh_import_id": ["lp:x"]}}' \
        '{}'; do
        ccsi_case '{"ssh_import_id": ["lp:top"]}' "$ccsi_users" '[]' \
            "$ccsi_ok"
    done
    # The top-level key in every shape the default user can read it in.
    for ccsi_top in '["lp:a"]' '"lp:a"' '"lp:a,gh:b"' '"lp:a, gh:b"' \
                    '[]' 'null' '5' '{"lp:a": 1}' '["lp:a", "lp:a"]' \
                    '[5, null]' 'true'; do
        ccsi_case "{\"ssh_import_id\": $ccsi_top}" "$ccsi_default" '[]' \
            "$ccsi_ok"
    done

    # --- which privilege tool -------------------------------------------------
    ccsi_case '{"ssh_import_id": ["lp:a"]}' "$ccsi_default" '[]' \
        '"present": ["ssh-import-id", "sudo", "doas"], "users": ["ubuntu"]'
    ccsi_case '{"ssh_import_id": ["lp:a"]}' "$ccsi_default" '[]' \
        '"present": ["ssh-import-id", "doas"], "users": ["ubuntu"]'
    ccsi_case '{"ssh_import_id": ["lp:a"]}' "$ccsi_default" '[]' \
        '"present": ["ssh-import-id"], "users": ["ubuntu"]'

    # --- failures -------------------------------------------------------------
    # One user failing is logged and collected; the *first* error is the one
    # re-raised after every user has been tried.
    ccsi_pair='{"alice": {"default": false, "ssh_import_id": ["lp:a"]}, "bob": {"default": false, "ssh_import_id": ["gh:b"]}}'
    ccsi_case '{"ssh_import_id": ["lp:top"]}' "$ccsi_pair" '[]' \
        "$ccsi_ok"', "failures": {"sudo --preserve-env=https_proxy -Hu alice ssh-import-id lp:a": {"exit_code": 1}}'
    ccsi_case '{"ssh_import_id": ["lp:top"]}' "$ccsi_pair" '[]' \
        "$ccsi_ok"', "failures": {"sudo --preserve-env=https_proxy -Hu alice ssh-import-id lp:a": {"exit_code": 1}, "sudo --preserve-env=https_proxy -Hu bob ssh-import-id gh:b": {"exit_code": 2}}'
    # A user that does not exist ends the module for everyone.
    ccsi_case '{"ssh_import_id": ["lp:top"]}' \
        '{"ghost": {"default": false, "ssh_import_id": ["lp:x"]}, "alice": {"default": false, "ssh_import_id": ["lp:a"]}}' \
        '[]' "$ccsi_ok"

    run_batch "cc_ssh_import_id" \
        "cd /tmp && python3 '$CCSI_PY' --batch" \
        "cd /tmp && '$CCSI_RS' --batch" \
        "$WORK/ccsi.cases"
fi

# --- ssh_authkey_fingerprints ------------------------------------------------
sec "cc-ssh-authkey-fp"
#
# The module is nothing but formatting, so the console text is the test: the
# box is drawn to the width of its widest column, and the two centring rules
# involved disagree about which side an odd space goes on. Both sides script
# `extract_authorized_keys` and collect `multi_log` instead of writing to a
# console.
CCAF_PY="$(cd "$(dirname "$0")" && pwd)/ccsshauthkeyfp.py"
CCAF_RS="$TARGET/examples/dump-cc-ssh-authkey-fp"
if [ -x "$CCAF_RS" ] &&
   python3 -c 'import cloudinit.config.cc_ssh_authkey_fingerprints' \
       2>/dev/null; then
    CCAF_RS="$(cd "$(dirname "$CCAF_RS")" && pwd)/dump-cc-ssh-authkey-fp"

    ccaf_case() {
        ccaf_users=${2:-}
        ccaf_host=${3:-}
        [ -n "$ccaf_users" ] || ccaf_users='{}'
        printf '{"cfg": %s, "users": %s, "host": {%s}}\n' \
            "$1" "$ccaf_users" "$ccaf_host" >>"$WORK/ccaf.cases"
    }
    # Real key material, so the fingerprints are the ones a machine prints.
    ccaf_rsa='AAAAB3NzaC1yc2EAAAADAQABAAABgQDLQZDcpXhBpJJTBpcJZLRAyD6VwBrEEHVMfJHTGYPjCEbEZlPBQEmvNgBcAlEIu4A2xLnXpDDUFPUUXWZUFnJl'
    ccaf_ed='AAAAC3NzaC1lZDI1NTE5AAAAIN9zqYVrzKtNBUWSs7QSbaBGhVfZ0FoDdPX8Kj7lWJqf'

    : >"$WORK/ccaf.cases"

    # --- the switch -----------------------------------------------------------
    # `util.is_true`, so only the true spellings stop the module.
    for ccaf_v in 'true' 'false' '"true"' '"True"' '"1"' '1' '0' '"yes"' \
                  '"on"' '"no"' '"off"' 'null' '""' '[]' '{}' '2' '"maybe"'; do
        ccaf_case "{\"no_ssh_fingerprints\": $ccaf_v}" \
            '{"ubuntu": {}}' \
            "\"keys\": {\"ubuntu\": [\"/home/ubuntu/.ssh/authorized_keys\", [[\"ssh-rsa\", \"$ccaf_rsa\", \"root@h\", \"\"]]]}"
    done
    printf '{"name": "cc_ssh_authkey_fingerprints", "cfg": {"no_ssh_fingerprints": true}, "users": {"u": {}}}\n' \
        >>"$WORK/ccaf.cases"

    # --- which users are looked at --------------------------------------------
    for ccaf_users in \
        '{"ubuntu": {}}' \
        '{"ubuntu": {"system": true}}' \
        '{"ubuntu": {"system": false}}' \
        '{"ubuntu": {"no_create_home": true}}' \
        '{"ubuntu": {"no_create_home": false}}' \
        '{"ubuntu": {"system": null}}' \
        '{"ubuntu": {"system": "yes"}}' \
        '{"ubuntu": {"system": ""}}' \
        '{"ubuntu": {"system": 0}}' \
        '{"a": {}, "b": {"system": true}, "c": {}}' \
        '{}'; do
        ccaf_case '{}' "$ccaf_users" \
            "\"keys\": {\"ubuntu\": [\"/home/ubuntu/.ssh/authorized_keys\", [[\"ssh-rsa\", \"$ccaf_rsa\", \"root@h\", \"\"]]], \"a\": [\"/home/a/.ssh/authorized_keys\", [[\"ssh-ed25519\", \"$ccaf_ed\", \"a@h\", \"\"]]], \"c\": [\"/home/c/.ssh/authorized_keys\", []]}"
    done

    # --- what the table has to render -----------------------------------------
    for ccaf_entry in \
        "[\"ssh-rsa\", \"$ccaf_rsa\", \"root@h\", \"\"]" \
        "[\"ssh-ed25519\", \"$ccaf_ed\", \"\", \"\"]" \
        "[\"ssh-ed25519\", \"$ccaf_ed\", \"a@h\", \"no-port-forwarding\"]" \
        "[\"ssh-ed25519\", \"$ccaf_ed\", \"a@h\", \"command=\\\"/bin/true\\\",no-pty\"]" \
        '["ssh-bogus", "AAAA", "x", ""]' \
        '["", "", "", ""]' \
        '["", "AAAA", "c", "o"]' \
        '["SSH-RSA", "AAAA", "", ""]' \
        '["  ssh-rsa  ", "AAAA", "", ""]' \
        '["ssh-rsa", "", "c", ""]' \
        '["ssh-rsa", "A", "c", ""]' \
        '["ssh-rsa", "AAA", "c", ""]' \
        '["ssh-rsa", "AAA=", "c", ""]' \
        '["ssh-rsa", "AA==", "c", ""]' \
        '["ssh-rsa", "AAAA=", "c", ""]' \
        '["ssh-rsa", "AA=A", "c", ""]' \
        '["ssh-rsa", "!!!not base64!!!", "c", ""]' \
        '["ssh-rsa", "AAAA", "a much longer comment than the column needs", ""]' \
        '["ecdsa-sha2-nistp256", "AAAA", "e", ""]' \
        '["sk-ssh-ed25519@openssh.com", "AAAA", "s", ""]'; do
        ccaf_case '{}' '{"ubuntu": {}}' \
            "\"keys\": {\"ubuntu\": [\"/home/ubuntu/.ssh/authorized_keys\", [$ccaf_entry]]}"
    done
    # Several rows at once, so the column widths are driven by the widest.
    ccaf_case '{}' '{"ubuntu": {}}' \
        "\"keys\": {\"ubuntu\": [\"/home/ubuntu/.ssh/authorized_keys\", [[\"ssh-rsa\", \"$ccaf_rsa\", \"root@h\", \"\"], [\"ssh-ed25519\", \"$ccaf_ed\", \"a-much-longer-comment\", \"no-pty\"], [\"ssh-bogus\", \"AAAA\", \"skipped\", \"\"]]]}"
    # A user whose key file is named but empty, and one with no file at all.
    ccaf_case '{}' '{"ubuntu": {}}' \
        '"keys": {"ubuntu": ["/home/ubuntu/.ssh/authorized_keys", []]}'
    ccaf_case '{}' '{"ubuntu": {}}'
    # A very short path, so the title is *wider* than the table and the
    # centring has nothing to do.
    ccaf_case '{}' '{"u": {}}' \
        "\"keys\": {\"u\": [\"/k\", [[\"ssh-rsa\", \"AAAA\", \"c\", \"\"]]]}"

    # --- the hash the fingerprint is taken with -------------------------------
    for ccaf_hash in '"sha256"' '"sha1"' '"md5"' '"SHA256"' '"nosuchhash"' \
                     '""' '5' 'null' 'true'; do
        ccaf_case "{\"authkey_hash\": $ccaf_hash}" '{"ubuntu": {}}' \
            "\"keys\": {\"ubuntu\": [\"/home/ubuntu/.ssh/authorized_keys\", [[\"ssh-rsa\", \"$ccaf_rsa\", \"root@h\", \"\"]]]}"
    done

    run_batch "cc_ssh_authkey_fingerprints" \
        "cd /tmp && python3 '$CCAF_PY' --batch" \
        "cd /tmp && '$CCAF_RS' --batch" \
        "$WORK/ccaf.cases"
fi

# --- seedfrom ----------------------------------------------------------------
sec "seedfrom"
# `util.read_seeded` builds four URLs from one base and fetches them in a fixed
# order. Both sides run with `timeout=1, retries=0` so a miss fails promptly.
SEED_PY="$(cd "$(dirname "$0")" && pwd)/seed.py"
SEED_RS="$TARGET/examples/dump-seed"
if [ -x "$SEED_RS" ]; then
    SEED_RS="$(cd "$(dirname "$SEED_RS")" && pwd)/dump-seed"
    SEED="$WORK/seed"
    mkdir -p "$SEED/root/full" "$SEED/root/empty" "$SEED/min"
    printf 'instance-id: iid-a\nlocal-hostname: h1\n' >"$SEED/root/full/meta-data"
    printf '#cloud-config\nruncmd: [a]\n' >"$SEED/root/full/user-data"
    printf '#!/bin/sh\necho v\n' >"$SEED/root/full/vendor-data"
    printf 'version: 2\nethernets: {eth0: {dhcp4: true}}\n' \
        >"$SEED/root/full/network-config"
    printf 'instance-id: iid-b\n' >"$SEED/min/meta-data"
    : >"$SEED/min/user-data"
    # meta-data that is not a mapping falls back to `{}` on both sides.
    mkdir -p "$SEED/scalar"
    printf 'just a string\n' >"$SEED/scalar/meta-data"
    : >"$SEED/scalar/user-data"

    for base in "$SEED/root/full" "$SEED/root/full/" "file://$SEED/root/full" \
        "$SEED/min" "$SEED/scalar" "$SEED/absent" "$SEED/root/%s/meta-data" \
        "$SEED/root/full?q=1"; do
        run_pair "seedfrom $(echo "$base" | sed "s|$SEED|SEED|")" \
            "cd /tmp && python3 '$SEED_PY' '$base'" \
            "cd /tmp && '$SEED_RS' '$base'"
    done

    python3 "$(dirname "$0")/httpd.py" "$SEED/root" >"$SEED/port" 2>/dev/null &
    seed_pid=$!
    seed_port=""
    tries=0
    while [ -z "$seed_port" ] && [ "$tries" -lt 50 ]; do
        seed_port=$(cat "$SEED/port" 2>/dev/null || true)
        [ -n "$seed_port" ] || sleep 0.1
        tries=$((tries + 1))
    done
    if [ -n "$seed_port" ]; then
        root="http://127.0.0.1:$seed_port"
        # `/full` without its slash gets one appended; `/full/?q=1` does not,
        # because a query string suppresses the append -- so the document name
        # lands inside the query and the server answers with a directory index.
        for base in "$root/full" "$root/full/" "$root/full/%s" "$root/empty/" \
            "$root/full/?q=1" "$root/missing/"; do
            run_pair "seedfrom $(echo "$base" | sed "s|$root|HTTP|")" \
                "cd /tmp && python3 '$SEED_PY' '$base'" \
                "cd /tmp && '$SEED_RS' '$base'"
        done
    else
        printf 'skip seedfrom over http (no server)\n'
    fi
    kill "$seed_pid" 2>/dev/null || true
    wait "$seed_pid" 2>/dev/null || true
fi

# --- config drive -----------------------------------------------------------
sec "config-drive"
# `read_config_drive` tries the v2 layout and falls back to v1. Both readers
# are pure filesystem walks, so every case here is a directory tree.
CD_PY="$(cd "$(dirname "$0")" && pwd)/configdrive.py"
CD_RS="$TARGET/examples/dump-configdrive"
if [ -x "$CD_RS" ]; then
    CD_RS="$(cd "$(dirname "$CD_RS")" && pwd)/dump-configdrive"
    CD="$WORK/configdrive"

    # A full v2 drive: every optional document, an injected file, an eni
    # pointer and an ec2 block.
    mkdir -p "$CD/v2/openstack/2018-08-27" "$CD/v2/openstack/content" \
        "$CD/v2/ec2/latest"
    cat >"$CD/v2/openstack/2018-08-27/meta_data.json" <<'EOF'
{"uuid": "i-abc", "hostname": "host1", "name": "vm1",
 "random_seed": "c2VlZC12YWx1ZQ==",
 "meta": {"dsmode": "local"},
 "public_keys": {"mykey": "ssh-rsa AAAA"},
 "files": [{"path": "/etc/motd", "content_path": "/content/0000"}],
 "network_config": {"content_path": "/content/0001"}}
EOF
    printf '#cloud-config\nruncmd: [ echo hi ]\n' \
        >"$CD/v2/openstack/2018-08-27/user_data"
    printf '{"cloud-init": "#!/bin/sh\\necho v\\n"}' \
        >"$CD/v2/openstack/2018-08-27/vendor_data.json"
    printf '["a", "b"]' >"$CD/v2/openstack/2018-08-27/vendor_data2.json"
    printf '{"links": [], "networks": [], "services": []}' \
        >"$CD/v2/openstack/2018-08-27/network_data.json"
    printf 'motd body\n' >"$CD/v2/openstack/content/0000"
    printf 'auto lo\niface lo inet loopback\n' >"$CD/v2/openstack/content/0001"
    printf '{"instance-id": "i-abc", "ami-id": "ami-1"}' \
        >"$CD/v2/ec2/latest/meta-data.json"

    # The oldest and newest version directories side by side: the newest wins.
    mkdir -p "$CD/versions/openstack/2012-08-10" \
        "$CD/versions/openstack/2016-10-06" "$CD/versions/openstack/latest"
    printf '{"uuid": "old"}' \
        >"$CD/versions/openstack/2012-08-10/meta_data.json"
    printf '{"uuid": "new"}' \
        >"$CD/versions/openstack/2016-10-06/meta_data.json"
    printf '{"uuid": "latest"}' >"$CD/versions/openstack/latest/meta_data.json"

    # Only an unrecognised version directory: neither side matches it, so both
    # fall through to `latest`, which is absent, and the drive reads as v1.
    mkdir -p "$CD/unknown/openstack/1999-01-01"
    printf '{"uuid": "u"}' >"$CD/unknown/openstack/1999-01-01/meta_data.json"

    # A v2 drive missing the mandatory `uuid` rename source.
    mkdir -p "$CD/nouuid/openstack/2018-08-27"
    printf '{"hostname": "h"}' >"$CD/nouuid/openstack/2018-08-27/meta_data.json"

    # Malformed JSON, and a root that is a list rather than a mapping.
    mkdir -p "$CD/badjson/openstack/2018-08-27" "$CD/list/openstack/2018-08-27"
    printf '{not json' >"$CD/badjson/openstack/2018-08-27/meta_data.json"
    printf '["a"]' >"$CD/list/openstack/2018-08-27/meta_data.json"

    # A content_path pointing at a file that is not there.
    mkdir -p "$CD/badcontent/openstack/2018-08-27"
    cat >"$CD/badcontent/openstack/2018-08-27/meta_data.json" <<'EOF'
{"uuid": "i-abc", "files": [{"path": "/etc/motd", "content_path": "/gone"}]}
EOF

    # v1: meta.js carries everything. An injected `authorized_keys` file is
    # deliberately absent here because reading one raises upstream (bug B40).
    mkdir -p "$CD/v1/etc/network"
    printf '{"instance-id": "i-v1", "user-data": "#!/bin/sh\\n", "dsmode": "local"}' \
        >"$CD/v1/meta.js"
    printf 'auto eth0\niface eth0 inet dhcp\n' >"$CD/v1/etc/network/interfaces"

    # v1 with keys in meta.js, which outrank the injected file -- and, because
    # they outrank it, keep upstream off the code path that raises.
    mkdir -p "$CD/v1keys/root/.ssh"
    printf '{"public-keys": "ssh-rsa META key1\\n# comment\\n\\nssh-rsa META key2\\n"}' \
        >"$CD/v1keys/meta.js"
    printf 'ssh-rsa INJECTED\n' >"$CD/v1keys/root/.ssh/authorized_keys"

    # Neither layout.
    mkdir -p "$CD/empty"

    # Parser and errno wording is implementation-specific; the class, the
    # path and the fact that it failed are what the port has to match.
    cd_trim="sed -e 's|\\(\\.json\\): .*|\\1: <detail>|' \
        -e 's|\\(provided file [^:]*\\): .*|\\1: <detail>|'"

    for case in v2 versions unknown nouuid badjson list badcontent v1 v1keys \
        empty absent; do
        run_pair "configdrive $case" \
            "cd /tmp && python3 '$CD_PY' '$CD/$case' | $cd_trim" \
            "cd /tmp && '$CD_RS' '$CD/$case' | $cd_trim"
    done
fi

# --- openstack metadata service ---------------------------------------------
sec "openstack-metadata-service"
# The same `read_v2` walk as the config drive, but over HTTP, so every case
# here is a directory tree served by `mdhttpd.py`.
OS_PY="$(cd "$(dirname "$0")" && pwd)/openstack.py"
OS_RS="$TARGET/examples/dump-openstack"
if [ -x "$OS_RS" ]; then
    OS_RS="$(cd "$(dirname "$OS_RS")" && pwd)/dump-openstack"
    OS="$WORK/openstack"

    # A full service: every optional document, an injected file and an eni
    # pointer. No `ec2` block -- upstream crawls the EC2 tree for that and
    # the port does not (deviation 73), so both sides print `<none>`.
    mkdir -p "$OS/full/openstack/2018-08-27" "$OS/full/openstack/content"
    cat >"$OS/full/openstack/2018-08-27/meta_data.json" <<'EOF'
{"uuid": "i-os", "hostname": "oshost", "name": "osvm",
 "random_seed": "c2VlZC12YWx1ZQ==",
 "meta": {"dsmode": "net"},
 "public_keys": {"mykey": "ssh-rsa AAAA"},
 "files": [{"path": "/etc/motd", "content_path": "/content/0000"}],
 "network_config": {"content_path": "/content/0001"}}
EOF
    printf '#cloud-config\nruncmd: [ echo os ]\n' \
        >"$OS/full/openstack/2018-08-27/user_data"
    printf '{"cloud-init": "#!/bin/sh\\necho v\\n"}' \
        >"$OS/full/openstack/2018-08-27/vendor_data.json"
    printf '["a", "b"]' >"$OS/full/openstack/2018-08-27/vendor_data2.json"
    printf '{"links": [], "networks": [], "services": []}' \
        >"$OS/full/openstack/2018-08-27/network_data.json"
    printf 'motd body\n' >"$OS/full/openstack/content/0000"
    printf 'auto lo\niface lo inet loopback\n' >"$OS/full/openstack/content/0001"

    # Only `meta_data.json`: every optional document 404s.
    mkdir -p "$OS/min/openstack/2016-10-06"
    printf '{"uuid": "i-min"}' >"$OS/min/openstack/2016-10-06/meta_data.json"

    # Several versions advertised at once: the newest known one wins.
    mkdir -p "$OS/versions/openstack/2012-08-10" \
        "$OS/versions/openstack/2016-10-06" "$OS/versions/openstack/latest"
    printf '{"uuid": "old"}' \
        >"$OS/versions/openstack/2012-08-10/meta_data.json"
    printf '{"uuid": "new"}' \
        >"$OS/versions/openstack/2016-10-06/meta_data.json"
    printf '{"uuid": "latest"}' >"$OS/versions/openstack/latest/meta_data.json"

    # No `uuid` to rename, malformed JSON, and a list at the root.
    mkdir -p "$OS/nouuid/openstack/2018-08-27" \
        "$OS/badjson/openstack/2018-08-27" "$OS/list/openstack/2018-08-27"
    printf '{"hostname": "h"}' >"$OS/nouuid/openstack/2018-08-27/meta_data.json"
    printf '{not json' >"$OS/badjson/openstack/2018-08-27/meta_data.json"
    printf '["a"]' >"$OS/list/openstack/2018-08-27/meta_data.json"

    # A content_path pointing at a document the service will not serve.
    mkdir -p "$OS/badcontent/openstack/2018-08-27"
    cat >"$OS/badcontent/openstack/2018-08-27/meta_data.json" <<'EOF'
{"uuid": "i-os", "files": [{"path": "/etc/motd", "content_path": "/gone"}]}
EOF

    # Nothing served at all: `latest` is tried and 404s.
    mkdir -p "$OS/absent"

    python3 "$(dirname "$0")/mdhttpd.py" "$OS" >"$OS.port" 2>/dev/null &
    os_pid=$!
    os_port=""
    tries=0
    while [ -z "$os_port" ] && [ "$tries" -lt 50 ]; do
        os_port=$(cat "$OS.port" 2>/dev/null || true)
        [ -n "$os_port" ] || sleep 0.1
        tries=$((tries + 1))
    done

    if [ -n "$os_port" ]; then
        # Both sides name the failing URL; upstream prints it bare while the
        # port prefixes the HTTP status, and the JSON parser's prose behind a
        # broken document is implementation-specific either way.
        os_trim="sed -e 's|mandatory path: .*\\(http://[^ ]*\\)|mandatory path: \\1|' \
            -e 's|\\(\\.json\\): .*|\\1: <detail>|' \
            -e 's|\\(provided file [^:]*\\): .*|\\1: <detail>|'"

        for case in full min versions nouuid badjson list badcontent absent; do
            base="http://127.0.0.1:$os_port/$case"
            run_pair "openstack $case" \
                "cd /tmp && python3 '$OS_PY' '$base' | $os_trim" \
                "cd /tmp && '$OS_RS' '$base' | $os_trim"
        done
    else
        printf 'SKIP openstack (server did not start)\n'
    fi
    kill "$os_pid" 2>/dev/null || true
    wait "$os_pid" 2>/dev/null || true
fi

# --- gce metadata service ----------------------------------------------------
sec "gce-metadata-service"
# `read_md` walks a fixed `url_map`, so every case is a tree of flat documents
# served by `gcehttpd.py`, which also enforces the `Metadata-Flavor` header.
GCE_PY="$(cd "$(dirname "$0")" && pwd)/gce.py"
GCE_RS="$TARGET/examples/dump-gce"
if [ -x "$GCE_RS" ]; then
    GCE_RS="$(cd "$(dirname "$GCE_RS")" && pwd)/dump-gce"
    GCE="$WORK/gce"

    gce_case() {
        mkdir -p "$GCE/$1/instance/attributes" "$GCE/$1/project/attributes"
        printf '%s' "$2" >"$GCE/$1/instance/id"
        printf '%s' "$3" >"$GCE/$1/instance/zone"
        printf '%s' "$4" >"$GCE/$1/instance/hostname"
        printf '%s' "$5" >"$GCE/$1/instance/attributes/index"
        printf '%s' "$6" >"$GCE/$1/project/attributes/index"
    }

    # Plain instance: keys on both the instance and the project.
    gce_case full 1234567890 projects/9/zones/us-central1-a host.c.p.internal \
        '{"ssh-keys": "u:ssh-rsa AAA u@h"}' \
        '{"ssh-keys": "u:ssh-rsa BBB u@h\nv:ssh-rsa CCC v@h"}'

    # Plain-text user-data.
    gce_case ud 2 projects/9/zones/europe-west1-b h \
        '{"user-data": "#cloud-config\nruncmd: [ echo gce ]\n"}' '{}'

    # Base64 user-data, and an encoding neither side understands.
    gce_case udb64 3 projects/9/zones/europe-west1-b h \
        '{"user-data": "I2Nsb3VkLWNvbmZpZwo=", "user-data-encoding": "base64"}' '{}'
    gce_case udbogus 4 projects/9/zones/europe-west1-b h \
        '{"user-data": "plain", "user-data-encoding": "rot13"}' '{}'

    # The legacy `sshKeys` attribute, and the explicit project-key block: both
    # shut the project keys out.
    gce_case legacy 5 projects/9/zones/us-east1-c h \
        '{"sshKeys": "u:ssh-rsa AAA"}' '{"ssh-keys": "u:ssh-rsa BBB"}'
    gce_case blocked 6 projects/9/zones/us-east1-c h \
        '{"block-project-ssh-keys": "TRUE"}' '{"ssh-keys": "u:ssh-rsa BBB"}'

    # Attributes that are absent entirely, and attributes that are not JSON.
    mkdir -p "$GCE/bare/instance"
    printf '7' >"$GCE/bare/instance/id"
    printf 'projects/9/zones/us-west1-a' >"$GCE/bare/instance/zone"
    printf 'h' >"$GCE/bare/instance/hostname"

    # A required key missing: not GCE.
    mkdir -p "$GCE/nokey/instance"
    printf 'projects/9/zones/us-west1-a' >"$GCE/nokey/instance/zone"

    # Nothing served at all.
    mkdir -p "$GCE/absent"

    python3 "$(dirname "$0")/gcehttpd.py" "$GCE" >"$GCE.port" 2>/dev/null &
    gce_pid=$!
    gce_port=""
    tries=0
    while [ -z "$gce_port" ] && [ "$tries" -lt 50 ]; do
        gce_port=$(cat "$GCE.port" 2>/dev/null || true)
        [ -n "$gce_port" ] || sleep 0.1
        tries=$((tries + 1))
    done

    if [ -n "$gce_port" ]; then
        for case in full ud udb64 udbogus legacy blocked bare nokey absent; do
            base="http://127.0.0.1:$gce_port/$case/"
            run_pair "gce $case" \
                "cd /tmp && python3 '$GCE_PY' '$base'" \
                "cd /tmp && '$GCE_RS' '$base'"
        done
    else
        printf 'SKIP gce (server did not start)\n'
    fi
    kill "$gce_pid" 2>/dev/null || true
    wait "$gce_pid" 2>/dev/null || true
fi

# --- ec2 metadata crawl -------------------------------------------------------
sec "ec2-metadata-crawl"
# The crawler walks whatever tree the service describes, so the fixtures are
# directory trees and `ec2httpd.py` lists them the way IMDS does.
EC2_PY="$(cd "$(dirname "$0")" && pwd)/ec2.py"
EC2_RS="$TARGET/examples/dump-ec2"
if [ -x "$EC2_RS" ]; then
    EC2_RS="$(cd "$(dirname "$EC2_RS")" && pwd)/dump-ec2"
    EC2="$WORK/ec2"

    # A plain instance: nested directories, a multi-line leaf, a JSON leaf.
    md="$EC2/full/latest/meta-data"
    mkdir -p "$md/placement" "$md/block-device-mapping" "$md/iam"
    printf 'i-0abc' >"$md/instance-id"
    printf 'ami-123' >"$md/ami-id"
    printf '0' >"$md/ami-launch-index"
    printf 'ip-10-0-0-5' >"$md/local-hostname"
    printf 'us-east-1a' >"$md/placement/availability-zone"
    printf '/dev/sda1' >"$md/block-device-mapping/ami"
    printf '/dev/sdb' >"$md/block-device-mapping/ephemeral0"
    printf '{"Code": "Success", "InstanceProfileId": "AIPA"}' >"$md/iam/info"
    printf '10.0.0.5\n10.0.0.6' >"$md/local-ipv4s"
    mkdir -p "$EC2/full/latest/dynamic/instance-identity"
    printf '{"instanceId": "i-0abc", "region": "us-east-1", "availabilityZone": "us-east-1a"}' \
        >"$EC2/full/latest/dynamic/instance-identity/document"
    printf '#cloud-config\nruncmd: [ echo ec2 ]\n' >"$EC2/full/latest/user-data"

    # The numbered public-key layout: `0=name` is read from `0/openssh-key`.
    md="$EC2/keys/latest/meta-data"
    mkdir -p "$md/public-keys/0"
    printf 'i-0key' >"$md/instance-id"
    printf '0=my-key' >"$md/public-keys/.listing"
    printf 'ssh-rsa AAAAB3 u@h' >"$md/public-keys/0/openssh-key"

    # `security-credentials` must never be fetched, even when it is listed.
    md="$EC2/creds/latest/meta-data"
    mkdir -p "$md/iam/security-credentials"
    printf 'i-0cred' >"$md/instance-id"
    printf '{"Code": "Success"}' >"$md/iam/info"
    printf 'SECRET' >"$md/iam/security-credentials/role"

    # Base64 user-data, which `maybe_b64decode` unwraps, and one that only
    # looks like it: `hello` is in the alphabet but the wrong length.
    md="$EC2/b64/latest/meta-data"
    mkdir -p "$md"
    printf 'i-0b64' >"$md/instance-id"
    printf 'I2Nsb3VkLWNvbmZpZwo=' >"$EC2/b64/latest/user-data"
    md="$EC2/notb64/latest/meta-data"
    mkdir -p "$md"
    printf 'i-0nob' >"$md/instance-id"
    printf 'hello' >"$EC2/notb64/latest/user-data"
    # Plain text that happens to be valid base64, and so is silently decoded.
    md="$EC2/b64trap/latest/meta-data"
    mkdir -p "$md"
    printf 'i-0trp' >"$md/instance-id"
    printf 'test' >"$EC2/b64trap/latest/user-data"

    # An empty leaf, and a leaf that is not valid UTF-8.
    md="$EC2/odd/latest/meta-data"
    mkdir -p "$md"
    printf 'i-0odd' >"$md/instance-id"
    : >"$md/empty"
    printf '\377\376' >"$md/binary"

    # Nothing served at all.
    mkdir -p "$EC2/absent"

    python3 "$(dirname "$0")/ec2httpd.py" "$EC2" >"$EC2.port" 2>/dev/null &
    ec2_pid=$!
    ec2_port=""
    tries=0
    while [ -z "$ec2_port" ] && [ "$tries" -lt 50 ]; do
        ec2_port=$(cat "$EC2.port" 2>/dev/null || true)
        [ -n "$ec2_port" ] || sleep 0.1
        tries=$((tries + 1))
    done

    if [ -n "$ec2_port" ]; then
        for case in full keys creds b64 notb64 b64trap odd absent; do
            base="http://127.0.0.1:$ec2_port/$case"
            run_pair "ec2 $case" \
                "cd /tmp && python3 '$EC2_PY' '$base'" \
                "cd /tmp && '$EC2_RS' '$base'"
        done
    else
        printf 'SKIP ec2 (server did not start)\n'
    fi
    kill "$ec2_pid" 2>/dev/null || true
    wait "$ec2_pid" 2>/dev/null || true
fi

# The LXD socket API, against a stand-in for /dev/lxd/sock.
LXD_PY="$(cd "$(dirname "$0")" && pwd)/lxd.py"
LXD_SOCKD="$(cd "$(dirname "$0")" && pwd)/lxdsock.py"
LXD_RS="$TARGET/examples/dump-lxd"
if [ -x "$LXD_RS" ]; then
    LXD_RS="$(cd "$(dirname "$LXD_RS")" && pwd)/dump-lxd"
    LXD="$WORK/lxd"

    # Both alias families, so `cloud-init.*` wins and `user.*` is ignored,
    # plus a key that is not an alias, a `user.meta-data` and some devices.
    c="$LXD/full"
    mkdir -p "$c/config"
    printf 'instance-id: i-lxd-1\nlocal-hostname: full\n' >"$c/meta-data"
    printf '#cloud-config\nruncmd: [ echo new ]\n' >"$c/config/cloud-init.user-data"
    printf '#cloud-config\nruncmd: [ echo old ]\n' >"$c/config/user.user-data"
    printf '#cloud-config\n{}\n' >"$c/config/user.vendor-data"
    printf 'version: 1\n' >"$c/config/cloud-init.network-config"
    printf 'local-hostname: merged\n' >"$c/config/user.meta-data"
    printf 'true' >"$c/config/security.nesting"
    printf '{"eth0": {"name": "eth0", "type": "nic"}, "root": {"type": "disk"}}' \
        >"$c/devices"

    # Only meta-data: no config keys, no devices.
    c="$LXD/minimal"
    mkdir -p "$c"
    printf 'instance-id: i-lxd-2\n' >"$c/meta-data"

    # A route the config listing advertises but that does not answer.
    c="$LXD/skipped"
    mkdir -p "$c/config"
    printf 'instance-id: i-lxd-3\n' >"$c/meta-data"
    printf 'ok' >"$c/config/user.vendor-data"
    printf '["/1.0/config/user.vendor-data","/1.0/config/user.absent"]' \
        >"$c/config.json"

    # A 500 on the first request to each route, which the crawl waits out.
    c="$LXD/flaky"
    mkdir -p "$c/config"
    printf 'instance-id: i-lxd-4\n' >"$c/meta-data"
    printf '/1.0/meta-data\n/1.0/config\n/1.0/devices\n' >"$c/flaky"

    # The config listing is not JSON.
    c="$LXD/badjson"
    mkdir -p "$c"
    printf 'instance-id: i-lxd-5\n' >"$c/meta-data"
    printf 'not json at all' >"$c/config.json"

    # No meta-data route at all, which is fatal.
    c="$LXD/nometa"
    mkdir -p "$c"
    printf '{}' >"$c/devices"

    for case in full minimal skipped flaky badjson nometa; do
        sock="$LXD/$case.sock"
        python3 "$LXD_SOCKD" "$LXD/$case" "$sock" >/dev/null 2>&1 &
        lxd_pid=$!
        tries=0
        while [ ! -S "$sock" ] && [ "$tries" -lt 50 ]; do
            sleep 0.1
            tries=$((tries + 1))
        done
        if [ -S "$sock" ]; then
            run_pair "lxd $case" \
                "cd /tmp && python3 '$LXD_PY' '$sock'" \
                "cd /tmp && '$LXD_RS' '$sock'"
        else
            printf 'SKIP lxd %s (server did not start)\n' "$case"
        fi
        kill "$lxd_pid" 2>/dev/null || true
        wait "$lxd_pid" 2>/dev/null || true
    done
fi

# The Azure identity helpers, over a prepared DMI syspath, and the IMDS client
# against a stand-in for 169.254.169.254.
AZ_PY="$(cd "$(dirname "$0")" && pwd)/azure.py"
AZ_HTTPD="$(cd "$(dirname "$0")" && pwd)/azimds.py"
AZ_RS="$TARGET/examples/dump-azure"
if [ -x "$AZ_RS" ]; then
    AZ_RS="$(cd "$(dirname "$AZ_RS")" && pwd)/dump-azure"
    AZ="$WORK/azure"

    # A host that says it is Azure. The system uuid is upper-case, as kernels
    # older than 4.15 report it.
    c="$AZ/azure"
    mkdir -p "$c"
    printf '7783-7084-3265-9085-8269-3286-77\n' >"$c/chassis_asset_tag"
    printf '12345678-1234-5678-1234-567812345678\n' >"$c/product_uuid"

    # A tag that is one character short of Azure's.
    c="$AZ/other"
    mkdir -p "$c"
    printf '7783-7084-3265-9085-8269-3286-7\n' >"$c/chassis_asset_tag"
    printf 'ABCDEF78-1234-5678-1234-567812345678\n' >"$c/product_uuid"

    # No DMI at all.
    mkdir -p "$AZ/bare"

    for case in azure other bare; do
        run_pair "azure identity $case" \
            "cd /tmp && python3 '$AZ_PY' identity '$AZ/$case'" \
            "cd /tmp && '$AZ_RS' identity '$AZ/$case'"
    done

    run_pair "azure swap" \
        "cd /tmp && python3 '$AZ_PY' swap 12345678-1234-5678-1234-567812345678 \
            ABCDEF78-1234-5678-1234-567812345678 \
            urn:uuid:12345678-1234-5678-1234-567812345678 \
            '{12345678-1234-5678-1234-567812345678}' \
            12345678123456781234567812345678 not-a-uuid ''" \
        "cd /tmp && '$AZ_RS' swap 12345678-1234-5678-1234-567812345678 \
            ABCDEF78-1234-5678-1234-567812345678 \
            urn:uuid:12345678-1234-5678-1234-567812345678 \
            '{12345678-1234-5678-1234-567812345678}' \
            12345678123456781234567812345678 not-a-uuid ''"

    # The extended API answers.
    c="$AZ/imds-full"
    mkdir -p "$c"
    printf '{"compute": {"vmId": "i-az-1", "name": "vm1"}, "network": {}}' \
        >"$c/extended.json"

    # No extended API: a 400 sends both sides to the 2019-06-01 version.
    c="$AZ/imds-fallback"
    mkdir -p "$c"
    printf '400' >"$c/extended.code"
    printf '{"compute": {"vmId": "i-az-2"}}' >"$c/plain.json"

    # 404 until the platform has provisioned, which the poll loop waits out.
    c="$AZ/imds-polled"
    mkdir -p "$c"
    printf '2' >"$c/retries"
    printf '{"compute": {"vmId": "i-az-3"}}' >"$c/extended.json"

    # A code that is neither a retry code nor the fallback trigger.
    c="$AZ/imds-refused"
    mkdir -p "$c"
    printf '403' >"$c/extended.code"

    # A body that is not JSON.
    c="$AZ/imds-badjson"
    mkdir -p "$c"
    printf 'not json at all' >"$c/extended.json"

    for case in imds-full imds-fallback imds-polled imds-refused imds-badjson; do
        run_pair "azure $case" \
            "cd /tmp && python3 '$AZ_HTTPD' '$AZ/$case' python3 '$AZ_PY' imds '{base}'" \
            "cd /tmp && python3 '$AZ_HTTPD' '$AZ/$case' '$AZ_RS' imds '{base}'"
    done

    # ovf-env.xml, the provisioning media half of the datasource.
    OVF="$AZ/ovf"
    mkdir -p "$OVF"
    ovf_doc() {
        printf '%s\n' '<?xml version="1.0" encoding="utf-8"?>' \
            '<Environment xmlns="http://schemas.dmtf.org/ovf/environment/1"' \
            ' xmlns:wa="http://schemas.microsoft.com/windowsazure">' \
            ' <wa:ProvisioningSection>' \
            '  <wa:LinuxProvisioningConfigurationSet>' \
            "$1" \
            '  </wa:LinuxProvisioningConfigurationSet>' \
            ' </wa:ProvisioningSection>' \
            ' <wa:PlatformSettingsSection>' \
            '  <wa:PlatformSettings>' \
            "$2" \
            '  </wa:PlatformSettings>' \
            ' </wa:PlatformSettingsSection>' \
            '</Environment>'
    }

    ovf_doc \
        '<wa:HostName>vm1</wa:HostName><wa:UserName>azureuser</wa:UserName>
         <wa:UserPassword>REDACTED</wa:UserPassword>
         <wa:CustomData>I2Nsb3VkLWNvbmZpZwo=</wa:CustomData>
         <wa:DisableSshPasswordAuthentication>true</wa:DisableSshPasswordAuthentication>
         <wa:SSH><wa:PublicKeys>
          <wa:PublicKey><wa:Fingerprint>ABC</wa:Fingerprint>
           <wa:Path>/root/.ssh/authorized_keys</wa:Path></wa:PublicKey>
          <wa:PublicKey><wa:Value>ssh-rsa AAAA user@host</wa:Value></wa:PublicKey>
         </wa:PublicKeys></wa:SSH>' \
        '<wa:PreprovisionedVm>true</wa:PreprovisionedVm>
         <wa:PreprovisionedVMType>Savable</wa:PreprovisionedVMType>
         <wa:ProvisionGuestProxyAgent>True</wa:ProvisionGuestProxyAgent>' \
        >"$OVF/full.xml"

    # Every optional field left out, so the defaults show.
    ovf_doc '<wa:HostName>vm1</wa:HostName>' '' >"$OVF/minimal.xml"

    # Present but empty: a boolean becomes false, custom-data stays absent.
    ovf_doc \
        '<wa:HostName>vm1</wa:HostName><wa:CustomData/>
         <wa:DisableSshPasswordAuthentication/>' \
        '<wa:PreprovisionedVm/>' >"$OVF/empty.xml"

    # Base64 split across lines, which the parser has to unwrap first.
    ovf_doc \
        '<wa:HostName>vm1</wa:HostName>
         <wa:CustomData>I2Nsb3Vk
            LWNvbmZpZwo=</wa:CustomData>' '' >"$OVF/wrapped.xml"

    # The spellings translate_bool has to agree on.
    ovf_doc '<wa:HostName>vm1</wa:HostName>' \
        '<wa:PreprovisionedVm>no</wa:PreprovisionedVm>
         <wa:ProvisionGuestProxyAgent>yes</wa:ProvisionGuestProxyAgent>' \
        >"$OVF/bools.xml"

    # An SSH section with no keys in it at all.
    ovf_doc \
        '<wa:HostName>vm1</wa:HostName><wa:SSH><wa:PublicKeys/></wa:SSH>' \
        '' >"$OVF/nokeys.xml"

    ovf_doc '<wa:UserName>u</wa:UserName>' '' >"$OVF/nohostname.xml"
    ovf_doc '<wa:HostName>a</wa:HostName><wa:HostName>b</wa:HostName>' '' \
        >"$OVF/duplicate.xml"

    printf '%s\n' '<?xml version="1.0"?>' \
        '<Environment xmlns:wa="http://schemas.microsoft.com/windowsazure"/>' \
        >"$OVF/nonazure.xml"
    printf 'not xml at all\n' >"$OVF/notxml.xml"

    for case in full minimal empty wrapped bools nokeys nohostname duplicate \
        nonazure notxml; do
        run_pair "azure ovf $case" \
            "cd /tmp && python3 '$AZ_PY' ovf '$OVF/$case.xml'" \
            "cd /tmp && '$AZ_RS' ovf '$OVF/$case.xml'"
    done

    # The provisioning report, whose CSV quoting is the interesting part: the
    # delimiter is `|` and the quote character is `'`, both of which turn up in
    # the messages being encoded.
    TS=2026-09-02T12:34:56.789012+00:00
    # The agent field names the implementation and its package version, and is
    # per-implementation by construction (deviation 24); the mask has to swallow
    # the `Cloud-Init` / `Cloud-Init-rs` prefix as well as the version.
    AZ_MASK="sed 's#agent=Cloud-Init[^|]*#agent=X#'"
    az_report() {
        label="$1"
        shift
        quoted=""
        for a in "$@"; do
            quoted="$quoted '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"
        done
        run_pair "azure report $label" \
            "cd /tmp && python3 '$AZ_PY' report$quoted | $AZ_MASK" \
            "cd /tmp && '$AZ_RS' report$quoted | $AZ_MASK"
    }

    az_report success "$TS" abc success
    az_report "success without a vm id" "$TS" - success
    az_report plain "$TS" abc os-disk-pps
    az_report "no supporting data" "$TS" abc proxy-missing
    az_report "trailing newlines in supporting data" "$TS" abc proxy-status 1 \
        'out
' 'err

'
    az_report "quote in the reason" "$TS" abc ovf-invalid \
        "missing configuration for 'HostName'"
    az_report "delimiter in the reason" "$TS" abc ovf-parsing 'a|b'
    az_report "both metacharacters" "$TS" abc ovf-parsing "a|b and 'c'"
    az_report "newline in supporting data" "$TS" abc proxy-status 3 'out
line two' 'err'
    az_report "empty supporting data" "$TS" abc proxy-status 0 '' ''
    az_report "trailing space in the last field" "$TS" 'abc ' vm-id boom uuid
    az_report vm-id "$TS" abc vm-id "ValueError('nope')" 12345678-1234
    az_report imds-parsing "$TS" abc imds-parsing 'Expecting value: line 1'
    az_report imds-url-code "$TS" abc imds-url http://h/meta 404 'not found' 1.0
    az_report imds-url-none "$TS" abc imds-url http://h/meta '' 'refused' 0.5
    for value in '"Savable"' '123' '1.5' 'true' 'null' '["a","b"]' \
        '{"k":"v"}' '"has | pipe"' '"has '"'"' quote"'; do
        az_report "imds-invalid $value" "$TS" abc imds-invalid ppsType "$value"
    done

    # The wireserver half of sources/helpers/azure.py: goal state parsing and
    # the two documents the port builds.
    WIRE="$AZ/wire"
    mkdir -p "$WIRE"
    goal_doc() {
        printf '%s\n' '<?xml version="1.0" encoding="utf-8"?>' \
            '<GoalState>' \
            "  <Incarnation>$1</Incarnation>" \
            '  <Container>' \
            "    <ContainerId>$2</ContainerId>" \
            '    <RoleInstanceList>' \
            '      <RoleInstance>' \
            "        <InstanceId>$3</InstanceId>" \
            '        <Configuration>' \
            "$4" \
            '        </Configuration>' \
            '      </RoleInstance>' \
            '    </RoleInstanceList>' \
            '  </Container>' \
            '</GoalState>'
    }
    CERTS='          <Certificates>http://h/certs</Certificates>'
    goal_doc 12 c-id i-id "$CERTS" >"$WIRE/full.xml"
    goal_doc 12 c-id i-id '' >"$WIRE/nocerts.xml"
    goal_doc '' c-id i-id "$CERTS" >"$WIRE/empty-incarnation.xml"
    printf '%s\n' '<GoalState><Container>' \
        '<ContainerId>c</ContainerId></Container></GoalState>' \
        >"$WIRE/no-instance.xml"
    printf '%s\n' '<GoalState><Incarnation>1</Incarnation>' \
        >"$WIRE/malformed.xml"
    # A first Container that runs out mid-path; ElementPath backtracks to the
    # sibling that carries the rest.
    printf '%s\n' '<GoalState><Incarnation>7</Incarnation>' \
        '<Container><ContainerId>first</ContainerId></Container>' \
        '<Container><ContainerId>second</ContainerId><RoleInstanceList>' \
        '<RoleInstance><InstanceId>i2</InstanceId><Configuration>' \
        '</Configuration></RoleInstance></RoleInstanceList></Container>' \
        '</GoalState>' >"$WIRE/backtrack.xml"

    for case in full nocerts empty-incarnation no-instance backtrack; do
        run_pair "azure wire goalstate $case" \
            "cd /tmp && python3 '$AZ_PY' wire goalstate '$WIRE/$case.xml'" \
            "cd /tmp && '$AZ_RS' wire goalstate '$WIRE/$case.xml'"
    done
    # Both sides refuse the document and exit 1, but the text after the colon
    # is the parser's own diagnostic, which is per-implementation (deviation 4).
    WIRE_MASK="sed 's#XML: .*#XML: ...#'"
    run_pair "azure wire goalstate malformed" \
        "cd /tmp && python3 '$AZ_PY' wire goalstate '$WIRE/malformed.xml' | $WIRE_MASK" \
        "cd /tmp && '$AZ_RS' wire goalstate '$WIRE/malformed.xml' | $WIRE_MASK"

    az_wire() {
        label="$1"
        shift
        quoted=""
        for a in "$@"; do
            quoted="$quoted '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"
        done
        run_pair "azure wire $label" \
            "cd /tmp && python3 '$AZ_PY' wire$quoted" \
            "cd /tmp && '$AZ_RS' wire$quoted"
    }
    az_wire "health ready" health "$WIRE/full.xml" ready
    az_wire "health failure" health "$WIRE/full.xml" failure 'boom'
    az_wire "health failure markup" health "$WIRE/full.xml" failure \
        "a <bad> & 'quoted' \"thing\""
    az_wire "health failure long" health "$WIRE/full.xml" failure \
        "$(printf 'x%.0s' $(seq 1 600))"
    az_wire "health failure empty" health "$WIRE/full.xml" failure ''
    az_wire "minimal-ovf full" minimal-ovf user host true
    az_wire "minimal-ovf false" minimal-ovf user host false
    az_wire "minimal-ovf none" minimal-ovf - host -
    az_wire "minimal-ovf markup" minimal-ovf 'a&b' 'h<t' true

    # WALinuxAgentShim._filter_pubkeys: which of the keys the goal state
    # decrypted end up in the returned list, and in what order. The two
    # warnings it can emit go to the log, which run_pair does not compare.
    printf '%s\n' '{"AA": "ssh-rsa from-goalstate-aa\n",
        "BB": "ssh-rsa from-goalstate-bb\n"}' >"$WIRE/keys.json"
    printf '%s\n' '[]' >"$WIRE/pk-empty.json"
    printf '%s\n' '[{"fingerprint": "AA", "path": "p", "value": "inline"}]' \
        >"$WIRE/pk-value.json"
    printf '%s\n' '[{"fingerprint": "BB", "path": "p", "value": ""}]' \
        >"$WIRE/pk-fingerprint.json"
    printf '%s\n' '[{"fingerprint": "ZZ", "path": "p", "value": ""}]' \
        >"$WIRE/pk-missing.json"
    printf '%s\n' '[{"fingerprint": null, "path": "p", "value": ""}]' \
        >"$WIRE/pk-neither.json"
    printf '%s\n' '[{"path": "p"}]' >"$WIRE/pk-bare.json"
    # Order follows ovf-env.xml, not the fingerprint map, and a repeat repeats.
    printf '%s\n' '[{"fingerprint": "BB", "path": "p", "value": ""},
        {"fingerprint": "AA", "path": "p", "value": "inline"},
        {"fingerprint": "AA", "path": "p", "value": ""},
        {"fingerprint": "ZZ", "path": "p", "value": ""},
        {"fingerprint": "BB", "path": "p", "value": ""}]' \
        >"$WIRE/pk-mixed.json"
    for case in empty value fingerprint missing neither bare mixed; do
        az_wire "filter-pubkeys $case" filter-pubkeys "$WIRE/keys.json" \
            "$WIRE/pk-$case.json"
    done

    # The metadata-shaping half of DataSourceAzure.py.
    DS="$AZ/ds"
    mkdir -p "$DS"
    az_ds() {
        label="$1"
        shift
        quoted=""
        for a in "$@"; do
            quoted="$quoted '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"
        done
        run_pair "azure ds $label" \
            "cd /tmp && python3 '$AZ_PY' ds$quoted" \
            "cd /tmp && '$AZ_RS' ds$quoted"
    }

    ovf_doc '<wa:HostName>host</wa:HostName>
   <wa:UserName>user</wa:UserName>' '' >"$DS/plain.xml"
    ovf_doc '<wa:HostName>host</wa:HostName>
   <wa:UserName>user</wa:UserName>
   <wa:UserPassword>hunter2</wa:UserPassword>' '' >"$DS/password.xml"
    ovf_doc '<wa:HostName>host</wa:HostName>
   <wa:UserName>user</wa:UserName>
   <wa:UserPassword>REDACTED</wa:UserPassword>
   <wa:DisableSshPasswordAuthentication>false
   </wa:DisableSshPasswordAuthentication>' '' >"$DS/redacted.xml"
    ovf_doc '<wa:HostName>host</wa:HostName>
   <wa:CustomData>aGVsbG8=</wa:CustomData>
   <wa:SSH><wa:PublicKeys>
    <wa:PublicKey><wa:Fingerprint>AA</wa:Fingerprint>
     <wa:Path>/root/.ssh/authorized_keys</wa:Path></wa:PublicKey>
    <wa:PublicKey><wa:Value>ssh-rsa AAAA</wa:Value></wa:PublicKey>
   </wa:PublicKeys></wa:SSH>' \
        '<wa:PreprovisionedVm>true</wa:PreprovisionedVm>
   <wa:PreprovisionedVMType>Savable</wa:PreprovisionedVMType>
   <wa:ProvisionGuestProxyAgent>true</wa:ProvisionGuestProxyAgent>' \
        >"$DS/rich.xml"
    printf '%s\n' '<Environment/>' >"$DS/notazure.xml"

    for case in plain password redacted rich notazure; do
        az_ds "crawl $case" crawl "$DS/$case.xml"
    done

    printf '%s' '{"compute": {"userData": "abc", "osProfile": {
        "adminUsername": "u", "computerName": "h",
        "disablePasswordAuthentication": "true"},
        "publicKeys": [{"keyData": "ssh-rsa AAAA u@h"}]},
        "extended": {"compute": {"ppsType": "Savable"}}}' >"$DS/imds-full.json"
    printf '%s' '{"compute": {"publicKeys": [{"keyData": "junk"}]}}' \
        >"$DS/imds-badkey.json"
    printf '%s' '{"compute": {"publicKeys": []}}' >"$DS/imds-nokeys.json"
    printf '%s' '{}' >"$DS/imds-empty.json"
    printf '%s' '{"compute": {"osProfile": {
        "disablePasswordAuthentication": true}}}' >"$DS/imds-realbool.json"
    for case in imds-full imds-badkey imds-nokeys imds-empty imds-realbool; do
        az_ds "$case" imds "$DS/$case.json"
    done

    printf '%s' '{}' >"$DS/ovf-none.json"
    printf '%s' '{"PreprovisionedVm": true}' >"$DS/ovf-legacy.json"
    printf '%s' '{"PreprovisionedVMType": "PreprovisionedOSDisk"}' \
        >"$DS/ovf-osdisk.json"
    printf '%s' '{"PreprovisionedVMType": "Running"}' >"$DS/ovf-running.json"
    for case in ovf-none ovf-legacy ovf-osdisk ovf-running; do
        for md in imds-empty imds-full; do
            az_ds "pps $case $md" pps "$DS/$case.json" "$DS/$md.json" false
        done
    done
    az_ds "pps marker wins" pps "$DS/ovf-osdisk.json" "$DS/imds-full.json" true

    for key in 'ssh-rsa AAAAB3 user@host' 'ssh-ed25519 AAAAC3' 'ssh-rsa' \
        'not-a-type AAAAB3' '# ssh-rsa AAAAB3' '' '   ' \
        'command="/bin/true" ssh-rsa AAAAB3' \
        'no-pty,no-X11-forwarding ssh-rsa AAAAB3 c' \
        'ssh-rsa AAAAB3
ssh-rsa CCCC' 'rsa AAAA' 'ssh-rsa  AAAAB3  c  d'; do
        az_ds "sshkey [$key]" sshkey "$key"
    done
    # LP: #1910835 - a CRLF inside the key is what the guard is for.
    az_ds "sshkey crlf" sshkey "$(printf 'ssh-rsa AAAAB3\r\nssh-rsa CCCC')"
    az_ds "sshkey trailing crlf" sshkey "$(printf 'ssh-rsa AAAAB3\r\n')"

    UUID=5f4dcc3b-5aa7-65d6-1b0e-99e5f4dcc3b5
    az_ds "iid without a previous" iid "$UUID" -
    az_ds "iid uppercase previous" iid "$UUID" \
        "$(printf '%s' "$UUID" | tr a-f A-F)"
    az_ds "iid unrelated previous" iid "$UUID" other
    az_ds "iid whitespace previous" iid "$UUID" "  $UUID
"
    for seed in - /dev/sr0 IMDS imds /var/lib/waagent; do
        az_ds "subplatform $seed" subplatform "$seed"
    done

    # The datasource config, the accessors over it, and the keys.
    echo '{}' >"$DS/empty.json"
    cat >"$DS/syscfg-user.json" <<'EOF'
{"datasource": {"Azure": {"apply_network_config": false,
  "never_destroy_ntfs": true, "data_dir": "/var/lib/other",
  "disk_aliases": {"ephemeral1": "/dev/sdc"}}}}
EOF
    cat >"$DS/syscfg-alias.json" <<'EOF'
{"datasource": {"Azure": {"disk_aliases": {"ephemeral0": "/dev/sdb"}}}}
EOF
    cat >"$DS/syscfg-other.json" <<'EOF'
{"datasource": {"Ec2": {"apply_network_config": false}}}
EOF
    for cfg in empty syscfg-user syscfg-alias syscfg-other; do
        az_ds "dscfg $cfg" dscfg "$DS/$cfg.json"
    done

    cat >"$DS/md-keys.json" <<'EOF'
{"public-keys": ["ssh-rsa OVF1", "ssh-rsa OVF2"],
 "imds": {"compute": {"publicKeys": [{"keyData": "ssh-rsa IMDS"}]}}}
EOF
    cat >"$DS/md-ovfonly.json" <<'EOF'
{"public-keys": ["ssh-rsa OVF1"], "imds": {"compute": {}}}
EOF
    cat >"$DS/md-badkey.json" <<'EOF'
{"public-keys": ["ssh-rsa OVF1"],
 "imds": {"compute": {"publicKeys": [{"keyData": "not-a-key"}]}}}
EOF
    echo '{"public-keys": []}' >"$DS/md-nokeys.json"
    for md in md-keys md-ovfonly md-badkey md-nokeys empty; do
        az_ds "keys $md" keys "$DS/$md.json"
    done

    cat >"$DS/cfg-pubkeys.json" <<'EOF'
{"_pubkeys": [{"fingerprint": "AA", "path": "/root/.ssh/authorized_keys",
               "value": ""}]}
EOF
    echo '{"_pubkeys": []}' >"$DS/cfg-nopubkeys.json"
    cat >"$DS/imds-keys.json" <<'EOF'
{"compute": {"publicKeys": [{"keyData": "ssh-rsa AAAA"}]}}
EOF
    for cfg in cfg-pubkeys cfg-nopubkeys empty; do
        for md in imds-keys empty; do
            az_ds "pubkeyinfo $cfg $md" pubkeyinfo "$DS/$cfg.json" \
                "$DS/$md.json"
        done
    done

    echo '{"instance-id": "iid-AZURE-NODE"}' >"$DS/md-iid.json"
    for md in md-iid empty; do
        az_ds "instanceid $md" instanceid "$DS/$md.json" \
            5f4dcc3b-5aa7-65d6-1b0e-99e5f4dcc3b5
    done

    cat >"$DS/md-region.json" <<'EOF'
{"imds": {"compute": {"location": "westus2", "platformFaultDomain": "0"}}}
EOF
    echo '{"imds": {"compute": {}}}' >"$DS/md-nocompute.json"
    for md in md-region md-nocompute empty; do
        az_ds "region $md" region "$DS/$md.json"
    done

    cat >"$DS/imds-net.json" <<'EOF'
{"network": {"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                         {"privateIpAddress": "10.0.0.5"}],
           "subnet": [{"prefix": "16"}]}}]}}
EOF
    cat >"$DS/imds-badnet.json" <<'EOF'
{"network": {"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                         {"privateIpAddress": "10.0.0.5"}]}}]}}
EOF
    echo '{"compute": {}}' >"$DS/imds-nonet.json"
    cat >"$DS/nics.json" <<'EOF'
[{"mac": "00:11:22:aa:bb:cc", "driver": "hv_netvsc"}]
EOF
    for cfg in empty syscfg-user; do
        for md in imds-net imds-badnet imds-nonet empty; do
            az_ds "netconfig $cfg $md" netconfig "$DS/$cfg.json" \
                "$DS/$md.json" "$DS/nics.json"
        done
    done

    # The netplan document IMDS network metadata turns into.
    NET="$AZ/netcfg"
    mkdir -p "$NET"
    az_net() {
        label="$1"
        shift
        quoted=""
        for a in "$@"; do
            quoted="$quoted '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"
        done
        run_pair "azure netcfg $label" \
            "cd /tmp && python3 '$AZ_PY' netcfg$quoted" \
            "cd /tmp && '$AZ_RS' netcfg$quoted"
    }

    cat >"$NET/nics.json" <<'EOF'
[{"mac": "00:11:22:aa:bb:cc", "driver": "hv_netvsc"},
 {"mac": "dd:ee:ff:00:11:22", "driver": "mlx5_core"}]
EOF
    cat >"$NET/nics-dup.json" <<'EOF'
[{"mac": "00:11:22:aa:bb:cc", "driver": "mlx5_core"},
 {"mac": "001122AABBCC", "driver": "ixgbevf"}]
EOF
    cat >"$NET/nics-nodrv.json" <<'EOF'
[{"mac": "00:11:22:aa:bb:cc", "driver": null}]
EOF
    cat >"$NET/nics-hv2.json" <<'EOF'
[{"mac": "00:11:22:aa:bb:cc", "driver": "hv_netvsc"},
 {"mac": "dd:ee:ff:00:11:22", "driver": "hv_netvsc"}]
EOF
    echo '[]' >"$NET/nics-none.json"

    cat >"$NET/single.json" <<'EOF'
{"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"}],
           "subnet": [{"address": "10.0.0.0", "prefix": "24"}]}}]}
EOF
    cat >"$NET/secondary.json" <<'EOF'
{"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                         {"privateIpAddress": "10.0.0.5"},
                         {"privateIpAddress": "10.0.0.6"}],
           "subnet": [{"address": "10.0.0.0", "prefix": "16"}]}}]}
EOF
    cat >"$NET/dual.json" <<'EOF'
{"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"}],
           "subnet": [{"prefix": "24"}]},
  "ipv6": {"ipAddress": [{"privateIpAddress": "fd00::4"},
                         {"privateIpAddress": "fd00::5"}],
           "subnet": [{"prefix": "64"}]}},
 {"macAddress": "DDEEFF001122",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.1.0.4"},
                         {"privateIpAddress": "10.1.0.5"}],
           "subnet": [{"address": "10.1.0.0"}]}}]}
EOF
    cat >"$NET/v6only.json" <<'EOF'
{"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": []},
  "ipv6": {"ipAddress": [{"privateIpAddress": "fd00::4"}], "subnet": [{}]}}]}
EOF
    cat >"$NET/noaddr.json" <<'EOF'
{"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [], "subnet": []}, "ipv6": {"ipAddress": []}}]}
EOF
    cat >"$NET/nosubnet.json" <<'EOF'
{"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                         {"privateIpAddress": "10.0.0.5"}]}}]}
EOF
    cat >"$NET/emptysubnet.json" <<'EOF'
{"interface": [{"macAddress": "001122AABBCC",
  "ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"},
                         {"privateIpAddress": "10.0.0.5"}],
           "subnet": []}}]}
EOF
    cat >"$NET/nomac.json" <<'EOF'
{"interface": [{"ipv4": {"ipAddress": [{"privateIpAddress": "10.0.0.4"}]}}]}
EOF
    echo '{}' >"$NET/nointerface.json"
    echo '{"interface": []}' >"$NET/empty.json"

    for doc in single secondary dual v6only noaddr nosubnet emptysubnet \
        nomac nointerface empty; do
        for secondary in true false; do
            az_net "config $doc $secondary" config \
                "$NET/$doc.json" "$secondary" "$NET/nics.json"
        done
    done
    az_net "config without nics" config "$NET/dual.json" true \
        "$NET/nics-none.json"
    az_net "config with ambiguous drivers" config "$NET/single.json" true \
        "$NET/nics-dup.json"

    az_net "driver hyperv" driver 00:11:22:aa:bb:cc "$NET/nics.json"
    az_net "driver single" driver dd:ee:ff:00:11:22 "$NET/nics.json"
    az_net "driver ambiguous" driver 00:11:22:aa:bb:cc "$NET/nics-dup.json"
    az_net "driver missing" driver aa:bb:cc:dd:ee:ff "$NET/nics.json"
    az_net "driver none reported" driver 00:11:22:aa:bb:cc \
        "$NET/nics-nodrv.json"

    cat >"$NET/imds-good.json" <<'EOF'
{"network": {"interface": [{"macAddress": "001122AABBCC"},
                           {"macAddress": "DDEEFF001122"}]}}
EOF
    cat >"$NET/imds-partial.json" <<'EOF'
{"network": {"interface": [{"macAddress": "001122AABBCC"}]}}
EOF
    echo '{"network": {}}' >"$NET/imds-noiface.json"
    echo '{}' >"$NET/imds-nonet.json"

    az_net "validate complete" validate "$NET/imds-good.json" \
        "$NET/nics-hv2.json" -
    az_net "validate missing nic" validate "$NET/imds-partial.json" \
        "$NET/nics-hv2.json" -
    az_net "validate missing primary" validate "$NET/imds-partial.json" \
        "$NET/nics-hv2.json" DDEEFF001122
    az_net "validate missing other" validate "$NET/imds-partial.json" \
        "$NET/nics-hv2.json" 001122AABBCC
    az_net "validate no interface key" validate "$NET/imds-noiface.json" \
        "$NET/nics-hv2.json" -
    az_net "validate no network key" validate "$NET/imds-nonet.json" \
        "$NET/nics-hv2.json" -
    az_net "validate nothing local" validate "$NET/imds-partial.json" \
        "$NET/nics-none.json" -
    az_net "validate only synthetic nics count" validate \
        "$NET/imds-partial.json" "$NET/nics.json" -

    az_net "mac forms" mac 001122AABBCC 00:11:22:AA:BB:CC 0011.22aa.bbcc \
        short '' 001122aabbccddeeff

    # `crawl_metadata` itself. The fixture stands in for the machine on both
    # sides -- Python monkeypatches the datasource the way upstream's own unit
    # tests do -- so what is compared is the order the readers run in, which
    # of them answers, and the four documents that come out.
    CRAWL="$AZ/crawl"
    mkdir -p "$CRAWL"
    az_crawl() {
        run_pair "azure crawl $1" \
            "cd /tmp && python3 '$AZ_PY' crawl '$CRAWL/$1.json'" \
            "cd /tmp && '$AZ_RS' crawl '$CRAWL/$1.json'"
    }

    # An OVF with everything the reader looks at, kept on one line per element
    # so the fixture writer below can embed it as a JSON string.
    cat >"$CRAWL/ovf.xml" <<'EOF'
<?xml version="1.0" encoding="utf-8"?>
<Environment xmlns="http://schemas.dmtf.org/ovf/environment/1"
 xmlns:wa="http://schemas.microsoft.com/windowsazure"
 xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
 <wa:ProvisioningSection>
  <wa:Version>1.0</wa:Version>
  <LinuxProvisioningConfigurationSet
   xmlns="http://schemas.microsoft.com/windowsazure"
   xmlns:i="http://www.w3.org/2001/XMLSchema-instance">
   <ConfigurationSetType>LinuxProvisioningConfiguration</ConfigurationSetType>
   <HostName>ovf-host</HostName>
   <UserName>ovfuser</UserName>
   <DisableSshPasswordAuthentication>true</DisableSshPasswordAuthentication>
   <CustomData>IyEvYmluL3NoCmVjaG8gaGkK</CustomData>
   <SSH><PublicKeys><PublicKey>
     <Fingerprint>ABCDEF</Fingerprint>
     <Path>/home/ovfuser/.ssh/authorized_keys</Path>
   </PublicKey></PublicKeys></SSH>
  </LinuxProvisioningConfigurationSet>
 </wa:ProvisioningSection>
 <wa:PlatformSettingsSection>
  <wa:Version>1.0</wa:Version>
  <PlatformSettings xmlns="http://schemas.microsoft.com/windowsazure"
   xmlns:i="http://www.w3.org/2001/XMLSchema-instance">
   <PreprovisionedVm>false</PreprovisionedVm>
  </PlatformSettings>
 </wa:PlatformSettingsSection>
</Environment>
EOF

    python3 "$(dirname "$0")/azcrawl.py" "$CRAWL"

    for case in nothing no-lease no-uuid imds-only imds-pwauth imds-userdata \
        bad-userdata iso-wins cached-ddir unmountable seed-dir report-fails \
        negotiated previous-iid random-seed gen1 proxy-agent pps-savable \
        pps-running pps-osdisk pps-unknown pps-no-lease; do
        az_crawl "$case"
    done
fi

# The datasource search, end to end: a seed directory on disk, the instance
# directory it produces, and the log lines that name both.
NCD="$WORK/nocloud"

# Only the lines both sides claim to emit, with the per-implementation root
# folded away so the two logs can be compared literally.
# The reporting handler writes through the log, so the check-cache and
# per-datasource scopes show up here too; their durations are masked.
NC_LINES="grep -E 'Loaded datasource|targeting instance id|Exiting\. datasource|No local datasource found|handlers\.py|stages\.py\[DEBUG\]: (no cache found|restored from|cache invalid)'"

# On a Hyper-V guest both sides read /etc/cloud/cloud.cfg.d/10-azure-kvp.cfg and
# register the `hyperv` handler, which then fails to open the root-owned pool.
# What each side says about that is not a parity surface: the port warns once
# per event, synchronously, while upstream warns once per drained batch from a
# daemon thread that is killed at exit unless `reporting.flush_events()` is
# reached -- measured on the VM, an unflushed run loses all of them (deviation
# 134). The events themselves are compared; the transport's complaints are not.
NC_DROP="/kvp pool file|posting events to kvp|flushing remaining events/d; "

setup_nocloud() {
    rm -rf "${NCD:?}"
    for impl in py rs; do
        root="$NCD/$impl"
        mkdir -p "$root/var/data" "$root/run"
        {
            echo "datasource_list: [ NoCloud, None ]"
            echo "def_log_file: $root/cloud-init.log"
            echo "log_cfgs:"
            echo "  - |"
            echo "$LOG_INI" | sed 's/^/    /'
            echo "    args=('$root/cloud-init.log', 'a', 'UTF-8')"
            echo "system_info:"
            echo "  paths:"
            echo "    cloud_dir: $root/var"
            echo "    run_dir: $root/run"
        } >"$root/cfg.yaml"
    done
}

# `python3 -m cloudinit.cmd.main` cannot be used here either (COMPAT.md B29).
run_nocloud() {
    label=$1
    env_pre=$2
    run_pair "$label" \
        "cd /tmp && $env_pre CLOUD_CFG='$NCD/py/cfg.yaml' /usr/bin/cloud-init init --local >/dev/null 2>&1; rc=\$?; $NC_LINES '$NCD/py/cloud-init.log' 2>/dev/null | sed -E '$NC_DROP s/\(duration: [0-9.]+s\)/(duration: Xs)/; s/^[0-9-]+ [0-9:,]+ - //; s|$NCD/py|ROOT|g'; exit \$rc" \
        "cd /tmp && $env_pre CLOUD_CFG='$NCD/rs/cfg.yaml' '$CI_RS' init --local >/dev/null 2>&1; rc=\$?; $NC_LINES '$NCD/rs/cloud-init.log' 2>/dev/null | sed -E '$NC_DROP s/\(duration: [0-9.]+s\)/(duration: Xs)/; s/^[0-9-]+ [0-9:,]+ - //; s|$NCD/rs|ROOT|g'; exit \$rc"
}

# `data/python-version` is upstream's alone (deviation 48); the datasource cache
# is at the same path in both but holds JSON here (deviation 67); `run/.impl` is
# the port's alone (deviation 103).
compare_nocloud_tree() {
    for impl in py rs; do
        (cd "$NCD/$impl" && find var run -printf '%y %m %p\n') |
            grep -Ev 'python-version|run/\.impl' |
            sort >"$NCD/$impl.tree"
    done
    if diff -u "$NCD/py.tree" "$NCD/rs.tree" >"$WORK/nc.diff" 2>&1; then
        pass=$((pass + 1))
        printf 'ok   %s\n' "$1"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n' "$1"
        sed 's/^/  /' "$WORK/nc.diff"
    fi
}

# The four fields that describe the interpreter and the C library it was linked
# against have no answer in the port (COMPAT.md deviation 66).
compare_instance_data() {
    name=$1
    for impl in py rs; do
        {
            sed -E "s|$NCD/$impl|ROOT|g" "$NCD/$impl/run/$name" |
                grep -Ev '"(python|python_version|system_platform)":|"platform": "Linux'
            echo "cloud-id: $(cat "$NCD/$impl/run/cloud-id" 2>/dev/null)"
            echo "cloud-id ->" "$(basename "$(readlink "$NCD/$impl/run/cloud-id" 2>/dev/null)" 2>/dev/null)"
        } >"$NCD/$impl.data" 2>/dev/null
    done
    if diff -u "$NCD/py.data" "$NCD/rs.data" >"$WORK/nc.diff" 2>&1; then
        pass=$((pass + 1))
        printf 'ok   %s\n' "cloud-init init --local ($name)"
    else
        fail=$((fail + 1))
        printf 'FAIL %s\n' "cloud-init init --local ($name)"
        sed 's/^/  /' "$WORK/nc.diff"
    fi
}

setup_nocloud
run_nocloud "cloud-init init --local (no nocloud seed)" ""

setup_nocloud
for impl in py rs; do
    mkdir -p "$NCD/$impl/var/seed/nocloud"
    printf 'instance-id: iid-local01\nlocal-hostname: me\n' \
        >"$NCD/$impl/var/seed/nocloud/meta-data"
    printf '#cloud-config\n' >"$NCD/$impl/var/seed/nocloud/user-data"
done
run_nocloud "cloud-init init --local (nocloud seed directory)" ""
compare_nocloud_tree "cloud-init init --local (nocloud state tree)"
compare_instance_data "instance-data.json"
compare_instance_data "instance-data-sensitive.json"

# The seed directory only counts as one when `user-data` is there too.
setup_nocloud
for impl in py rs; do
    mkdir -p "$NCD/$impl/var/seed/nocloud"
    printf 'instance-id: iid-local01\n' >"$NCD/$impl/var/seed/nocloud/meta-data"
done
run_nocloud "cloud-init init --local (nocloud seed without user-data)" ""

# `nocloud-net` is a network datasource, so the local stage must not take it.
setup_nocloud
run_nocloud "cloud-init init --local (ds=nocloud-net on the command line)" \
    "DEBUG_PROC_CMDLINE='ds=nocloud-net'"

# The second boot restores the datasource from the cache rather than crawling
# the seed again, and the run-directory instance id short-circuits the check.
setup_nocloud
for impl in py rs; do
    mkdir -p "$NCD/$impl/var/seed/nocloud"
    printf 'instance-id: iid-local01\nlocal-hostname: me\ndsmode: net\n' \
        >"$NCD/$impl/var/seed/nocloud/meta-data"
    printf '#cloud-config\n' >"$NCD/$impl/var/seed/nocloud/user-data"
done
run_nocloud "cloud-init init --local (first boot writes the cache)" ""
run_nocloud "cloud-init init --local (second boot restores the cache)" ""
compare_nocloud_tree "cloud-init init --local (state tree after two boots)"

# With the run directory cleared, the cache is only good if the datasource can
# still confirm the instance id it holds.
for impl in py rs; do rm -f "$NCD/$impl/run/.instance-id"; done
run_nocloud "cloud-init init --local (third boot rechecks the cache)" ""

# A seed that no longer matches the cached instance id invalidates it.
for impl in py rs; do
    rm -f "$NCD/$impl/run/.instance-id"
    printf 'instance-id: iid-local02\nlocal-hostname: me\ndsmode: net\n' \
        >"$NCD/$impl/var/seed/nocloud/meta-data"
done
run_nocloud "cloud-init init --local (seed no longer matches the cache)" ""

# A local seedfrom on the kernel command line, with the short metadata keys.
# A local seedfrom implies `dsmode=local`, so the command line has to override
# it: otherwise both sides carry on into the init modules, and upstream's are
# real -- it would set this host's hostname and write to /etc. The module
# engine is compared through `dump-modules` instead (COMPAT.md deviation 86).
setup_nocloud
for impl in py rs; do
    mkdir -p "$NCD/$impl/seed"
    printf 'instance-id: iid-cmdline\n' >"$NCD/$impl/seed/meta-data"
    printf '#cloud-config\n' >"$NCD/$impl/seed/user-data"
done
run_pair "cloud-init init --local (ds=nocloud;s= on the command line)" \
    "cd /tmp && DEBUG_PROC_CMDLINE=\"ds=nocloud;s=$NCD/py/seed/;i=iid-cmdline;dsmode=net\" CLOUD_CFG='$NCD/py/cfg.yaml' /usr/bin/cloud-init init --local >/dev/null 2>&1; rc=\$?; $NC_LINES '$NCD/py/cloud-init.log' 2>/dev/null | sed -E '$NC_DROP s/\(duration: [0-9.]+s\)/(duration: Xs)/; s/^[0-9-]+ [0-9:,]+ - //; s|$NCD/py|ROOT|g'; exit \$rc" \
    "cd /tmp && DEBUG_PROC_CMDLINE=\"ds=nocloud;s=$NCD/rs/seed/;i=iid-cmdline;dsmode=net\" CLOUD_CFG='$NCD/rs/cfg.yaml' '$CI_RS' init --local >/dev/null 2>&1; rc=\$?; $NC_LINES '$NCD/rs/cloud-init.log' 2>/dev/null | sed -E '$NC_DROP s/\(duration: [0-9.]+s\)/(duration: Xs)/; s/^[0-9-]+ [0-9:,]+ - //; s|$NCD/rs|ROOT|g'; exit \$rc"

# --- update events -----------------------------------------------------------
sec "update-events"
# `cloudinit.event` and `stages.update_event_enabled`: what an `updates:` block
# turns into, and the yes/no each datasource class gives for one event. The
# harness prints every WARNING it logged, because those reach
# `status.json`'s `recoverable_errors`.
EV_PY="$(cd "$(dirname "$0")" && pwd)/events.py"
EV_RS="$TARGET/examples/dump-events"
if [ -x "$EV_RS" ]; then
    EV_RS="$(cd "$(dirname "$EV_RS")" && pwd)/dump-events"
    EV="$WORK/events"
    mkdir -p "$EV/cloud"

    echo '{}' >"$EV/plain.json"
    echo '{"updates": {}}' >"$EV/noscope.json"
    echo '{"updates": {"network": {"when": ["boot"]}}}' >"$EV/boot.json"
    echo '{"updates": {"network": {"when": []}}}' >"$EV/empty.json"
    echo '{"updates": {"storage": {"when": ["boot"]}}}' >"$EV/badscope.json"
    echo '{"updates": {"network": {"when": ["boot", "bogus"]}}}' >"$EV/badtype.json"
    echo '{"updates": {"network": {"when": [1]}}}' >"$EV/inttype.json"
    echo '{"updates": {"network": {"when": [null]}}}' >"$EV/nulltype.json"
    # Python iterates a string by character and a mapping by key, so neither is
    # rejected for being the wrong type.
    echo '{"updates": {"network": {"when": "boot"}}}' >"$EV/strwhen.json"
    echo '{"updates": {"network": {"when": {"boot": 1}}}}' >"$EV/dictwhen.json"
    cat >"$EV/multi.json" <<'EOF'
{"updates": {"network": {"when": ["boot-new-instance", "hotplug", "boot-legacy"]}}}
EOF
    cat >"$EV/twoscope.json" <<'EOF'
{"updates": {"network": {"when": ["boot"]}, "storage": {"when": ["boot"]}}}
EOF

    EV_CFGS="plain noscope boot empty badscope badtype inttype nulltype \
strwhen dictwhen multi twoscope"
    for cfg in $EV_CFGS; do
        run_pair "updates block $cfg" \
            "cd /tmp && python3 '$EV_PY' convert '$EV/$cfg.json'" \
            "cd /tmp && '$EV_RS' convert '$EV/$cfg.json'"
    done

    # Every datasource the port has, against every event: the class defaults,
    # with and without a `hotplug.enabled` file naming the network scope.
    EV_CLASSES="DataSourceNoCloud DataSourceNoCloudNet DataSourceNone \
DataSourceConfigDrive DataSourceLXD DataSourceOpenStack DataSourceGCE \
DataSourceEc2"
    for hotplug in absent present; do
        if [ "$hotplug" = present ]; then
            echo '{"scopes": ["network"]}' >"$EV/cloud/hotplug.enabled"
        fi
        for cls in $EV_CLASSES; do
            for ev in boot boot-new-instance boot-legacy hotplug; do
                run_pair \
                    "update_event_enabled $cls defaults $ev hotplug=$hotplug" \
                    "cd /tmp && python3 '$EV_PY' enabled '$cls' \
'$EV/plain.json' '$ev' '$EV/cloud'" \
                    "cd /tmp && '$EV_RS' enabled '$cls' '$EV/plain.json' \
'$ev' '$EV/cloud'"
            done
        done
    done
    rm -f "$EV/cloud/hotplug.enabled"

    # A scope named in user-data replaces the datasource's default for it
    # whole, so `when: []` denies everything and a typo in the list does too.
    for cls in DataSourceNone DataSourceEc2 DataSourceGCE; do
        for cfg in boot empty badtype; do
            for ev in boot boot-new-instance hotplug; do
                run_pair "update_event_enabled $cls $cfg $ev" \
                    "cd /tmp && python3 '$EV_PY' enabled '$cls' \
'$EV/$cfg.json' '$ev' '$EV/cloud'" \
                    "cd /tmp && '$EV_RS' enabled '$cls' '$EV/$cfg.json' \
'$ev' '$EV/cloud'"
            done
        done
    done
fi

# --- ds-identify -------------------------------------------------------------
sec "ds-identify"
# The generator's datasource pre-flight. Unlike everything above, the reference
# implementation here is a POSIX shell script rather than Python, and it is
# entirely filesystem-and-text driven: PATH_ROOT re-roots every path it reads,
# so the whole decision table can be exercised from fixtures. dsidentify.sh
# prints the exit code, the generated /run/cloud-init/cloud.cfg, the cached
# result and the debug log, so a divergence anywhere shows up as a diff.
DSI_SH_IMPL="${DSI_SH_IMPL:-/usr/lib/cloud-init/ds-identify}"
DSI_DRIVER="$(cd "$(dirname "$0")" && pwd)/dsidentify.sh"
DSI_RS="$TARGET/ds-identify"
if [ -x "$DSI_SH_IMPL" ] && [ -x "$DSI_RS" ]; then
    DSI_RS="$(cd "$(dirname "$DSI_RS")" && pwd)/ds-identify"
    DSI="$WORK/dsi"
    mkdir -p "$DSI"

    # A bare fixture root. /proc/uptime is deliberately absent so that both
    # implementations report the same "unavailable" uptime.
    dsi_root() {
        dsi_r="$DSI/$1"
        rm -rf "$dsi_r"
        mkdir -p "$dsi_r/etc/cloud/cloud.cfg.d" "$dsi_r/var/lib/cloud" \
            "$dsi_r/proc" "$dsi_r/sys/class/dmi/id" "$dsi_r/sys/class/block"
        : >"$dsi_r/proc/cmdline"
    }

    dsi_cmdline() {
        printf '%s\n' "$2" >"$DSI/$1/proc/cmdline"
    }

    dsi_pair() {
        dsi_label=$1
        dsi_name=$2
        shift 2
        run_pair "ds-identify $dsi_label" \
            "cd /tmp && sh '$DSI_DRIVER' '$DSI_SH_IMPL' '$DSI/w-sh' \
'$DSI/$dsi_name' $*" \
            "cd /tmp && sh '$DSI_DRIVER' '$DSI_RS' '$DSI/w-rs' \
'$DSI/$dsi_name' $*"
    }

    # Nothing anywhere: the default policy disables cloud-init.
    dsi_root empty
    dsi_pair "empty root" empty

    # DI_MAIN=print_info dumps collect_info's twenty variables, which is the
    # whole reader layer (uname, virt, dmi, blkid, cmdline) in one comparison.
    run_pair "ds-identify print_info" \
        "cd /tmp && DI_MAIN=print_info sh '$DSI_DRIVER' '$DSI_SH_IMPL' \
'$DSI/w-sh' '$DSI/empty'" \
        "cd /tmp && DI_MAIN=print_info sh '$DSI_DRIVER' '$DSI_RS' \
'$DSI/w-rs' '$DSI/empty'"

    # DI_MAIN is a side-load hook upstream; the port refuses anything it does
    # not implement, so only the two names it does implement are compared.
    run_pair "ds-identify noop" \
        "cd /tmp && DI_MAIN=noop sh '$DSI_DRIVER' '$DSI_SH_IMPL' \
'$DSI/w-sh' '$DSI/empty'" \
        "cd /tmp && DI_MAIN=noop sh '$DSI_DRIVER' '$DSI_RS' \
'$DSI/w-rs' '$DSI/empty'"

    # Every mode, from ds-identify.cfg.
    for pol in disabled enabled search report; do
        dsi_root "pol-$pol"
        echo "policy: $pol" >"$DSI/pol-$pol/etc/cloud/ds-identify.cfg"
        dsi_pair "policy: $pol" "pol-$pol"
    done

    # ... and the same four from the kernel command line, which is read after
    # the config file and therefore wins.
    for pol in disabled enabled search report; do
        dsi_root "kpol-$pol"
        echo "policy: search" >"$DSI/kpol-$pol/etc/cloud/ds-identify.cfg"
        dsi_cmdline "kpol-$pol" "ro root=/dev/sda1 ci.di.policy=$pol"
        dsi_pair "ci.di.policy=$pol overrides the config file" "kpol-$pol"
    done

    # The found/maybe/notfound knobs, including the values parse_policy warns
    # about and the trailing-garbage forms.
    dsi_i=0
    for pol in \
        "search,found=all,maybe=none,notfound=disabled" \
        "search,found=first,maybe=all,notfound=enabled" \
        "search,notfound=enabled" \
        "search,found=bogus" \
        "search,maybe=bogus" \
        "search,notfound=bogus" \
        "bogusmode" \
        "search,found" \
        "search,,,," \
        "" \
        "report,found=first,maybe=all,notfound=enabled"; do
        dsi_i=$((dsi_i + 1))
        dsi_root "pk-$dsi_i"
        printf 'policy: "%s"\n' "$pol" >"$DSI/pk-$dsi_i/etc/cloud/ds-identify.cfg"
        dsi_pair "policy '$pol'" "pk-$dsi_i"
    done

    # A named datasource skips the search entirely and writes a one-entry list
    # with no trailing None.
    for ds in Ec2 NoCloud OpenStack None Bogus; do
        dsi_root "dsn-$ds"
        echo "datasource: $ds" >"$DSI/dsn-$ds/etc/cloud/ds-identify.cfg"
        dsi_pair "datasource: $ds" "dsn-$ds"
    done

    # The three command-line spellings, the `;`-truncation of `ds=nocloud;s=`,
    # and a bare token with no `=` at all.
    dsi_i=0
    for kv in \
        "ci.ds=NoCloud" \
        "ci.datasource=Ec2" \
        "ds=nocloud" \
        "ds=nocloud;s=http://10.0.0.1/seed/" \
        "ds=nocloud-net;s=http://10.0.0.1/;h=host" \
        "ds" \
        "ci.ds="; do
        dsi_i=$((dsi_i + 1))
        dsi_root "kds-$dsi_i"
        dsi_cmdline "kds-$dsi_i" "BOOT_IMAGE=/vmlinuz $kv quiet splash"
        dsi_pair "cmdline $kv" "kds-$dsi_i"
    done

    # `cc:{...}end_cc` on the command line, which is scraped with a chain of
    # parameter expansions rather than parsed.
    dsi_i=0
    for cc in \
        "cc:{'datasource_list': ['NoCloud']}end_cc" \
        "cc:{'datasource_list': [ NoCloud, None ]}end_cc" \
        "cc:{'datasource_list': [\"Ec2\", \"None\"]}end_cc" \
        "cc:{'datasource_list': []}end_cc" \
        "cc:{'datasource_list': [ Ec2 ], 'other': 1}end_cc"; do
        dsi_i=$((dsi_i + 1))
        dsi_root "kcc-$dsi_i"
        dsi_cmdline "kcc-$dsi_i" "root=/dev/vda1 $cc ro"
        dsi_pair "cmdline cc #$dsi_i" "kcc-$dsi_i"
    done

    # datasource_list from the config files: cloud.cfg, a drop-in, both (the
    # last grep hit wins), and the quoting and comment forms check_config has
    # to survive.
    dsi_root dl-cfg
    echo 'datasource_list: [ NoCloud, None ]' >"$DSI/dl-cfg/etc/cloud/cloud.cfg"
    dsi_pair "datasource_list in cloud.cfg" dl-cfg

    dsi_root dl-drop
    echo 'datasource_list: [ Ec2, None ]' \
        >"$DSI/dl-drop/etc/cloud/cloud.cfg.d/90_dpkg.cfg"
    dsi_pair "datasource_list in a drop-in" dl-drop

    dsi_root dl-both
    echo 'datasource_list: [ NoCloud, None ]' >"$DSI/dl-both/etc/cloud/cloud.cfg"
    echo 'datasource_list: [ Ec2, None ]' \
        >"$DSI/dl-both/etc/cloud/cloud.cfg.d/90_dpkg.cfg"
    dsi_pair "the last datasource_list wins" dl-both

    dsi_root dl-order
    echo 'datasource_list: [ NoCloud ]' \
        >"$DSI/dl-order/etc/cloud/cloud.cfg.d/10_first.cfg"
    echo 'datasource_list: [ Ec2 ]' \
        >"$DSI/dl-order/etc/cloud/cloud.cfg.d/20_second.cfg"
    dsi_pair "drop-ins are read in glob order" dl-order

    dsi_i=0
    for line in \
        'datasource_list: [NoCloud, None]' \
        'datasource_list : [ NoCloud, None ]' \
        '"datasource_list": [ NoCloud, None ]' \
        "'datasource_list': [ NoCloud, None ]" \
        'datasource_list: [ NoCloud, None ] # trailing comment' \
        '#datasource_list: [ Ec2, None ]' \
        'datasource_list: [ "NoCloud", "None" ]' \
        "datasource_list: [ 'NoCloud', 'None' ]" \
        'datasource_list: []' \
        'datasource_list: [ ]' \
        'datasource_list:' \
        'datasource_list: NoCloud' \
        'datasource_list: [ NoCloud,None,Ec2 ]'; do
        dsi_i=$((dsi_i + 1))
        dsi_root "dlq-$dsi_i"
        printf '%s\n' "$line" >"$DSI/dlq-$dsi_i/etc/cloud/cloud.cfg"
        dsi_pair "config line #$dsi_i" "dlq-$dsi_i"
    done

    # A multi-line flow sequence: only the first line is ever looked at.
    dsi_root dl-multi
    cat >"$DSI/dl-multi/etc/cloud/cloud.cfg" <<'EOF'
datasource_list: [
  NoCloud,
  None ]
EOF
    dsi_pair "a wrapped flow sequence" dl-multi

    # ds-identify.cfg itself: both keys, comments, quoting, and the "exists but
    # is not a file" error branch.
    dsi_root cfg-both
    cat >"$DSI/cfg-both/etc/cloud/ds-identify.cfg" <<'EOF'
# a comment
datasource: 'NoCloud'
policy: "search,found=first,maybe=all,notfound=enabled"
EOF
    dsi_pair "ds-identify.cfg with both keys" cfg-both

    dsi_root cfg-dir
    rm -f "$DSI/cfg-dir/etc/cloud/ds-identify.cfg"
    mkdir -p "$DSI/cfg-dir/etc/cloud/ds-identify.cfg"
    dsi_pair "ds-identify.cfg is a directory" cfg-dir

    # Seed directories, which are the checks that need no hardware at all.
    dsi_root seed-nocloud
    mkdir -p "$DSI/seed-nocloud/var/lib/cloud/seed/nocloud"
    : >"$DSI/seed-nocloud/var/lib/cloud/seed/nocloud/meta-data"
    : >"$DSI/seed-nocloud/var/lib/cloud/seed/nocloud/user-data"
    dsi_pair "nocloud seed dir" seed-nocloud

    dsi_root seed-nocloud-net
    mkdir -p "$DSI/seed-nocloud-net/var/lib/cloud/seed/nocloud-net"
    : >"$DSI/seed-nocloud-net/var/lib/cloud/seed/nocloud-net/meta-data"
    dsi_pair "nocloud-net seed dir" seed-nocloud-net

    dsi_root seed-nocloud-nomd
    mkdir -p "$DSI/seed-nocloud-nomd/var/lib/cloud/seed/nocloud"
    : >"$DSI/seed-nocloud-nomd/var/lib/cloud/seed/nocloud/user-data"
    dsi_pair "a seed dir without meta-data" seed-nocloud-nomd

    dsi_root seed-cd
    mkdir -p "$DSI/seed-cd/var/lib/cloud/seed/config_drive/openstack/latest"
    : >"$DSI/seed-cd/var/lib/cloud/seed/config_drive/openstack/latest/meta_data.json"
    dsi_pair "config_drive seed dir" seed-cd

    dsi_root seed-os
    mkdir -p "$DSI/seed-os/var/lib/cloud/seed/openstack/openstack/latest"
    : >"$DSI/seed-os/var/lib/cloud/seed/openstack/openstack/latest/meta_data.json"
    dsi_pair "openstack seed dir" seed-os

    dsi_root seed-lxd
    mkdir -p "$DSI/seed-lxd/var/lib/cloud/seed/nocloud-net"
    : >"$DSI/seed-lxd/var/lib/cloud/seed/nocloud-net/meta-data"
    echo 'datasource_list: [ LXD, NoCloud, None ]' \
        >"$DSI/seed-lxd/etc/cloud/cloud.cfg"
    dsi_pair "a short list with a seed dir" seed-lxd

    # DMI fixtures. get_dmi_field reads the directory when it exists and never
    # falls back to dmidecode, so these are hermetic.
    dsi_write_dmi() {
        printf '%s\n' "$3" >"$DSI/$1/sys/class/dmi/id/$2"
    }

    dsi_root dmi-ec2
    dsi_write_dmi dmi-ec2 sys_vendor "Amazon EC2"
    dsi_write_dmi dmi-ec2 product_name "t3.micro"
    dsi_pair "dmi sys_vendor Amazon EC2" dmi-ec2

    dsi_root dmi-ec2-uuid
    dsi_write_dmi dmi-ec2-uuid product_uuid "EC2E1916-9099-7CAF-FD21-012345ABCDEF"
    dsi_write_dmi dmi-ec2-uuid product_serial "ec2e1916-9099-7caf-fd21-012345abcdef"
    dsi_pair "dmi ec2 uuid heuristic" dmi-ec2-uuid

    dsi_root dmi-azure
    dsi_write_dmi dmi-azure chassis_asset_tag \
        "7783-7084-3265-9085-8269-3286-77"
    dsi_pair "dmi azure chassis asset tag" dmi-azure

    dsi_root dmi-gce
    dsi_write_dmi dmi-gce product_name "Google Compute Engine"
    dsi_pair "dmi gce product name" dmi-gce

    dsi_root dmi-gce-vendor
    dsi_write_dmi dmi-gce-vendor sys_vendor "Google"
    dsi_pair "dmi gce sys_vendor" dmi-gce-vendor

    dsi_root dmi-oracle
    dsi_write_dmi dmi-oracle chassis_asset_tag "OracleCloud.com"
    dsi_pair "dmi oracle chassis asset tag" dmi-oracle

    dsi_root dmi-openstack
    dsi_write_dmi dmi-openstack product_name "OpenStack Nova"
    dsi_pair "dmi openstack nova" dmi-openstack

    dsi_root dmi-opennebula
    dsi_write_dmi dmi-opennebula sys_vendor "OpenNebula"
    dsi_pair "dmi opennebula" dmi-opennebula

    dsi_root dmi-cloudsigma
    dsi_write_dmi dmi-cloudsigma product_name "CloudSigma"
    dsi_pair "dmi cloudsigma" dmi-cloudsigma

    dsi_root dmi-scaleway
    dsi_write_dmi dmi-scaleway sys_vendor "Scaleway"
    dsi_pair "dmi scaleway" dmi-scaleway

    dsi_root dmi-vultr
    dsi_write_dmi dmi-vultr sys_vendor "Vultr"
    dsi_pair "dmi vultr" dmi-vultr

    dsi_root dmi-hetzner
    dsi_write_dmi dmi-hetzner sys_vendor "Hetzner"
    dsi_pair "dmi hetzner" dmi-hetzner

    dsi_root dmi-exoscale
    dsi_write_dmi dmi-exoscale product_name "Exoscale"
    dsi_pair "dmi exoscale" dmi-exoscale

    dsi_root dmi-bigstep
    dsi_write_dmi dmi-bigstep sys_vendor "Bigstep"
    dsi_pair "dmi bigstep" dmi-bigstep

    dsi_root dmi-akamai
    dsi_write_dmi dmi-akamai sys_vendor "Akamai"
    dsi_pair "dmi akamai" dmi-akamai

    dsi_root dmi-vmware
    dsi_write_dmi dmi-vmware sys_vendor "VMware, Inc."
    dsi_write_dmi dmi-vmware product_name "VMware Virtual Platform"
    dsi_pair "dmi vmware" dmi-vmware

    dsi_root dmi-cloudcix
    dsi_write_dmi dmi-cloudcix sys_vendor "CloudCIX"
    dsi_pair "dmi cloudcix" dmi-cloudcix

    dsi_root dmi-nwcs
    dsi_write_dmi dmi-nwcs sys_vendor "NWCS"
    dsi_pair "dmi nwcs" dmi-nwcs

    dsi_root dmi-unavailable
    rm -rf "$DSI/dmi-unavailable/sys/class/dmi/id"
    dsi_pair "no dmi directory at all" dmi-unavailable

    # A DMI hit narrowed to a single-entry datasource_list takes the shortcut
    # that writes the entry with no trailing None.
    dsi_root dmi-one
    dsi_write_dmi dmi-one sys_vendor "Amazon EC2"
    echo 'datasource_list: [ Ec2 ]' >"$DSI/dmi-one/etc/cloud/cloud.cfg"
    dsi_pair "single-entry list shortcut" dmi-one

    dsi_root dmi-two-none
    dsi_write_dmi dmi-two-none sys_vendor "Amazon EC2"
    echo 'datasource_list: [ Ec2, None ]' >"$DSI/dmi-two-none/etc/cloud/cloud.cfg"
    dsi_pair "two-entry list ending in None" dmi-two-none

    # Ec2 strict_id: the only check that emits an extra config stanza.
    for sid in true false warn "warn,3" bogus; do
        dsi_root ec2-strict
        dsi_write_dmi ec2-strict sys_vendor "Unknown Vendor"
        echo 'datasource_list: [ Ec2, None ]' >"$DSI/ec2-strict/etc/cloud/cloud.cfg"
        printf 'datasource:\n  Ec2:\n    strict_id: %s\n' "$sid" \
            >"$DSI/ec2-strict/etc/cloud/cloud.cfg.d/99_ec2.cfg"
        dsi_pair "ec2 strict_id: $sid" ec2-strict
    done

    # manual_cache_clean: an instance directory with the marker file short
    # circuits the whole search.
    dsi_root manual
    mkdir -p "$DSI/manual/var/lib/cloud/instance"
    : >"$DSI/manual/var/lib/cloud/instance/manual-clean"
    dsi_pair "manual-clean marker" manual

    dsi_root manual-nodir
    : >"$DSI/manual-nodir/var/lib/cloud/instance"
    dsi_pair "instance is a file, not a directory" manual-nodir

    # A stale result is reused unless --force is passed. The driver wipes the
    # run directory each time, so this runs the pair twice by hand.
    run_pair "ds-identify --force" \
        "cd /tmp && sh '$DSI_DRIVER' '$DSI_SH_IMPL' '$DSI/w-sh' \
'$DSI/empty' --force" \
        "cd /tmp && sh '$DSI_DRIVER' '$DSI_RS' '$DSI/w-rs' \
'$DSI/empty' --force"

    # The two disable paths that a fixture can reach. The third, the
    # /etc/cloud/cloud-init.disabled marker, is hardcoded rather than
    # PATH_ROOT-relative (docs/COMPAT.md B55), so it cannot be tested here.
    dsi_root disabled-cmdline
    dsi_cmdline disabled-cmdline "root=/dev/vda1 cloud-init=disabled ro"
    dsi_pair "cloud-init=disabled on the kernel command line" disabled-cmdline

    dsi_root disabled-substr
    dsi_cmdline disabled-substr "root=/dev/vda1 xcloud-init=disabledy ro"
    dsi_pair "cloud-init=disabled matched as a substring" disabled-substr

    run_pair "ds-identify KERNEL_CMDLINE=cloud-init=disabled" \
        "cd /tmp && KERNEL_CMDLINE=cloud-init=disabled sh '$DSI_DRIVER' \
'$DSI_SH_IMPL' '$DSI/w-sh' '$DSI/empty'" \
        "cd /tmp && KERNEL_CMDLINE=cloud-init=disabled sh '$DSI_DRIVER' \
'$DSI_RS' '$DSI/w-rs' '$DSI/empty'"

    # The env variable is compared for equality, not searched, so a longer
    # command line in it does not disable anything.
    run_pair "ds-identify KERNEL_CMDLINE with more than the marker" \
        "cd /tmp && KERNEL_CMDLINE='ro cloud-init=disabled' sh '$DSI_DRIVER' \
'$DSI_SH_IMPL' '$DSI/w-sh' '$DSI/empty'" \
        "cd /tmp && KERNEL_CMDLINE='ro cloud-init=disabled' sh '$DSI_DRIVER' \
'$DSI_RS' '$DSI/w-rs' '$DSI/empty'"

    # DEBUG_LEVEL gates what reaches the log at all.
    for lvl in 0 1 2 3; do
        run_pair "ds-identify DEBUG_LEVEL=$lvl" \
            "cd /tmp && DEBUG_LEVEL=$lvl sh '$DSI_DRIVER' '$DSI_SH_IMPL' \
'$DSI/w-sh' '$DSI/dl-cfg'" \
            "cd /tmp && DEBUG_LEVEL=$lvl sh '$DSI_DRIVER' '$DSI_RS' \
'$DSI/w-rs' '$DSI/dl-cfg'"
    done
fi

# --- cloud-init-generator ----------------------------------------------------
sec "cloud-init-generator"
# The other half of the early-boot pair: the systemd generator turns
# ds-identify's exit code into a symlink and a flag file. It has no
# configuration, so generator.sh puts the fixture where the program already
# looks by way of an unprivileged mount namespace; see the comments there.
# Skipped when that namespace is unavailable, and skipped when running as root,
# where a stray write would land on the live host instead of the fixture.
GEN_SH_IMPL="${GEN_SH_IMPL:-/usr/lib/systemd/system-generators/cloud-init-generator}"
GEN_DRIVER="$(cd "$(dirname "$0")" && pwd)/generator.sh"
GEN_RS="$TARGET/cloud-init-generator"
if [ -x "$GEN_SH_IMPL" ] && [ -x "$GEN_RS" ] &&
    [ "$(id -u)" -ne 0 ] &&
    unshare -Urm true 2>/dev/null; then
    GEN_RS="$(cd "$(dirname "$GEN_RS")" && pwd)/cloud-init-generator"
    GEN="$WORK/gen"
    mkdir -p "$GEN"

    gen_pair() {
        gen_label=$1
        shift
        run_pair "cloud-init-generator $gen_label" \
            "cd /tmp && sh '$GEN_DRIVER' '$GEN_SH_IMPL' '$GEN/w-sh' $*" \
            "cd /tmp && sh '$GEN_DRIVER' '$GEN_RS' '$GEN/w-rs' $*"
    }

    # The decision table. 0 enables, 1 and 2 disable with different wording,
    # and anything else is refused with exit 3 and no change to the boot.
    gen_pair "ds rc 0" ds=0
    gen_pair "ds rc 1" ds=1
    gen_pair "ds rc 2" ds=2
    for rc in 3 4 9 66 255; do
        gen_pair "ds rc $rc" "ds=$rc"
    done

    # What the fail-open branch actually does, which is nothing: the script
    # sets ds=0 and forgets to return, so the run it meant to skip overwrites
    # the value and the machine ends up with no decision at all
    # (docs/COMPAT.md B56). 127 and 126 are what a shell reports for an
    # ds-identify that is absent and one it may not execute.
    gen_pair "ds missing" ds=missing
    gen_pair "ds not executable" ds=noexec

    # ds-identify's own output belongs to the journal, not to the generator.
    gen_pair "ds writes to stdout and stderr" ds=noisy

    # Enabling is idempotent, and replaces whatever was there before: a link to
    # the wrong target, a dangling link, or a regular file in the way.
    for st in good stale dangling file; do
        gen_pair "enable over $st link" ds=0 "link=$st"
    done

    # Disabling removes the link, but `[ -f ]` follows it, so a dangling link
    # reads as "already disabled" and survives.
    for st in good stale dangling file; do
        gen_pair "disable over $st link" ds=1 "link=$st"
    done

    # An empty wants directory is the state after a previous disable.
    gen_pair "enable with empty wants dir" ds=0 wants=yes
    gen_pair "disable with empty wants dir" ds=1 wants=yes

    # The flag files in /run/cloud-init are swapped, not merely written.
    for fl in enabled disabled both; do
        gen_pair "enable with $fl flag" ds=0 "flag=$fl"
        gen_pair "disable with $fl flag" ds=1 "flag=$fl"
        gen_pair "refuse with $fl flag" ds=9 "flag=$fl"
    done

    # A log file that cannot be truncated kills the shell outright, because a
    # redirection failure on the special builtin `:` is fatal in dash, so the
    # /dev/kmsg fallback below it is unreachable (docs/COMPAT.md B57). No
    # decision is made and nothing is logged anywhere.
    for rc in 0 1 2 9; do
        gen_pair "unwritable log with ds rc $rc" "ds=$rc" log=dir
    done

    # /run/cloud-init not existing yet is the normal case on a real boot.
    gen_pair "fresh run dir, enable" ds=0 run=fresh
    gen_pair "fresh run dir, disable" ds=1 run=fresh
    gen_pair "fresh run dir, refuse" ds=9 run=fresh
    gen_pair "fresh run dir, ds missing" ds=missing run=fresh
fi

# --- cloud-init devel net-convert
#
# The renderer's contract is the exact bytes of /etc/netplan/50-cloud-init.yaml,
# so every case compares the whole output tree, modes included, rather than a
# summary. `-O netplan` is the only output kind implemented so far; the header
# it prepends comes from the distro, so the distro is a variable too.

NC_SH="$(dirname "$0")/netconvert.sh"
NET_D="$WORK/net"
mkdir -p "$NET_D"

nc_write() {
    cat >"$NET_D/$1.yaml"
}

nc_pair() {
    label=$1
    name=$2
    quiet=${3:-show}
    mkdir -p "$WORK/nc-py" "$WORK/nc-rs"
    run_pair "net-convert $label" \
        "sh $NC_SH $PY_CLOUD_INIT $WORK/nc-py $NET_D/$name.yaml ubuntu $quiet" \
        "sh $NC_SH $TARGET/cloud-init $WORK/nc-rs $NET_D/$name.yaml ubuntu $quiet"
}

# Same fixture, a different `-D`. Everything but ubuntu/debian/raspberry-pi-os
# has either its own header or no netplan renderer at all.
nc_distro() {
    name=$1
    distro=$2
    quiet=${3:-show}
    mkdir -p "$WORK/nc-py" "$WORK/nc-rs"
    run_pair "net-convert $name -D $distro" \
        "sh $NC_SH $PY_CLOUD_INIT $WORK/nc-py $NET_D/$name.yaml $distro $quiet" \
        "sh $NC_SH $TARGET/cloud-init $WORK/nc-rs $NET_D/$name.yaml $distro $quiet"
}

nc_write v1-dhcp4 <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      mac_address: 'aa:bb:cc:dd:ee:ff'
      subnets:
        - type: dhcp4
EOF

nc_write v1-dhcp4-metric <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: dhcp4
          metric: 100
EOF

nc_write v1-dhcp6 <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: dhcp6
EOF

nc_write v1-slaac <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: ipv6_slaac
EOF

nc_write v1-static <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      mac_address: 'AA:BB:CC:DD:EE:FF'
      subnets:
        - type: static
          address: 192.168.1.2/24
          gateway: 192.168.1.1
EOF

nc_write v1-static-onlink <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
          gateway: 10.0.0.1
EOF

nc_write v1-static-netmask <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 10.0.0.2
          netmask: 255.255.0.0
EOF

nc_write v1-static-hostmask <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 10.0.0.2
          netmask: 0.0.0.255
EOF

nc_write v1-static6 <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static6
          address: '2001:db8::2/64'
          gateway: '2001:db8::1'
EOF

nc_write v1-dual-stack <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
          gateway: 192.168.1.1
        - type: static6
          address: '2001:db8::2/64'
          gateway: '2001:db8::1'
EOF

nc_write v1-no-mac <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
EOF

nc_write v1-subnet-routes <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
          routes:
            - destination: 10.0.0.0/8
              gateway: 192.168.1.254
              metric: 3
            - destination: 172.16.0.0/12
              gateway: 192.168.1.253
EOF

nc_write v1-default-route <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
    - type: route
      destination: default
      gateway: 192.168.1.1
EOF

nc_write v1-global-dns <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
    - type: nameserver
      address:
        - 8.8.8.8
        - 8.8.4.4
      search:
        - example.com
EOF

nc_write v1-iface-dns <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
    - type: nameserver
      interface: eth0
      address: 8.8.8.8
      search: example.com
EOF

nc_write v1-subnet-dns-string <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 192.168.1.2/24
          dns_nameservers: '8.8.8.8 8.8.4.4'
          dns_search: 'example.com example.org'
EOF

nc_write v1-mtu-device <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      mtu: 1492
      subnets:
        - type: static
          address: 192.168.1.2/24
EOF

nc_write v1-mtu-conflict <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      mtu: 1492
      subnets:
        - type: static
          address: 192.168.1.2/24
          mtu: 9000
EOF

nc_write v1-mtu-ipv6 <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static6
          address: '2001:db8::2/64'
          mtu: 1400
EOF

nc_write v1-accept-ra-true <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      accept-ra: true
      subnets:
        - type: dhcp6
EOF

nc_write v1-accept-ra-false <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      accept-ra: false
      subnets:
        - type: dhcp6
EOF

nc_write v1-keep-configuration <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      keep_configuration: true
      subnets:
        - type: dhcp4
EOF

nc_write v1-wakeonlan <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      wakeonlan: true
      subnets:
        - type: dhcp4
EOF

nc_write v1-vlan <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
    - type: vlan
      name: eth0.101
      vlan_id: 101
      vlan_link: eth0
      subnets:
        - type: static
          address: 192.168.101.2/24
EOF

nc_write v1-bond <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
    - type: physical
      name: eth1
    - type: bond
      name: bond0
      bond_interfaces:
        - eth0
        - eth1
      params:
        bond-mode: active-backup
        bond-miimon: 100
      subnets:
        - type: static
          address: 192.168.1.2/24
EOF

nc_write v1-bridge <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
    - type: bridge
      name: br0
      bridge_interfaces:
        - eth0
      params:
        bridge_stp: 'off'
        bridge_fd: 0
        bridge_pathcost:
          - 'eth0 50'
        bridge_portprio:
          - 'eth0 28'
      subnets:
        - type: static
          address: 192.168.1.2/24
EOF

nc_write v1-loopback <<'EOF'
network:
  version: 1
  config:
    - type: loopback
      name: lo
    - type: physical
      name: eth0
      subnets:
        - type: dhcp4
EOF

nc_write v2-simple <<'EOF'
network:
  version: 2
  ethernets:
    eth0:
      dhcp4: true
EOF

nc_write v2-static <<'EOF'
network:
  version: 2
  ethernets:
    eth0:
      match:
        macaddress: 'aa:bb:cc:dd:ee:ff'
      set-name: eth0
      addresses:
        - 192.168.1.2/24
      routes:
        - to: default
          via: 192.168.1.1
      nameservers:
        addresses:
          - 8.8.8.8
        search:
          - example.com
EOF

nc_write v2-bond-bridge-vlan <<'EOF'
network:
  version: 2
  ethernets:
    eth0: {}
    eth1: {}
  bonds:
    bond0:
      interfaces:
        - eth0
        - eth1
      parameters:
        mode: 802.3ad
        mii-monitor-interval: 100
  bridges:
    br0:
      interfaces:
        - bond0
      parameters:
        stp: false
  vlans:
    vlan101:
      id: 101
      link: br0
      addresses:
        - 192.168.101.2/24
EOF

nc_write v2-unwrapped <<'EOF'
version: 2
ethernets:
  eth0:
    dhcp4: true
    dhcp4-overrides:
      route-metric: 200
EOF

nc_write bad-no-version <<'EOF'
network:
  config: []
EOF

nc_write bad-unknown-type <<'EOF'
network:
  version: 1
  config:
    - type: quantum
      name: eth0
EOF

nc_write bad-address <<'EOF'
network:
  version: 1
  config:
    - type: physical
      name: eth0
      subnets:
        - type: static
          address: 'not-an-address'
EOF

if [ -x "$TARGET/cloud-init" ]; then
    for name in \
        v1-dhcp4 v1-dhcp4-metric v1-dhcp6 v1-slaac \
        v1-static v1-static-onlink v1-static-netmask v1-static-hostmask \
        v1-static6 v1-dual-stack v1-no-mac \
        v1-subnet-routes v1-default-route \
        v1-global-dns v1-iface-dns v1-subnet-dns-string \
        v1-mtu-device v1-mtu-conflict v1-mtu-ipv6 \
        v1-accept-ra-true v1-accept-ra-false v1-keep-configuration \
        v1-wakeonlan v1-vlan v1-bond v1-bridge v1-loopback \
        v2-simple v2-static v2-bond-bridge-vlan v2-unwrapped; do
        nc_pair "$name" "$name"
    done

    # Upstream raises and prints a Python traceback for these; only the exit
    # code and the absence of output are comparable.
    for name in bad-no-version bad-unknown-type bad-address; do
        nc_pair "$name" "$name" quiet
    done

    # The six distros with a netplan renderer config, each with its own header,
    # plus one with none and one with no distro module at all. The last two
    # end in a traceback upstream, so they run quiet.
    for distro in arch azurelinux debian mariner raspberry-pi-os; do
        nc_distro v1-static "$distro"
    done
    for distro in rhel alpine dragonfly; do
        nc_distro v1-static "$distro" quiet
    done
fi

# --- puppet ------------------------------------------------------------------
sec "cc-puppet"
#
# The whole module against a scripted machine: what each command prints, which
# calls fail, and which files already exist and what they hold. Nothing runs
# and nothing is written on either side -- the Python half stubs `subp.subp`,
# the `util` file helpers, `url_helper.readurl`, `temp_utils.tempdir`,
# `socket.getfqdn` and the two `Cloud`/`Distro` methods onto the same script --
# so what is compared is the log, the ordered list of things the module did,
# and what each file ended up holding.
#
# The module's own `get_config_value`, `install_puppet_aio` and
# `_manage_puppet_services` run as written on both sides; only the leaves are
# scripted. That matters, because the three `puppet config print` calls happen
# whether or not their answers are wanted (B96) and the backup is taken once
# per section rather than once per run (B93).
CCPP_PY="$(cd "$(dirname "$0")" && pwd)/ccpuppet.py"
CCPP_RS="$TARGET/examples/dump-cc-puppet"
if [ -x "$CCPP_RS" ] &&
   python3 -c 'import cloudinit.config.cc_puppet' 2>/dev/null; then
    CCPP_RS="$(cd "$(dirname "$CCPP_RS")" && pwd)/dump-cc-puppet"

    ccpp_case() {
        # `${2:-{}}` cannot be written in POSIX sh -- the brace closes the
        # expansion -- so the default is spelled out.
        ccpp_host=${2:-}
        printf '{"cfg": %s, "host": {%s}}\n' "$1" "$ccpp_host" \
            >>"$WORK/ccpp.cases"
    }
    # The three answers `puppet config print` gives on a stock agent, and a
    # `puppet.conf` that already exists. Most cases want both.
    ccpp_paths='"stdout": {"subp puppet config print config capture=True": "/etc/puppetlabs/puppet/puppet.conf\n", "subp puppet config print ssldir capture=True": "/etc/puppetlabs/puppet/ssl\n", "subp puppet config print csr_attributes capture=True": "/etc/puppetlabs/puppet/csr_attributes.yaml\n"}'
    ccpp_conf='"files": {"/etc/puppetlabs/puppet/puppet.conf": "[main]\norig = yes\n"}'
    ccpp_std="$ccpp_paths, $ccpp_conf"

    : >"$WORK/ccpp.cases"

    # --- the switch -----------------------------------------------------------
    ccpp_case '{}'
    ccpp_case '{"other": 1}'
    printf '{"name": "cc_puppet", "cfg": {}}\n' >>"$WORK/ccpp.cases"
    # An empty mapping still walks the whole module.
    ccpp_case '{"puppet": {}}' "$ccpp_std"
    ccpp_case '{"puppet": null}' "$ccpp_std"

    # --- install / version / install_type -------------------------------------
    ccpp_case '{"puppet": {"install": false}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install": false, "version": "7.12.0"}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install": true, "version": "7.12.0"}}' "$ccpp_std"
    # `get_cfg_option_str` stringifies, so a null version installs "None".
    ccpp_case '{"puppet": {"version": null}}' "$ccpp_std"
    ccpp_case '{"puppet": {"version": 7}}' "$ccpp_std"
    ccpp_case '{"puppet": {"version": ""}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install": "false"}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install": 0}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "unknown"}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "unknown", "exec": true}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": null}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "packages", "package_name": "puppet6"}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"package_name": "puppet6", "version": "6.1"}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"package_name": null}}' "$ccpp_std"

    # --- the package-name loop ------------------------------------------------
    # The first name failing falls through to the second; both failing is a
    # warning rather than an error.
    ccpp_case '{"puppet": {}}' \
        "$ccpp_std"', "failures": {"install_packages ['"'"'puppet-agent'"'"']": "no candidate"}'
    ccpp_case '{"puppet": {}}' \
        "$ccpp_std"', "failures": {"install_packages ['"'"'puppet-agent'"'"']": "no candidate", "install_packages ['"'"'puppet'"'"']": "no candidate either"}'
    ccpp_case '{"puppet": {"version": "7"}}' \
        "$ccpp_std"', "failures": {"install_packages [['"'"'puppet-agent'"'"', '"'"'7'"'"']]": "no candidate"}'
    # An explicit package_name gets no second chance.
    ccpp_case '{"puppet": {"package_name": "puppet6"}}' \
        "$ccpp_std"', "failures": {"install_packages ['"'"'puppet6'"'"']": "no candidate"}'

    # --- aio ------------------------------------------------------------------
    ccpp_case '{"puppet": {"install_type": "aio"}}' "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "aio", "version": "7.12.0"}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "aio", "collection": "puppet7"}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "aio", "cleanup": false}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "aio", "version": "7", "collection": "puppet7", "cleanup": false}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "aio", "aio_install_url": "https://example.invalid/install.sh"}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"install_type": "aio", "aio_install_url": "https://example.invalid/install.sh"}}' \
        "$ccpp_std"', "failures": {"readurl https://example.invalid/install.sh": "unreachable"}'
    ccpp_case '{"puppet": {"install_type": "aio"}}' \
        "$ccpp_std"', "failures": {"write_file /var/tmp/cloud-init/tmpdir/puppet-install mode=0700": "[Errno 13] Permission denied"}'
    # The AIO agent lives somewhere else, so `config print` is a different
    # command and the paths it prints are different.
    ccpp_case '{"puppet": {"install_type": "aio", "conf": {"agent": {"server": "p"}}}}' \
        '"stdout": {"subp /opt/puppetlabs/bin/puppet config print config capture=True": "/etc/puppetlabs/puppet/puppet.conf\n", "subp /opt/puppetlabs/bin/puppet config print ssldir capture=True": "/etc/puppetlabs/puppet/ssl\n", "subp /opt/puppetlabs/bin/puppet config print csr_attributes capture=True": "/etc/puppetlabs/puppet/csr_attributes.yaml\n"}, '"$ccpp_conf"

    # --- the three `config print` calls ---------------------------------------
    # They run whether or not their answers are used (B96), and what they print
    # is right-stripped.
    ccpp_case '{"puppet": {"conf_file": "/e/p.conf", "ssl_dir": "/e/ssl", "csr_attributes_path": "/e/csr.yaml", "conf": {"main": {"a": "1"}}}}' \
        '"files": {"/e/p.conf": "[main]\norig = yes\n"}'
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}}}}' \
        '"stdout": {"subp puppet config print config capture=True": "  /e/p.conf  \n\n", "subp puppet config print ssldir capture=True": "/e/ssl\n"}, "files": {"  /e/p.conf": "[main]\n"}'
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}}}}' \
        "$ccpp_conf"', "failures": {"subp puppet config print ssldir capture=True": "no such subcommand"}'
    ccpp_case '{"puppet": {"conf_file": null}}' "$ccpp_std"

    # --- conf: reading the existing file --------------------------------------
    ccpp_case '{"puppet": {"conf": {"main": {"server": "p.example.com"}}}}' \
        "$ccpp_std"
    # The file not being there at all.
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}}}}' "$ccpp_paths"
    # Every shape the "cleaning" step has an opinion about. A continuation
    # line does not survive it (B94).
    for ccpp_have in \
        '""' \
        '"[main]\n"' \
        '"[main]\nfoo = bar\n"' \
        '"[main]\nfoo: bar\nBAZ = 1\n"' \
        '"  [main]\n    foo = bar\n"' \
        '"[main]\nfoo = bar\n\tmore\n"' \
        '"# a comment\n[main]\nfoo = bar\n; another\n"' \
        '"a = 1\n"' \
        '"[main]\n[main]\n"' \
        '"[main]\na = 1\na = 2\n"' \
        '"[DEFAULT]\nd = 1\n[main]\na = 2\n"' \
        '"[main]\nnovalue\n"' \
        '"[main]\nfoo = 100%%s\n"' \
        '"[main\n"' \
        '"[a]b]\nx = 1\n"'; do
        ccpp_case '{"puppet": {"conf": {"agent": {"a": "1"}}}}' \
            "$ccpp_paths"', "files": {"/etc/puppetlabs/puppet/puppet.conf": '"$ccpp_have"'}'
    done
    # The read itself failing.
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}}}}' \
        "$ccpp_paths"', "failures": {"load_text_file /etc/puppetlabs/puppet/puppet.conf": "[Errno 13] Permission denied"}'

    # --- conf: the sections ---------------------------------------------------
    # Two sections take two backups, and the second one backs up the file the
    # first one already wrote (B93).
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}, "agent": {"b": "2"}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}, "agent": {"b": "2"}, "user": {"c": "3"}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {}}}' "$ccpp_std"
    # A lowercase `default` is the error upstream raises; `DEFAULT` works
    # (B95).
    ccpp_case '{"puppet": {"conf": {"default": {"a": "1"}}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"DEFAULT": {"a": "1"}}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"Default": {"a": "1"}}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"": {"a": "1"}}}}' "$ccpp_std"
    # Overwriting a key the file already had, and adding one to a section it
    # already had.
    ccpp_case '{"puppet": {"conf": {"main": {"orig": "no"}}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"main": {"ORIG": "no"}}}}' "$ccpp_std"
    # Values that are not strings go through `str` on the way out.
    for ccpp_v in '5' '5.5' 'true' 'false' 'null' '["x", "y"]' '{"k": 1}' '""'; do
        ccpp_case "{\"puppet\": {\"conf\": {\"main\": {\"a\": $ccpp_v}}}}" \
            "$ccpp_std"
    done
    # A multiline value gets the tab-indented continuation back on write.
    ccpp_case '{"puppet": {"conf": {"main": {"a": "one\ntwo"}}}}' "$ccpp_std"
    # The rename or the write failing.
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}}}}' \
        "$ccpp_std"', "failures": {"rename /etc/puppetlabs/puppet/puppet.conf /etc/puppetlabs/puppet/puppet.conf.old": "[Errno 30] Read-only file system"}'
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}}}}' \
        "$ccpp_std"', "failures": {"write_file /etc/puppetlabs/puppet/puppet.conf": "[Errno 30] Read-only file system"}'

    # --- conf: ca_cert --------------------------------------------------------
    ccpp_case '{"puppet": {"conf": {"ca_cert": "-----BEGIN CERTIFICATE-----\nx\n"}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"ca_cert": "x", "main": {"a": "1"}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"main": {"a": "1"}, "ca_cert": "x"}}}' \
        "$ccpp_std"
    # AIO runs the same steps as a different user.
    ccpp_case '{"puppet": {"install_type": "aio", "conf": {"ca_cert": "x"}}}' \
        '"stdout": {"subp /opt/puppetlabs/bin/puppet config print config capture=True": "/etc/puppetlabs/puppet/puppet.conf\n", "subp /opt/puppetlabs/bin/puppet config print ssldir capture=True": "/etc/puppetlabs/puppet/ssl\n", "subp /opt/puppetlabs/bin/puppet config print csr_attributes capture=True": "/etc/puppetlabs/puppet/csr_attributes.yaml\n"}, '"$ccpp_conf"
    ccpp_case '{"puppet": {"ssl_dir": "/var/lib/puppet/ssl", "conf": {"ca_cert": "x"}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"ca_cert": "x"}}}' \
        "$ccpp_std"', "failures": {"ensure_dir /etc/puppetlabs/puppet/ssl mode=0771": "[Errno 13] Permission denied"}'
    ccpp_case '{"puppet": {"conf": {"ca_cert": "x"}}}' \
        "$ccpp_std"', "failures": {"chownbyname /etc/puppetlabs/puppet/ssl puppet root": "[Errno 1] Operation not permitted"}'

    # --- conf: certname -------------------------------------------------------
    ccpp_case '{"puppet": {"conf": {"agent": {"certname": "%f"}}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"agent": {"certname": "%i"}}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"agent": {"certname": "%f.%i"}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"agent": {"certname": "PLAIN.NAME"}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"conf": {"agent": {"certname": "%f"}}}}' \
        "$ccpp_std"', "fqdn": "HOST.EXAMPLE.COM", "iid": "I-ABCDEF"'
    ccpp_case '{"puppet": {"conf": {"agent": {"certname": "%f%f%i"}}}}' \
        "$ccpp_std"
    # `certname` under a section that is not `agent` is expanded just the same.
    ccpp_case '{"puppet": {"conf": {"main": {"certname": "%f"}}}}' "$ccpp_std"

    # --- csr_attributes -------------------------------------------------------
    ccpp_case '{"puppet": {"csr_attributes": {"custom_attributes": {"1.2.840.113549.1.9.7": "secret"}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": {"custom_attributes": {"a": "1"}, "extension_requests": {"pp_uuid": "ED803750", "pp_image_name": "my_ami"}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": {}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": null}}' "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": "a string"}}' "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": [1, 2]}}' "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": {"a": [1, 2, 3]}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": {"a": {"b": {"c": 1}}}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes_path": "/e/csr.yaml", "csr_attributes": {"a": 1}}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"csr_attributes": {"a": 1}}}' \
        "$ccpp_std"', "failures": {"write_file /etc/puppetlabs/puppet/csr_attributes.yaml": "[Errno 13] Permission denied"}'

    # --- the services ---------------------------------------------------------
    ccpp_case '{"puppet": {"start_service": false}}' "$ccpp_std"
    ccpp_case '{"puppet": {"start_service": true}}' "$ccpp_std"
    ccpp_case '{"puppet": {"start_service": "no"}}' "$ccpp_std"
    ccpp_case '{"puppet": {}}' \
        "$ccpp_std"', "failures": {"manage_service enable puppet-agent": "no such unit"}'
    ccpp_case '{"puppet": {}}' \
        "$ccpp_std"', "failures": {"manage_service enable puppet-agent": "no such unit", "manage_service enable puppet": "no such unit either"}'
    ccpp_case '{"puppet": {}}' \
        "$ccpp_std"', "failures": {"manage_service start puppet-agent": "no such unit"}'

    # --- exec / exec_args -----------------------------------------------------
    ccpp_case '{"puppet": {"exec": true}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": ["--onetime", "--no-daemonize"]}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": "--onetime --no-daemonize"}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": "  --a   --b  "}}' \
        "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": ""}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": []}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": 5}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": null}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": true}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "exec_args": {"a": 1}}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": false, "exec_args": ["--x"]}}' "$ccpp_std"
    ccpp_case '{"puppet": {"exec": true, "install_type": "aio"}}' \
        '"stdout": {"subp /opt/puppetlabs/bin/puppet config print config capture=True": "/etc/puppetlabs/puppet/puppet.conf\n"}'
    ccpp_case '{"puppet": {"exec": true}}' \
        "$ccpp_std"', "failures": {"subp puppet agent --test capture=False": "exit 1"}'
    # The agent running before the service is started, both on.
    ccpp_case '{"puppet": {"exec": true, "start_service": true, "conf": {"main": {"a": "1"}}, "csr_attributes": {"b": 2}}}' \
        "$ccpp_std"

    run_batch "cc_puppet" \
        "cd /tmp && python3 '$CCPP_PY' --batch" \
        "cd /tmp && '$CCPP_RS' --batch" \
        "$WORK/ccpp.cases"
fi

# --- ansible -----------------------------------------------------------------
sec "cc-ansible"
#
# The whole module against a scripted machine: what each command prints, which
# calls fail, which programs `which` finds, whether `import pip` works and
# whether the stdlib is marked externally managed. Nothing runs on either side
# -- the Python half stubs `subp.subp`, `subp.which`, `distro.do_as`,
# `distro.install_packages`, the `import pip` probe, the `EXTERNALLY-MANAGED`
# check and `sys.stdout` -- so what is compared is the log, the ordered list of
# things the module did, and what reached the console.
#
# `sys.executable` and `$HOME` are set from the case on the Python side so both
# halves name the same interpreter and the same home; the port has no
# `sys.executable` and hardcodes the same default (deviation 165).
CCAN_PY="$(cd "$(dirname "$0")" && pwd)/ccansible.py"
CCAN_RS="$TARGET/examples/dump-cc-ansible"
if [ -x "$CCAN_RS" ] &&
   python3 -c 'import cloudinit.config.cc_ansible' 2>/dev/null; then
    CCAN_RS="$(cd "$(dirname "$CCAN_RS")" && pwd)/dump-cc-ansible"

    ccan_case() {
        ccan_host=${2:-}
        printf '{"cfg": %s, "host": {%s}}\n' "$1" "$ccan_host" \
            >>"$WORK/ccan.cases"
    }
    # `ansible` already on PATH, and a version new enough for everything.
    ccan_new='"present": ["ansible"], "stdout": {"subp ansible-pull --version env=[HOME=/root] cwd=": "ansible-pull [core 2.17.6]\n  config file = None\n"}'
    # Old enough that playbooks are pulled one at a time and --diff is refused.
    ccan_old='"present": ["ansible"], "stdout": {"subp ansible-pull --version env=[HOME=/root] cwd=": "ansible-pull 2.6.0\n"}'
    ccan_pull='{"url": "https://g/r.git", "playbook_name": "u.yml"}'

    : >"$WORK/ccan.cases"

    # --- the switch -----------------------------------------------------------
    ccan_case '{}'
    ccan_case '{"ansible": {}}'
    ccan_case '{"ansible": null}'
    ccan_case '{"ansible": []}'
    ccan_case '{"ansible": "yes"}'
    ccan_case '{"ansible": 5}'

    # --- validate_config ------------------------------------------------------
    ccan_case '{"ansible": {"package_name": "ansible"}}'
    ccan_case '{"ansible": {"install_method": "distro"}}'
    ccan_case '{"ansible": {"install_method": "", "package_name": "ansible"}}'
    ccan_case '{"ansible": {"install_method": null, "package_name": "ansible"}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": ""}}'
    # The message that forgot its f-prefix (B97).
    ccan_case '{"ansible": {"install_method": "rpm", "package_name": "ansible"}}'
    ccan_case '{"ansible": {"install_method": "PIP", "package_name": "ansible"}}'
    # `pull` of every shape validate_config names.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": "a string"}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": 5}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": ["a string"]}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": [{"url": "u", "playbook_name": "p"}, 5]}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"playbook_name": "p"}}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u"}}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": "p", "playbook_names": ["q"]}}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_names": []}}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {}}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": []}}'
    # `setup_controller` with neither key.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {}}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {"other": 1}}}'

    # --- install: distro ------------------------------------------------------
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible"}}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible"}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible"}}' \
        '"present": []'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible"}}' \
        '"failures": {"install_packages ['"'"'ansible'"'"']": "no candidate"}'
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "run_user": "ubuntu"}}' \
        "$ccan_new"

    # --- install: pip ---------------------------------------------------------
    ccan_case '{"ansible": {"install_method": "pip", "package_name": "ansible"}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "pip", "package_name": "ansible"}}' \
        '"pip": false, "stdout": {"subp /usr/bin/python3 -m pip list env=[HOME=/root] cwd=": "ansible 2.17.6\n"}'
    ccan_case '{"ansible": {"install_method": "pip", "package_name": "ansible"}}' \
        '"pip": true, "stdout": {"subp /usr/bin/python3 -m pip list env=[HOME=/root] cwd=": "pip 24.0\n", "subp ansible-pull --version env=[HOME=/root] cwd=": "ansible-pull 2.17.6\n"}'
    ccan_case '{"ansible": {"install_method": "pip", "package_name": "ansible"}}' \
        '"managed": true, "stdout": {"subp /usr/bin/python3 -m pip list env=[HOME=/root] cwd=": "ansible 2.17.6\n"}'
    # The pip upgrade failing is a warning, not an error.
    ccan_case '{"ansible": {"install_method": "pip", "package_name": "ansible"}}' \
        '"failures": {"subp /usr/bin/python3 -m pip install --upgrade pip env=[HOME=/root] cwd=": "no network"}, "stdout": {"subp ansible-pull --version env=[HOME=/root] cwd=": "ansible-pull 2.17.6\n"}'
    # The install itself failing is not.
    ccan_case '{"ansible": {"install_method": "pip", "package_name": "ansible"}}' \
        '"failures": {"subp /usr/bin/python3 -m pip install ansible env=[HOME=/root] cwd=": "no network"}'
    # With a run_user the pip site is looked up first, and the trailing newline
    # from `print` lands in PATH (B100) -- and is then thrown away, because
    # `distro.do_as` drops the environment entirely (B99).
    ccan_case '{"ansible": {"install_method": "pip", "package_name": "ansible", "run_user": "ubuntu"}}' \
        '"stdout": {"do_as ubuntu /usr/bin/python3 -c import site; print(site.getuserbase()) cwd=": "/home/ubuntu/.local\n", "do_as ubuntu /usr/bin/python3 -m pip list --user cwd=": "ansible 2.17.6\n", "do_as ubuntu ansible-pull --version cwd=": "ansible-pull 2.17.6\n"}'

    # --- check_deps -----------------------------------------------------------
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible"}}' \
        '"present": ["ansible-pull"]'

    # --- get_version ----------------------------------------------------------
    for ccan_ver in \
        '"ansible-pull [core 2.17.6]\n"' \
        '"ansible-pull 2.12.0\n"' \
        '"ansible-pull 2.11.99\n"' \
        '"ansible-pull 2.7.0\n"' \
        '"ansible-pull 2.6.9\n"' \
        '"ansible-pull 2.10\n"' \
        '"ansible-pull 2\n"' \
        '"ansible-pull 1.2.3.4\n"' \
        '"ansible-pull 1.2.3.4.5\n"' \
        '"ansible-pull ...\n"' \
        '"ansible-pull\n"' \
        '"\n"' \
        '"2.17.6"'; do
        ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": '"$ccan_pull"'}}' \
            '"present": ["ansible"], "stdout": {"subp ansible-pull --version env=[HOME=/root] cwd=": '"$ccan_ver"'}'
    done
    # `--version` failing outright.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": '"$ccan_pull"'}}' \
        '"present": ["ansible"], "failures": {"subp ansible-pull --version env=[HOME=/root] cwd=": "not found"}'

    # --- pull -----------------------------------------------------------------
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": '"$ccan_pull"'}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_names": ["a.yml", "b.yml"]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_names": ["a.yml", "b.yml"]}}}' \
        "$ccan_old"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": [{"url": "u", "playbook_name": "a.yml"}, {"url": "v", "playbook_name": "b.yml"}]}}' \
        "$ccan_new"
    # Whatever the pull printed goes to the console.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": "a.yml"}}}' \
        '"present": ["ansible"], "stdout": {"subp ansible-pull --version env=[HOME=/root] cwd=": "ansible-pull 2.17.6\n", "subp ansible-pull --url=u a.yml env=[HOME=/root] cwd=": "PLAY RECAP\nok=3\n"}'
    # The pull failing.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": "a.yml"}}}' \
        '"present": ["ansible"], "stdout": {"subp ansible-pull --version env=[HOME=/root] cwd=": "ansible-pull 2.17.6\n"}, "failures": {"subp ansible-pull --url=u a.yml env=[HOME=/root] cwd=": "no route to host"}'
    # `--diff` on an old ansible is the error with the missing space.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": "a.yml", "diff": true}}}' \
        "$ccan_old"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": "a.yml", "diff": false}}}' \
        "$ccan_old"
    # filter_args drops only `False`; everything else survives as it prints.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": "a.yml", "accept_host_key": true, "clean": false, "full": true, "verify_commit": null, "timeout": 0, "private_key": "", "checkout": "main", "module_path": "/m", "sleep_interval": 5}}}' \
        "$ccan_new"
    # An underscore in a key becomes a dash, including in the url key itself.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": "a.yml", "vault_password_file": "/v", "extra_vars": "a=1 b=2"}}}' \
        "$ccan_new"
    # `playbook_name` wins over nothing, and both keys are popped before the
    # args are built.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "pull": {"url": "u", "playbook_name": ""}}}' \
        "$ccan_new"

    # --- galaxy ---------------------------------------------------------------
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "galaxy": {"actions": [["ansible-galaxy", "collection", "install", "community.general"]]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "galaxy": {"actions": []}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "galaxy": {}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "galaxy": {"other": 1}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "galaxy": {"actions": [["a"], ["b", "c"]]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "galaxy": {"actions": [["a"]]}, "run_user": "ubuntu"}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "galaxy": {"actions": [["a"]]}}}' \
        '"present": ["ansible"], "failures": {"subp a env=[HOME=/root] cwd=": "boom"}'

    # --- setup_controller -----------------------------------------------------
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {"repositories": [{"path": "/r", "source": "https://g/r.git"}]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {"run_ansible": [{"playbook_dir": "/r", "playbook_name": "p.yml"}]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {"run_ansible": [{"playbook_dir": "/r", "playbook_name": "p.yml", "become_password_file": "/b", "diff": false, "check": true}]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {"run_ansible": [{"playbook_name": "p.yml"}]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {"repositories": [{"path": "/r"}]}}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "setup_controller": {"repositories": [{"path": "/r", "source": "s"}, {"path": "/q", "source": "t"}], "run_ansible": [{"playbook_dir": "/r", "playbook_name": "p.yml"}]}}}' \
        "$ccan_new"

    # --- ansible_config and the environment -----------------------------------
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "ansible_config": "/etc/ansible/ansible.cfg", "pull": '"$ccan_pull"'}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "ansible_config": "", "pull": '"$ccan_pull"'}}' \
        "$ccan_new"
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "ansible_config": "/c", "galaxy": {"actions": [["a"]]}}}' \
        "$ccan_new"
    # The whole module in one go, in the order upstream runs it.
    ccan_case '{"ansible": {"install_method": "distro", "package_name": "ansible", "ansible_config": "/c", "galaxy": {"actions": [["ansible-galaxy", "install", "geerlingguy.ntp"]]}, "pull": '"$ccan_pull"', "setup_controller": {"repositories": [{"path": "/r", "source": "s"}]}}}' \
        "$ccan_new"

    run_batch "cc_ansible" \
        "cd /tmp && python3 '$CCAN_PY' --batch" \
        "cd /tmp && '$CCAN_RS' --batch" \
        "$WORK/ccan.cases"
fi

sec "cc-salt-minion"
#
# The whole module against a scripted machine: which directories already exist
# and which calls fail. Nothing runs on either side -- the Python half stubs
# `subp.subp`, `util.write_file`, `util.ensure_dir`, the module's `os` and the
# two `Distro` methods -- so what is compared is the log, the ordered list of
# things the module did, and what each file ended up holding.
#
# `util.umask(0o77)` shows up as a suffix on the one `ensure_dir` it wraps: the
# Python half pins the process umask to 0o022 and records anything else.
CCSM_PY="$(cd "$(dirname "$0")" && pwd)/ccsaltminion.py"
CCSM_RS="$TARGET/examples/dump-cc-salt-minion"
if [ -x "$CCSM_RS" ] &&
   python3 -c 'import cloudinit.config.cc_salt_minion' 2>/dev/null; then
    CCSM_RS="$(cd "$(dirname "$CCSM_RS")" && pwd)/dump-cc-salt-minion"

    ccsm_case() {
        ccsm_host=${2:-}
        printf '{"cfg": %s, "host": {%s}}\n' "$1" "$ccsm_host" \
            >>"$WORK/ccsm.cases"
    }
    ccsm_keys='"public_key": "PUB\nKEY\n", "private_key": "PRIV\nKEY\n"'
    ccsm_pki='"dirs": ["/etc/salt/pki/minion"]'

    : >"$WORK/ccsm.cases"

    # --- the switch -----------------------------------------------------------
    ccsm_case '{}'
    ccsm_case '{"other": 1}'
    ccsm_case '{"salt_minion": {}}'
    ccsm_case '{"salt_minion": {"unknown": 1}}'

    # --- SaltConstants --------------------------------------------------------
    ccsm_case '{"salt_minion": {"pkg_name": "py-salt"}}'
    ccsm_case '{"salt_minion": {"service_name": "salt_minion"}}'
    ccsm_case '{"salt_minion": {"config_dir": "/usr/local/etc/salt"}}'
    ccsm_case '{"salt_minion": {"pkg_name": "py-salt", "service_name": "salt_minion", "config_dir": "/usr/local/etc/salt"}}'
    # Every override goes through `str()`, so nothing here is rejected.
    ccsm_case '{"salt_minion": {"pkg_name": 5}}'
    ccsm_case '{"salt_minion": {"pkg_name": null}}'
    ccsm_case '{"salt_minion": {"pkg_name": true}}'
    ccsm_case '{"salt_minion": {"pkg_name": ["a", "b"]}}'
    ccsm_case '{"salt_minion": {"pkg_name": {"a": 1}}}'
    ccsm_case '{"salt_minion": {"pkg_name": 1.5}}'
    ccsm_case '{"salt_minion": {"pkg_name": ""}}'
    ccsm_case '{"salt_minion": {"service_name": 5, "config_dir": 7}}'
    # A config_dir that is not a directory name still gets joined.
    ccsm_case '{"salt_minion": {"config_dir": "", "conf": {"a": 1}}}'
    ccsm_case '{"salt_minion": {"config_dir": "/etc/salt/", "conf": {"a": 1}}}'
    ccsm_case '{"salt_minion": {"config_dir": "relative", "conf": {"a": 1}}}'

    # --- `salt_minion:` that is not a mapping ---------------------------------
    # `key in cfg` is a membership test, so a scalar raises where a list and a
    # string quietly answer (B101).
    ccsm_case '{"salt_minion": null}'
    ccsm_case '{"salt_minion": 5}'
    ccsm_case '{"salt_minion": true}'
    ccsm_case '{"salt_minion": 1.5}'
    ccsm_case '{"salt_minion": ""}'
    ccsm_case '{"salt_minion": "nothing here"}'
    ccsm_case '{"salt_minion": "pkg_name"}'
    ccsm_case '{"salt_minion": "config_dir"}'
    ccsm_case '{"salt_minion": "service_name"}'
    ccsm_case '{"salt_minion": "conf"}'
    ccsm_case '{"salt_minion": "grains"}'
    ccsm_case '{"salt_minion": "public_key and private_key"}'
    ccsm_case '{"salt_minion": []}'
    ccsm_case '{"salt_minion": ["other"]}'
    ccsm_case '{"salt_minion": ["pkg_name"]}'
    ccsm_case '{"salt_minion": ["conf"]}'
    ccsm_case '{"salt_minion": ["grains"]}'
    ccsm_case '{"salt_minion": ["public_key", "private_key"]}'
    ccsm_case '{"salt_minion": [5]}'

    # --- conf -----------------------------------------------------------------
    ccsm_case '{"salt_minion": {"conf": {}}}'
    ccsm_case '{"salt_minion": {"conf": {"master": "salt.example.com"}}}'
    ccsm_case '{"salt_minion": {"conf": {"master": ["a", "b"], "id": "m1", "log_level": "warning"}}}'
    ccsm_case '{"salt_minion": {"conf": {"nested": {"a": {"b": [1, 2, {"c": null}]}}}}}'
    ccsm_case '{"salt_minion": {"conf": {"yes": "no", "on": "off", "n": "y"}}}'
    ccsm_case '{"salt_minion": {"conf": {"num": 1.5, "int": 3, "bool": true, "none": null}}}'
    ccsm_case '{"salt_minion": {"conf": {"weird": "a: b", "dash": "- x", "hash": "#c", "star": "*", "amp": "&a", "pct": "%y"}}}'
    ccsm_case '{"salt_minion": {"conf": {"multi": "one\ntwo\nthree\n"}}}'
    ccsm_case '{"salt_minion": {"conf": {"unicode": "caf\u00e9 \u2603"}}}'
    ccsm_case '{"salt_minion": {"conf": {"empty": "", "space": " ", "tab": "\t"}}}'
    ccsm_case '{"salt_minion": {"conf": {"long": "the quick brown fox jumps over the lazy dog and keeps on jumping well past the eightieth column"}}}'
    # `minion_data.get` on something that is not a mapping, after the file is
    # already on disk (B102).
    ccsm_case '{"salt_minion": {"conf": "hello"}}'
    ccsm_case '{"salt_minion": {"conf": [1, 2]}}'
    ccsm_case '{"salt_minion": {"conf": 5}}'
    ccsm_case '{"salt_minion": {"conf": true}}'
    # Falsy `conf:` short-circuits before the `.get`, so these survive.
    ccsm_case '{"salt_minion": {"conf": null}}'
    ccsm_case '{"salt_minion": {"conf": ""}}'
    ccsm_case '{"salt_minion": {"conf": []}}'
    ccsm_case '{"salt_minion": {"conf": 0}}'
    ccsm_case '{"salt_minion": {"conf": false}}'

    # --- file_client ----------------------------------------------------------
    ccsm_case '{"salt_minion": {"conf": {"file_client": "local"}}}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": "remote"}}}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": null}}}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": 5}}}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": ["local"]}}}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": "Local"}}}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": "local "}}}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": "local"}, "service_name": "salt_minion"}}'
    # Masterless and the `state.apply` that follows it failing.
    ccsm_case '{"salt_minion": {"conf": {"file_client": "local"}}}' \
        '"failures": {"subp salt-call --local state.apply": "no states"}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": "local"}}}' \
        '"failures": {"manage_service disable salt-minion": "no such unit"}'
    ccsm_case '{"salt_minion": {"conf": {"file_client": "local"}}}' \
        '"failures": {"manage_service stop salt-minion": "not running"}'

    # --- grains ---------------------------------------------------------------
    ccsm_case '{"salt_minion": {"grains": {"role": "web"}}}'
    ccsm_case '{"salt_minion": {"grains": {"roles": ["web", "db"], "dc": "eu1"}}}'
    ccsm_case '{"salt_minion": {"grains": {}}}'
    ccsm_case '{"salt_minion": {"grains": null}}'
    ccsm_case '{"salt_minion": {"grains": "a string"}}'
    ccsm_case '{"salt_minion": {"grains": [1, 2]}}'
    ccsm_case '{"salt_minion": {"grains": 5}}'
    ccsm_case '{"salt_minion": {"grains": true}}'
    ccsm_case '{"salt_minion": {"conf": {"master": "m"}, "grains": {"role": "web"}}}'
    ccsm_case '{"salt_minion": {"config_dir": "/opt/salt", "grains": {"role": "web"}}}'

    # --- the key pair ---------------------------------------------------------
    ccsm_case '{"salt_minion": {"public_key": "PUB"}}'
    ccsm_case '{"salt_minion": {"private_key": "PRIV"}}'
    ccsm_case '{"salt_minion": {'"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {'"$ccsm_keys"'}}' "$ccsm_pki"
    ccsm_case '{"salt_minion": {'"$ccsm_keys"'}}' '"dirs": ["/etc/salt/pki"]'
    ccsm_case '{"salt_minion": {"config_dir": "/opt/salt", '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"config_dir": "/opt/salt", '"$ccsm_keys"'}}' \
        '"dirs": ["/opt/salt/pki/minion"]'
    ccsm_case '{"salt_minion": {"pki_dir": "/keys", '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"pki_dir": "/keys", '"$ccsm_keys"'}}' "$ccsm_pki"
    ccsm_case '{"salt_minion": {"pki_dir": "", '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"pki_dir": "keys/", '"$ccsm_keys"'}}'
    # A `pki_dir` that is not a path dies in `stat` or in `makedirs`, depending
    # on whether it could pass for a file descriptor.
    ccsm_case '{"salt_minion": {"pki_dir": 5, '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"pki_dir": true, '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"pki_dir": null, '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"pki_dir": [], '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"pki_dir": {}, '"$ccsm_keys"'}}'
    ccsm_case '{"salt_minion": {"pki_dir": 1.5, '"$ccsm_keys"'}}'
    # A key that is not a string dies in `encode_text`, after the directory.
    ccsm_case '{"salt_minion": {"public_key": 5, "private_key": "q"}}'
    ccsm_case '{"salt_minion": {"public_key": "p", "private_key": 5}}'
    ccsm_case '{"salt_minion": {"public_key": null, "private_key": null}}'
    ccsm_case '{"salt_minion": {"public_key": ["p"], "private_key": "q"}}'
    ccsm_case '{"salt_minion": {"public_key": "", "private_key": ""}}'
    # The key is written through untouched, newlines, tabs and all.
    ccsm_case '{"salt_minion": {"public_key": "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n", "private_key": "a\tb\r\nc"}}'

    # --- failures -------------------------------------------------------------
    ccsm_case '{"salt_minion": {}}' \
        '"failures": {"install_packages ['"'"'salt-minion'"'"']": "no candidate"}'
    ccsm_case '{"salt_minion": {"pkg_name": "py-salt"}}' \
        '"failures": {"install_packages ['"'"'py-salt'"'"']": "no candidate"}'
    ccsm_case '{"salt_minion": {"conf": {"a": 1}}}' \
        '"failures": {"ensure_dir /etc/salt": "read-only file system"}'
    ccsm_case '{"salt_minion": {"conf": {"a": 1}}}' \
        '"failures": {"write_file /etc/salt/minion": "no space left on device"}'
    ccsm_case '{"salt_minion": {"grains": {"a": 1}}}' \
        '"failures": {"write_file /etc/salt/grains": "no space left on device"}'
    ccsm_case '{"salt_minion": {'"$ccsm_keys"'}}' \
        '"failures": {"ensure_dir /etc/salt/pki umask=0077": "permission denied"}'
    ccsm_case '{"salt_minion": {'"$ccsm_keys"'}}' \
        '"failures": {"write_file /etc/salt/pki/minion.pub": "permission denied"}'
    ccsm_case '{"salt_minion": {'"$ccsm_keys"'}}' \
        '"failures": {"write_file /etc/salt/pki/minion.pem": "permission denied"}'
    ccsm_case '{"salt_minion": {}}' \
        '"failures": {"manage_service enable salt-minion": "no such unit"}'
    ccsm_case '{"salt_minion": {}}' \
        '"failures": {"manage_service restart salt-minion": "job failed"}'

    # --- everything at once ---------------------------------------------------
    ccsm_case '{"salt_minion": {"pkg_name": "salt-minion", "service_name": "salt-minion", "config_dir": "/etc/salt", "conf": {"master": "salt.example.com", "id": "m1"}, "grains": {"roles": ["web"]}, '"$ccsm_keys"'}}' \
        "$ccsm_pki"
    ccsm_case '{"salt_minion": {"config_dir": "/opt/salt", "conf": {"file_client": "local"}, "grains": {"roles": ["web"]}, "pki_dir": "/opt/keys", '"$ccsm_keys"'}}'

    run_batch "cc_salt_minion" \
        "cd /tmp && python3 '$CCSM_PY' --batch" \
        "cd /tmp && '$CCSM_RS' --batch" \
        "$WORK/ccsm.cases"
fi

sec "cc-mcollective"
#
# The whole module against a scripted machine: which files already exist, which
# reads or copies fail with something other than ENOENT, and which calls fail.
# Nothing runs on either side -- the Python half stubs `subp.subp`,
# `util.load_binary_file`, `util.write_file`, `util.copy` and
# `Distro.install_packages` -- so what is compared is the log, the ordered list
# of things the module did, and what each file ended up holding.
#
# Most of the surface here is not the module, which is forty lines, but
# `configobj`: `server.cfg` is parsed and written back whether or not `conf:`
# mentions any of it, so the file the operator wrote is compared against the
# file the module leaves behind. The `files` entries are therefore chosen for
# what they make `configobj` do -- lists, quotes, comments, nesting, the
# parse errors -- rather than for what mcollective would make of them.
CCMC_PY="$(cd "$(dirname "$0")" && pwd)/ccmcollective.py"
CCMC_RS="$TARGET/examples/dump-cc-mcollective"
if [ -x "$CCMC_RS" ] &&
   python3 -c 'import cloudinit.config.cc_mcollective' 2>/dev/null &&
   python3 -c 'import configobj' 2>/dev/null; then
    CCMC_RS="$(cd "$(dirname "$CCMC_RS")" && pwd)/dump-cc-mcollective"

    ccmc_case() {
        ccmc_host=${2:-}
        printf '{"cfg": %s, "host": {%s}}\n' "$1" "$ccmc_host" \
            >>"$WORK/ccmc.cases"
    }
    ccmc_file() {
        printf '"files": {"/etc/mcollective/server.cfg": "%s"}' "$1"
    }

    : >"$WORK/ccmc.cases"

    # --- the switch -----------------------------------------------------------
    ccmc_case '{}'
    ccmc_case '{"other": 1}'
    ccmc_case '{"mcollective": {}}'
    ccmc_case '{"mcollective": {"unknown": 1}}'
    ccmc_case '{"mcollective": {"conf": {}}}'

    # --- `mcollective:` is subscripted without a type check (B103) -------------
    ccmc_case '{"mcollective": ""}'
    ccmc_case '{"mcollective": "nothing here"}'
    ccmc_case '{"mcollective": "conf"}'
    ccmc_case '{"mcollective": "the conf file"}'
    ccmc_case '{"mcollective": 5}'
    ccmc_case '{"mcollective": 0}'
    ccmc_case '{"mcollective": 1.5}'
    ccmc_case '{"mcollective": true}'
    ccmc_case '{"mcollective": false}'
    ccmc_case '{"mcollective": null}'
    ccmc_case '{"mcollective": []}'
    ccmc_case '{"mcollective": ["conf"]}'
    ccmc_case '{"mcollective": ["other"]}'
    ccmc_case '{"mcollective": [1]}'
    ccmc_case '{"mcollective": {"conf": null}}'

    # --- `conf:` is handed to `.items()` without one either (B104) -------------
    ccmc_case '{"mcollective": {"conf": ""}}'
    ccmc_case '{"mcollective": {"conf": "hello"}}'
    ccmc_case '{"mcollective": {"conf": 5}}'
    ccmc_case '{"mcollective": {"conf": 0}}'
    ccmc_case '{"mcollective": {"conf": true}}'
    ccmc_case '{"mcollective": {"conf": false}}'
    ccmc_case '{"mcollective": {"conf": []}}'
    ccmc_case '{"mcollective": {"conf": [1, 2]}}'
    ccmc_case '{"mcollective": {"conf": ["a"]}}'

    # --- values, and what `_quote` does to them -------------------------------
    ccmc_case '{"mcollective": {"conf": {"a": "b"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": ""}}}'
    ccmc_case '{"mcollective": {"conf": {"a": " "}}}'
    ccmc_case '{"mcollective": {"conf": {"a": " padded "}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "with spaces"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "x,y"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "x#y"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "#lead"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "x'"'"'y"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "x\"y"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "x'"'"'y\"z"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "line\nbreak"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "=x"}}}'
    ccmc_case '{"mcollective": {"conf": {"a": "a]b"}}}'
    # `str(cfg)` for everything that is neither a string nor a mapping, which
    # is why a list arrives as its repr rather than as a list value.
    ccmc_case '{"mcollective": {"conf": {"a": 0}}}'
    ccmc_case '{"mcollective": {"conf": {"a": 5}}}'
    ccmc_case '{"mcollective": {"conf": {"a": -1}}}'
    ccmc_case '{"mcollective": {"conf": {"a": 1.5}}}'
    ccmc_case '{"mcollective": {"conf": {"a": true}}}'
    ccmc_case '{"mcollective": {"conf": {"a": false}}}'
    ccmc_case '{"mcollective": {"conf": {"a": null}}}'
    ccmc_case '{"mcollective": {"conf": {"a": []}}}'
    ccmc_case '{"mcollective": {"conf": {"a": ["x"]}}}'
    ccmc_case '{"mcollective": {"conf": {"a": ["x", "y"]}}}'
    ccmc_case '{"mcollective": {"conf": {"a": [1, 2]}}}'
    ccmc_case '{"mcollective": {"conf": {"a": ["a,b"]}}}'

    # --- keys, which are quoted by the same rules -----------------------------
    ccmc_case '{"mcollective": {"conf": {"": "empty key"}}}'
    ccmc_case '{"mcollective": {"conf": {" ": "space key"}}}'
    ccmc_case '{"mcollective": {"conf": {"a b": "spaced key"}}}'
    ccmc_case '{"mcollective": {"conf": {"a,b": "comma key"}}}'
    ccmc_case '{"mcollective": {"conf": {"a#b": "hash key"}}}'
    ccmc_case '{"mcollective": {"conf": {"[a]": "bracket key"}}}'
    ccmc_case '{"mcollective": {"conf": {"a=b": "equals key"}}}'
    ccmc_case '{"mcollective": {"conf": {"plugin.psk": "unset"}}}'

    # --- a mapping value becomes a section ------------------------------------
    ccmc_case '{"mcollective": {"conf": {"a": {}}}}'
    ccmc_case '{"mcollective": {"conf": {"a": {"b": "c"}}}}'
    ccmc_case '{"mcollective": {"conf": {"a": {"b": {"c": "d"}}}}}'
    ccmc_case '{"mcollective": {"conf": {"a": {"b": 1, "c": [1, 2], "d": null, "e": ""}}}}'
    ccmc_case '{"mcollective": {"conf": {"a": {"b": "c"}, "z": "y"}}}'
    ccmc_case '{"mcollective": {"conf": {"z": "y", "a": {"b": "c"}}}}'
    ccmc_case '{"mcollective": {"conf": {"a": {}, "b": {}}}}'

    # --- the certificates -----------------------------------------------------
    ccmc_case '{"mcollective": {"conf": {"public-cert": "PUB\nCERT\n"}}}'
    ccmc_case '{"mcollective": {"conf": {"private-cert": "PRI\nCERT\n"}}}'
    ccmc_case '{"mcollective": {"conf": {"public-cert": "PUB", "private-cert": "PRI"}}}'
    ccmc_case '{"mcollective": {"conf": {"private-cert": "PRI", "public-cert": "PUB"}}}'
    ccmc_case '{"mcollective": {"conf": {"public-cert": ""}}}'
    ccmc_case '{"mcollective": {"conf": {"securityprovider": "psk", "public-cert": "PUB"}}}'
    ccmc_case '{"mcollective": {"conf": {"public-cert": "PUB", "securityprovider": "psk"}}}'
    # A certificate that is not a string dies in `encode_text`, before the file
    # it belongs beside is opened.
    ccmc_case '{"mcollective": {"conf": {"public-cert": 5}}}'
    ccmc_case '{"mcollective": {"conf": {"public-cert": null}}}'
    ccmc_case '{"mcollective": {"conf": {"public-cert": ["a"]}}}'
    ccmc_case '{"mcollective": {"conf": {"private-cert": {"a": "b"}}}}'

    # --- an existing `server.cfg`, which is rewritten either way ---------------
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = web1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity=web1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity  =  web1  \n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = \"web1\"\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file "identity = 'web1'\n")"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = web1 # inline\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = a, b, c\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = a,\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = ,\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = a, b # tail\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = \"a,b\"\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = \n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = \"\"\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file "identity = '''multi\nline'''\n")"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'identity = \"\"\"one\"\"\"\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '# just a comment\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '\n\n\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '# lead\n\nidentity = a  # inline\n\n[sec]\nb = 2\n# tail\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[plugin]\nx = 1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[plugin]\n  x = 1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '  [plugin]\n  x = 1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[a]\n[[b]]\nx = 1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[a]\n[[b]]\n[[[c]]]\nx = 1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[ spaced ]\nx = 1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[\"q k\"]\nx = 1\n')"
    # A scalar written after a section still comes out before it.
    ccmc_case '{"mcollective": {"conf": {"top": "v"}}}' "$(ccmc_file '[s]\nx = 1\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[s]\nx = 1\ntop = 2\n')"
    # A scalar overwritten with a mapping keeps the place it had in `scalars`.
    ccmc_case '{"mcollective": {"conf": {"top": {"a": "b"}}}}' "$(ccmc_file 'top = 1\n[s]\nx = 2\n')"
    ccmc_case '{"mcollective": {"conf": {"s": {"y": "2"}}}}' "$(ccmc_file 'top = 1\n[s]\nx = 2\n')"
    ccmc_case '{"mcollective": {"conf": {"s": "flat"}}}' "$(ccmc_file 'top = 1\n[s]\nx = 2\n')"
    ccmc_case '{"mcollective": {"conf": {"identity": "new", "plugin": {"y": 2}}}}' \
        "$(ccmc_file '# lead\nidentity = old  # keep\nsecurityprovider = psk\n[plugin]\nx = 1\n')"
    ccmc_case '{"mcollective": {"conf": {"public-cert": "PUB"}}}' \
        "$(ccmc_file 'securityprovider = psk\nplugin.psk = unset\n')"
    # CRLF, which the whole file is rewritten with.
    ccmc_case '{"mcollective": {"conf": {"b": "2"}}}' "$(ccmc_file 'a = 1\r\n')"

    # --- files `configobj` refuses --------------------------------------------
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'garbage\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '= 5\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '  = 5\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[]\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[b\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'a = \"unclosed\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'a = \"\"\"x\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[a]\n[a]\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'a=1\na=2\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'bad1\nbad2\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[a]\n[[[c]]]\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file '[[a]]\n')"
    ccmc_case '{"mcollective": {"conf": {}}}' "$(ccmc_file 'a = 1\ngarbage\n')"

    # --- failures -------------------------------------------------------------
    ccmc_case '{"mcollective": {"conf": {}}}' \
        '"failures": {"/etc/mcollective/server.cfg": 13}'
    ccmc_case '{"mcollective": {"conf": {}}}' \
        '"failures": {"/etc/mcollective/server.cfg": 21}'
    ccmc_case '{"mcollective": {"conf": {}}}' \
        '"files": {"/etc/mcollective/server.cfg": "a = 1\n"}, "failures": {"/etc/mcollective/server.cfg": 13}'
    ccmc_case '{"mcollective": {"conf": {}}}' \
        '"errors": {"write_file /etc/mcollective/server.cfg mode=0644": "disk full"}'
    ccmc_case '{"mcollective": {"conf": {"public-cert": "PUB"}}}' \
        '"errors": {"write_file /etc/mcollective/ssl/server-public.pem mode=0644": "no dir"}'
    ccmc_case '{"mcollective": {}}' \
        '"errors": {"install_packages ['"'"'mcollective'"'"']": "no such package"}'
    ccmc_case '{"mcollective": {}}' \
        '"errors": {"subp service mcollective restart": "unit not found"}'
    ccmc_case '{"mcollective": {"conf": {"a": "b"}}}' \
        '"errors": {"subp service mcollective restart": "unit not found"}'

    # --- everything at once ---------------------------------------------------
    ccmc_case '{"mcollective": {"conf": {"identity": "web1", "securityprovider": "ssl", "public-cert": "PUB\nCERT\n", "private-cert": "PRI\nCERT\n", "connector": "activemq", "plugin": {"activemq.pool.size": 1, "activemq.pool.1.host": "mq", "activemq.pool.1.port": 61613}, "factsource": "yaml"}}}' \
        "$(ccmc_file '# managed by hand\nidentity = old\nlibdir = /usr/share/mcollective/plugins\n\n[plugin]\nactivemq.pool.size = 2  # two\n')"

    run_batch "cc_mcollective" \
        "cd /tmp && python3 '$CCMC_PY' --batch" \
        "cd /tmp && '$CCMC_RS' --batch" \
        "$WORK/ccmc.cases"
fi

sec "cc-chef"
#
# The whole module against a scripted machine: which files and directories
# already exist, which paths `subp.is_exe` answers yes for, what each URL
# serves, and which calls fail. Nothing runs and nothing is installed on either
# side -- the Python half stubs `os`, `shutil.move`, `subp.subp`,
# `subp.is_exe`, `url_helper.readurl`, `temp_utils.tempdir`,
# `Distro.install_packages` and the `util` helpers -- so what is compared is the
# log, the ordered list of things the module did, and what each file ended up
# holding.
#
# `templates_dir` is a real directory: the Python half calls the real
# `Cloud.get_template_filename`, so upstream's own "No template found in %s"
# warning is compared rather than stubbed away. Cases that want the template
# use the packaged `/etc/cloud/templates`; cases that want it missing point
# somewhere that does not exist.
#
# cc_chef is the module that most rewards a scripted machine: half its surface
# is the order in which it does things -- six directories, then the legacy
# migration, then the validation key, then the template, then the firstboot
# file, then the install, then the run -- and the interesting configurations
# are the ones that fail partway through with the directories already made.
CCCHEF_PY="$(cd "$(dirname "$0")" && pwd)/ccchef.py"
CCCHEF_RS="$TARGET/examples/dump-cc-chef"
if [ -x "$CCCHEF_RS" ] &&
   python3 -c 'import cloudinit.config.cc_chef' 2>/dev/null &&
   [ -f /etc/cloud/templates/chef_client.rb.tmpl ]; then
    CCCHEF_RS="$(cd "$(dirname "$CCCHEF_RS")" && pwd)/dump-cc-chef"

    # $1 is the `chef:` block, $2 the extra host fields. Every case gets a real
    # templates directory unless $2 overrides it.
    ccchef_case() {
        ccchef_host=${2:-'"templates_dir": "/etc/cloud/templates"'}
        printf '{"cfg": {"chef": %s}, "host": {%s}}\n' "$1" "$ccchef_host" \
            >>"$WORK/ccchef.cases"
    }
    ccchef_raw() {
        ccchef_host=${2:-'"templates_dir": "/etc/cloud/templates"'}
        printf '{"cfg": %s, "host": {%s}}\n' "$1" "$ccchef_host" \
            >>"$WORK/ccchef.cases"
    }
    ccchef_notpl='"templates_dir": "/nonexistent/templates"'
    ccchef_tpl='"templates_dir": "/etc/cloud/templates"'
    ccchef_installed="$ccchef_tpl, \"exes\": [\"/usr/bin/chef-client\"]"
    ccchef_min='"server_url": "https://chef.example/", "validation_name": "v"'

    : >"$WORK/ccchef.cases"

    # --- the switch -----------------------------------------------------------
    ccchef_raw '{}'
    ccchef_raw '{"other": 1}'
    ccchef_case '{}'
    ccchef_case '{}' "$ccchef_notpl"
    ccchef_case '{"unknown": 1}'

    # --- `chef:` is subscripted without a type check (B107) --------------------
    #
    # The membership test comes first, so whether it raises at all depends on
    # the key being looked up -- and by the time it does, six directories exist.
    ccchef_case '""'
    ccchef_case '"nothing here"'
    ccchef_case '"directories"'
    ccchef_case '"the directories list"'
    ccchef_case '5'
    ccchef_case '0'
    ccchef_case '1.5'
    ccchef_case 'true'
    ccchef_case 'false'
    ccchef_case 'null'
    ccchef_case '[]'
    ccchef_case '["directories"]'
    ccchef_case '["other"]'
    ccchef_case '[1]'

    # --- the two mandatory keys are unguarded subscripts (B108) ---------------
    #
    # Only reached when a template is found, and only after the directories are
    # made and the validation key may already be on disk.
    ccchef_case "{$ccchef_min}"
    ccchef_case '{"server_url": "https://s/"}'
    ccchef_case '{"validation_name": "v"}'
    ccchef_case '{"server_url": "https://s/"}' "$ccchef_notpl"
    ccchef_case '{"validation_name": "v"}' "$ccchef_notpl"
    ccchef_case '{"server_url": null, "validation_name": null}'
    ccchef_case '{"server_url": 5, "validation_name": true}'
    ccchef_case '{"server_url": ["a"], "validation_name": {"b": 1}}'
    ccchef_case '{"server_url": "", "validation_name": ""}'
    ccchef_case "{\"validation_cert\": \"KEY\", \"server_url\": \"https://s/\"}"

    # --- directories ----------------------------------------------------------
    ccchef_case "{$ccchef_min, \"directories\": null}"
    ccchef_case "{$ccchef_min, \"directories\": []}"
    ccchef_case "{$ccchef_min, \"directories\": [\"/opt/chef\"]}"
    ccchef_case "{$ccchef_min, \"directories\": [\"/etc/chef\"]}"
    ccchef_case "{$ccchef_min, \"directories\": [\"/a\", \"/a\"]}"
    ccchef_case "{$ccchef_min, \"directories\": [\"/a/b/c\", \"/a/b\", \"/a\"]}"
    ccchef_case "{$ccchef_min, \"directories\": [\"\"]}"
    ccchef_case "{$ccchef_min, \"directories\": [\"rel/dir\"]}"
    ccchef_case "{$ccchef_min, \"directories\": [\"/x\", 5, true, null]}"
    ccchef_case "{$ccchef_min, \"directories\": \"/one/dir\"}"
    ccchef_case "{$ccchef_min, \"directories\": 5}"
    ccchef_case "{$ccchef_min, \"directories\": {}}"
    ccchef_case "{$ccchef_min, \"directories\": {\"a\": 1}}"
    ccchef_case "{$ccchef_min, \"directories\": [\"/a\"]}" \
        "$ccchef_tpl, \"errors\": {\"ensure_dir /a\": \"denied\"}"

    # --- the legacy cache and backup directories ------------------------------
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"dirs\": {\"/var/cache/chef\": [\"a\", \"b\"]}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"dirs\": {\"/var/backups/chef\": [\"p\", \"q\"]}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"dirs\": {\"/var/cache/chef\": [\"x\"], \"/var/backups/chef\": [\"y\"]}"
    ccchef_case "{$ccchef_min}" "$ccchef_tpl, \"dirs\": {\"/var/cache/chef\": []}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"dirs\": {\"/var/cache/chef\": [\"a\"]}, \"files\": {\"/var/chef/cache/a\": \"x\"}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"dirs\": {\"/var/cache/chef\": [\"\", \"a b\", \"sub/dir\", \".hidden\"]}"
    ccchef_case "{$ccchef_min}" "$ccchef_tpl, \"files\": {\"/var/cache/chef\": \"notadir\"}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"dirs\": {\"/var/cache/chef\": [\"a\"]}, \"errors\": {\"listdir /var/cache/chef\": \"nope\"}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"dirs\": {\"/var/cache/chef\": [\"a\"]}, \"errors\": {\"move /var/cache/chef/a /var/chef/cache\": \"boom\"}"

    # --- the validation key ---------------------------------------------------
    ccchef_case "{$ccchef_min, \"validation_cert\": \"system\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"system\"}" \
        "$ccchef_tpl, \"files\": {\"/etc/chef/validation.pem\": \"old\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"System\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \" system\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"-----BEGIN\nkey\n\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": null}"
    ccchef_case "{$ccchef_min, \"validation_cert\": 5}"
    ccchef_case "{$ccchef_min, \"validation_cert\": [\"a\"]}"
    ccchef_case "{$ccchef_min, \"validation_key\": \"/tmp/vk.pem\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"KEY\", \"validation_key\": \"/tmp/vk.pem\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"KEY\", \"validation_key\": null}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"KEY\", \"validation_key\": 7}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"system\", \"validation_key\": \"/opt/k.pem\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"system\", \"validation_key\": \"/opt/k.pem\"}" \
        "$ccchef_tpl, \"files\": {\"/opt/k.pem\": \"old\"}"
    ccchef_case "{$ccchef_min, \"validation_cert\": \"KEY\"}" \
        "$ccchef_tpl, \"errors\": {\"write_file /etc/chef/validation.pem mode=0644\": \"ro fs\"}"

    # --- the template ---------------------------------------------------------
    ccchef_case "{$ccchef_min}" '"templates_dir": "/nonexistent/templates"'
    ccchef_case "{$ccchef_min}" '"templates_dir": "/"'
    ccchef_case "{$ccchef_min}" '"templates_dir": ""'
    ccchef_case "{$ccchef_min}" '"templates_dir": "templates"'
    ccchef_case "{$ccchef_min}" '"templates_dir": "//nope//x"'
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"errors\": {\"load_text_file /etc/cloud/templates/chef_client.rb.tmpl\": \"read fail\"}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"errors\": {\"write_file /etc/chef/client.rb mode=0644\": \"ro fs\"}"

    # --- a template of our own, to reach the renderer -------------------------
    ccchef_tplfile() {
        printf '"templates_dir": "/etc/cloud/templates", "files": {"/etc/cloud/templates/chef_client.rb.tmpl": "%s"}' "$1"
    }
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile 'plain text\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile 'plain text')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: jinja\nhi {{node_name}}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: jinja\nhi {{node_name}}')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: jinja\n{% if node_name %}a{% endif %}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: jinja\n{{ nope }}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: jinja\n{% for x in [1,2,3] %}{{x}}{% endfor %}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: jinja\n{{ server_url|upper }}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: basic\nnode $node_name\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: BASIC\nnode ${node_name}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '##template:jinja\n{{server_url}}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template: bogus\nx\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '## template:\nx\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '$node_name\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '${node_name}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '$nope\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile '${nope}\n')"
    ccchef_case "{$ccchef_min}" "$(ccchef_tplfile 'no vars at all\n')"
    ccchef_case "{$ccchef_min, \"chef_license\": null}" \
        "$(ccchef_tplfile '## template: jinja\nlic={{ chef_license }}\n')"
    ccchef_case "{$ccchef_min, \"chef_license\": [\"a\", 2]}" \
        "$(ccchef_tplfile '## template: jinja\nlic={{ chef_license }}\n')"
    ccchef_case "{$ccchef_min, \"chef_license\": {\"k\": [1]}}" \
        "$(ccchef_tplfile '## template: jinja\nlic={{ chef_license }}\n')"
    ccchef_case "{$ccchef_min, \"show_time\": false}" \
        "$(ccchef_tplfile '## template: jinja\nst={{ show_time }}\n')"
    ccchef_case "{$ccchef_min, \"show_time\": \"no\"}" \
        "$(ccchef_tplfile '## template: jinja\nst={{ show_time }}\n')"

    # --- template parameters --------------------------------------------------
    ccchef_case "{$ccchef_min, \"node_name\": \"mynode\"}"
    ccchef_case "{$ccchef_min, \"node_name\": null}"
    ccchef_case "{$ccchef_min, \"node_name\": \"n\\u00f8de\"}"
    ccchef_case "{$ccchef_min, \"environment\": \"prod\"}"
    ccchef_case "{$ccchef_min, \"chef_license\": \"accept\"}"
    ccchef_case "{$ccchef_min, \"log_level\": \":debug\"}"
    ccchef_case "{$ccchef_min, \"ssl_verify_mode\": \":verify_peer\"}"
    ccchef_case "{$ccchef_min, \"encrypted_data_bag_secret\": \"/etc/chef/secret\"}"
    ccchef_case "{$ccchef_min, \"zzz\": \"unknown key\"}"
    ccchef_case "{$ccchef_min, \"server_url\": \"https://a\\\"b/\"}"
    ccchef_case "{$ccchef_min, \"node_name\": \"{{ oops }}\"}"
    ccchef_case "{$ccchef_min, \"node_name\": \"\$node_name\"}"

    # --- the path keys, which decide what `ensure_dirs` is handed -------------
    ccchef_case "{$ccchef_min, \"log_location\": \"/a/b/c/d.log\", \"pid_file\": \"/e/f/g.pid\"}"
    ccchef_case "{$ccchef_min, \"log_location\": \"relative.log\"}"
    ccchef_case "{$ccchef_min, \"log_location\": \"/a.log\"}"
    ccchef_case "{$ccchef_min, \"log_location\": \"//a//b.log\"}"
    ccchef_case "{$ccchef_min, \"log_location\": \"/\"}"
    ccchef_case "{$ccchef_min, \"log_location\": \"\"}"
    ccchef_case "{$ccchef_min, \"log_location\": null, \"pid_file\": null}"
    ccchef_case "{$ccchef_min, \"client_key\": 5}"
    ccchef_case "{$ccchef_min, \"file_cache_path\": true}"
    ccchef_case "{$ccchef_min, \"validation_key\": \"/k/v.pem\", \"client_key\": \"/k/c.pem\"}"
    ccchef_case "{$ccchef_min, \"json_attribs\": \"/j/x.json\", \"file_cache_path\": \"/j\", \"file_backup_path\": \"/j\"}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"errors\": {\"ensure_dirs ['/etc/chef', '/var/chef', '/var/log/chef', '/var/run/chef']\": \"mkdir fail\"}"

    # --- the firstboot file ---------------------------------------------------
    ccchef_case "{$ccchef_min, \"run_list\": [\"recipe[a]\", \"role[b]\"]}"
    ccchef_case "{$ccchef_min, \"run_list\": []}"
    ccchef_case "{$ccchef_min, \"run_list\": \"recipe[a]\"}"
    ccchef_case "{$ccchef_min, \"run_list\": 5}"
    ccchef_case "{$ccchef_min, \"run_list\": null}"
    ccchef_case "{$ccchef_min, \"run_list\": {\"k\": 1}}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": {\"z\": 1, \"a\": {\"b\": [1, 2]}}}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": {}}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": null}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": \"hi\"}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": [1, 2]}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": {\"k\": \"\\u00e9\\u00fc\"}}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": {\"k\": 1.5, \"j\": true, \"n\": null}}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": {\"n\": 1e20}}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": {\"quote\": \"he said \\\"hi\\\"\"}}"
    ccchef_case "{$ccchef_min, \"initial_attributes\": {\"tab\": \"a\\tb\\nc\"}}"
    ccchef_case "{$ccchef_min, \"run_list\": [\"r\"], \"initial_attributes\": {\"k\": \"v\"}}"
    ccchef_case "{$ccchef_min, \"firstboot_path\": \"/tmp/x/y/fb.json\"}"
    ccchef_case "{$ccchef_min, \"firstboot_path\": \"fb.json\"}"
    ccchef_case "{$ccchef_min, \"firstboot_path\": \"/fb.json\"}"
    ccchef_case "{$ccchef_min, \"firstboot_path\": \"//a//fb.json\"}"
    ccchef_case "{$ccchef_min, \"firstboot_path\": \"\"}"
    ccchef_case "{$ccchef_min, \"firstboot_path\": null}"
    ccchef_case "{$ccchef_min, \"firstboot_path\": 5}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"errors\": {\"write_file /etc/chef/firstboot.json mode=0644\": \"ro fs\"}"

    # --- install types --------------------------------------------------------
    ccchef_case "{$ccchef_min, \"install_type\": \"packages\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"bogus\"}"
    ccchef_case "{$ccchef_min, \"install_type\": null}"
    ccchef_case "{$ccchef_min, \"install_type\": \"\"}"
    ccchef_case "{$ccchef_min, \"install_type\": 1}"
    ccchef_case "{$ccchef_min, \"install_type\": true}"
    ccchef_case "{$ccchef_min, \"install_type\": []}"
    ccchef_case "{$ccchef_min, \"install_type\": {}}"
    ccchef_case "{$ccchef_min}" \
        "$ccchef_tpl, \"errors\": {\"install_packages ['chef']\": \"apt fail\"}"

    # --- force_install, exec, and whether the client is already there ---------
    ccchef_case "{$ccchef_min}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"force_install\": true}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"force_install\": \"true\"}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"force_install\": \"yes\"}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"force_install\": 1}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"force_install\": 0}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": \"true\"}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": false}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": 1}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true}"
    ccchef_case "{$ccchef_min, \"exec\": true, \"force_install\": true}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true}" \
        "$ccchef_tpl, \"errors\": {\"install_packages ['chef']\": \"no network\"}"
    ccchef_case "{$ccchef_min}" "$ccchef_tpl, \"exes\": [\"/usr/bin/chef\"]"

    # --- run_chef -------------------------------------------------------------
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": []}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": [\"-l\", \"debug\"]}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": [\"--once\"]}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": [null]}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": [true, 5]}" "$ccchef_installed"
    # A string is appended whole, spaces and all (B110).
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": \"-l debug\"}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": \"-l\"}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": \"\"}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": 5}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": {}}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"exec_arguments\": {\"a\": 1}}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true}" \
        "$ccchef_installed, \"errors\": {\"subp /usr/bin/chef-client -d -i 1800 -s 20\": \"chef died\"}"

    # --- delete_validation_post_exec ------------------------------------------
    ccchef_pem="$ccchef_installed, \"files\": {\"/etc/chef/validation.pem\": \"pem\"}"
    ccchef_case "{$ccchef_min, \"exec\": true, \"delete_validation_post_exec\": true}" "$ccchef_pem"
    ccchef_case "{$ccchef_min, \"exec\": true, \"delete_validation_post_exec\": true}" "$ccchef_installed"
    ccchef_case "{$ccchef_min, \"exec\": true, \"delete_validation_post_exec\": false}" "$ccchef_pem"
    ccchef_case "{$ccchef_min, \"exec\": true, \"delete_validation_post_exec\": \"true\"}" "$ccchef_pem"
    ccchef_case "{$ccchef_min, \"exec\": true, \"delete_validation_post_exec\": \"x\"}" "$ccchef_pem"
    ccchef_case "{$ccchef_min, \"exec\": false, \"delete_validation_post_exec\": true}" "$ccchef_pem"
    # The custom key is ignored: it is always the constant path that is removed.
    ccchef_case "{$ccchef_min, \"exec\": true, \"delete_validation_post_exec\": true, \"validation_key\": \"/etc/chef/other.pem\"}" \
        "$ccchef_installed, \"files\": {\"/etc/chef/other.pem\": \"pem\"}"
    ccchef_case "{$ccchef_min, \"exec\": true, \"delete_validation_post_exec\": true}" \
        "$ccchef_pem, \"errors\": {\"unlink /etc/chef/validation.pem\": \"busy\"}"

    # --- omnibus --------------------------------------------------------------
    #
    # `omnibus_url_retries` comes back as the integer 0 when the key is absent,
    # so OMNIBUS_URL_RETRIES = 5 is unreachable from `handle` (B106).
    ccchef_omni="$ccchef_tpl, \"urls\": {\"https://www.chef.io/chef/install.sh\": \"#!/bin/sh\\necho hi\\n\", \"https://omni.example/x.sh\": \"#!/bin/sh\\necho custom\\n\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\"}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url\": \"https://omni.example/x.sh\"}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url\": null}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url\": \"\"}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url\": 5}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url_retries\": 0}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url_retries\": 3}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url_retries\": \"2\"}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url_retries\": true}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url_retries\": null}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url_retries\": \"x\"}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url_retries\": 3.7}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_version\": \"14.0\"}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_version\": 14}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_version\": \"\"}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_version\": null}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_version\": \"16\", \"omnibus_url_retries\": 4}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"exec\": true}" "$ccchef_omni"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\"}" \
        "$ccchef_tpl, \"urls\": {\"https://www.chef.io/chef/install.sh\": \"\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\"}" "$ccchef_tpl"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\"}" \
        "$ccchef_omni, \"tmpdir\": \"/var/tmp/xyz\""
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\"}" \
        "$ccchef_omni, \"errors\": {\"tempdir\": \"no space\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\"}" \
        "$ccchef_omni, \"errors\": {\"write_file /tmp/tmpdir/chef-omnibus-install mode=0700\": \"ro\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\"}" \
        "$ccchef_omni, \"errors\": {\"subp /tmp/tmpdir/chef-omnibus-install\": \"exec fail\"}"

    # --- gems -----------------------------------------------------------------
    #
    # `-v %s` is one argv element with a space in it (B109).
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"ruby_version\": \"2.7\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"ruby_version\": \"\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"ruby_version\": null}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"ruby_version\": 3}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"version\": \"12.0\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"version\": \"\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"version\": null}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"version\": 5}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"ruby_version\": \"2.1\", \"version\": \"13\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\"}" \
        "$ccchef_tpl, \"files\": {\"/usr/bin/gem\": \"x\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\"}" \
        "$ccchef_tpl, \"files\": {\"/usr/bin/gem\": \"x\", \"/usr/bin/ruby\": \"y\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\", \"ruby_version\": \"2.7\"}" \
        "$ccchef_tpl, \"files\": {\"/usr/bin/ruby\": \"y\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\"}" \
        "$ccchef_tpl, \"errors\": {\"install_packages ['ruby1.8', 'ruby1.8-dev', 'libopenssl-ruby1.8']\": \"no pkg\"}"
    ccchef_case "{$ccchef_min, \"install_type\": \"gems\"}" \
        "$ccchef_tpl, \"errors\": {\"sym_link /usr/bin/gem1.8 /usr/bin/gem\": \"exists\"}"

    # --- everything at once ---------------------------------------------------
    ccchef_case "{$ccchef_min, \"install_type\": \"omnibus\", \"omnibus_url\": \"https://omni.example/x.sh\", \"omnibus_url_retries\": 2, \"omnibus_version\": \"17.0.242\", \"force_install\": true, \"exec\": true, \"exec_arguments\": [\"-l\", \"info\"], \"delete_validation_post_exec\": true, \"validation_cert\": \"-----BEGIN RSA-----\\nabc\\n-----END-----\\n\", \"validation_key\": \"/etc/chef/validation.pem\", \"run_list\": [\"recipe[apt]\", \"role[web]\"], \"initial_attributes\": {\"apt\": {\"mirror\": \"http://m\"}, \"n\": 3}, \"directories\": [\"/opt/chef\", \"/etc/chef\"], \"node_name\": \"bignode\", \"environment\": \"staging\", \"chef_license\": \"accept-no-persist\", \"show_time\": false, \"log_level\": \":warn\"}" \
        "$ccchef_omni, \"files\": {\"/etc/chef/validation.pem\": \"old\"}, \"dirs\": {\"/var/cache/chef\": [\"m\"]}"

    run_batch "cc_chef" \
        "cd /tmp && python3 '$CCCHEF_PY' --batch" \
        "cd /tmp && '$CCCHEF_RS' --batch" \
        "$WORK/ccchef.cases"
fi

#!!END-OF-SECTIONS
printf '\n%s passed, %s failed' "$pass" "$fail"
if [ "${DIFF_SUBSET-0}" != 0 ]; then
    printf ' -- SUBSET, %s sections not run\n' "${DIFF_SUBSET}"
else
    printf ' (all sections)\n'
fi
[ "$fail" -eq 0 ]
