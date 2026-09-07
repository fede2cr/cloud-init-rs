#!/bin/sh
# Stamp a release version into the workspace.
#
# The `v*` tag is the only source of truth for the version; Cargo.toml carries
# the placeholder 0.0.0 at every other time. Both the tarball build and the deb
# build call this, so there is exactly one definition of "how a version gets
# into the tree".
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 VERSION (without the leading v)" >&2
    exit 2
fi
version=$1

if ! printf '%s' "$version" |
    grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
    echo "$0: '$version' is not MAJOR.MINOR.PATCH[-PRERELEASE]" >&2
    exit 1
fi

# Every crate uses version.workspace = true, so this one line is the whole
# workspace. Fail loudly if the placeholder ever moves: a silent no-op here
# would ship a release labelled 0.0.0.
sed -i "0,/^version = \"0\.0\.0\"\$/s//version = \"$version\"/" Cargo.toml
if ! grep -qx "version = \"$version\"" Cargo.toml; then
    echo "$0: placeholder version = \"0.0.0\" not found in Cargo.toml" >&2
    exit 1
fi

# The lock records the members at 0.0.0 as well, and a --locked build rejects a
# lock that no longer matches. --workspace rewrites only those entries and
# leaves every registry dependency pinned where it was.
cargo update --workspace --quiet
