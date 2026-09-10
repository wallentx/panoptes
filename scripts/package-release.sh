#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo_dir"
output_dir=${1:-"$repo_dir/release"}
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)
metadata=$(cargo metadata --no-deps --format-version 1 --locked)
version=$(jq -er '.packages[] | select(.name == "panoptes") | .version' <<< "$metadata")
target_dir=$(jq -er .target_directory <<< "$metadata")
tag=${RELEASE_TAG:-v$version}
if [ "${tag#v}" != "$version" ]; then
  echo "Release tag $tag does not match Cargo.toml version $version" >&2
  exit 1
fi
host=$(rustc -vV | sed -n 's/^host: //p')
target=${RELEASE_TARGET:-$host}
if [ "$target" != "$host" ]; then
  echo "Native packaging requires $host; requested $target" >&2
  exit 1
fi
git_sha=$(git rev-parse HEAD)
PANOPTES_GIT_SHA="$git_sha" cargo build --release --locked --package panoptes --target "$target"
binary="$target_dir/$target/release/panoptes"
"$binary" version --json | jq -e --arg version "$version" --arg commit "$git_sha" \
  '.version == $version and .build == $commit' > /dev/null

scratch=$(mktemp -d "${TMPDIR:-/tmp}/panoptes-release.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
name="panoptes-$version-$target"
stage="$scratch/$name"
mkdir -p "$stage/completions" "$scratch/unpacked"
cp "$binary" "$repo_dir/README.md" "$repo_dir/LICENSE" "$stage/"
jq -n --arg version "$version" --arg tag "$tag" --arg commit "$git_sha" --arg target "$target" \
  '{version:$version,tag:$tag,commit:$commit,target:$target}' > "$stage/release.json"
"$binary" completions bash > "$stage/completions/panoptes.bash"
"$binary" completions zsh > "$stage/completions/_panoptes"
"$binary" completions fish > "$stage/completions/panoptes.fish"
archive="$output_dir/$name.tar.gz"
tar -C "$scratch" -czf "$archive" "$name"
tar -C "$scratch/unpacked" -xzf "$archive"
bash "$repo_dir/scripts/smoke.sh" "$scratch/unpacked/$name/panoptes" "$version"
(
  cd "$output_dir"
  if command -v sha256sum > /dev/null; then
    sha256sum "$name.tar.gz" > "$name.tar.gz.sha256"
  else
    shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256"
  fi
)
printf '%s\n' "$archive" "$archive.sha256"
