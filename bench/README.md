# Benchmarks

Panoptes keeps two benchmark layers separate:

- `scripts/benchmark.sh` is a deterministic synthetic regression benchmark.
- `scripts/benchmark-repo.sh` measures indexing and queries on a pinned public
  repository.
- `scripts/benchmark-agent.sh` runs the same read-only coding questions with a
  fresh Codex session and checkout, first without Panoptes and then with it.

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
