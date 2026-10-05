#!/usr/bin/env sh
# Shared deterministic corpus for the CLI and MCP benchmark runners.
set -eu
[ "$#" -eq 2 ] || { echo "usage: $0 DESTINATION FILES" >&2; exit 2; }
case $2 in ''|*[!0-9]*|0) echo 'FILES must be a positive integer' >&2; exit 2 ;; esac
[ ! -e "$1" ] || { echo 'fixture destination already exists' >&2; exit 2; }

generate_repo() {
    destination=$1
    count=$2
    mkdir -p "$destination/src"
    git -C "$destination" init -q
    i=0
    while [ "$i" -lt "$count" ]; do
        current=$(printf '%05d' "$i")
        path="$destination/src/module_$current.ts"
        if [ "$i" -gt 0 ]; then
            previous=$(printf '%05d' "$((i - 1))")
            printf 'import { process_payment_%s_0 } from "./module_%s";\n' "$previous" "$previous" > "$path"
            printf 'export function process_payment_%s_0(input: number) { return process_payment_%s_0(input) + 1; }\n' "$current" "$previous" >> "$path"
        else
            printf 'export function process_payment_%s_0(input: number) { return input + 1; }\n' "$current" > "$path"
        fi
        printf 'export function validate_gateway_%s(input: number) { return process_payment_%s_0(input) > 0; }\n' "$current" "$current" >> "$path"
        printf 'export class PaymentService%s { process(input: number) { return validate_gateway_%s(input); } }\n' "$current" "$current" >> "$path"
        i=$((i + 1))
    done
}

generate_repo "$1" "$2"
