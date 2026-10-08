# The post-c=500 throughput dip: diagnosis

Status: **root cause found, fix implemented and under validation.** Lives
entirely on `diagnostics-fuel-dip-investigation`, never intended for master
as-is -- see that branch's own commits for the exact, reviewable diffs this
document summarizes.

## Symptom

`SCALING.md`'s own benchmark (now in github.com/qrazil/httpserver under `bench/`), and every re-run of it
tonight, found m31 and Go closely matched up to roughly c=500-1000
concurrent connections, after which m31 diverges sharply and
non-monotonically -- a deep, often 30-50%+ dip relative to Go somewhere in
the c=2500-10000 range, not a smooth, proportional slowdown. Go's own curve
stays smooth and monotonic in every comparison run.

## What it isn't

**Not host noise alone.** The dip reproduced on the quietest runs of the
night (load 0.5-7 throughout, no other process competing), not just the
noisy ones -- ruled out by running the identical methodology multiple times
under deliberately verified-calm conditions.

**Not the reactor's waiter-map lock.** A separate patch
(`reactor-waiter-map-lock-removal` branch) removed that lock entirely and
the dip's shape was unchanged on a from-scratch control run against
unpatched `master` -- same dip, same rough location, with or without that
patch. Correctness-validated (TSan-clean) but shelved for insufficient
measured benefit, independent of this investigation.

**Not carrier work imbalance.** The first hypothesis tested here was the
randomized wake-permutation (`notify_new_work`, `runtime/scheduler.c`)
somehow starving some carriers while overloading others. Directly measured
(per-carrier dispatch counts, sampled via an extended `greenthread_probe.c`
SIGUSR1 handler) at both a normal level (c=500) and a dip level (c=5000):
work distribution across all 12 carriers is even in both cases (4-12%
min/max spread) -- ruled out.

## What it is

Further instrumentation (same probe, extended with draw-batch-size and
`notify_new_work` contention counters) found this instead, at every level
tested including the healthy c=500 case:

| level | req/s | avg draw size (fuel_size=4 cap) | avg permutation steps (of 12) | **full-lap rate** |
|---:|---:|---:|---:|---:|
| 500 | 100,180 | 3.24 | 11.56 | **95.4%** |
| 5000 | 38,609 | 2.61 | 11.02 | **90.2%** |
| 10000 | 11,840 | 1.92 | 9.94 | **80.0%** |

`notify_new_work` runs on every single push to the shared run queue. It
takes one global mutex (`rt_wake_perm_t.lock`) and walks up to
`n_carriers` (12) steps of a Fisher-Yates-shuffled permutation, doing one
atomic compare-exchange per step, looking for an idle carrier to wake.
**80-95% of the time, at every concurrency level measured, that walk finds
nobody idle at all** -- every carrier is already busy, which is the normal,
expected state once there is more work than carriers. The function still
pays the full locked O(n_carriers) cost to discover this, every time,
because there was no cheaper way to ask "is anyone idle at all" before
committing to the walk.

This is a real serialization point: as event rate rises with concurrency,
every one of the 12 carriers' wake-up paths contends for this one mutex to
do, in the large majority of cases, nothing at all with it. Draw batch
sizes (1.9-3.2 items, under the configured `fuel_size=4`) show carriers
are not starved by small batches specifically -- the waste is squarely in
`notify_new_work`'s own walk, not `fuel_size`.

## The fix (experimental, on this branch only)

A single additional atomic counter, `idle_count`, kept exactly in sync
with every place a carrier's `idle` flag actually changes (the CAS in
`notify_new_work` itself, and the two `carrier_main` sites that set it
directly -- changed from plain stores to exchanges specifically so
`idle_count` is only adjusted on a genuine transition, never on a
redundant store of the same value, which would otherwise drift the count
wrong over time). `notify_new_work` checks this one atomic, lock-free,
*before* ever touching the mutex or the permutation -- O(1) instead of a
locked O(n_carriers) walk in the 80-95% case that already dominates.

**Correctness, not just speed.** The existing mechanism already tolerates
raciness by design (`carrier_main` sets `idle = true` *before* waiting,
specifically so a racing notification during that window is never
permanently lost -- a periodic 5ms wake-semaphore timeout is the documented
fallback for anything still missed). `idle_count` going stale by a few
microseconds in either direction only means one push takes the slow path
one call later than it could have, or the 5ms periodic timeout finds the
work itself -- never a permanent miss, bounded by a fallback that already
existed and is unchanged.

**Validated so far** (all on this branch, before any benchmark run):
`scheduler_test.sh` (120/120 checks x4 builds + TSan-sanitized),
`greenthread_test.sh` (33/33 x4 + sanitized), `phase3_test.sh` (45/45 x4 +
sanitized), `spawn_backpressure_test.sh` (40/40 across 3 carrier counts,
15/15 under TSan at 1-2 carriers) -- all clean, zero regressions in any
existing correctness suite that exercises this exact mechanism.

**Benchmark comparison against the pre-fix baseline: see this branch's
subsequent commits / the conversation this document was written in for the
actual numbers** -- intentionally not duplicated here to avoid this
document going stale relative to the real data.

## Open, if this moves toward master

- `idle_count`'s type is `_Atomic uint32_t`; a correctness audit of every
  increment/decrement site paired correctly (done by hand here) would
  benefit from a dedicated stress test asserting `idle_count` never drifts
  outside `[0, n_carriers]` over a long, high-churn run -- not yet written.
- This branch's diagnostic counters (`diag_*`) are deliberately left in
  place alongside the fix for now, to keep measuring the full-lap rate
  post-fix; they would need a decision (keep as permanent introspection,
  matching `rt_sched_max_draw_seen`'s existing precedent, or strip before
  any merge) before this goes anywhere near master.
- The reactor waiter-map-lock-removal work (separate branch) is
  independent of this fix and was shelved for insufficient measured
  benefit -- not reconsidered here, but worth knowing both exist.
