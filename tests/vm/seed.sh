#!/bin/sh
# Assemble tests/vm/user-data.yaml and tests/vm/user-data-script.sh into the
# MIME multipart a datasource carries, and optionally seed a machine with it.
#
#   sh tests/vm/seed.sh build            # write ./user-data.mime and stop
#   sudo sh tests/vm/seed.sh install     # seed NoCloud, clean, and stop
#   sudo sh tests/vm/seed.sh install --reboot
#   sudo sh tests/vm/seed.sh revert      # remove the seed and the override
#   sh tests/vm/seed.sh report           # print the verdict after a boot
#
# `install` is the interesting one and it is deliberately not `--reboot` by
# default: it rewrites the datasource list, and an admin should see what it did
# before the machine acts on it.
#
# Why NoCloud and not Azure: Azure CustomData is fixed when the VM is created,
# so testing a change to a config module on an existing VM means either
# redeploying or seeding locally. The module under test cannot tell the
# difference -- it is handed a config dict either way -- and NoCloud is the
# only one of the two that is free and reversible.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
SEED=/var/lib/cloud/seed/nocloud
OVERRIDE=/etc/cloud/cloud.cfg.d/99-cloud-init-rs-test.cfg
OUT=${OUT:-$PWD/user-data.mime}
TESTDIR=/var/log/cloud-init-rs-test

need_root() {
    [ "$(id -u)" -eq 0 ] || {
        echo "$0: must run as root" >&2
        exit 1
    }
}

build() {
    # The archive's own version for `hello`, so the [name, version] form is
    # pinned to something that exists on this machine today. apt-cache is used
    # rather than a hard-coded string because the point of the check is that
    # the version survives the round trip, not what the version is.
    version=$(apt-cache policy hello 2>/dev/null |
        sed -n 's/^ *Candidate: *//p' | head -n1)
    [ -n "$version" ] && [ "$version" != "(none)" ] || {
        echo "$0: no candidate version for 'hello'; run apt-get update" >&2
        exit 1
    }
    echo "pinning hello=$version"

    sed "s/@HELLO_VERSION@/$version/g" "$HERE/user-data.yaml" >"$OUT.cfg"

    # Built with the email module rather than by hand: the boundary has to not
    # occur in either part, and Content-Type has to carry it, and getting
    # either wrong produces a document cloud-init treats as a single opaque
    # blob instead of two parts.
    OUT="$OUT" CFG="$OUT.cfg" SH="$HERE/user-data-script.sh" python3 - <<'EOF'
import email.mime.multipart, email.mime.text, os

msg = email.mime.multipart.MIMEMultipart()
for path, subtype, name in (
    (os.environ["CFG"], "cloud-config", "cloud-config.txt"),
    (os.environ["SH"], "x-shellscript", "user-script.sh"),
):
    with open(path) as fh:
        part = email.mime.text.MIMEText(fh.read(), subtype)
    part.add_header("Content-Disposition", "attachment", filename=name)
    msg.attach(part)
with open(os.environ["OUT"], "w") as fh:
    fh.write(msg.as_string())
EOF
    rm -f "$OUT.cfg"
    echo "wrote $OUT ($(wc -c <"$OUT") bytes)"
}

install_seed() {
    need_root
    build
    mkdir -p "$SEED" "$TESTDIR"
    cp "$OUT" "$SEED/user-data"
    # A new instance-id is what makes the per-instance modules -- which is all
    # of the ones under test -- run again instead of being skipped.
    printf 'instance-id: rs-test-%s\nlocal-hostname: %s\n' \
        "$(date +%s)" "$(hostname)" >"$SEED/meta-data"

    # 90_dpkg.cfg pins datasource_list to [Azure] on this image. NoCloud has to
    # come first, and Azure has to stay, so a revert without a reboot still
    # finds a datasource.
    cat >"$OVERRIDE" <<'EOF'
# Added by tests/vm/seed.sh. Remove with `seed.sh revert`.
datasource_list: [NoCloud, Azure]
EOF
    rm -rf "${TESTDIR:?}"/*
    # Recorded before the reboot so the script can tell "cc_ssh regenerated the
    # host keys" apart from "cc_ssh never touched them".
    ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub \
        >"$TESTDIR/old-fingerprints.txt" 2>/dev/null || true
    cloud-init clean --logs
    echo "seeded. reboot to run it."
}

revert() {
    need_root
    rm -f "$OVERRIDE"
    rm -rf "$SEED"
    cloud-init clean --logs
    echo "reverted; datasource_list is the image's again"
}

report() {
    if [ -f "$TESTDIR/result.txt" ]; then
        cat "$TESTDIR/result.txt"
    else
        echo "no result: the script part did not run"
        echo "--- cloud-init status ---"
        cloud-init status --long || true
        exit 1
    fi
    tail -n1 "$TESTDIR/result.txt" | grep -q '^PASS$'
}

case ${1:-build} in
build) build ;;
install)
    install_seed
    # An `if`, not `&&`: as the last command in the branch a false test becomes
    # the script's exit status, and a caller running under `set -e` then stops
    # without ever reaching its own reboot.
    if [ "${2:-}" = --reboot ]; then
        systemctl reboot
    fi
    ;;
revert) revert ;;
report) report ;;
*)
    echo "usage: $0 {build|install [--reboot]|revert|report}" >&2
    exit 2
    ;;
esac
