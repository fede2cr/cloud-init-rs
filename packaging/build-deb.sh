#!/bin/sh
# Build the cloud-init-rs .deb inside an Ubuntu 26.04 container.
#
#   mkdir -p dist
#   docker run --rm -e VERSION=1.2.3 \
#       -v "$PWD:/src:ro" -v "$PWD/dist:/out" \
#       ubuntu:26.04 sh /src/packaging/build-deb.sh
#
# The architecture of the .deb is the architecture of the container, so run it
# once per architecture on a matching host. Nothing is cross-compiled on
# purpose: a native build lets cargo-deb resolve the real shared-library
# dependencies, and lets the test suite run on the architecture being shipped.
#
# The source is copied out of /src before anything touches it, because the
# build writes target/ and a stamped Cargo.toml, and neither belongs in the
# caller's working tree.
set -eu

: "${VERSION:?set VERSION to the release version, without the leading v}"
BUILD=${BUILD:-/build}
OUT=${OUT:-/out}

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# dpkg-dev: cargo-deb shells out to it to resolve `depends = "$auto"`.
# ca-certificates: what cargo needs to reach crates.io from a bare image.
apt-get install -y -qq --no-install-recommends \
    build-essential dpkg-dev cargo rustc ca-certificates

src=$BUILD/cloud-init-rs
mkdir -p "$src"
tar -C /src -cf - \
    --exclude=./target --exclude=./.git --exclude=./dist . |
    tar -C "$src" -xf -
cd "$src"

sh packaging/stamp-version.sh "$VERSION"

# Reuses an already-installed cargo-deb, so a cached CARGO_HOME makes repeat
# runs cheap.
cargo install cargo-deb --locked

# Build and test explicitly rather than letting cargo-deb drive the build: this
# is the only place the release binaries are produced, so they go through the
# same --locked gate as everything else, and the tests run on the architecture
# being shipped.
cargo build --release --locked -p cloud-init -p cloud-id -p cloud-init-per
cargo test --release --locked --workspace

# dpkg sorts `~` before everything, including the empty string, so v1.2.3-rc1
# stays older than v1.2.3 rather than newer. The `-1` revision is not
# decoration: cargo-deb always names the changelog changelog.Debian.gz, which
# is only the correct name for a non-native package.
deb_version="$(printf '%s' "$VERSION" | sed 's/-/~/')-1"

# A native package without a changelog is a lintian error. Generated rather
# than committed: the version comes from the tag, so a checked-in changelog
# would be one more place for it to drift.
#
# The trailer has to name the same person as the maintainer field in
# crates/cloud-init/Cargo.toml; change both together.
cat >packaging/changelog.Debian <<EOF
cloud-init-rs ($deb_version) $(. /etc/os-release && printf '%s' "$VERSION_CODENAME"); urgency=medium

  * Release $VERSION. See the GitHub release notes for what changed.

 -- Álvaro Figueroa <alvaro.figueroa@microsoft.com>  $(date -uR -d "@${SOURCE_DATE_EPOCH:-$(date +%s)}")
EOF

mkdir -p "$OUT"
cargo deb --no-build -p cloud-init --deb-version "$deb_version" --output "$OUT"
ls -l "$OUT"
