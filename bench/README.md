# Benchmarks

Panoptes keeps these benchmark layers separate:

- `scripts/benchmark.sh` is a deterministic synthetic CLI regression benchmark.
- `scripts/benchmark-mcp.py` measures MCP indexing with restored fixture stores
  and records streamed progress.
- `scripts/benchmark-repo.sh` measures indexing and queries on a pinned public
  repository.
- `scripts/benchmark-agent.sh` runs the same read-only coding questions with a
  fresh Codex session and checkout, first without Panoptes and then with it.

The [cached search benchmark](results/search-optimization-2026-09-08.md) compares
query latency, memory, indexing time, and store size against the uncached search
implementation, with raw measurements from both execution orders.


## Repeatable MCP indexing baseline

This runner uses deterministic synthetic TypeScript repositories with the same
shared generator as `benchmark.sh`. It exercises the real binary, SQLite store,
parser, graph builder, and MCP transport; only the source corpus is generated.

```sh
cargo build --release --locked -j 1
python3 scripts/benchmark-mcp.py --output .local/benchmarks/mcp-baseline
```

Requirements: Python 3.9+, Git, a C compiler, and an already-built Panoptes
binary. `--binary PATH` chooses another build; the runner never silently builds
or replaces it. Temporary fixtures use `$TMPDIR` when set. No network downloads,
commits, or changes to the working checkout are needed.

| Case | Starting database | Target source |
| --- | --- | --- |
| `empty-index` | Absent | Original fixture, unindexed |
| `populated-index` | Snapshot containing background repositories | Original fixture, unindexed |
| `changed-files` | Snapshot containing background repositories and target | Same fixed set of edits every trial |
| `unchanged` | Same fully indexed snapshot | Original files and timestamps |

Each trial follows this sequence:

```text
restore source + database -> timed MCP find -> validate -> remove working DB/WAL/SHM
```

Setup builds each seed once, then uses SQLite backups for closed, consistent
snapshots. Each restore is verified byte-for-byte. Seeds remain private until
the runner exits; the entire temporary tree is then removed, including on
failure. **Every Panoptes command receives an explicit scratch `--store`. The
normal user store is never opened or cleared.** Output directories must be empty
so previous reports cannot be overwritten.

Defaults: 200 target files, 3 background repositories of 1,000 files each,
20 changed files, 1 extraction worker, 1 warmup and 3 measured trials per case.
Scenario order rotates between trials. Use the same flags for baseline and
candidate runs. Larger `--background-files` values expose costs that grow with
the shared store; this is deliberately distinct from indexing an empty store.
For a quick check:

```sh
python3 scripts/benchmark-mcp.py --files 20 --changed-files 2 \
  --background-repos 2 --background-files 30 --runs 2 --warmups 0 \
  --output .local/benchmarks/mcp-small
```

The primary measurement is `request_ms`: from sending `tools/call` until its
response, including automatic indexing/refresh and retrieval. `wall_ms` also
includes process startup, initialization, and shutdown. Setup, snapshot copying,
source reset, validation, and cleanup are excluded. `empty-index` means an empty
**index**, not a cold operating-system cache; the runner does not flush OS caches.
Memory comes from the existing `bench/rusage.c` helper: the largest process peak
RSS reported by `wait4`, not aggregate concurrent memory; macOS bytes are
normalized to KiB.

`report.json` retains every sample, medians/min/max, first-progress latency,
progress counts and maximum gaps (including the final gap to the response),
store sizes, binary SHA-256/version, fixture SHA-256, host details, parser jobs,
and seed graph counts. JSONL files retain real MCP messages. Validation checks
SQLite integrity, source freshness without refreshing, expected query hits,
normalized target graph checksums, and unchanged background graphs. Row IDs,
absolute paths, timestamps, and hash-algorithm-specific stored hashes do not
enter the graph checksum. Failed/time-limited runs exit nonzero and retain a
`failed` report, never a successful median.

`--idle-seconds` controls the MCP inactivity limit (default 30).
`--max-seconds` is a separate diagnostic ceiling per trial/setup command (default
600); reaching it fails the run and stops its process group. Progress does not
turn such a failure into a successful sample.

Compare an optimization after rebuilding the candidate binary:

```sh
python3 scripts/benchmark-mcp.py --output .local/benchmarks/mcp-candidate \
  --baseline .local/benchmarks/mcp-baseline/report.json
```

Comparison rejects different fixtures, configuration, host metadata, or resulting
graphs. It records median request-time percentage changes; negative means faster.
Use the same machine and power/thermal conditions, and repeat both execution
orders before treating small differences as improvements. The stored binary
hash/version identifies the measured executable; the recorded checkout revision
is runner context and is not a claim that the executable was built from it.

Each report directory includes a rerun command and `SHA256SUMS`. From that
directory, run `sha256sum -c SHA256SUMS` to check artifact integrity. Local output
under `.local/benchmarks/` is ignored by Git. The reset/isolation/comparison tests
run with `python3 -B tests/test_benchmark_mcp.py` after `cargo build`.

## Real repository

The default corpus is PocketBase at the exact revision recorded in
`corpora/pocketbase.conf`.
It is a useful first production corpus and enables direct comparison with
[Graft](https://github.com/NanoNets/Graft), but no single repository is
representative. Add pinned TypeScript, Python, and Rust corpora before treating
the systems results as a broad claim.

```sh
scripts/benchmark-repo.sh --output /tmp/panoptes-pocketbase
```

Pass `--source PATH` to reuse an existing checkout without downloading it. The
runner clones that checkout into temporary storage before making its one-file
incremental changes. Raw command output, per-run resource measurements, corpus
metadata, index status, and medians are retained in the output directory.
The latest checked-in measurement is under
[`results/pocketbase-2026-08-04`](results/pocketbase-2026-08-04).

## Controlled agent comparison

The agent runner requires `codex`, `jq`, a built release binary, and a local
checkout of the pinned corpus.

```sh
cargo build --release --locked --package panoptes
scripts/benchmark-agent.sh \
  --source /path/to/pocketbase \
  --output /tmp/panoptes-agent-benchmark
```

Use `--task auth-routing` for a two-run smoke test before running the full task
set.

Both arms use the same model, reasoning level, task, navigation guidance,
read-only sandbox, clean checkout, and ignored user configuration. The
treatment arm adds only the Panoptes MCP server. Runs are ephemeral; the order
is reversed on even-numbered trials. JSONL events and final answers are
retained, while `results.tsv` records actual Codex token usage, tool calls, MCP
failures, wall time, exit status, deterministic rubric coverage, and an
API-price equivalent. The pricing inputs are retained in `metadata.tsv` and can
be overridden with `PANOPTES_AGENT_INPUT_USD_PER_MTOK`,
`PANOPTES_AGENT_CACHED_INPUT_USD_PER_MTOK`, and
`PANOPTES_AGENT_OUTPUT_USD_PER_MTOK`.

Both total and uncached input tokens are recorded. Treat them separately:
cached context still reaches the model, but providers may bill it differently.

One trial is a smoke test, not a product claim. Use multiple trials and inspect
the saved answers before publishing comparisons.

The first three-task pilot is recorded in
[`results/agent-pocketbase-pilot-2026-08-04.md`](results/agent-pocketbase-pilot-2026-08-04.md).
It showed lower aggregate cost, tokens, tool calls, and wall time, but did not
match baseline rubric coverage. The product README presents those measured
results with the pilot size and correctness result visible.

### Worker stage timing and schema-v3 comparisons

MCP benchmark format 2 fingerprints symbol signatures, crux, summaries,
containers, and all search field counts as well as graph edges. Older format-1
reports are rejected as comparison baselines; rerun the old executable using
this runner to make a format-2 baseline.

Each trial records `worker_stages` from worker-side monotonic clocks, including
writer-lock wait separately. `elapsed_micros` includes SQL within the named
phase; `sqlite_vm_steps` is an approximate instruction count, not SQL duration.
These measurements remove IPC delivery latency from phase boundaries. They do
not claim an exclusive CPU or nested SQL-time profile.

Schema v3 adds indexes for both edge foreign keys and records the exact
`go.mod` bytes (including absence) used for import resolution. Opening an old
store migrates it atomically without rewriting graph rows. Legacy graphs lack
verified resolver inputs and refresh once on demand. Older binaries reject v3;
use a SQLite backup taken before migration if binary rollback is required.
The installed executable and its store are not upgraded by running fixture
benchmarks, which always use explicit disposable stores.
