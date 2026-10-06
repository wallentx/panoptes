# ꙮ Panoptes

**Give your coding agent a fast, local map of the codebase.**

Panoptes indexes definitions, imports, and calls, then serves compact source
context through MCP. Agents can find implementations, trace callers, inspect a
file's API, search every occurrence, and orient themselves without reading the
repository file by file.

- Local SQLite index; source stays on your machine.
- TypeScript, TSX, JavaScript, Python, Go, Rust, shell, YAML, HCL, and Terraform.
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

### Homebrew (macOS and Linux)

```sh
brew install wallentx/tap/panoptes
```

Homebrew automatically adds the [tap](https://github.com/wallentx/homebrew-tap)
and installs the matching release binary for Apple Silicon or Intel macOS, or
ARM64 or x86-64 Linux, with checksum verification and shell completions. Rust is
only needed when explicitly installing the development version with `--HEAD` (for example, `brew install --HEAD wallentx/tap/panoptes`).

### From source (including Termux)

You need Git, Rust, Cargo, and a C compiler. On Termux, the equivalent packages
are `git`, `rust`, and `clang`.

```sh
git clone https://github.com/wallentx/panoptes.git
cd panoptes
./install.sh
```

For source installations, make sure `~/.local/bin` is on `PATH`. The installer
sets Cargo's package version from local Git tags: `v1.0.1` builds as `1.0.1`,
and later commits build as `1.0.2-g<short-hash>`. It uses the nearest reachable
SemVer tag, so tags on unrelated branches do not affect the installed version.
Gitless archives or checkouts without release tags retain the manifest version.
The installer updates only the package version in `Cargo.toml` and `Cargo.lock`;
it does not fetch tags. Use `git fetch --tags` to refresh locally available tags.

### Connect coding agents (optional)

After either installation method, connect your coding agents:

```sh
panoptes init
```

Choose providers in the picker and restart them. Panoptes installs the MCP
registration and usage guidance while preserving existing configuration. The
MCP server indexes the current repository when needed and refreshes changed
files automatically.

MCP tool calls have a **30-second inactivity timeout**. Real progress in file
scanning, parsing, symbol writing, graph resolution, and SQLite execution resets
that timer, so a
productive build can run longer than 30 seconds. Repeated waiting messages do
not reset it. A queued request also expires after 30 seconds without starting.
On inactivity, Panoptes stops the worker, releases database locks, and rolls back
uncommitted index changes; the MCP connection remains usable.

Progress is streamed while work runs. Clients that provide
`_meta.progressToken` receive `notifications/progress` with an increasing update
count and a stage message. Other clients receive structured
`notifications/message` logs at `info` level; `logging/setLevel` controls those
logs. Stage messages include file/symbol counts where available; database updates
count approximate SQLite VM instructions, not rows or percent complete. A busy
lock wait does not produce database progress. Interactive
`panoptes build` also prints progress to stderr. The client controls how MCP
notifications appear in its interface.

To change the inactivity window, add `--timeout-secs 60` to the server's `mcp`
arguments (allowed range: 1-300 seconds), then restart the client. Direct CLI
builds have no MCP inactivity limit. A stalled indexing transaction is rolled
back; retrying starts another attempt rather than resuming its partial writes.

For concurrent PR reviews, pass the absolute worktree path in `repo` on each
MCP call, for example:

```json
{"name":"find","arguments":{"query":"authentication","repo":"/src/project-pr-42"}}
```

Use `worktrees` to discover related checkouts, including ones created after the
MCP connection started. Discovery retains the common Git directory when the
startup worktree is removed, and omits missing checkouts. Unique checkout labels
also work; absolute paths avoid label collisions. Omitting `repo` continues to select the startup repositories.
Changing a shell's working directory does not retarget an existing connection.
Repository results are under `repositories[checkout_label]`, separate from
metadata even when the checkout name matches a metadata key. Results include
`panoptesCheckouts` with the canonical root, common Git directory,
branch, and observed HEAD. These describe live working files, including edits;
they are not immutable branch snapshots.

Each connection runs at most two isolated query workers, with up to 32 outstanding
operations and 128 callers. Identical parameters arriving while an operation is
queued or running share that operation, its progress, and its inactivity timer; completed
results are not cached. `panoptesExecution` identifies shared operations. Ping and
tool listing remain responsive during indexing. Cancellation removes only that
caller's interest; a worker stops when no callers remain. SQLite WAL permits
concurrent readers; builders serialize and recheck freshness after obtaining the
write lock. Separate checkouts retain separate graphs and parse caches.

Ranked search caches per-field terms when files are indexed, so queries retrieve
matching symbols without tokenizing the whole repository again. ASCII text uses
a portable bulk lowercase fast path; Unicode token boundaries remain unchanged.
Existing version 1 stores upgrade once to version 2 using their stored symbol
text. The cache uses additional disk space and indexing time in exchange for
faster queries; see the [search benchmark](bench/results/search-optimization-2026-09-08.md).

For scripted setup:

```sh
panoptes init --provider codex --provider claude
```

Or configure Cursor and OpenCode:

```sh
panoptes init --provider cursor --provider opencode
```

### Upgrade a Homebrew installation

```sh
brew update
brew upgrade wallentx/tap/panoptes
```

After upgrading, rerun `panoptes init` for your providers and restart their MCP
clients so they use the newly installed executable.

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

Ansible playbooks and roles expose tasks, handlers, role dependencies, and
notifications. GitHub Actions workflows and composite actions expose jobs, steps,
`needs`, nested `uses`, and input/output references. Docker Compose connects services
to their dependencies and declared resources. Kubernetes manifests, Kustomize
overlays, GitLab CI pipelines, and CloudFormation templates expose their static
resource and configuration dependencies. Shared YAML aliases and merge inheritance
preserve source provenance. See [automation relationships](docs/automation.md) for
supported syntax and limits.

## Build and verify

See [testing and release automation](docs/releases.md) for platform builds,
CLI/MCP smoke tests, and publishing a release.

```sh
cargo fmt --all --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
./scripts/benchmark.sh --output /tmp/panoptes-benchmark
```

See [SECURITY.md](SECURITY.md) for security policy. Panoptes is available under
the [MIT license](LICENSE).

### Checkout identity and lineage

`panoptes identity /absolute/checkout` registers/prints a store-local checkout ID
and Git-instance ID without indexing source. Linked worktrees share an instance;
independent clones retain distinct instances even when they share object storage.
MCP indexed results include these IDs in `panoptesCheckouts`, and CLI status JSON
includes `identity`. Discovery alone remains observational and does not create a
store. A replaced `.git` directory invalidates the observed instance locator.

After moving a checkout, use `panoptes relocate /old/absolute/root /new/root` to
preserve its ID. The old path must be absent and the destination unregistered.
Panoptes never infers relocation from matching contents, HEAD, or remote URLs.

Add `--lineage` to `identity` for optional local ancestry inspection. Each of two
Git commands has a two-second limit and a 128 KiB output cap. Git must support
`--no-lazy-fetch`; missing objects, unavailable Git, shallow boundaries, or grafts
produce an incomplete status rather than invented roots. Replacement refs are
ignored deliberately: lineage records physical commit ancestry. Each verified
root is keyed by object format and full OID; unrelated-history merges retain all
roots independently. Observations are tied to HEAD and refreshed on request;
lineage is relationship metadata, never an extraction or graph cache key.

Schema v4 adds identity metadata without rewriting existing graphs. Cache reset
removes graph data while retaining registered checkout IDs; global cache clear
also clears identity metadata. Keep a pre-migration SQLite backup when an older
binary must remain a rollback option.

### Shared source and extraction objects

Schema v5 stores exact source bytes once under an algorithm-tagged BLAKE3-256
identity. The official Rust implementation selects its supported SIMD backend
automatically (NEON on little-endian AArch64). Hashes are independent of CPU,
checkout path, Git instance, branch, and ancestry.

Base extraction profiles include language, full relative path, extractor stamp,
payload schema, and base mode. Keeping the path prevents identical YAML bytes in
workflow and ordinary directories from sharing incompatible results. Ansible and
GitLab include context is still recomputed per checkout; contextual payloads and
legacy `file_extracts` rows are never promoted into the shared base cache.

A second checkout reuses matching base extractions; an identical complete input
manifest also shares the graph and search postings. Old FNV file hashes invalidate
on the next refresh; migration itself preserves legacy graphs.
`cache clear` removes the shared objects too.

### Immutable graph snapshots

Schema v6 separates canonical checkout locations from graph ownership. Each
snapshot records a complete input manifest: exact source-byte identities and
relative paths, language selection, exact `go.mod` bytes or absence, scan policy,
and extractor/graph/search versions. A separate source-tree key excludes resolver
context and language interpretation. Neither key depends on Git ancestry.

A changed build creates a new graph, captures its source, verifies the inputs a
second time, marks it ready, and atomically advances the checkout attachment.
Failed scans or writes leave the previous attachment intact. Unchanged explicit
builds keep their current snapshot. Graph IDs and symbol IDs belong to snapshots;
a changed snapshot does not promise to retain the old numeric symbol IDs.

`find` excerpts and `grep` read captured bytes belonging to the selected graph.
CLI and MCP readers pin their attachment and graph in one SQLite read view.
`--no-refresh` can therefore answer from the last captured graph after live edits;
MCP reports `live_checked: false` when it deliberately skips the live scan. Legacy
snapshots remain queryable but have `sourceComplete: false` and no captured source
excerpts until refreshed. They are never treated as verified reusable snapshots.

The scan rejects unreadable/non-UTF-8 supported source and lossy path conversion,
rather than publishing an incomplete reusable graph. Literal backslashes in Unix
filenames stay distinct from directory separators. Symlinks and unsupported
languages remain excluded by the recorded scan policy. Two matching scans are a
stability check, not an operating-system-level atomic filesystem snapshot.

Old unreferenced graphs are reclaimed after attachment changes. Attached graphs
survive another checkout's reset, and SQLite readers retain their prior view
until their read transaction ends. Base source/extraction objects remain cached
until explicit cache cleanup. `panoptes cache gc --yes` reclaims unused source
objects and extraction profiles without removing any attached snapshot.


### Shared snapshots and explicit query scopes

Schema v7 attaches checkouts with identical complete input manifests to one ready
snapshot, even across unrelated Git instances. A second identical checkout does
not parse, resolve, or write graph/search rows again. Editing one checkout creates
or selects a different snapshot without changing other checkouts. Migration
consolidates verified schema-v6 duplicates while preserving checkout IDs; legacy
unverified graphs remain private until refreshed.

MCP `find`, `grep`, `callers`, `skeleton`, `map`, and `status` accept:

| `scope` | Membership |
| --- | --- |
| `checkout` (default) | Selected checkout(s), preserving current behavior |
| `repository` | Registered live checkouts with the selected store-local Git instance ID |
| `lineage` | Registered live checkouts with a current complete observation containing exactly `lineageRoot` |

Use `panoptes identity /absolute/checkout --lineage` to record bounded physical
ancestry first. Lineage scope requires `lineageRoot: "sha1:<root-oid>"` (or
`sha256:<root-oid>`). Missing/stale observations, shallow history, and grafts do not
establish membership. MCP does not start ancestry subprocesses; a changed HEAD
requires an explicit new observation. A merge containing roots A and B belongs
to either explicitly selected root; selecting A never includes B-only histories.
New worktrees must be registered/indexed before expanded scopes include them.

Expanded queries execute once per distinct snapshot, retain per-snapshot ranking,
and return every checkout's provenance in `panoptesCheckouts`. Duplicate basename
labels use absolute paths as result keys. `panoptesScope` reports scope, number
of snapshots searched, and skipped checkouts with unverified membership. No
cross-snapshot symbol-ID deduplication or blended-corpus ranking is implied.
`freshness` and `worktrees` remain observational checkout-scope tools.

CLI examples (expanded scopes always print JSON with provenance):

```sh
panoptes ask 'resolve imports' /absolute/checkout --view-scope repository
panoptes identity /absolute/checkout --lineage
panoptes ask 'resolve imports' /absolute/checkout --view-scope lineage --lineage-root sha1:<root-oid>
```

Snapshot sharing optimizes identical trees. A changed manifest still materializes
a complete graph using cached base extractions; fine-grained incremental graph
sharing is deferred. Two-pass input verification adds source reads. Benchmark
changed-checkout latency separately from identical-checkout attachment.



Git-instance locator validation uses a read-only filesystem incarnation token
(inode generation where available, otherwise birth time) together with device
and inode. Device/inode alone is never trusted after reopening. Filesystems that
expose neither token report an unknown Git instance; checkout IDs and ordinary
indexing remain usable. Legacy inode-only registrations are re-established on
explicit registration, preserving the checkout ID while assigning a new Git
instance ID. Ancestry inspection captures the registered instance before the
walk and rejects persistence if its generation or registration changes.
