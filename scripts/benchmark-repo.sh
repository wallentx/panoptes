#!/usr/bin/env sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp_base=${TMPDIR:-/tmp}
corpus=pocketbase
source_repo=
output=
build_runs=${PANOPTES_BENCH_BUILD_RUNS:-3}
query_runs=${PANOPTES_BENCH_QUERY_RUNS:-5}
panoptes_bin=${PANOPTES_BENCH_BIN:-"$repo_dir/target/release/panoptes"}
name=
repo_url=
revision=
query=
grep_pattern=
caller_symbol=
mutate_path=

usage() {
    printf '%s\n' \
        "usage: $0 [--corpus NAME] [--source PATH] [--output DIR]" \
        "          [--binary PATH] [--build-runs N] [--query-runs N]"
}

positive_integer() {
    case $2 in
        ''|*[!0-9]*|0) printf '%s must be a positive integer\n' "$1" >&2; exit 2 ;;
    esac
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --corpus) corpus=$2; shift 2 ;;
        --source) source_repo=$2; shift 2 ;;
        --output) output=$2; shift 2 ;;
        --binary) panoptes_bin=$2; shift 2 ;;
        --build-runs) build_runs=$2; shift 2 ;;
        --query-runs) query_runs=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

positive_integer --build-runs "$build_runs"
positive_integer --query-runs "$query_runs"

corpus_config="$repo_dir/bench/corpora/$corpus.conf"
if [ ! -f "$corpus_config" ]; then
    printf 'unknown corpus: %s\n' "$corpus" >&2
    exit 2
fi
# The corpus files are repository-controlled data, not user input.
# shellcheck disable=SC1090
. "$corpus_config"

if [ -z "$output" ]; then
    output="$tmp_base/panoptes-$name-benchmark-$(date -u +%Y%m%dT%H%M%SZ)-$$"
fi
scratch=$(mktemp -d "$tmp_base/panoptes-real-repo-benchmark.XXXXXX")
measure_bin="$scratch/rusage"
worktree="$scratch/$name"
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir -p "$output"

cc=${CC:-cc}
"$cc" -O2 -Wall -Wextra -Werror "$repo_dir/bench/rusage.c" -o "$measure_bin"

if [ ! -x "$panoptes_bin" ]; then
    cargo build --manifest-path "$repo_dir/Cargo.toml" --release --locked --package panoptes
fi
panoptes_bin=$(CDPATH='' cd -- "$(dirname -- "$panoptes_bin")" && pwd)/$(basename -- "$panoptes_bin")

if [ -n "$source_repo" ]; then
    source_repo=$(CDPATH='' cd -- "$source_repo" && pwd)
    git clone --quiet --no-hardlinks "$source_repo" "$worktree"
else
    git clone --quiet --no-checkout "$repo_url" "$worktree"
fi
git -C "$worktree" checkout --quiet --detach "$revision"

actual_revision=$(git -C "$worktree" rev-parse HEAD)
if [ "$actual_revision" != "$revision" ]; then
    printf 'expected revision %s, got %s\n' "$revision" "$actual_revision" >&2
    exit 1
fi

store="$scratch/$name.db"
results="$output/results.tsv"
printf 'workload\titeration\twall_ms\tpeak_rss_kb\tdb_bytes\texit_code\n' > "$results"

measure() {
    workload=$1
    iteration=$2
    measured_store=$3
    shift 3
    metrics="$scratch/metrics-$workload-$iteration.tsv"
    stdout="$output/$workload-$iteration.stdout"
    stderr="$output/$workload-$iteration.stderr"
    set +e
    "$measure_bin" "$metrics" "$@" > "$stdout" 2> "$stderr"
    wrapper_status=$?
    set -e
    if [ ! -s "$metrics" ]; then
        printf '%s failed before metrics were recorded\n' "$workload" >&2
        sed -n '1,80p' "$stderr" >&2
        exit "$wrapper_status"
    fi
    tab=$(printf '\t')
    IFS="$tab" read -r wall_ms peak_rss exit_code < "$metrics"
    db_bytes=0
    if [ -f "$measured_store" ]; then
        db_bytes=$(wc -c < "$measured_store" | tr -d ' ')
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$workload" "$iteration" "$wall_ms" "$peak_rss" "$db_bytes" "$exit_code" >> "$results"
    if [ "$exit_code" -ne 0 ]; then
        printf '%s exited %s\n' "$workload" "$exit_code" >&2
        sed -n '1,80p' "$stderr" >&2
        exit "$exit_code"
    fi
}

i=1
while [ "$i" -le "$build_runs" ]; do
    cold_store=$store
    if [ "$i" -gt 1 ]; then
        cold_store="$scratch/$name-cold-$i.db"
    fi
    measure cold-build "$i" "$cold_store" "$panoptes_bin" --store "$cold_store" build "$worktree"
    i=$((i + 1))
done

i=1
while [ "$i" -le "$build_runs" ]; do
    measure warm-build "$i" "$store" "$panoptes_bin" --store "$store" build "$worktree"
    i=$((i + 1))
done

i=1
while [ "$i" -le "$build_runs" ]; do
    printf '\n// Panoptes benchmark change %s.\n' "$i" >> "$worktree/$mutate_path"
    measure one-file-build "$i" "$store" "$panoptes_bin" --store "$store" build "$worktree"
    i=$((i + 1))
done

i=1
while [ "$i" -le "$query_runs" ]; do
    measure ask "$i" "$store" "$panoptes_bin" --store "$store" ask "$query" "$worktree" --limit 20 --json
    measure grep "$i" "$store" "$panoptes_bin" --store "$store" grep "$grep_pattern" --fixed --path "$worktree" --json
    measure callers "$i" "$store" "$panoptes_bin" --store "$store" callers "$caller_symbol" --path "$worktree" --depth 2 --json
    measure map "$i" "$store" "$panoptes_bin" --store "$store" map "$worktree" --json
    i=$((i + 1))
done

"$panoptes_bin" --store "$store" status "$worktree" --json > "$output/status.json"

median_column() {
    workload=$1
    column=$2
    values="$scratch/median-$workload-$column"
    awk -F '\t' -v workload="$workload" -v column="$column" \
        'NR > 1 && $1 == workload { print $column }' "$results" | sort -n > "$values"
    count=$(wc -l < "$values" | tr -d ' ')
    awk -v count="$count" '
        NR == int((count + 1) / 2) { lower = $1 }
        NR == int((count + 2) / 2) { upper = $1 }
        END {
            if (count % 2 == 1) print upper
            else printf "%.1f\n", (lower + upper) / 2
        }
    ' "$values"
}

summary="$output/summary.tsv"
printf 'workload\tsamples\tmedian_wall_ms\tmedian_peak_rss_kb\tdb_bytes\n' > "$summary"
awk -F '\t' 'NR > 1 { print $1 }' "$results" | sort -u | while IFS= read -r workload; do
    samples=$(awk -F '\t' -v workload="$workload" 'NR > 1 && $1 == workload { count++ } END { print count + 0 }' "$results")
    wall=$(median_column "$workload" 3)
    rss=$(median_column "$workload" 4)
    db_bytes=$(awk -F '\t' -v workload="$workload" 'NR > 1 && $1 == workload { value = $5 } END { print value + 0 }' "$results")
    printf '%s\t%s\t%s\t%s\t%s\n' "$workload" "$samples" "$wall" "$rss" "$db_bytes" >> "$summary"
done

{
    printf 'key\tvalue\n'
    printf 'timestamp_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'corpus\t%s\n' "$name"
    printf 'repo_url\t%s\n' "$repo_url"
    printf 'revision\t%s\n' "$revision"
    printf 'indexed_source_files\t%s\n' "$(find "$worktree" -type f \( -name '*.go' -o -name '*.js' -o -name '*.jsx' -o -name '*.ts' -o -name '*.tsx' -o -name '*.py' -o -name '*.rs' \) | wc -l | tr -d ' ')"
    printf 'repository_bytes\t%s\n' "$(du -sb "$worktree" | awk '{print $1}')"
    printf 'uname\t%s\n' "$(uname -a | tr '\t' ' ')"
    printf 'rustc\t%s\n' "$(rustc --version)"
    printf 'panoptes\t%s\n' "$("$panoptes_bin" --version)"
    printf 'panoptes_git_sha\t%s\n' "$(git -C "$repo_dir" rev-parse HEAD 2>/dev/null || printf uncommitted)"
    printf 'panoptes_git_dirty\t%s\n' "$(test -n "$(git -C "$repo_dir" status --porcelain)" && printf true || printf false)"
    printf 'build_runs\t%s\n' "$build_runs"
    printf 'query_runs\t%s\n' "$query_runs"
} > "$output/metadata.tsv"

printf 'benchmark results: %s\n' "$output"
printf 'summary: %s\n' "$summary"
