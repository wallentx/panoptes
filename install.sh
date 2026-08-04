#!/usr/bin/env sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
install_root=${PANOPTES_INSTALL_ROOT:-"${HOME:?HOME is not set}/.local"}

usage() {
    printf '%s\n' \
        'Install Panoptes from this inspected checkout.' \
        '' \
        "usage: $0 [--prefix DIR]" \
        '' \
        'options:' \
        "  --prefix DIR  installation root (default: \$HOME/.local)" \
        '  -h, --help    show this help' \
        '' \
        'The script does not fetch Panoptes or modify provider configuration.' \
        'Cargo may fetch the dependency versions pinned in Cargo.lock.'
}

while [ "$#" -gt 0 ]; do
    case $1 in
        --prefix)
            if [ "$#" -lt 2 ]; then
                printf '%s\n' 'error: --prefix requires a directory' >&2
                exit 2
            fi
            install_root=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ ! -f "$repo_dir/Cargo.lock" ] || [ ! -f "$repo_dir/Cargo.toml" ]; then
    printf 'error: %s is not a complete Panoptes checkout\n' "$repo_dir" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
    printf '%s\n' 'error: Rust and Cargo are required.' >&2
    if command -v pkg >/dev/null 2>&1; then
        printf '%s\n' 'On Termux: pkg install git rust clang' >&2
    else
        printf '%s\n' 'Install Rust with your operating system package manager, then rerun this script.' >&2
    fi
    exit 1
fi

mkdir -p "$install_root"
install_root=$(CDPATH='' cd -- "$install_root" && pwd)
git_sha=source-build
if command -v git >/dev/null 2>&1; then
    detected_sha=$(git -C "$repo_dir" rev-parse --verify HEAD 2>/dev/null || true)
    if [ -n "$detected_sha" ]; then
        git_sha=$detected_sha
    fi
fi

printf 'Installing Panoptes from %s\n' "$repo_dir"
printf 'Installation root: %s\n' "$install_root"

run_cargo_install() {
    PANOPTES_GIT_SHA=$git_sha cargo install \
        --path "$repo_dir" \
        --locked \
        --force \
        --root "$install_root"
}

# Cargo can obtain rustc-wrapper = "sccache" from user configuration even when
# RUSTC_WRAPPER is absent from the environment. Keep a working cache enabled,
# but recover when sccache itself cannot start (for example, in a managed shell
# that denies its server IPC). Capturing through tee preserves normal progress
# output while retaining Cargo's status and enough diagnostics to distinguish a
# wrapper failure from a Panoptes compilation error.
install_log_dir=$(mktemp -d "${TMPDIR:-/tmp}/panoptes-install.XXXXXX")
install_log=$install_log_dir/cargo-install.log
install_status_file=$install_log_dir/cargo-install.status
cleanup_install_log() {
    rm -rf "$install_log_dir"
}
trap cleanup_install_log 0 HUP INT TERM

(
    set +e
    run_cargo_install
    install_status=$?
    printf '%s\n' "$install_status" >"$install_status_file"
    exit 0
) 2>&1 | tee "$install_log"

if [ ! -f "$install_status_file" ]; then
    printf '%s\n' 'error: cargo install ended without reporting its status' >&2
    exit 1
fi
install_status=$(sed -n '1p' "$install_status_file")
if [ "$install_status" -ne 0 ]; then
    if grep -Fq 'sccache' "$install_log" && \
        { grep -Fq 'sccache: error:' "$install_log" || grep -Fq 'could not execute process' "$install_log"; }
    then
        printf '%s\n' \
            'Configured sccache wrapper is unavailable; retrying without a Rust compiler wrapper.' >&2
        RUSTC_WRAPPER='' RUSTC_WORKSPACE_WRAPPER='' run_cargo_install
    else
        exit "$install_status"
    fi
fi

cleanup_install_log
trap - 0 HUP INT TERM

binary="$install_root/bin/panoptes"
if [ ! -x "$binary" ]; then
    printf 'error: Cargo completed but %s is not executable\n' "$binary" >&2
    exit 1
fi
"$binary" --version

case :${PATH:-}: in
    *:"$install_root/bin":*) ;;
    *)
        printf '\nAdd Panoptes to PATH:\n'
        printf '  export PATH="%s/bin:%s"\n' "$install_root" "\$PATH"
        ;;
esac

printf '%s\n' '' 'Next:' '  panoptes init' '  Restart a configured provider; Panoptes indexes its repository when it connects.'
