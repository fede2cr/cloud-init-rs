#!/bin/sh
# Build the cloud-init-rs .rpm inside an Azure Linux or Fedora container.
#
#   mkdir -p dist
#   docker run --rm -e VERSION=1.2.3 \
#       -v "$PWD:/src:ro" -v "$PWD/dist:/out" \
#       mcr.microsoft.com/azurelinux-beta/base/core:4.0 sh /src/packaging/build-rpm.sh
#
# Azure Linux 4 is the priority target and Fedora is built from the same script;
# the only differences between them are the package manager, the names of three
# build dependencies and the dist tag, all resolved from /etc/os-release below.
# Anything else that has to differ per distribution is a sign the coexistence
# claim has stopped being the same claim, and should be argued in COMPAT.md
# rather than hidden behind a case statement here.
#
# As with build-deb.sh, the architecture of the .rpm is the architecture of the
# container, so run it once per architecture on a matching host. Nothing is
# cross-compiled: a native build lets find-requires resolve the real shared
# library dependencies, and lets the test suite run on the architecture being
# shipped.
#
# The source is copied out of /src before anything touches it, because the build
# writes target/ and a stamped Cargo.toml, and neither belongs in the caller's
# working tree.
set -eu

: "${VERSION:?set VERSION to the release version, without the leading v}"
BUILD=${BUILD:-/build}
OUT=${OUT:-/out}

# --- distribution ----------------------------------------------------------
#
# There is no rpmbuild in this pipeline, so nothing expands %{?dist} and the tag
# has to be derived here. Azure Linux spells it azl<major> (its own packages are
# versioned 1.94.1-6.azl4) and Fedora spells it fc<major>.
#
# Sourced in a subshell, never in this one: /etc/os-release sets VERSION, and on
# Azure Linux 4 that is the string "4.0 (Container Image Beta)". Sourcing it here
# would overwrite the release version this script was invoked with.
os_id=$(. /etc/os-release && printf '%s' "$ID")
os_major=$(. /etc/os-release && printf '%s' "${VERSION_ID%%.*}")
case "$os_id" in
azurelinux)
    # Azure Linux 4 is dnf5. Its `tdnf` is only a compatibility symlink to
    # /usr/bin/dnf5, so calling tdnf here would work by accident and break the
    # day the symlink goes. Azure Linux 3 is genuinely tdnf and versions its
    # packages against a different macro set, so refuse it rather than emit an
    # untested package under a plausible-looking azl3 tag.
    [ "$os_major" -ge 4 ] || {
        echo "Azure Linux $os_major is tdnf-based; this script targets 4 and up" >&2
        exit 2
    }
    dist="azl$os_major"
    # azl names the pkg-config provider `pkgconf`; there is no `pkg-config`.
    pkgconf_pkg=pkgconf
    ;;
fedora)
    dist="fc$os_major"
    pkgconf_pkg=pkgconf-pkg-config
    ;;
*)
    echo "build-rpm.sh does not know how to build for '$os_id'" >&2
    echo "add a case here only if the coexistence claim still holds there" >&2
    exit 2
    ;;
esac

# rpm-build is what puts /usr/lib/rpm/find-requires on disk, which is how the
# package acquires a dependency on the distribution's OpenSSL rather than a
# hardcoded one. It is a build-time tool; nothing in the package depends on it.
# openssl-devel + pkgconf are for openssl-sys, the project's one C dependency.
#
# shadow-utils is for the test suite, not the build: ci-distro probes for passwd
# and usermod and fails when neither exists, and these minimal images ship
# neither. Ubuntu's base image has them already, which is why build-deb.sh does
# not name them.
dnf install -y --refresh \
    gcc glibc-devel binutils make tar \
    cargo rust ca-certificates \
    openssl-devel "$pkgconf_pkg" rpm-build \
    shadow-utils

# openssl-sys shells out to pkg-config by that exact name, so a provider that
# installed under a different one would fail the build several minutes later.
command -v pkg-config >/dev/null || {
    echo "$pkgconf_pkg did not provide a pkg-config binary" >&2
    exit 1
}

src=$BUILD/cloud-init-rs
mkdir -p "$src"
tar -C /src -cf - \
    --exclude=./target --exclude=./.git --exclude=./dist . |
    tar -C "$src" -xf -
cd "$src"

sh packaging/stamp-version.sh "$VERSION"

# RPM forbids `-` in a version and sorts `~` before everything including the
# empty string, so v1.2.3-rc1 becomes 1.2.3~rc1 and stays older than 1.2.3.
# This is the same transformation build-deb.sh makes for dpkg, which orders `~`
# identically — the two package managers agree on this one point.
rpm_version="$(printf '%s' "$VERSION" | sed 's/-/~/g')"
rpm_release="1.$dist"

# Upstream's `_DOWNSTREAM_VERSION`: what the `agent=` field of an Azure
# provisioning report and the User-Agent header name themselves as. Exported
# before the build because `option_env!` reads it at compile time. The form
# matches what `rpm -q` prints, so a report can be traced back to a package.
export CLOUD_INIT_RS_DOWNSTREAM_VERSION="$rpm_version-$rpm_release"

# Reuses an already-installed cargo-generate-rpm, so a cached CARGO_HOME makes
# repeat runs cheap. This is the maintained cargo-to-rpm tool: it builds the
# package with the `rpm` crate and needs no spec file, which keeps the file list
# in Cargo.toml beside the .deb's rather than in a second place that can drift.
cargo install cargo-generate-rpm --locked

# Build and test explicitly rather than letting the packager drive the build:
# this is the only place the release binaries are produced, so they go through
# the same --locked gate as everything else, and the tests run on the
# architecture being shipped.
cargo build --release --locked \
    -p cloud-init -p cloud-id -p cloud-init-per \
    -p ds-identify -p cloud-init-generator
cargo test --release --locked --workspace

# cargo-generate-rpm does not strip, and an unstripped binary would ship the
# debug symbols of a release artifact.
for bin in cloud-init cloud-id cloud-init-per ds-identify cloud-init-generator; do
    strip -s "target/release/$bin"
done

# Written to a private directory rather than straight into $OUT: /out is the
# caller's, and build-deb.sh already learned once that globbing it picks up a
# stale package for another architecture and tests something this run did not
# build.
rpmout=$BUILD/rpmout
rm -rf "$rpmout"
mkdir -p "$rpmout" "$OUT"
cargo generate-rpm -p crates/cloud-init \
    -s "release = \"$rpm_release\"" \
    -o "$rpmout"

# Exactly one package, or the assumption behind "$rpm" below is wrong.
count=$(find "$rpmout" -name '*.rpm' | wc -l)
[ "$count" -eq 1 ] || {
    echo "expected one .rpm, found $count" >&2
    exit 1
}
rpm=$(find "$rpmout" -name '*.rpm')
cp "$rpm" "$OUT/"
rpm=$OUT/$(basename "$rpm")
ls -l "$OUT"

# --- acceptance test -------------------------------------------------------
#
# The same test build-deb.sh runs, against the same claim: installing this
# package cannot change how the machine boots. That is a property of the file
# list and the package metadata, so it is checked here rather than in a
# reviewer's head. A failure fails the release.

fail=0
check() {
    if [ "$2" = ok ]; then
        printf 'ok    %s\n' "$1"
    else
        printf 'FAIL  %s\n' "$1"
        fail=1
    fi
}

contents=$(rpm -qlp "$rpm")

# Any path outside these two prefixes is a path this package must not own.
stray=$(printf '%s\n' "$contents" |
    grep -vE '^/usr/(libexec/cloud-init-rs|share)(/|$)' || true)
if [ -n "$stray" ]; then
    printf 'unexpected paths:\n%s\n' "$stray"
    check "installs nothing outside /usr/libexec/cloud-init-rs and /usr/share" bad
else
    check "installs nothing outside /usr/libexec/cloud-init-rs and /usr/share" ok
fi

# The specific paths that would make the package boot-affecting. Named
# individually because "outside two prefixes" would not catch a future asset
# that is inside /usr/share but still read by systemd. /etc/apparmor.d is on the
# list even though neither RPM target ships AppArmor: the profile is shipped as
# source under /usr/share on every distribution, and the day one of them grows
# AppArmor support is not the day to discover the rule was dropped here.
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

# No relationship with the Python package, in either direction. Requires is
# excluded because find-requires legitimately resolves libc and libcrypto.
if rpm -qp --provides --conflicts --obsoletes "$rpm" | grep -q 'cloud-init[^-]'; then
    check "declares no Provides/Conflicts/Obsoletes against cloud-init" bad
else
    check "declares no Provides/Conflicts/Obsoletes against cloud-init" ok
fi
if rpm -qp --requires "$rpm" | grep -qE '^cloud-init([ ]|$)'; then
    check "does not depend on the cloud-init package" bad
else
    check "does not depend on the cloud-init package" ok
fi

# The dependency on the distribution's OpenSSL is the whole reason this is a
# native build. Losing it means the package stopped tracking security updates,
# which is invisible until a CVE.
if rpm -qp --requires "$rpm" | grep -q 'libcrypto\|libssl'; then
    check "depends on the distribution's OpenSSL" ok
else
    check "depends on the distribution's OpenSSL" bad
fi

# §6.7 requires any maintainer script to be idempotent and to never touch
# /var/lib/cloud. There are none; if one appears it has to be reviewed
# deliberately rather than arriving with a metadata change.
if [ -z "$(rpm -qp --scripts "$rpm")" ]; then
    check "ships no scriptlets" ok
else
    rpm -qp --scripts "$rpm"
    check "ships no scriptlets" bad
fi

# Install for real and exercise what was installed. Done last so that a failure
# above is reported before the system is modified. --nogpgcheck because this
# package was built a moment ago and has deliberately not been signed yet;
# signing is a release-pipeline concern, not a build one.
dnf install -y --nogpgcheck "$rpm"
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
    grep -qF "$CLOUD_INIT_RS_DOWNSTREAM_VERSION" "$libexec/cloud-init"; then
    check "the agent string carries the package version" ok
else
    check "the agent string carries the package version" bad
fi

# Prove the audit runs on a real install, and that the files shipped in
# /usr/share were generated by the very binary shipped next to them. A stale
# .fc is worse than no policy at all: it would relabel the shared state tree
# into something the running binary does not agree with.
"$libexec/cloud-init" devel verify-layout || true
"$libexec/cloud-init" devel verify-layout --dump-tmpfiles >"$BUILD/tmpfiles.expected"
if cmp -s "$BUILD/tmpfiles.expected" \
    /usr/share/cloud-init-rs/tmpfiles.d/cloud-init-rs.conf; then
    check "the shipped tmpfiles fragment matches the shipped binary" ok
else
    check "the shipped tmpfiles fragment matches the shipped binary" bad
fi

"$libexec/cloud-init" devel verify-layout --dump-selinux-fc >"$BUILD/fc.expected"
if cmp -s "$BUILD/fc.expected" /usr/share/cloud-init-rs/selinux/cloud-init-rs.fc; then
    check "the shipped SELinux file contexts match the shipped binary" ok
else
    check "the shipped SELinux file contexts match the shipped binary" bad
fi

# The staleness half of the AppArmor check carries over; the `apparmor_parser
# -QT` half does not, because neither Azure Linux nor Fedora ships AppArmor.
# That is a real gap against the .deb — the profile is only ever compiled by
# the Ubuntu build — and it is why build-deb.sh remains a required job rather
# than one of several equivalent ones.
apparmor=/usr/share/cloud-init-rs/apparmor/cloud-init-rs
"$libexec/cloud-init" devel verify-layout --dump-apparmor >"$BUILD/apparmor.expected"
if cmp -s "$BUILD/apparmor.expected" "$apparmor"; then
    check "the shipped AppArmor profile matches the shipped binary" ok
else
    check "the shipped AppArmor profile matches the shipped binary" bad
fi
if grep -q 'flags=(complain)' "$apparmor"; then
    check "the shipped AppArmor profile is in complain mode" ok
else
    check "the shipped AppArmor profile is in complain mode" bad
fi

# rpm deletes only the directories a package owns, and cargo-generate-rpm 0.21
# cannot express directory ownership at all: it has no %dir equivalent and skips
# directories outright when expanding assets. So this package owns ten files and
# no directories, and removal leaves the six it created behind, empty.
#
# That is the one place the .rpm is weaker than the .deb, where dpkg reaps the
# directories it made. It is cosmetic -- an empty unowned directory changes no
# boot -- but it is a Fedora packaging guideline violation and will need a real
# spec file to fix. Until then assert the part that actually matters, and keep
# asserting it: no file survives removal.
rpm -e cloud-init-rs
leftover=$(find "$libexec" /usr/share/cloud-init-rs /usr/share/doc/cloud-init-rs \
    ! -type d 2>/dev/null || true)
if [ -n "$leftover" ]; then
    printf 'files left after removal:\n%s\n' "$leftover"
    check "removal leaves no file behind" bad
else
    check "removal leaves no file behind" ok
fi

[ "$fail" -eq 0 ] || {
    echo "package acceptance test failed"
    exit 1
}
echo "package acceptance test passed"
