#!/bin/sh
# Differential test harness (PLAN.md §6.1).
#
# Runs the same read-only command against the packaged Python cloud-init and the
# Rust port, and fails on any stdout/stderr/exit-code divergence. Only read-only
# commands belong here: this script is expected to be safe to run on a live host.
#
# Usage: tests/differential/run.sh [path-to-rust-target-dir]

set -eu

TARGET="${1:-target/debug}"
PY_CLOUD_INIT="${PY_CLOUD_INIT:-/usr/bin/cloud-init}"
PY_CLOUD_ID="${PY_CLOUD_ID:-/usr/bin/cloud-id}"

if [ ! -x "$PY_CLOUD_INIT" ]; then
    echo "SKIP: python cloud-init not found at $PY_CLOUD_INIT" >&2
    exit 77
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0

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
for opts in "" "--long" "--format=json" "--format=yaml" "--format=tabular"; do
    run_pair "cloud-init status $opts" \
        "$PY_CLOUD_INIT status $opts" \
        "$TARGET/cloud-init status $opts"
done

# --- cloud-init query --------------------------------------------------------
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
run_pair "cloud-init devel render" \
    "$PY_CLOUD_INIT devel render -i '$ID' '$WORK/user-data'" \
    "$TARGET/cloud-init devel render -i '$ID' '$WORK/user-data'"

# --- cloud-init analyze ------------------------------------------------------
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
EOF

    for fixture in "$UD"/*; do
        name=$(basename "$fixture")
        # Python must not run from inside the cloudinit package directory.
        run_pair "user-data $name" \
            "cd /tmp && python3 '$UD_PY' <'$fixture'" \
            "cd /tmp && '$UD_RS' <'$fixture'"
    done
fi

# --- part handlers -----------------------------------------------------------
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

# --- cloud-id ----------------------------------------------------------------
if [ -x "$PY_CLOUD_ID" ]; then
    for opts in "" "-l" "-j"; do
        run_pair "cloud-id $opts" \
            "$PY_CLOUD_ID -i '$ID' $opts" \
            "$TARGET/cloud-id -i '$ID' $opts"
    done
fi

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
