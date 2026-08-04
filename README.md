# Panoptes

Panoptes is a local code-intelligence index for navigating software repositories.
It turns definitions, imports, and resolvable calls into a searchable structural
map, then exposes compact, line-addressable results through a CLI and MCP server.

Use it to find where behavior is implemented, inspect a file's public shape,
trace dependencies, search every occurrence of a pattern, or orient yourself in
an unfamiliar codebase. Results include exact paths, line spans, signatures,
relationships, and bounded source excerpts so you can decide what to read next.

## Highlights

- Indexes TypeScript, TSX, JavaScript, Python, Go, and Rust.
- Finds relevant symbols for natural-language questions.
- Groups regex or literal matches under their enclosing symbol.
- Traverses incoming and outgoing call/import relationships.
- Produces file skeletons, repository maps, JSON, Markdown, and an HTML viewer.
- Serves the same tools to Codex, Claude Code, Cursor, Gemini CLI, Antigravity,
  OpenCode, and GitHub Copilot CLI through MCP.
- Incrementally reparses changed files and automatically refreshes normal CLI
  queries when source changes.
- Performs indexing and retrieval locally against a SQLite database.

Panoptes stores its shared index outside source repositories at
`$XDG_DATA_HOME/panoptes/panoptes.db`, or
`~/.local/share/panoptes/panoptes.db` when `XDG_DATA_HOME` is unset.

## Install

Panoptes requires Git, Rust, Cargo, and a C compiler. Install them with your
operating system's package manager. On Termux, use:

```sh
pkg update
pkg install git rust clang
```

Clone the repository and run the installer:

```sh
git clone --branch main --single-branch https://github.com/wallentx/panoptes.git
cd panoptes
./install.sh
```

This builds Panoptes for the current host, installs it as
`~/.local/bin/panoptes`, and verifies that the installed executable starts.

If `~/.local/bin` is not already on `PATH`, add it to your shell configuration:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Choose another user-owned installation root when needed:

```sh
./install.sh --prefix "$HOME/.local/panoptes"
```

Installation is user-local. The script builds the current checkout and prints
any required `PATH` update and next steps.

### Direct Cargo install

To install directly with Cargo:

```sh
cargo install \
  --git https://github.com/wallentx/panoptes.git \
  --branch main \
  --locked \
  --root "$HOME/.local" \
  panoptes
```

For a reproducible source install, replace `--branch main` with
`--rev <full-commit-sha>`.

### Update or uninstall

Update an installation from its checkout:

```sh
cd panoptes
git pull --ff-only origin main
./install.sh
```

For a complete uninstall, first run the provider picker and uncheck every
registered provider. This removes Panoptes's MCP entries, managed guidance, and
installed skill copies while preserving all unrelated provider settings and
instructions:

```sh
panoptes init
```

Optionally remove the local indexes and reclaim their database space:

```sh
panoptes cache clear --yes
```

Then remove an installation made under the default root:

```sh
cargo uninstall --root "$HOME/.local" panoptes
```

## Connect coding agents

`panoptes init` connects Panoptes to supported coding agents. For each selected
provider it registers the MCP server and installs guidance describing when and
how to use the indexed tools.

Run it without options to open the provider picker:

```sh
panoptes init
```

```text
Select providers (space toggles, enter confirms)
> [ ] Claude Code           ~/.claude.json
  [x] Codex                 ~/.codex/config.toml             registered
  [ ] Cursor                ~/.cursor/mcp.json
```

Registered providers start checked; detected but unconfigured providers are
labelled `detected`. Move with the arrow keys, toggle with space, and press enter
to apply the selection. Checking a provider installs or refreshes its Panoptes
configuration, while unchecking it removes the Panoptes integration.

For scripts or headless machines, repeat `--provider` instead of opening the
picker:

```sh
panoptes init --provider codex --provider claude
panoptes init --provider claude --deregister
panoptes init --provider cursor --dry-run
panoptes init --provider opencode --dry-run --json
```

Supported provider IDs and destinations:

| Provider ID | Provider | MCP configuration | Guidance and skill |
|---|---|---|---|
| `claude` | Claude Code | `~/.claude.json` | `~/.claude/rules/panoptes.md`, `~/.claude/skills/panoptes/` |
| `codex` | Codex | `~/.codex/config.toml` | active `~/.codex/AGENTS*.md`, `~/.agents/skills/panoptes/` |
| `cursor` | Cursor | `~/.cursor/mcp.json` | `~/.agents/skills/panoptes/` |
| `gemini` | Gemini CLI | `~/.gemini/settings.json` | `~/.gemini/GEMINI.md`, `~/.agents/skills/panoptes/` |
| `antigravity` | Antigravity | `~/.gemini/config/mcp_config.json` | `~/.gemini/GEMINI.md`, `~/.gemini/antigravity-cli/skills/panoptes/` |
| `opencode` | OpenCode | `~/.config/opencode/opencode.json` | `~/.agents/skills/panoptes/` |
| `copilot` | GitHub Copilot CLI | `~/.copilot/mcp-config.json` | `~/.agents/skills/panoptes/` |

Codex, Cursor, Gemini CLI, OpenCode, and Copilot share the skill installed under
`~/.agents`. Claude Code and Antigravity use their provider-specific global skill
directories. Managed markers identify the Panoptes sections in shared instruction
files, allowing later updates and removal while preserving unrelated content.
All provider integration stays in user configuration outside indexed repositories.

## CLI quick start

```sh
cd /path/to/repository
panoptes build
panoptes map
panoptes ask "where is request validation handled?" --source
panoptes callers validate_request --depth 2
panoptes grep 'TODO|FIXME' --path .
panoptes check
```

`build` walks the Git worktree using gitignore-compatible rules and skips
TypeScript declaration files. Subsequent builds reuse unchanged extraction
payloads. Large cold builds use at most four parser workers; use `--jobs 1` or
`PANOPTES_JOBS=1` on a memory-constrained device.

When a provider connects, MCP indexes the current repository and incrementally
refreshes changed source before retrieval calls. Running `panoptes build` first
is optional but can pre-warm the index. The MCP `freshness` tool reports drift
without triggering a build.

CLI query commands refresh an existing stale index before answering. Create the
initial CLI index with `panoptes build`; `check` and `status` only report its
state. Use global `--no-refresh` or `PANOPTES_NO_REFRESH=1` when queries should
use an intentional stored snapshot.

## Commands

| Command | Purpose |
|---|---|
| `build [path]` | Incrementally index a repository or multi-repository workspace |
| `ask <query> [path]` | Ranked lexical and structural symbol retrieval |
| `grep <pattern> --path <path>` | Regex or literal source search grouped by symbol |
| `callers <symbol>` | Traverse incoming or outgoing calls and imports |
| `skeleton <file>` | List definitions, signatures, and spans in one file |
| `map [path]` | Show directory clusters, hubs, and hotspots |
| `viz [path]` | Serve a loopback viewer or write self-contained HTML |
| `export <destination>` | Write deterministic Markdown cards or JSON |
| `init` | Configure MCP, guidance, and skills for coding agents |
| `mcp [path]` | Serve newline-delimited MCP JSON-RPC over stdin/stdout |
| `check [path]` | Fail when an index is missing, stale, or incompatible |
| `status [path]` | Report graph counts, age, schema, extractor, and source drift |
| `cache clear --yes` | Remove all indexed graphs and reclaim database space |
| `cache prune` | Remove records for repository paths that no longer exist |
| `cache doctor` | Run SQLite's full integrity check |
| `cache recover` | Preserve a damaged store and create a clean replacement |
| `cache reset [path]` | Remove one repository graph from the shared store |
| `version` | Show binary, build, schema, extractor, and store identity |
| `upgrade` | Show the safe update path without modifying the executable |
| `completions <shell>` | Generate shell completion code |

Run `panoptes <command> --help` for the complete flags. Read commands provide JSON
where applicable. `ask`, `grep`, and `callers` accept segment-aware `--in`
scopes.

Exit status `0` means the command completed, including an empty search. Status
`1` means a failed freshness/integrity check or another runtime error. Status `2`
means bad input, an unindexed repository, or a missing/ambiguous requested node.

## MCP tools

The MCP server exposes:

- `find` for ranked context and bounded source excerpts, included by default
- `grep` for exhaustive regex or literal occurrences
- `callers` for incoming or outgoing dependency traversal
- `skeleton` for definitions and signatures without a whole-file read
- `map` for repository orientation, hubs, and hotspots
- `status` for graph counts and freshness
- `freshness` for an observational live-source comparison

Retrieval results include a `panoptesSavings` object with per-call and MCP-session
token-savings estimates. The baseline is the size of distinct matched indexed
files represented in the result; the response cost is the serialized tool
payload. Both use a four-characters-per-token heuristic. These values describe
an estimated reduction versus reading those files whole, not model billing or a
provider-measured token count. The session total resets when the MCP process
restarts.

Requests and responses are capped at 1 MiB. Indexing and retrieval are local and
deterministic.

## Multi-repository workspaces

A non-repository directory containing at least two immediate Git repository
children is treated as a workspace. Build and query commands federate those
children and label results by repository. Ranked results are interleaved so one
large child cannot consume the entire result limit.

Linked Git worktrees keep separate graph rows because their checked-out source
can differ. Their shared Git directory is recorded only for diagnostics.

## Viewer and exports

`panoptes viz` starts a self-contained HTML viewer on loopback. Binding a non-loopback
address requires explicit `--allow-remote`:

```sh
panoptes viz
panoptes viz --output map.html
```

Exports are deterministic and require an explicit destination. Existing output
is not replaced without `--force`:

```sh
panoptes export docs/panoptes-map
panoptes export graph.json --json
```

## Graph boundaries

Panoptes records `contains`, `imports`, and calls it can resolve without guessing.
Unknown receiver types stay unresolved instead of creating false same-name
edges. Inheritance and arbitrary reference edges are not part of the native
`callers` contract.

Natural-language `ask` is deterministic local retrieval over symbol names,
signatures, bounded definition bodies, graph coupling, and test-path de-ranking.

## Security and privacy

- Panoptes performs indexing and retrieval locally and makes no outbound network
  requests at runtime.
- MCP clients receive the requested tool results; the selected coding agent's
  data-handling policy applies to those results.
- The index lives outside the repository, and indexing leaves source files
  unchanged.
- Provider setup is limited to selected user configuration files and preserves
  unrelated settings and instructions.
- Release archives contain a stripped binary, README, license, and SHA-256
  sidecar; verify the checksum before copying a release binary onto `PATH`.

See [SECURITY.md](SECURITY.md) for reporting and policy details.

## Development

```sh
git clone --branch main https://github.com/wallentx/panoptes.git
cd panoptes
cargo fmt --all --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
```

The deterministic performance harness records wall time, peak RSS, database
size, device/toolchain metadata, SQLite plans, and optional syscall profiles:

```sh
./scripts/benchmark.sh --output /tmp/panoptes-benchmark
```

The output directory contains raw measurements, a median summary, environment
metadata, and SQLite query plans. The performance workflow uploads that directory
as a per-run artifact. Results are device- and fixture-specific, so the repository
does not treat measurements from one machine as canonical performance data.

## License

MIT. See [LICENSE](LICENSE).
