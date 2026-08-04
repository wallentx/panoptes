#!/data/data/com.termux/files/usr/bin/sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
output_dir=${1:-"$repo_dir/release"}
target=$(rustc -vV | sed -n 's/^host: //p')
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_dir/Cargo.toml" | head -n 1)
git_sha=$(git -C "$repo_dir" rev-parse --verify HEAD)

mkdir -p "$output_dir"
PANOPTES_GIT_SHA=$git_sha cargo build --manifest-path "$repo_dir/Cargo.toml" --release --locked --package panoptes

stage=${TMPDIR:-/tmp}/panoptes-release-$$
trap 'rm -rf "$stage"' EXIT HUP INT TERM
mkdir -p "$stage/panoptes-$version-$target"
cp "$repo_dir/target/release/panoptes" "$stage/panoptes-$version-$target/panoptes"
cp "$repo_dir/README.md" "$repo_dir/LICENSE" "$stage/panoptes-$version-$target/"

archive="$output_dir/panoptes-$version-$target.tar.gz"
tar -C "$stage" -czf "$archive" "panoptes-$version-$target"
sha256sum "$archive" > "$archive.sha256"
printf '%s\n' "$archive"
printf '%s\n' "$archive.sha256"
