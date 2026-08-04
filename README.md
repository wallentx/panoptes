# ꙮ Panoptes

**Give your coding agent a fast, local map of the codebase.**

Panoptes indexes definitions, imports, and calls, then serves compact source
context through MCP. Agents can find implementations, trace callers, inspect a
file's API, search every occurrence, and orient themselves without reading the
repository file by file.

- Local SQLite index; source stays on your machine.
- TypeScript, TSX, JavaScript, Python, Go, and Rust.
- Incremental refresh when files change.
- MCP setup for Codex, Claude Code, Cursor, Gemini CLI, Antigravity, OpenCode,
  and GitHub Copilot CLI.

## Fast enough to stay in the loop

Local benchmark on a 400-file synthetic repository:

| Operation | Median |
| --- | ---: |
| Cold index | **71 ms** |
| Refresh unchanged index | **18 ms** |
| Ranked code search | **22 ms** |
| Exhaustive text search | **8 ms** |
| Trace callers | **4 ms** |

Measured over 3 build runs and 5 query runs on Linux with an AMD Ryzen AI 7
PRO 350. The resulting index was 2.0 MiB and peak RSS stayed below 25 MiB.
Reproduce it with `scripts/benchmark.sh`; results vary by machine and codebase.

## Get running

You need Git, Rust, Cargo, and a C compiler. On Termux, the equivalent packages
are `git`, `rust`, and `clang`.

```sh
git clone https://github.com/wallentx/panoptes.git
cd panoptes
./install.sh
```

Make sure `~/.local/bin` is on `PATH`, then connect your coding agents:

```sh
panoptes init
```

Choose providers in the picker and restart them. Panoptes installs the MCP
registration and usage guidance while preserving existing configuration. The
MCP server indexes the current repository when needed and refreshes changed
files automatically.

For scripted setup:

```sh
panoptes init --provider codex --provider claude
```

## What agents get

| Tool | Purpose |
| --- | --- |
| `find` | Ranked code context for a question |
| `grep` | Every regex or literal occurrence |
| `callers` | Incoming or outgoing dependency paths |
| `skeleton` | Every signature in one file |
| `map` | Repository structure, hubs, and hotspots |
| `status` | Index state and estimated context savings |

The same operations are available directly from the CLI:

```sh
panoptes map .
panoptes ask "where is authentication handled?" .
panoptes callers authenticate --path .
panoptes grep 'TODO|FIXME' --path .
```

Run `panoptes --help` or `panoptes <command> --help` for the full interface.
Indexes live at `$XDG_DATA_HOME/panoptes/panoptes.db`, falling back to
`~/.local/share/panoptes/panoptes.db`.

## Build and verify

```sh
cargo fmt --all --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
./scripts/benchmark.sh --output /tmp/panoptes-benchmark
```

See [SECURITY.md](SECURITY.md) for security policy. Panoptes is available under
the [MIT license](LICENSE).
