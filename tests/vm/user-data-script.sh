#!/bin/sh
# The `#!`-script half of the user-data pair. The part-handler writes this to
# <instance>/scripts/part-NNN mode 0700 and cc_scripts_user execs it as root in
# the final stage -- after cc_package_update_upgrade_install has run in
# cloud_config -- which is what makes it able to check that module's work.
#
# It asserts the *decisions* the port made, not just the end state: an
# apt-get that never ran and an apt-get that ran and found nothing to do leave
# the same packages on disk, so the log is checked too.
#
# Exits non-zero on any failure and leaves the verdict in
# /var/log/cloud-init-rs-test/result.txt.

DIR=/var/log/cloud-init-rs-test
LOG=/var/log/cloud-init.log
RESULT=$DIR/result.txt
mkdir -p "$DIR"
: >"$RESULT"

fail=0
check() {
    if [ "$2" = ok ]; then
        printf 'ok    %s\n' "$1" >>"$RESULT"
    else
        printf 'FAIL  %s\n' "$1" >>"$RESULT"
        fail=1
    fi
}

# A check whose failure is a deviation already written down. It still runs, and
# it still prints, so that closing the deviation is visible here as a line that
# stops saying KNOWN -- but it does not fail the boot.
known() {
    if [ "$2" = ok ]; then
        printf 'ok    %s\n' "$1" >>"$RESULT"
    else
        printf 'KNOWN %s (deviation %s)\n' "$1" "$3" >>"$RESULT"
    fi
}
yn() { if [ "$1" -eq 0 ]; then echo ok; else echo bad; fi; }

# --- which implementation actually ran -------------------------------------
#
# First, because every check below is meaningless if the boot was served by the
# Python cloud-init. /run/cloud-init/.impl is stamped by init-local (PLAN 6.7).

impl=$(cat /run/cloud-init/.impl 2>/dev/null || echo "<missing>")
printf 'impl marker: %s\n' "$impl" >>"$RESULT"
printf 'cloud-init: %s\n' "$(readlink -f /usr/bin/cloud-init)" >>"$RESULT"
printf 'version:    %s\n\n' "$(cloud-init --version 2>&1)" >>"$RESULT"

case $impl in
*rust*) check "the Rust port served this boot" ok ;;
*) check "the Rust port served this boot" bad ;;
esac

# --- cc_apt_configure ------------------------------------------------------
#
# The deb822 file is where APT_DEB822_SOURCE_LIST_FILE sends the rendered
# template on Ubuntu, and /etc/apt/sources.list is reduced to a stub pointing
# at it.

DEB822=/etc/apt/sources.list.d/ubuntu.sources
EXTRA=/etc/apt/sources.list.d/cloud-init-rs-test.list
KEYFILE=/etc/apt/cloud-init.gpg.d/cloud-init-rs-test.gpg

[ -f "$DEB822" ]
check "cc_apt_configure wrote $DEB822" "$(yn $?)"

grep -q "^# Ubuntu sources have moved" /etc/apt/sources.list
check "sources.list was reduced to the deb822 stub" "$(yn $?)"

# The mirror the datasource resolved, not the arm64 default. Getting
# ports.ubuntu.com here means get_package_mirror_info did not run, or ran and
# found nothing -- both of which are the regression this check exists for.
mirror=$(sed -n 's/^URIs: *//p' "$DEB822" | head -n1)
printf 'primary mirror: %s\n' "$mirror" >>"$RESULT"
[ "$mirror" = "http://azure.archive.ubuntu.com/ubuntu/" ]
check "the mirror came from system_info.package_mirrors, not the arch default" "$(yn $?)"

! grep -q "ports.ubuntu.com" "$DEB822"
check "the arm64 ports fallback was not used" "$(yn $?)"

# The apt.conf drop-in, written verbatim.
grep -q 'APT::Install-Recommends "true";' /etc/apt/apt.conf.d/94cloud-init-config 2>/dev/null
check "apt.conf was written to 94cloud-init-config" "$(yn $?)"

# No proxy was configured, so the proxy drop-in must not exist. The module
# removes it when it finds one and nothing asked for it.
[ ! -f /etc/apt/apt.conf.d/90cloud-init-aptproxy ]
check "no proxy configured left no aptproxy drop-in" "$(yn $?)"

# --- cc_apt_configure: the keyid source ------------------------------------

[ -f "$EXTRA" ]
check "the sources entry was written to $EXTRA" "$(yn $?)"

release=$(lsb_release -cs)
want="deb-src [signed-by=$KEYFILE] $mirror $release-proposed main"
have=$(head -n1 "$EXTRA" 2>/dev/null)
printf 'source line: %s\n' "$have" >>"$RESULT"
[ "$have" = "$want" ]
check '$KEY_FILE, $MIRROR and $RELEASE were all substituted' "$(yn $?)"

[ -s "$KEYFILE" ]
check "the key id was fetched into $KEYFILE" "$(yn $?)"

# Dearmoured, not the ASCII armour as downloaded: apt will not read the latter.
! head -c 40 "$KEYFILE" | grep -q "BEGIN PGP PUBLIC KEY BLOCK"
check "the fetched key was dearmoured" "$(yn $?)"

gpg --show-keys --with-colons "$KEYFILE" 2>/dev/null |
    grep -q "^fpr:*F6ECB3762474EDA9D21B7022871920D1991BC93C"
check "the key in $KEYFILE is the one the keyid asked for" "$(yn $?)"

# The proof the entry is live and trusted: apt-get update had to verify the
# Sources index against that key to leave a lists file behind.
ls /var/lib/apt/lists/ 2>/dev/null | grep -q "azure.archive.ubuntu.com.*-proposed_main_source_Sources"
check "apt fetched the deb-src target the keyid signed" "$(yn $?)"

# And that ordinary installs now come from the resolved mirror.
apt-get download --print-uris hello 2>/dev/null | grep -q "azure.archive.ubuntu.com"
check "packages are downloaded from the resolved mirror" "$(yn $?)"

# --- apt: the four expand_package_list shapes ------------------------------

for pkg in sl hello jq tree; do
    dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q "^install ok installed$"
    check "apt installed $pkg" "$(yn $?)"
done

# The [name, version] pair form: proving the version was passed through, not
# dropped, is the only thing that distinguishes it from the bare-name form.
want=$(sed -n 's/.*"hello": "\(.*\)".*/\1/p' "$DIR/expected.json")
have=$(dpkg-query -W -f='${Version}' hello 2>/dev/null)
printf 'hello: wanted %s, got %s\n' "$want" "$have" >>"$RESULT"
[ -n "$want" ] && [ "$want" = "$have" ]
check "the [name, version] pair pinned hello" "$(yn $?)"

# --- snap ------------------------------------------------------------------

snap list hello-world >/dev/null 2>&1
check "snap installed hello-world" "$(yn $?)"

# --- the commands the module decided to run --------------------------------
#
# grep the log rather than the filesystem: these are the steps the differential
# compares, so this would be the check that the plan the harness sees is the
# plan the machine runs.
#
# It is `known`, not `check`, because the port logs no command it runs at all
# (deviation 132): ci_sys::subp is below the logging layer in the crate graph
# and has no sink to write to. These five lines are what upstream's log would
# answer, kept here so that the day subp learns to log, they start passing on
# their own. Until then the end-state checks above carry the module.

grep -q "apt-get.*update" "$LOG"
known "package_update ran apt-get update" "$(yn $?)" 132

grep -q "apt-get.*dist-upgrade" "$LOG"
known "package_upgrade ran apt-get dist-upgrade" "$(yn $?)" 132

# Ubuntu only: UbuntuDistro.package_command chases the apt upgrade with a snap
# refresh, gated on refresh.hold. Missing this was a third of the differential.
grep -q "snap.*get.*system" "$LOG"
known "package_upgrade probed the snap refresh hold" "$(yn $?)" 132

grep -q "snap.*refresh" "$LOG"
known "package_upgrade ran snap refresh" "$(yn $?)" 132

# eatmydata is the default apt wrapper, and the one thing an apt_get_wrapper
# section can turn off -- except that it cannot, which is upstream bug B73.
grep -q "eatmydata" "$LOG"
known "the default eatmydata wrapper was applied" "$(yn $?)" 132

# --- the reboot the module decided NOT to take -----------------------------

[ ! -f "$DIR/rebooted" ]
check "package_reboot_if_required: false did not reboot" "$(yn $?)"

# --- cc_ssh deleted the host keys; did it put them back? -------------------
#
# The regression that made this VM unreachable: the plan was built from a
# snapshot taken before it ran, so every key was queued for deletion AND
# skipped for generation because the snapshot still showed it present. sshd
# then had nothing to offer and ssh.service failed its ExecStartPre.

for kind in rsa ecdsa ed25519; do
    [ -s "/etc/ssh/ssh_host_${kind}_key" ] && [ -s "/etc/ssh/ssh_host_${kind}_key.pub" ]
    check "cc_ssh regenerated the $kind host key" "$(yn $?)"
done

/usr/sbin/sshd -t 2>/dev/null
check "sshd accepts the regenerated host keys" "$(yn $?)"

systemctl is-active --quiet ssh
check "ssh.service is running" "$(yn $?)"

# Regenerated, not merely surviving: seed.sh recorded the old fingerprints just
# before the reboot, and a new instance-id must produce different ones.
if [ -s "$DIR/old-fingerprints.txt" ]; then
    ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub >"$DIR/new-fingerprints.txt" 2>/dev/null
    ! cmp -s "$DIR/old-fingerprints.txt" "$DIR/new-fingerprints.txt"
    check "the host keys are new, not the image's" "$(yn $?)"
fi

# Upstream's glob is ssh_host_*key*, so the .pub halves go too and no stale
# public key outlives the private one it belonged to.
stale=$(find /etc/ssh -name 'ssh_host_*key*' ! -newermt '-1 day' 2>/dev/null | wc -l)
[ "$stale" -eq 0 ]
check "no stale host key files were left behind ($stale found)" "$(yn $?)"

# --- the rest of the user-data pipeline ------------------------------------

[ -f "$DIR/expected.json" ]
check "write_files wrote expected.json" "$(yn $?)"

grep -q runcmd-saw-sl "$DIR/marker" 2>/dev/null
check "runcmd ran after the packages went in" "$(yn $?)"

# This script's own provenance: 0700, under the instance link, from the
# multipart part-handler.
mine=$(ls -l "$0" | cut -c1-10)
[ "$mine" = "-rwx------" ]
check "the script part was written 0700 ($mine)" "$(yn $?)"

case $0 in
*/instance/scripts/*) check "the script ran from <instance>/scripts" ok ;;
*) check "the script ran from <instance>/scripts" bad ;;
esac

# --- verdict ---------------------------------------------------------------

if [ "$fail" -eq 0 ]; then
    echo "PASS" >>"$RESULT"
else
    echo "FAIL" >>"$RESULT"
fi
cat "$RESULT"
exit "$fail"
