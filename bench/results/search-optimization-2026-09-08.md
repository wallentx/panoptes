# Cached search and ASCII tokenization

On the deterministic 4,000-file TypeScript fixture, cached terms reduce ranked
CLI search from 251 ms to 72 ms and MCP startup plus search from 254 ms to 85 ms.
The costs are a larger store and additional indexing work. These are local
synthetic measurements, not an agent-quality or cross-project benchmark.

## Measurements

| Operation | Before | Cached terms | Samples per implementation |
| --- | ---: | ---: | ---: |
| Ranked CLI search | 251 ms | 72 ms | 10 |
| MCP startup plus `find` | 254 ms | 85 ms | 10 |
| Initial index | 622 ms | 1,065 ms | 6 |
| Unchanged refresh | 140 ms | 182 ms | 6 |
| One-file refresh | 182 ms | 189 ms | 6 |

Values are pooled medians from two runs in opposite implementation order.
Ranked CLI search median peak RSS falls from 74.9 MiB to 27.0 MiB; MCP startup
plus search falls from 72.2 MiB to 27.7 MiB. After the fixture's incremental
changes, the store grows from 19.1 MiB to 28.5 MiB (49%). Each store contains
4,000 files, 20,003 symbols, and 32,004 edges at that point.

This workload favors repeated queries over a stable index. Initial indexing is
71% slower, and unchanged refresh is 30% slower in these samples. Measurements
include normal host scheduling and filesystem cache effects; "initial index"
means an empty database, not a flushed operating-system page cache.

## Method

Measured on 2026-09-08 UTC with an AMD Ryzen AI 7 PRO 350, Linux x86-64,
Rust/Cargo 1.97.1. The baseline is commit
`9c05d4336b211d7b90659385354fd798e950d1fc`; the candidate is the search-cache
change in this revision. Both use release optimization and the default CPU
target. Candidate debug symbols were retained for assembly inspection.

Build each revision into a separate binary, then run:

```sh
scripts/benchmark.sh --binary /path/to/baseline --output /tmp/search-before \
  --files 4000 --query-runs 5 --build-runs 3 --no-profile
scripts/benchmark.sh --binary /path/to/candidate --output /tmp/search-after \
  --files 4000 --query-runs 5 --build-runs 3 --no-profile
```

Repeat in reverse order. Raw per-command measurements are in
[`search-2026-09-08`](search-2026-09-08), with the metadata reported by the runner.
The benchmark's MCP measurement starts a new server for every sample; it is
not the latency of a request to an already-running server.

## Implementation and validation

Search stores counts for name, path, signature, and body terms in SQLite,
retrieves only matching symbols, and computes each query term's IDF once.
Repeated query terms retain their original scoring contribution. A read
transaction keeps postings, corpus counts, and graph counts consistent.
Index updates and cache updates share a write transaction. Version 1 stores
backfill atomically from stored symbol text without refreshing source files.
An older binary cannot open a version 2 store.

The ASCII path uses Rust's bulk lowercase operation and avoids Unicode
character iteration. Release assembly contains SSE2 vector operations on
x86-64; an AArch64 Android cross-compile also emits NEON vector operations.
Boundary splitting remains scalar. No new dependency, custom unsafe code, or
stronger minimum CPU feature is required. Non-ASCII input retains the Unicode
path. These SIMD checks establish emitted instructions, not an ARM throughput
measurement; the main query speedup comes from avoiding repeated work.

Validation passed 81 native tests and 81 tests in the Termux container, Clippy
with warnings denied, formatting, and the release smoke workflow. A separate
baseline/candidate comparison matched all JSON fields, scores, and ordering
for 10 queries across 6 scopes, including repeated terms, missing terms,
structural fallback, and Unicode. Tokenizer tests cover boundary lengths,
unaligned slices, random ASCII, and Unicode case expansion. Cache tests cover
edits, renames, deletion, unchanged refresh, migration, and cascading removal.
