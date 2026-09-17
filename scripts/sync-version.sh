#!/usr/bin/env sh
# Cargo needs a concrete manifest version before it starts compiling.
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
version=
while [ "$#" -gt 0 ]; do
    case $1 in
        --repo) repo_dir=${2:?--repo requires a directory}; shift 2 ;;
        --version) version=${2:?--version requires a version}; shift 2 ;;
        *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

repo_dir=$(CDPATH='' cd -- "$repo_dir" && pwd -P)

# Numeric prerelease identifiers cannot have leading zeroes.
number='(0|[1-9][0-9]*)'
identifier="($number|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
semver="$number\\.$number\\.$number(-$identifier(\\.$identifier)*)?(\\+[0-9A-Za-z-]+(\\.[0-9A-Za-z-]+)*)?"
valid_version() { printf '%s\n' "$1" | grep -Eq "^$semver$"; }

if [ -n "$version" ]; then
    version=${version#v}
    valid_version "$version" || { printf 'Invalid SemVer: %s\n' "$version" >&2; exit 1; }
elif command -v git >/dev/null 2>&1 && [ "$(git -C "$repo_dir" rev-parse --show-toplevel 2>/dev/null || true)" = "$repo_dir" ] && git -C "$repo_dir" rev-parse --verify HEAD >/dev/null 2>&1; then
    # Restrict describe to valid, reachable release tags. A newer tag on another
    # branch must not change the version of this source revision.
    tags=$(git -C "$repo_dir" tag --merged HEAD --list 'v[0-9]*')
    set --
    for tag in $tags; do
        if valid_version "${tag#v}"; then
            set -- "$@" --match "$tag"
        fi
    done
    if [ "$#" -gt 0 ]; then
        tag=$(git -C "$repo_dir" describe --tags --abbrev=0 "$@" HEAD)
        version=${tag#v}
        if [ "$(git -C "$repo_dir" rev-parse HEAD)" != "$(git -C "$repo_dir" rev-parse "$tag^{commit}")" ]; then
            core=${version%%+*}
            core=${core%%-*}
            major=${core%%.*}
            rest=${core#*.}
            minor=${rest%%.*}
            patch=${rest#*.}
            hash=$(git -C "$repo_dir" rev-parse --short=8 HEAD)
            version="$major.$minor.$((patch + 1))-g$hash"
        fi
    fi
fi

# Gitless archives and repositories without release tags retain Cargo's version.
if [ -z "$version" ]; then
    exit 0
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/panoptes-version.XXXXXX")
trap 'rm -rf "$scratch"' 0
trap 'exit 1' HUP INT TERM

# These manifests are repository-owned. Fail closed if their expected package
# entries are absent or duplicated, and preserve all dependency versions.
awk -v version="$version" '
    /^\[/ { package = ($0 ~ /^\[package\][[:space:]]*$/) }
    package && /^version[[:space:]]*=/ { $0 = "version = \"" version "\""; changed++ }
    { print }
    END { if (changed != 1) exit 1 }
' "$repo_dir/Cargo.toml" > "$scratch/Cargo.toml"
awk -v version="$version" '
    /^\[/ { package = 0 }
    /^name = "panoptes"$/ { package = 1 }
    package && /^version[[:space:]]*=/ { $0 = "version = \"" version "\""; changed++ }
    { print }
    END { if (changed != 1) exit 1 }
' "$repo_dir/Cargo.lock" > "$scratch/Cargo.lock"

for manifest in Cargo.toml Cargo.lock; do
    if ! cmp -s "$repo_dir/$manifest" "$scratch/$manifest"; then
        cat "$scratch/$manifest" > "$repo_dir/$manifest"
        printf 'Set %s package version to %s\n' "$manifest" "$version" >&2
    fi
done
