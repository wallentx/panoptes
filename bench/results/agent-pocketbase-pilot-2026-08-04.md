# PocketBase agent pilot — 2026-08-04

This is engineering evidence, not a product claim. It contains one paired run
for each of three read-only questions, using `gpt-5.6-terra` at medium reasoning
against PocketBase commit `0a74d2f25d6decfc9bd0fc64656ec431f23bf610`.
Both arms used the same prompt, navigation guidance, clean checkout, ignored
user configuration, and read-only sandbox. The treatment added only Panoptes.

| Metric | Baseline | Panoptes | Change |
| --- | ---: | ---: | ---: |
| Total input tokens | 861,944 | 754,069 | -12.5% |
| Uncached input tokens | 134,136 | 109,973 | -18.0% |
| Output tokens | 7,492 | 5,790 | -22.7% |
| Tool calls | 20 | 19 | -5.0% |
| Wall time | 168.0 s | 138.8 s | -17.4% |
| Deterministic rubric coverage | 18/18 | 16/18 | -2 checks |

All runs completed with no MCP failures. The Panoptes arm respected the
three-call budget in every task. One miss was substantive (`bindApi` was named
instead of `NewRouter`); the other answer described auth-alert behavior without
naming the `authAlert` function required by the rubric.

The efficiency direction is promising, but correctness did not match the
baseline and one trial per task is too small for marketing. Run multiple
counterbalanced trials and independently review answers before publishing an
agent-level percentage.
