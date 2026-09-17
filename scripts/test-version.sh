#!/usr/bin/env bash
# Exercise ref selection and manifest edits without compiling or using the network.
set -euo pipefail
sync_script=$(cd "$(dirname "$0")" && pwd)/sync-version.sh
scratch=$(mktemp -d "${TMPDIR:-/tmp}/panoptes-version-test.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
repo="$scratch/repo"
mkdir -p "$repo"
cat > "$repo/Cargo.toml" <<'TOML'
[package]
name = "panoptes"
version = "0.1.0"
[dependencies]
example = { version = "0.1.0" }
TOML
cat > "$repo/Cargo.lock" <<'LOCK'
version = 4
[[package]]
name = "example"
version = "0.1.0"
[[package]]
name = "panoptes"
version = "0.1.0"
LOCK
assert_version() {
    local expected=$1
    test "$(sed -n '3p' "$repo/Cargo.toml")" = "version = \"$expected\""
    test "$(tail -1 "$repo/Cargo.lock")" = "version = \"$expected\""
    test "$(sed -n '4p' "$repo/Cargo.lock")" = 'version = "0.1.0"'
    test "$(tail -1 "$repo/Cargo.toml")" = 'example = { version = "0.1.0" }'
}
sync_version() { sh "$sync_script" --repo "$repo" "$@"; }

# No Git metadata, and no release tags, both preserve the stored version.
sync_version
assert_version 0.1.0
git -C "$repo" init -q
git -C "$repo" config user.email version-test@example.invalid
git -C "$repo" config user.name version-test
git -C "$repo" config commit.gpgsign false
git -C "$repo" config tag.gpgsign false
git -C "$repo" add .
git -C "$repo" commit -qm initial
sync_version
assert_version 0.1.0

# Both lightweight and annotated tags are supported.
git -C "$repo" tag v1.2.3
sync_version
assert_version 1.2.3
git -C "$repo" commit --allow-empty -qm next
hash=$(git -C "$repo" rev-parse --short=8 HEAD)
sync_version
assert_version "1.2.4-g$hash"
git -C "$repo" tag -a v1.3.0-rc.1+build.7 -m prerelease
sync_version
assert_version 1.3.0-rc.1+build.7

# A tag on another branch and malformed tags must not affect this ref.
git -C "$repo" checkout -qb unrelated
git -C "$repo" commit --allow-empty -qm unrelated
git -C "$repo" tag v99.0.0
git -C "$repo" checkout -q --detach HEAD^
git -C "$repo" commit --allow-empty -qm development
git -C "$repo" tag v99.01.0
git -C "$repo" tag v99.1.0-01
hash=$(git -C "$repo" rev-parse --short=8 HEAD)
sync_version
assert_version "1.3.1-g$hash"

# Release workflows select the explicit tag before that tag exists.
sync_version --version v2.0.0
assert_version 2.0.0
cp "$repo/Cargo.toml" "$scratch/before.toml"
cp "$repo/Cargo.lock" "$scratch/before.lock"
if sync_version --version '2.0.0-01'; then
    echo 'Invalid SemVer was accepted' >&2; exit 1
fi
cmp "$repo/Cargo.toml" "$scratch/before.toml"
cmp "$repo/Cargo.lock" "$scratch/before.lock"

# Gitless archives nested inside a checkout must not inherit its tag.
mkdir "$repo/archive"
cp "$repo/Cargo.toml" "$repo/Cargo.lock" "$repo/archive/"
sh "$sync_script" --repo "$repo/archive"
cmp "$repo/archive/Cargo.toml" "$repo/Cargo.toml"

# A malformed lockfile must fail before touching the manifest.
printf 'version = 4\n' > "$repo/Cargo.lock"
if sync_version --version 3.0.0; then
    echo 'Missing lockfile package was accepted' >&2; exit 1
fi
cmp "$repo/Cargo.toml" "$scratch/before.toml"
printf '%s\n' 'Git-derived version checks passed'
