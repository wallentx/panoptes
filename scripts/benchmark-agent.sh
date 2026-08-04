#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
tmp_base=${TMPDIR:-/tmp}
source_repo=
output=
tasks="$repo_dir/bench/agent-tasks/pocketbase.tsv"
selected_task=
model=${PANOPTES_AGENT_MODEL:-gpt-5.6-terra}
reasoning=${PANOPTES_AGENT_REASONING:-medium}
trials=${PANOPTES_AGENT_TRIALS:-1}
panoptes_bin=${PANOPTES_BENCH_BIN:-"$repo_dir/target/release/panoptes"}

usage() {
    printf '%s\n' \
        "usage: $0 --source PATH [--output DIR] [--tasks FILE] [--task ID]" \
        "          [--model MODEL] [--reasoning LEVEL] [--trials N] [--binary PATH]"
}

positive_integer() {
    [[ $2 =~ ^[1-9][0-9]*$ ]] || { printf '%s must be a positive integer\n' "$1" >&2; exit 2; }
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --source) source_repo=$2; shift 2 ;;
        --output) output=$2; shift 2 ;;
        --tasks) tasks=$2; shift 2 ;;
        --task) selected_task=$2; shift 2 ;;
        --model) model=$2; shift 2 ;;
        --reasoning) reasoning=$2; shift 2 ;;
        --trials) trials=$2; shift 2 ;;
        --binary) panoptes_bin=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

[[ -n $source_repo ]] || { usage >&2; exit 2; }
positive_integer --trials "$trials"
command -v codex >/dev/null || { printf 'codex is required\n' >&2; exit 1; }
command -v jq >/dev/null || { printf 'jq is required\n' >&2; exit 1; }
[[ -x $panoptes_bin ]] || { printf 'Panoptes binary not found: %s\n' "$panoptes_bin" >&2; exit 1; }
[[ -f $tasks ]] || { printf 'task file not found: %s\n' "$tasks" >&2; exit 1; }

source_repo=$(CDPATH='' cd -- "$source_repo" && pwd)
panoptes_bin=$(CDPATH='' cd -- "$(dirname -- "$panoptes_bin")" && pwd)/$(basename -- "$panoptes_bin")
if [[ -z $output ]]; then
    output="$tmp_base/panoptes-agent-benchmark-$(date -u +%Y%m%dT%H%M%SZ)-$$"
fi
scratch=$(mktemp -d "$tmp_base/panoptes-agent-benchmark.XXXXXX")
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
mkdir -p "$output/runs"
benchmark_failed=0

results="$output/results.tsv"
printf 'task\tarm\ttrial\texit_code\twall_ms\tinput_tokens\tcached_input_tokens\tuncached_input_tokens\toutput_tokens\treasoning_output_tokens\ttool_calls\tpanoptes_calls\tmcp_failures\trubric_hits\trubric_total\n' > "$results"

run_one() {
    local task=$1
    local prompt=$2
    local required_terms=$3
    local arm=$4
    local trial=$5
    local run_id="${task}-${arm}-${trial}"
    local worktree="$scratch/$run_id"
    local events="$output/runs/$run_id.jsonl"
    local answer="$output/runs/$run_id.md"
    local stderr="$output/runs/$run_id.stderr"
    local store="$scratch/$run_id.db"
    local started finished wall_ms exit_code

    git clone --quiet --no-hardlinks "$source_repo" "$worktree"
    git -C "$worktree" checkout --quiet --detach "$(git -C "$source_repo" rev-parse HEAD)"

    {
        printf '\n<!-- panoptes-benchmark:start -->\n'
        printf '## Source navigation\n\n'
        printf 'If Panoptes MCP tools are available, use them before shell search or whole-file reads. '
        printf 'Use find for ranked context, grep for exhaustive matches, callers for dependency paths, skeleton for a file API, and map for orientation. '
        printf 'For this scoped task, start with find or grep instead of map, use at most three Panoptes calls, and do not repeat find for individual files. '
        printf 'If they are unavailable, use the built-in read-only tools.\n'
        printf '<!-- panoptes-benchmark:end -->\n'
    } >> "$worktree/AGENTS.md"

    local -a config=(
        -c "model_reasoning_effort=\"$reasoning\""
        -c 'features.multi_agent=false'
    )
    if [[ $arm == panoptes ]]; then
        config+=(
            -c "mcp_servers.panoptes.command=\"$panoptes_bin\""
            -c "mcp_servers.panoptes.args=[\"--store\",\"$store\",\"mcp\",\"$worktree\"]"
            -c 'mcp_servers.panoptes.startup_timeout_sec=30'
        )
    fi

    started=$(date +%s%N)
    set +e
    codex exec \
        --ephemeral \
        --ignore-user-config \
        --sandbox read-only \
        --json \
        --model "$model" \
        --cd "$worktree" \
        "${config[@]}" \
        "$prompt" > "$events" 2> "$stderr"
    exit_code=$?
    set -e
    finished=$(date +%s%N)
    wall_ms=$(( (finished - started) / 1000000 ))
    if (( exit_code != 0 )); then
        benchmark_failed=1
    fi

    jq -rs '[.[] | select(.type == "item.completed" and .item.type == "agent_message") | .item.text] | last // ""' \
        "$events" > "$answer"

    local input_tokens cached_tokens uncached_tokens output_tokens reasoning_tokens tool_calls panoptes_calls mcp_failures
    input_tokens=$(jq -rs '[.[] | select(.type == "turn.completed") | .usage.input_tokens] | add // 0' "$events")
    cached_tokens=$(jq -rs '[.[] | select(.type == "turn.completed") | .usage.cached_input_tokens] | add // 0' "$events")
    uncached_tokens=$((input_tokens - cached_tokens))
    output_tokens=$(jq -rs '[.[] | select(.type == "turn.completed") | .usage.output_tokens] | add // 0' "$events")
    reasoning_tokens=$(jq -rs '[.[] | select(.type == "turn.completed") | .usage.reasoning_output_tokens] | add // 0' "$events")
    tool_calls=$(jq -rs '[.[] | select(.type == "item.completed") | .item | select(.type != "agent_message" and .type != "reasoning")] | length' "$events")
    panoptes_calls=$(jq -rs '[.[] | select(.type == "item.completed") | .item | select((.server // "") == "panoptes" and .status == "completed")] | length' "$events")
    mcp_failures=$(jq -rs '[.[] | select(.type == "item.completed") | .item | select(.type == "mcp_tool_call" and .status == "failed")] | length' "$events")

    local rubric_hits=0 rubric_total=0 term
    IFS='|' read -r -a rubric <<< "$required_terms"
    for term in "${rubric[@]}"; do
        ((rubric_total += 1))
        if rg -Fqi -- "$term" "$answer"; then
            ((rubric_hits += 1))
        fi
    done

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$task" "$arm" "$trial" "$exit_code" "$wall_ms" "$input_tokens" "$cached_tokens" \
        "$uncached_tokens" "$output_tokens" "$reasoning_tokens" "$tool_calls" "$panoptes_calls" \
        "$mcp_failures" "$rubric_hits" "$rubric_total" >> "$results"
}

for ((trial = 1; trial <= trials; trial++)); do
    arms=(baseline panoptes)
    if (( trial % 2 == 0 )); then
        arms=(panoptes baseline)
    fi
    while IFS=$'\t' read -r task prompt required_terms; do
        [[ $task == task ]] && continue
        [[ -n $task ]] || continue
        [[ -z $selected_task || $task == "$selected_task" ]] || continue
        for arm in "${arms[@]}"; do
            printf 'running %s trial %s (%s)\n' "$task" "$trial" "$arm"
            run_one "$task" "$prompt" "$required_terms" "$arm" "$trial"
        done
    done < "$tasks"
done

if [[ $(wc -l < "$results") -eq 1 ]]; then
    printf 'no tasks matched: %s\n' "${selected_task:-task file was empty}" >&2
    exit 2
fi

summary="$output/summary.tsv"
awk -F '\t' '
    BEGIN { OFS="\t"; print "arm", "runs", "successful", "input_tokens", "uncached_input_tokens", "output_tokens", "tool_calls", "wall_ms", "rubric_hits", "rubric_total" }
    NR > 1 {
        arm=$2; runs[arm]++
        if ($4 == 0) successful[arm]++
        input[arm]+=$6; uncached[arm]+=$8; output[arm]+=$9; tools[arm]+=$11; wall[arm]+=$5
        hits[arm]+=$14; total[arm]+=$15
    }
    END {
        for (arm in runs) print arm, runs[arm], successful[arm]+0, input[arm]+0, uncached[arm]+0, output[arm]+0, tools[arm]+0, wall[arm]+0, hits[arm]+0, total[arm]+0
    }
' "$results" | { IFS= read -r header; printf '%s\n' "$header"; sort; } > "$summary"

{
    printf 'key\tvalue\n'
    printf 'timestamp_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'source_revision\t%s\n' "$(git -C "$source_repo" rev-parse HEAD)"
    printf 'model\t%s\n' "$model"
    printf 'reasoning\t%s\n' "$reasoning"
    printf 'trials\t%s\n' "$trials"
    printf 'tasks\t%s\n' "$tasks"
    printf 'codex\t%s\n' "$(codex --version 2>/dev/null)"
    printf 'panoptes\t%s\n' "$("$panoptes_bin" --version)"
    printf 'uname\t%s\n' "$(uname -a | tr '\t' ' ')"
} > "$output/metadata.tsv"

printf 'agent benchmark results: %s\n' "$output"
printf 'summary: %s\n' "$summary"
if (( benchmark_failed != 0 )); then
    printf 'one or more agent runs failed\n' >&2
    exit 1
fi
