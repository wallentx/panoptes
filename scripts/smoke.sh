#!/usr/bin/env bash
set -euo pipefail

binary=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
expected_version=${2:-}
scratch=$(mktemp -d "${TMPDIR:-/tmp}/panoptes-smoke.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
repo="$scratch/fixture"
smoke_home="$scratch/home"
store="$scratch/panoptes.db"
mkdir -p "$repo/src" "$smoke_home"
git -C "$repo" init --quiet
cd "$repo"

run() {
  env HOME="$smoke_home" XDG_CONFIG_HOME="$smoke_home/.config" \
    XDG_DATA_HOME="$smoke_home/.local/share" CODEX_HOME="$smoke_home/.codex" \
    "$binary" --store "$store" "$@"
}

printf '%s\n' \
  'export function greet(name: string) { return "hello " + name; }' \
  'export function main() { return greet("world"); }' > src/a.ts
version=$(run version --json | jq -er .version)
test "$(run --version)" = "panoptes $version"
if [ -n "$expected_version" ]; then
  test "$version" = "$expected_version"
fi
for command in build ask grep callers skeleton map mcp init check export completions cache version upgrade viz status; do
  run "$command" --help > /dev/null
done
run build "$repo"
run grep greet --path "$repo" --json | jq -e 'length == 1 and .[0].total_hits == 2 and .[0].unreadable == 0' > /dev/null
run ask greet "$repo" --json > /dev/null
run callers greet --path "$repo" --json > /dev/null
run skeleton a.ts --path "$repo" --json > /dev/null
for command in map check status; do
  run "$command" "$repo" --json > /dev/null
done
run export "$scratch/export.json" --path "$repo" --json
run viz "$repo" --output "$scratch/map.html"
test -s "$scratch/export.json"
test -s "$scratch/map.html"
for shell in bash zsh fish; do
  run completions "$shell" > "$scratch/$shell"
  test -s "$scratch/$shell"
done

config="$smoke_home/.codex/config.toml"
run init --provider codex --dry-run --json > /dev/null
test ! -e "$config"
run init --provider codex
grep -q '\[mcp_servers.panoptes\]' "$config"
run init --provider codex --deregister
if grep -q '\[mcp_servers.panoptes\]' "$config"; then
  echo 'Codex deregistration left the server registered' >&2
  exit 1
fi
run init --provider cursor --provider opencode --dry-run > /dev/null

mcp() {
  {
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
      '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    jq -cn --arg method "$1" --argjson params "$2" \
      '{jsonrpc:"2.0",id:2,method:$method,params:$params}'
  } | run mcp "$repo" > "$scratch/mcp.jsonl"
  jq -es --arg version "$version" \
    'any(.[]; .id == 1 and .result.serverInfo.version == $version)' "$scratch/mcp.jsonl" > /dev/null
  jq -es 'any(.[]; .id == 2 and has("result") and (.result.isError != true))' \
    "$scratch/mcp.jsonl" > /dev/null
  jq -c 'select(.id == 2) | .result' "$scratch/mcp.jsonl"
}

mcp tools/list '{}' | jq -e \
  '[.tools[].name] as $names | ["find","grep","callers","skeleton","map"] - $names == []' > /dev/null
run cache clear --yes
mcp tools/call '{"name":"status","arguments":{}}' | jq -e \
  '.. | objects | select(.indexed? == true)' > /dev/null
printf '%s\n' 'export function added() { return greet("again"); }' >> src/a.ts
mcp tools/call '{"name":"find","arguments":{"query":"added"}}' | jq -e \
  '.. | objects | select(.name? == "added")' > /dev/null
run cache doctor
printf 'CLI/MCP smoke passed: version %s, indexing, retrieval, refresh, and provider setup\n' "$version"
