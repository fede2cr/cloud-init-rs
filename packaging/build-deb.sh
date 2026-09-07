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
# libssl-dev + pkg-config: openssl-sys links against the distribution's
# libssl, so the package picks up Ubuntu's OpenSSL security updates through a
# dependency on libssl3 that dpkg-shlibdeps derives from the linked binary.
# apparmor: for apparmor_parser, which the acceptance test uses to prove the
# shipped profile actually parses against this release's abstractions. It is a
# build-time tool only; nothing in the package depends on it.
apt-get install -y -qq --no-install-recommends \
    build-essential dpkg-dev cargo rustc ca-certificates libssl-dev pkg-config \
    apparmor

src=$BUILD/cloud-init-rs
mkdir -p "$src"
tar -C /src -cf - \
    --exclude=./target --exclude=./.git --exclude=./dist . |
    tar -C "$src" -xf -
cd "$src"

sh packaging/stamp-version.sh "$VERSION"

# dpkg sorts `~` before everything, including the empty string, so v1.2.3-rc1
# stays older than v1.2.3 rather than newer. The revision is not decoration:
# cargo-deb always names the changelog changelog.Debian.gz, which is only the
# correct name for a non-native package.
#
# `-0ubuntu1~<release>.1` is the Ubuntu backport form, and the `0` says this has
# never been in Debian. The release comes from the container, which is the
# release being built for.
deb_version="$(printf '%s' "$VERSION" | sed 's/-/~/')-0ubuntu1~$(. /etc/os-release && printf '%s' "$VERSION_ID").1"

# Upstream's `_DOWNSTREAM_VERSION`: what the `agent=` field of an Azure
# provisioning report and the User-Agent header name themselves as. Exported
# before the build because `option_env!` reads it at compile time.
export CLOUD_INIT_RS_DOWNSTREAM_VERSION="$deb_version"

# Reuses an already-installed cargo-deb, so a cached CARGO_HOME makes repeat
# runs cheap.
cargo install cargo-deb --locked

# Build and test explicitly rather than letting cargo-deb drive the build: this
# is the only place the release binaries are produced, so they go through the
# same --locked gate as everything else, and the tests run on the architecture
# being shipped.
cargo build --release --locked \
    -p cloud-init -p cloud-id -p cloud-init-per \
    -p ds-identify -p cloud-init-generator
cargo test --release --locked --workspace

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
# The path cargo-deb reports, not whatever else the caller left in $OUT: /out is
# the caller's directory, and globbing it once picked a stale .deb for another
# architecture and failed the acceptance test against a package this run had
# not built.
deb=$(cargo deb --no-build -p cloud-init --deb-version "$deb_version" --output "$OUT")
ls -l "$OUT"

# --- acceptance test -------------------------------------------------------
#
# The package's central claim is that installing it cannot change how the
# machine boots. That is a property of the file list and the control fields, so
# it can be checked instead of asserted, and it is checked here rather than in
# a reviewer's head. Everything below runs against the .deb that is about to be
# published; a failure fails the release.

fail=0
check() {
    if [ "$2" = ok ]; then
        printf 'ok    %s\n' "$1"
    else
        printf 'FAIL  %s\n' "$1"
        fail=1
    fi
}

contents=$(dpkg-deb -c "$deb" | awk '{print $6}' | sed 's|^\./|/|')
fields=$(dpkg-deb -f "$deb")

# Any path outside these two prefixes is a path this package must not own.
stray=$(printf '%s\n' "$contents" |
    grep -v '/$' |
    grep -vE '^/usr/(libexec/cloud-init-rs|share)/' || true)
if [ -n "$stray" ]; then
    printf 'unexpected paths:\n%s\n' "$stray"
    check "installs nothing outside /usr/libexec/cloud-init-rs and /usr/share" bad
else
    check "installs nothing outside /usr/libexec/cloud-init-rs and /usr/share" ok
fi

# The specific paths that would make the package boot-affecting. Named
# individually because "outside two prefixes" would not catch a future asset
# that is inside /usr/share but still read by systemd.
for forbidden in \
    /usr/lib/systemd /lib/systemd /etc/systemd \
    /usr/lib/tmpfiles.d /etc/tmpfiles.d \
    /usr/lib/udev /etc/udev \
    /etc/apparmor.d \
    /etc/cloud /usr/bin /usr/lib/cloud-init; do
    if printf '%s\n' "$contents" | grep -q "^$forbidden"; then
        check "does not install into $forbidden" bad
    else
        check "does not install into $forbidden" ok
    fi
done

# No relationship with the Python package, in either direction. `Depends` is
# excluded from the grep because $auto legitimately resolves to libc/libssl.
if printf '%s\n' "$fields" |
    grep -E '^(Provides|Conflicts|Replaces|Breaks):' |
    grep -q 'cloud-init'; then
    check "declares no Provides/Conflicts/Replaces against cloud-init" bad
else
    check "declares no Provides/Conflicts/Replaces against cloud-init" ok
fi
if printf '%s\n' "$fields" | grep -E '^Depends:' | grep -qE '(^|[ ,])cloud-init([ ,]|$)'; then
    check "does not depend on the cloud-init package" bad
else
    check "does not depend on the cloud-init package" ok
fi

# cargo-deb writes no maintainer scripts for this package; if one ever appears
# it has to be reviewed deliberately, because §6.7 requires them to be
# idempotent and to never touch /var/lib/cloud.
if dpkg-deb --ctrl-tarfile "$deb" | tar -t | grep -qE '(preinst|postinst|prerm|postrm)$'; then
    check "ships no maintainer scripts" bad
else
    check "ships no maintainer scripts" ok
fi

# Install for real and exercise what was installed. Done last so that a failure
# above is reported before the system is modified.
dpkg -i "$deb" || apt-get install -y -qq -f
libexec=/usr/libexec/cloud-init-rs

for bin in cloud-init cloud-id cloud-init-per ds-identify cloud-init-generator; do
    if [ -x "$libexec/$bin" ]; then
        check "$bin is installed and executable" ok
    else
        check "$bin is installed and executable" bad
    fi
done

# The version reported is the upstream release being tracked, not the crate
# version, so it must not move when the tag does. The binary prints its own
# argv[0] first, exactly as upstream does.
if [ "$("$libexec/cloud-init" --version)" = "$libexec/cloud-init 26.1" ]; then
    check "--version reports the tracked upstream release" ok
else
    check "--version reports the tracked upstream release" bad
fi

# The agent string is the other half: it names the package, not the upstream
# release, so a provisioning report says which implementation wrote it.
if grep -qF "Cloud-Init-rs/" "$libexec/cloud-init" &&
    grep -qF "$deb_version" "$libexec/cloud-init"; then
    check "the agent string carries the package version" ok
else
    check "the agent string carries the package version" bad
fi

# Prove the audit runs on a real install, and that the fragment shipped in
# /usr/share was generated by the very binary shipped next to it.
"$libexec/cloud-init" devel verify-layout || true
"$libexec/cloud-init" devel verify-layout --dump-tmpfiles >"$BUILD/tmpfiles.expected"
if cmp -s "$BUILD/tmpfiles.expected" \
    /usr/share/cloud-init-rs/tmpfiles.d/cloud-init-rs.conf; then
    check "the shipped tmpfiles fragment matches the shipped binary" ok
else
    check "the shipped tmpfiles fragment matches the shipped binary" bad
fi

# Same argument for the SELinux file contexts: a policy module built from a
# stale .fc would relabel the shared state tree into something the running
# binary does not agree with, which is worse than shipping no policy at all.
"$libexec/cloud-init" devel verify-layout --dump-selinux-fc >"$BUILD/fc.expected"
if cmp -s "$BUILD/fc.expected" /usr/share/cloud-init-rs/selinux/cloud-init-rs.fc; then
    check "the shipped SELinux file contexts match the shipped binary" ok
else
    check "the shipped SELinux file contexts match the shipped binary" bad
fi

# And for the AppArmor profile, with one more check the other two cannot have:
# a profile is a program the kernel compiles, so it can be checked for more
# than staleness. `-Q` stops short of loading it and `-T` ignores the cache, so
# this is a pure syntax and semantics check against the abstractions of the
# release being built for. It catches the mistake that is easy to make and
# invisible on inspection: two overlapping rules with different exec
# transitions, which apparmor_parser rejects outright rather than resolving by
# precedence.
apparmor=/usr/share/cloud-init-rs/apparmor/cloud-init-rs
"$libexec/cloud-init" devel verify-layout --dump-apparmor >"$BUILD/apparmor.expected"
if cmp -s "$BUILD/apparmor.expected" "$apparmor"; then
    check "the shipped AppArmor profile matches the shipped binary" ok
else
    check "the shipped AppArmor profile matches the shipped binary" bad
fi
if apparmor_parser -QT "$apparmor"; then
    check "the shipped AppArmor profile parses" ok
else
    check "the shipped AppArmor profile parses" bad
fi
# Complain mode is what makes the profile safe to ship at all: it is generated
# from source, not measured from real boots, so enforcing it would deny
# something real on the first unusual cloud. Enforce is a Phase 7 decision.
if grep -q 'flags=(complain)' "$apparmor"; then
    check "the shipped AppArmor profile is in complain mode" ok
else
    check "the shipped AppArmor profile is in complain mode" bad
fi

dpkg -r cloud-init-rs
if [ -e "$libexec" ]; then
    check "removal leaves nothing behind under $libexec" bad
else
    check "removal leaves nothing behind under $libexec" ok
fi

[ "$fail" -eq 0 ] || {
    echo "package acceptance test failed"
    exit 1
}
echo "package acceptance test passed"
