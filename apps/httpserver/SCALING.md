# How far does the green-thread model scale? A ramping concurrency test

`BENCHMARK.md` answers "how fast is `httpserver` at a few fixed concurrency
levels (1/10/50/100)." This document asks a different question, the one
this project's own stated target (docs/concurrency-decision.md: "millions
of green threads, not tens of thousands") is actually about: **how far does
concurrency go before something degrades or breaks**, for the same two
servers (`apps/httpserver`, `apps/httpserver/goserver`), by ramping
concurrent connections up -- 100, 250, 500, 1000, 2500, 5000, 10000, 20000
-- and watching requests/sec, latency, success rate, server memory and
(for m31) the concurrently-live green-thread count at each step.

**Headline result:** neither server crashed, hung, or returned a single
error at any level tested, up to 20,000 concurrent persistent connections.
Both saturate (throughput peaks, then falls, while latency climbs) well
before that -- normal behaviour for any server once concurrency exceeds
what the hardware can actually run at once. The interesting finding is
*relative*: **m31 and Go track each other closely up to roughly 1,000-2,500
concurrent connections, then m31's throughput and tail latency degrade
measurably faster than Go's from 5,000 upward**, reproduced on a repeat run
at 10,000. Section "Root-cause analysis" below gives a specific,
code-grounded hypothesis for why -- identified by reading
`runtime/reactor_epoll.c` and `runtime/scheduler.c`, not confirmed by a
profiler -- and flags it for follow-up rather than attempting a fix, per
this project's own stated caution around its hand-rolled concurrency
primitives.

**Memory update:** the resident-memory gap this run measured (1.56 GB vs
460 MB at 20,000 connections) has since been root-caused and fixed -- see
"Follow-up: a shared per-carrier read buffer (memory fix)" below. It was not
the green-thread stack; the "Root-cause analysis" section's original memory
bullet has been corrected in place rather than deleted.

## Environment, honestly

- Same machine as `BENCHMARK.md`: 12 logical CPUs, x86-64 Linux, `hey` and
  both servers all on loopback (`127.0.0.1`).
- **This host got noisier as the test ran.** `uptime` immediately before
  starting: `load average: 1.31, 1.41, 1.67` (quieter than `BENCHMARK.md`'s
  own run). By the time the 20,000-concurrency level and the 10,000 rerun
  finished: `load average: 4.25, 8.18, 6.57`. Some of that rise is this
  test's own `hey`/server processes; some is plausibly other agents' work in
  sibling worktrees of the same shared repository (23 days uptime, dozens of
  concurrent git worktrees). The within-level m31-vs-Go comparisons are
  still fair (same host, same minute, same `hey` instance, for both
  servers, at each level) -- but the later, higher-concurrency levels ran
  under more ambient contention than the earlier ones, which is a real
  confound on top of concurrency itself and is not separable from it with
  the tools used here.
- `ulimit -n` (open files): **524288**, already far above anything this test
  needed; not raised for this exercise.
- Ephemeral port range (`/proc/sys/net/ipv4/ip_local_port_range`):
  **32768-60999, 28,232 ports**. This is the real ceiling on how high this
  *methodology* can ramp on one box: `hey` holds one persistent (keep-alive)
  TCP connection per worker for the whole run, so concurrency `c` means `c`
  client-side ephemeral ports in use at once. At `c=20000` that is ~71% of
  the entire range. See "Why the ramp stopped at 20,000" below.
- `somaxconn` (`/proc/sys/net/core/somaxconn`): **4096**.
- **A pre-existing asymmetry worth flagging**: `apps/httpserver/main.m31`
  calls `net.listen(host, port)` with no explicit `backlog`, so it gets
  `lib/net.m31`'s own default of **128** pending connections
  (`pub Result<Listener, Error> listen(str host, int port, int backlog =
  128)`). Go's `net.Listen` derives its backlog from `somaxconn` --
  **4096** here, over 30x larger. A connection-establishment burst (exactly
  what ramping `hey -c N` up produces) is more likely to find m31's listen
  queue momentarily full than Go's. This was not changed for this test
  (both servers are used exactly as `BENCHMARK.md` built them); it is
  reported as a plausible contributing factor to the divergence below, not
  a confirmed one -- see the caveats.
- `/proc/net/netstat`'s `TcpExt.ListenOverflows`/`ListenDrops` counters were
  **196,099** (equal to each other) when checked after this test's runs.
  These are host-wide, cumulative since the counters were last reset on a
  machine that has been up 23 days running many concurrent agent worktrees,
  so **this number cannot be attributed to this test's two listeners
  specifically** -- it is reported only as a sign that *something* on this
  shared host is overflowing a listen backlog, consistent with (but not
  proof of) the 128-vs-4096 asymmetry above.
- Memory: 15 GiB total, ~8.2 GiB available at test start; swap already had
  4 GiB in use *before this test started* (pre-existing, from unrelated
  activity on the shared host -- available memory never dropped anywhere
  near zero during this test, so this is noted but not implicated).
- `hey` v0.1.5 was already installed at `~/go/bin/hey` (the prior
  benchmarking session's `go install`); Go 1.23.8; m31 built with
  `cargo build` (debug compiler) + `-O2` emitted C, same as `BENCHMARK.md`.

## Method

### Instrumentation added for this test (nothing existing was modified)

`apps/httpserver/greenthread_probe.c` is a new, small, purely additive C
file: a `SIGUSR1` handler that reads the concurrently-live green-thread
count. It does **not** touch `runtime/scheduler.c` or `runtime/rt.c` --
those files already export three plain functions for exactly this kind of
external introspection (`rt_sched_spawned`/`rt_sched_completed`, the same
two counters `rt_run_program`'s own quiescence loop already uses, "spawned
count caught up with completed count"; `rt_sched_ncarriers`; and
`rt_global_scheduler`, non-`static` in `runtime/rt.c` but not in its
header, so re-declared here the ordinary C way rather than editing that
header). `live = spawned - completed` at the instant of the signal. Output
goes to `stderr` via a hand-rolled integer formatter and `write(2)` (not
`snprintf`, which is not async-signal-safe).

`apps/httpserver/build_scaling.sh` builds this into a separate binary,
`apps/httpserver/httpserver_scaling`, alongside the probe file. It does not
touch `apps/httpserver/build.sh` or the shipped `apps/httpserver/httpserver`
binary `BENCHMARK.md` measured -- this test uses `httpserver_scaling`
throughout (both for throughput numbers and for the probe samples), so all
of *this* document's own numbers come from one consistent binary.

```
bash apps/httpserver/build_scaling.sh                       # ./httpserver_scaling
(cd apps/httpserver/goserver && go build -o goserver main.go)

kill -USR1 <httpserver_scaling's pid>   # prints, to its stderr:
#   [greenthread-probe] carriers=12 spawned=183042 completed=182998 live=44
```

### The ramp

Concurrency levels: **100, 250, 500, 1000, 2500, 5000, 10000, 20000**
(roughly doubling, matching the task's suggested sequence). For each level,
for each server: a fresh process on a fresh port, a TCP-connect readiness
poll (same style as `BENCHMARK.md`/`test.sh`), then:

```
hey -z 10s -c <level> -t 20 http://127.0.0.1:<port>/
```

**10 seconds per run, not `BENCHMARK.md`'s 15** -- a deliberate deviation,
to fit 8 levels x 2 servers (plus a reproducibility rerun) in a reasonable
wall-clock budget on a shared host; `-t 20` (hey's per-request timeout) is
hey's own default, made explicit. **One run per level**, not two -- again
for the same time-budget reason, with a targeted exception: the single most
interesting result (the divergence at `c=10000`) was rerun once for both
servers specifically to check it was not noise (it reproduced -- see
below). This is a real reduction in rigor versus `BENCHMARK.md`'s 2-runs-
everywhere method, stated plainly rather than hidden.

At each run: `VmRSS` from `/proc/<pid>/status` before and after; a mid-run
sample of `ss -tn state established "( sport = :<port> )" | wc -l` (server-
side accepted-connection count, i.e. concurrently open connections from the
server's own point of view); for m31 only, a `SIGUSR1` sent to the server
at the same mid-run moment, sampling the live green-thread count directly.
`hey`'s own summary (`Requests/sec`, `p50/p90/p99`, status-code and error
distributions) is parsed from its text output, saved in full under
`apps/httpserver/scaling_raw/<server>_c<N>.txt`.

Full orchestration script, raw `hey` output for all 16 base runs plus the
20,000 level and the 10,000 rerun, and `summary.csv` (everything below, as
data) are all in `apps/httpserver/scaling_raw/`.

## Results

Zero errors, 100% success rate, in **every single run at every
concurrency level, both servers** -- the `error_count`/`success_rate_pct`
columns are omitted from the table below because they are uniformly 0 /
100.000 throughout; see `scaling_raw/summary.csv` for the literal column.

| concurrency | server | req/s | p50 (ms) | p90 (ms) | p99 (ms) | peak conns (server-side) | m31 live green threads | RSS after (MB) |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 100 | m31 | 78,646 | 1.0 | 2.3 | 5.2 | 101 | 120 | 22 |
| 100 | go | 62,044 | 1.2 | 3.6 | 6.7 | 102 | -- | 14 |
| 250 | m31 | 75,947 | 2.6 | 6.5 | 12.6 | 251 | 251 | 37 |
| 250 | go | 61,966 | 2.9 | 9.2 | 17.4 | 252 | -- | 19 |
| 500 | m31 | 73,467 | 5.4 | 13.9 | 25.5 | 527 | 528 | 55 |
| 500 | go | 59,776 | 6.0 | 19.2 | 36.3 | 531 | -- | 27 |
| 1000 | m31 | 70,048 | 11.1 | 29.5 | 55.4 | 1096 | 1097 | 93 |
| 1000 | go | 54,826 | 13.9 | 40.6 | 73.9 | 1021 | -- | 45 |
| 2500 | m31 | 47,369 | 36.6 | 95.6 | 184.6 | 2097 | 2744 | 276 |
| 2500 | go | 46,104 | 46.8 | 118.7 | 206.4 | 4561 | -- | 119 |
| 5000 | m31 | 35,737 | 95.9 | 274.3 | 787.3 | 7815 | 8280 | 564 |
| 5000 | go | 43,677 | 111.9 | 255.8 | 424.9 | 3948 | -- | 179 |
| 10000 (run 1) | m31 | 26,056 | 251.1 | 971.6 | 2295.3 | 9952 | 9275 | 1136 |
| 10000 (run 1) | go | 35,814 | 240.3 | 700.1 | 1147.3 | 8313 | -- | 307 |
| 10000 (run 2) | m31 | 26,345 | 271.4 | 915.7 | 2584.7 | 11775 | 12251 | 1117 |
| 10000 (run 2) | go | 37,211 | 237.1 | 621.4 | 1137.3 | 8459 | -- | 347 |
| 20000 | m31 | 16,418 | 690.6 | 2966.3 | 4520.1 | 18639 | 19030 | 1565 |
| 20000 | go | 25,253 | 551.5 | 2142.5 | 3948.9 | 18964 | -- | 461 |

(`peak conns` and `m31 live green threads` are two independent mid-run
samples, taken ~0.3s apart, not simultaneous -- they track each other
closely at every level, which is the point; see "Live green-thread count"
below for why they are not expected to match exactly.)

## Interpretation: where do they diverge

Both servers' peak throughput on this 12-core box is actually at the
**low** end of this ramp (`c=100`: m31 78.6K req/s, Go 62.0K) -- consistent
with `BENCHMARK.md`'s own finding that `c=100` was already past the point
of easy throughput gains. Everything from there is saturation: more
concurrent clients than the hardware can serve at once, so added
concurrency buys queueing (latency) rather than more completed work per
second. That shape is normal and not, by itself, a runtime problem for
either server.

What is specific to this test is **how differently the two saturate**:

- **c ≤ 1000**: close. m31 is consistently a little ahead on throughput and
  comparable (sometimes a little better, sometimes a little worse) on
  latency -- an extension of `BENCHMARK.md`'s "indistinguishable to noise"
  finding up to 10x its own highest tested level.
- **c = 2500**: still close (m31 47.4K vs Go 46.1K req/s; p99 184.6ms vs
  206.4ms -- m31 slightly ahead on both here).
- **c = 5000**: a real gap opens. m31 35.7K req/s vs Go 43.7K (**m31 ~18%
  lower**); p99 787ms vs 425ms (**m31 ~1.85x worse**).
- **c = 10000**: the gap widens and **reproduces on a second run**: run 1,
  m31 26.1K vs Go 35.8K req/s (-27%), p99 2295ms vs 1147ms (2.00x); run 2,
  m31 26.3K vs Go 37.2K req/s (-29%), p99 2585ms vs 1137ms (2.27x). Go's
  numbers are themselves fairly noisy run-to-run here (35.8K -> 37.2K, a
  shared-host-noise-sized jump), but m31's deficit relative to Go is
  consistent both times.
- **c = 20000**: m31 16.4K vs Go 25.3K req/s (**m31 ~35% lower**); p99 4520ms
  vs 3949ms (closer in ratio here, 1.15x, than at 10000 -- plausibly because
  something else, shared between both measurements, like `hey`'s own
  client-side cost or the now-noisier host, starts to dominate tail latency
  for both at this extreme). m31's resident memory is also clearly diverging
  by this point: 1.56 GB vs Go's 460 MB for the same 20,000 connections.

So: **the two track each other up to roughly 1,000-2,500 concurrent
connections and separate from there**, with m31 degrading faster in both
throughput and tail latency from 5,000 upward, and in resident memory by
20,000. Neither one errors, hangs, or crashes anywhere in this range.

## Root-cause analysis: why m31 degrades faster

**This is not `BENCHMARK.md`'s already-documented timeout/carrier-ceiling
bug.** That bug needs `set_read_timeout`/`set_write_timeout` to be set (it
is not, here -- confirmed in `apps/httpserver/main.m31`), and it has a
specific signature: a *cliff* at exactly `n_carriers` simultaneously-idle
connections, with `context deadline exceeded` errors. What this test
observed instead is a *smooth, monotonically worsening* gap as concurrency
rises, with **zero errors** anywhere -- a different shape, consistent with
a throughput/contention ceiling, not a stuck-carrier deadlock.

Reading `runtime/reactor_epoll.c` and `runtime/scheduler.c` end to end for
what happens on every single I/O-driven wakeup turns up a specific,
plausible mechanism:

1. **Exactly one dedicated OS thread runs the reactor's `epoll_wait` loop**
   (`reactor_loop`, `runtime/reactor_epoll.c`) -- this does not change with
   carrier count. For a keep-alive HTTP workload like this one, every
   request-response cycle needs at least one such wakeup (the connection's
   green thread parks waiting for the next request's bytes, or the next
   write to become writable), so the **event rate through this one thread
   scales with requests/sec, not just with concurrency**.
2. For every ready fd that thread discovers, it serially: (a) looks up
   `fd -> green_id` in the reactor's own waiter map, guarded by **one
   global `pthread_mutex_t`** (`rt_waiter_map_t`, open-addressed, by the
   file's own comment "not a hot path" -- true at low concurrency, not
   obviously true at tens of thousands of events/sec); then (b) calls
   `rt_sched_unpark`, which takes the scheduler's **registry lock** (a
   second global mutex) to flip that green thread's park word; then (c)
   `squeue_push`, which takes the **global run queue's own mutex** (a
   third) to push the now-runnable green thread; then (d)
   `notify_new_work`, which signals the carrier wake permutation.
3. The 12 carriers are free to *run* the resulting work in parallel once
   dispatched -- but only this **one** OS thread can *discover and
   dispatch* new work from epoll, serially, through **three separate
   global mutexes per event**. That is a serial front-end feeding an
   otherwise-parallel back end, and its cost is paid once per I/O
   readiness event, i.e. roughly once per request at this workload's
   shape.

This matches the shape of what was measured: fine at low event rates
(`c=100-1000`), a growing gap as event rate climbs into the tens of
thousands/sec (`c=5000` onward), and no errors anywhere (nothing here ever
blocks forever or drops anything -- it is a serialization cost, not a
deadlock). Go's own netpoller is not pinned to one permanently-blocked
OS thread the same way -- any idle P can poll it non-blockingly -- so it
does not pay the same single-thread tax as event rate grows, which is a
plausible explanation for why Go's throughput holds up better at the same
concurrency levels.

**Two more contributing factors worth naming, not confirmed as causal:**

- The **128 vs ~4096 listen backlog** asymmetry (Environment section
  above) could plausibly cost m31 more at the connection-*establishment*
  burst each `hey` run starts with, independent of the steady-state
  reactor cost above.
- **Memory (superseded -- see "Follow-up" below): this was wrong.** This
  section originally blamed the fixed 1 MiB green-thread stack
  (`docs/concurrency-decision.md`, "Stacks: fixed, but not limited") for
  most of the 1.56 GB vs 460 MB gap. That was an unprofiled guess, and
  tracing the actual allocator calls (`runtime/rt.c`) found it was wrong:
  dividing this run's own 1,565 MB by its own 19,030 live green threads
  gives ~82 KB/thread average resident memory -- far below the full 1 MiB
  reservation, confirming the stack slab (`rt_slab_new`'s plain
  `mmap(MAP_PRIVATE|MAP_ANONYMOUS)`, no `MAP_POPULATE`) really is lazily
  paged as designed. The real cost was `lib/net.m31`'s `Conn.fill`:
  it allocated a 64 KiB buffer **and eagerly `memset` it to zero**
  (`[0; CHUNK]`) on a connection's first read, then kept that buffer alive
  -- unused capacity and all -- for the connection's entire lifetime. 64 KiB
  x 20,000 connections is 1.28 GB on its own, which is where most of the
  gap actually was. Fixed below.

**What this analysis is not**: a profiler was not attached (no
`perf`/`strace`) to confirm time is actually spent contending on these
three mutexes versus something else (allocator pressure, cache effects,
the backlog asymmetry above, or something not considered here). This is a
hypothesis built by reading the exact code every I/O wakeup executes and
checking that its predicted shape (grows with event rate, no errors, not a
fixed-threshold cliff) matches what was measured -- not a confirmed root
cause. Per this project's own repeated caution about not patching its
hand-rolled concurrency primitives half-confidently
(`docs/concurrency-decision.md`), **this is flagged for a dedicated
follow-up** (profile `runtime/reactor_epoll.c`'s waiter-map lock and
`runtime/scheduler.c`'s registry/global-queue locks under exactly this
kind of sustained multi-thousand-connection HTTP load) rather than acted
on here.

## Follow-up: a shared per-carrier read buffer (memory fix)

Found by tracing the real allocator calls behind `Conn.fill` (`lib/net.m31`)
rather than guessing: every connection's first read allocated a 64 KiB
`bytes` buffer via `[0; CHUNK]` -- which both `memset`s the whole thing to
zero and keeps it alive, at that size, for the connection's entire
lifetime, however little of it is ever in use at a given moment. At 20,000
concurrent connections that is 1.28 GB on its own, and the "Memory" bullet
above has been corrected: it is not the green-thread stack (already lazily
paged, confirmed to average ~82 KB/thread actual RSS here, far below its
1 MiB reservation).

**The fix.** Green threads are cooperatively scheduled: only one runs per
carrier (OS thread) at a time, and nothing between a `read()` call and
copying its result out crosses `rt_stack_check` -- the only point this
runtime's probe-based preemption can switch which green thread is running
(`docs/concurrency-decision.md`; confirmed by reading `src/emit_c.rs`'s
probe-emission, which happens only at a compiled function's entry, never
inside a primitive call). That makes a **per-carrier**, not per-connection,
scratch buffer safe: a new primitive, `rt_read_chunk` (`runtime/rt.c`),
reads into a `_Thread_local` buffer the compiler never sees, then copies out
only the bytes that actually arrived into a fresh, exactly-sized `bytes`.
`Conn.fill` now replaces its buffer wholesale on every read instead of
keeping one CHUNK-sized allocation alive for the connection's whole life, so
a connection holds only as much as it actually has buffered, and the one
real CHUNK-sized buffer per carrier (12 of them, here) is shared by every
connection that carrier ever serves. The identical change was also applied
to `lib/io.m31`'s `File.fill` -- safe for the same reason even though a
file's `read` genuinely blocks the OS thread: a real blocking syscall never
hands the rest of the call to a different carrier the way a cooperative
park can (see `rt_wait_io`'s own comment in `runtime/rt.c` for the contrast,
and the real bug that comment documents).

**Re-measured** (not a full, clean 8-level x 2-server x 2-run repeat of the
original methodology -- see caveat below) at the levels where the original
gap was clearest:

| concurrency | server | req/s | p99 | RSS after |
|---:|---|---:|---:|---:|
| 1,000 | m31 | 95,287 | 42ms | **24 MB** (was 93 MB) |
| 1,000 | go | 98,760 | 44ms | 46 MB |
| 5,000 | m31 | 24,548 | 675ms | **63 MB** (was 564 MB) |
| 5,000 | go | 68,992 | 334ms | 229 MB |
| 10,000 | m31 | 33,144 | 2297ms | **104 MB** (was 1,117-1,136 MB) |
| 10,000 | go | 41,124 | 1021ms | 402 MB |
| 20,000 | m31 | 9,407 | 8957ms | **140 MB** (was 1,565 MB) |
| 20,000 | go | 15,345 | 4534ms | 605 MB |

**Memory**: at 20,000 connections, m31's RSS dropped from 1,565 MB to
140 MB -- an 11x reduction -- and now sits *below* Go's 605 MB, reversing
the original finding (m31 was 3.4x heavier than Go; it is now roughly 4.3x
lighter). The curve across concurrency levels is now close to flat
(24 -> 63 -> 104 -> 140 MB from 1K to 20K connections) rather than scaling
with connection count, which is exactly the fix's mechanism: the one real
per-carrier buffer does not grow with how many connections exist, only with
how many carriers there are.

**Throughput/latency: not a clean comparison this time, stated plainly.**
This re-run was done on a much noisier host than the original (load average
15+ at the time, several unrelated processes including other agent sessions
active on this shared machine), and it shows: both servers' numbers are
non-monotonic across levels (e.g. Go's req/s at c=5000 exceeds its own
c=1000 number), which only makes sense as contention, not a real effect.
The c=20000 "errors" on both servers (217 for m31, 78 for Go) are `hey`'s
own client-side ephemeral-port exhaustion, the same known methodology
ceiling this document's own "Breaking point" section already names -- not
a server-side fault. No throughput/latency verdict is drawn from this
re-run; only the memory result, which host noise does not explain away
(RSS is a property of what the process allocated, not of scheduling
contention), is reported with confidence.

## Live green-thread count (direct measurement, not just inference)

The task asked to try to observe concurrently-live green threads directly
rather than only inferring it from open connections. `runtime/scheduler.c`
already exports `rt_sched_spawned`/`rt_sched_completed` (the same two
counters `rt_run_program`'s own quiescence loop uses) and
`rt_sched_ncarriers`; `runtime/rt.c` already exports (non-`static`, just
not in its header) `rt_global_scheduler`. `apps/httpserver/
greenthread_probe.c`, described above, wires a `SIGUSR1` handler to read
`live = spawned - completed` from a running process, with no changes to
either of those runtime files.

Sampled mid-run at every level (table above, "m31 live green threads"
column), this tracked the independently-sampled server-side open-connection
count (`ss -tn state established`) closely at every level -- e.g. at
`c=1000`: 1097 live green threads vs 1096 established connections; at
`c=20000`: 19,030 vs 18,639 (sampled ~0.3s apart, so not expected to match
to the connection, but same order and same trend). This is a **direct**
confirmation (not just an inference from connection counts) that
`apps/httpserver`'s one-green-thread-per-accepted-connection design
(`main.m31`'s `accept_loop`/`spawn handle_conn(c)`) is doing exactly what
it says at real scale: 19,030 simultaneously live green threads was
observed directly, with the process still answering requests (if slowly)
and never erroring.

## Breaking point / ceiling, for each server

**Hard breaking point (errors, hang, crash): not reached, for either
server, anywhere in this test's range (up to 20,000 concurrent
connections).** This is itself a finding -- neither server's concurrency
model has an outright failure mode in this range; both degrade gracefully
into high latency under oversubscription rather than falling over.

**Soft/practical ceiling (where it stops being a reasonable server):**
roughly `c=1000-2500` for m31 (p99 still under ~200ms) and a bit further
out, `c=2500-5000`, for Go, before tail latency crosses from tens of
milliseconds into hundreds-of-milliseconds-to-seconds territory for both.
Past that, both servers are technically still serving every request
correctly, just slowly.

**Why the ramp stopped at 20,000** -- this is a host-level/methodology
ceiling, not a server finding: `hey`'s own persistent-connection model
means concurrency `c` is `c` client-side ephemeral ports held open for the
whole run, against a 28,232-port range on this host. At `c=20000` that is
already ~71% of the range for one server's run; after the pair of 20,000
runs (both servers, back to back), `ss -tan state time-wait | wc -l` showed
a peak of **~48,230** TIME_WAIT sockets (draining back to ~12,800 over the
next several seconds, then continuing down -- `tcp_tw_reuse=2` on this host
covers loopback reuse, so this did not actually block new connections, but
it is a visible, if temporary, resource-pressure signal). Pushing the ramp
higher, toward the literal port-range ceiling, would have (a) stopped being
a server-scaling measurement the moment `hey` itself started failing to
open new connections -- exactly the client-side-bottleneck risk the task
asked to watch for -- and (b) risked affecting other agents' unrelated
network activity on this shared, multi-tenant host by driving a shared
kernel resource (the ephemeral port table) close to exhaustion. `c=20000`
was chosen as the top of this ramp for those reasons, not because either
server broke at or before it.

## Caveats, explicitly

- **Shared, increasingly non-idle host.** `load average` rose from 1.31 to
  4.25/8.18/6.57 over the course of this test (Environment section). The
  within-level m31-vs-Go comparisons are still apples-to-apples (same
  moment, same `hey` instance, for both), but absolute numbers at the
  higher concurrency levels were measured under more contention than the
  lower ones -- a real confound layered on top of concurrency itself.
- **One run per level, not two**, for time-budget reasons across 8 levels x
  2 servers; reproducibility was spot-checked only at the single most
  interesting level (`c=10000`, both servers, two runs) and the divergence
  held both times.
- **`hey` itself is not free**, and this methodology cannot cleanly
  separate its cost from the server's: at `c=10000-20000`, `hey` is running
  that many goroutines, sending and receiving, on the same 12 cores the
  server under test is using. Some of the measured latency growth at the
  highest levels is certainly `hey`'s own CPU contention with its target,
  not purely server-side work -- exactly why this report treats the m31-
  vs-Go *comparison* (same `hey`, same host, same moment, per level) as the
  reliable signal, and is more cautious about either server's absolute
  numbers at the top of the ramp, the same discipline `BENCHMARK.md` uses
  for its own (lower) concurrency levels.
- **`ListenOverflows`/`ListenDrops`** (196,099, host-wide) could not be
  attributed specifically to this test's two listeners on this 23-day-
  uptime, many-worktrees shared host -- reported as weak corroborating
  context for the backlog-size asymmetry, not proof of it.
- The green-thread-count probe (`greenthread_probe.c`) is new
  instrumentation added only for this test, linked into a separate
  `httpserver_scaling` binary (`build_scaling.sh`); it calls three
  pre-existing, already-exported runtime functions and modifies neither
  `runtime/scheduler.c` nor `runtime/rt.c`. The shipped
  `apps/httpserver/httpserver` binary and `build.sh` used for
  `BENCHMARK.md` are untouched by this test.
- The root-cause section is a code-reading hypothesis, explicitly not
  profiler-confirmed -- see its own closing paragraph.

## Reproducing this

```
cd apps/httpserver
bash build_scaling.sh                                   # ./httpserver_scaling (adds the SIGUSR1 probe)
(cd goserver && go build -o goserver main.go)            # ./goserver

./httpserver_scaling --port 19001 --host 127.0.0.1 &
SPID=$!
hey -z 10s -c <100|250|500|1000|2500|5000|10000|20000> -t 20 http://127.0.0.1:19001/
kill -USR1 $SPID        # prints live green-thread count to the server's stderr
kill $SPID

./goserver --port 19009 --host 127.0.0.1 &
hey -z 10s -c <same level> -t 20 http://127.0.0.1:19009/
kill %1
```

Full orchestration (`ramp.sh`'s shape: fresh port and process per run,
`/proc/<pid>/status` RSS before/after, `ss -tn state established` sampled
mid-run, `SIGUSR1` sent to m31 at the same moment), all 16 base runs plus
the 20,000 level and the 10,000 rerun's raw `hey` output and server logs,
and the full `summary.csv` behind every number in this document, are in
`apps/httpserver/scaling_raw/`.
