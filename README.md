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

## Benchmark

**17% cheaper and 17% faster in a controlled three-task PocketBase pilot.**

| Metric | Without Panoptes | With Panoptes | Change |
| --- | ---: | ---: | ---: |
| API-price equivalent | $0.50 | $0.42 | **−17%** |
| Total input tokens | 861,944 | 754,069 | **−12.5%** |
| Uncached input tokens | 134,136 | 109,973 | **−18.0%** |
| Output tokens | 7,492 | 5,790 | **−22.7%** |
| Tool calls | 20 | 19 | **−5.0%** |
| Wall time | 168.0 s | 138.8 s | **−17.4%** |
| Correctness checks | 18/18 | 16/18 | −2 checks |

Both arms used the same Codex model, prompts, navigation guidance, clean
checkouts, and read-only sandbox. Cost applies the standard
[GPT-5.6 Terra API rates](https://developers.openai.com/api/docs/models/gpt-5.6-terra)—$2.00/M
uncached input, $0.20/M cached input, and $12.00/M output—to the recorded usage;
it is not a Codex subscription charge. This pilot is directional, not a broad
claim: one miss was substantive and one omitted a required function name. See
the [methodology and results](bench/results/agent-pocketbase-pilot-2026-08-04.md).

Panoptes also reports the source context avoided during each session:

> ꙮ Estimated tokens saved for this session: 166,376

That session figure is Panoptes's local estimate versus reading matched files
whole, not model billing.

## Fast enough to stay in the loop

**Index a 704-file production repository in 2.5 seconds. Refresh a one-file
change in 280 ms. Trace callers in 29 ms.**

Measured against a pinned PocketBase revision:

| Operation | Median |
| --- | ---: |
| Fresh index | **2.52 s** |
| Refresh unchanged index | **207 ms** |
| Refresh one changed file | **280 ms** |
| Ranked code search | **423 ms** |
| Exhaustive text search | **44 ms** |
| Trace callers | **29 ms** |

PocketBase produced 15,588 symbols and 45,693 resolved edges in a 40.3 MiB
index. Medians cover 3 build runs and 5 query runs on Linux with an AMD Ryzen
AI 7 PRO 350; peak RSS stayed below 90 MiB. The corpus, revision, raw outputs,
synthetic regression test, and controlled agent runner are documented in
[bench/README.md](bench/README.md). Results vary by machine and codebase.

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
