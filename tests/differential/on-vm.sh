#!/bin/sh
# Run the differential harness on the Azure arm64 VM instead of this host.
#
# The VM is several times faster than WSL for this workload, and it is arm64,
# which the suite needs anyway: `O_NOFOLLOW` differs between x86_64 and aarch64
# and a hardcoded literal once silently disabled the symlink defence on every
# arm64 build (COMPAT deviation 124).
#
# Usage:
#   tests/differential/on-vm.sh                 # whole suite
#   ONLY='locale timezone' tests/differential/on-vm.sh
#   tests/differential/on-vm.sh --shell         # interactive shell in the tree
#   tests/differential/on-vm.sh --install       # rebuild and reinstall the .deb
#
# Environment:
#   VM        ssh destination           (default azureuser@57.154.14.153)
#   VM_RG     resource group            (default REPROS)
#   VM_NAME   VM name                   (default ocs-ubuntu2604-arm64)
#   VM_SRC    remote tree               (default /home/azureuser/src)
#   ONLY/SKIP passed through to run.sh
#   NO_BUILD  set to skip the container build (reuse the last one)
#
# The VM boots with a REAL Azure datasource, so a handful of sections compare
# host state and cannot agree with a laptop's. They are dropped by default via
# VM_SKIP rather than being marked flaky in run.sh, because on a machine with
# no datasource they are exactly the cases that matter.
#
# `/usr/bin/cloud-init` on that VM is an *alternative*, and it has been pointed
# at the port. A stale package there answers `single --name <unported module>`
# with the module's own "Running module" line and then silence, which reads
# exactly like upstream Python skipping the module -- so the banner below says
# which implementation and which version is installed before anything runs.

set -eu

VM="${VM:-azureuser@57.154.14.153}"
VM_RG="${VM_RG:-REPROS}"
VM_NAME="${VM_NAME:-ocs-ubuntu2604-arm64}"
VM_SRC="${VM_SRC:-/home/azureuser/src}"
ONLY="${ONLY:-}"
SKIP="${SKIP:-}"

# Sections whose result depends on the host having no cloud datasource and no
# provisioned /etc/cloud. Keep this list short and justified; anything else
# differing on the VM is a real finding.
VM_SKIP="${VM_SKIP:-boot-stages boot-stage-reporting boot-stage-logging \
recoverable-errors the-cache-trust-decision merged-system-config clean}"

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

say() { printf '\033[1m== %s\033[0m\n' "$*" >&2; }

# The VM is usually deallocated between sessions; starting it is idempotent and
# a no-op when it is already running.
if ! ssh -o ConnectTimeout=10 -o BatchMode=yes "$VM" true 2>/dev/null; then
    say "no answer from $VM, starting $VM_NAME"
    az vm start -g "$VM_RG" -n "$VM_NAME" >/dev/null
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12; do
        if ssh -o ConnectTimeout=10 -o BatchMode=yes "$VM" true 2>/dev/null; then
            break
        fi
    done
    ssh -o ConnectTimeout=10 -o BatchMode=yes "$VM" true
fi

say "syncing $ROOT -> $VM:$VM_SRC"
rsync -a --delete \
    --exclude=target --exclude=.git --exclude=dist --exclude='.subset.sh' \
    "$ROOT/" "$VM:$VM_SRC/"

# Which implementation `cloud-init` means on that machine, and how old it is.
ssh "$VM" "printf '/usr/bin/cloud-init -> %s (cloud-init-rs %s)\n' \
    \"\$(readlink -f /usr/bin/cloud-init)\" \
    \"\$(dpkg-query -W -f='\${Version}' cloud-init-rs 2>/dev/null || echo absent)\"" |
    while read -r line; do say "$line"; done

if [ "${1-}" = "--install" ]; then
    # A dev version so dpkg sees it as newer than the last real release.
    version="0.2.3-dev$(date +%Y%m%d%H%M)"
    say "building the .deb on the VM as $version"
    ssh "$VM" "set -eu
        cd $VM_SRC && mkdir -p dist
        sudo -n docker run --rm -e VERSION='$version' \
            -v $VM_SRC:/src:ro -v $VM_SRC/dist:/out \
            ubuntu:26.04 sh /src/packaging/build-deb.sh 2>&1 | tail -3
        sudo -n dpkg -i dist/cloud-init-rs_\$(printf '%s' '$version' | tr - '~')-0ubuntu1~\$(. /etc/os-release && printf '%s' \"\$VERSION_ID\").1_\$(dpkg --print-architecture).deb
        sh packaging/alternatives.sh status | sed -n '6,8p'"
    exit 0
fi

if [ "${1-}" = "--shell" ]; then
    exec ssh -t "$VM" "cd $VM_SRC && exec bash -l"
fi

# `target/` can hold root-owned artefacts from an earlier build-deb.sh run, and
# cargo then dies with a fingerprint write error rather than anything readable.
ssh "$VM" "sudo -n chown -R azureuser:azureuser $VM_SRC/target 2>/dev/null || true"

if [ -z "${NO_BUILD-}" ]; then
    say "building in a container on the VM"
    ssh "$VM" "sudo -n docker run --rm \
        -v $VM_SRC:/src -v cirs-cargo:/root/.cargo -w /src ubuntu:26.04 \
        sh -c 'apt-get update -qq && apt-get install -y -qq \
            --no-install-recommends build-essential cargo rustc ca-certificates \
            libssl-dev pkg-config >/dev/null && \
            cargo build --workspace --bins --examples'" 2>&1 | tail -5
    ssh "$VM" "sudo -n chown -R azureuser:azureuser $VM_SRC/target"
fi

skip_all="$SKIP $VM_SKIP"
say "running the harness (ONLY='$ONLY' SKIP='$skip_all')"

# `umask 022` is the boot value; under 002 four state-tree cases fail for a
# reason that has nothing to do with the port.
ssh "$VM" "cd $VM_SRC && umask 022 && \
    ONLY='$ONLY' SKIP='$skip_all' sh tests/differential/run.sh target/debug"
