# `httpserver` vs. a minimal Go `net/http` server

A throughput/latency comparison between `apps/httpserver` (this project's
"Hello, World!" server, built on `lib/http.src`'s real `Handler`/`serve_conn`
machinery, one green thread per connection) and `apps/httpserver/goserver`
(the same thing, `net/http` only, no third-party dependencies). Two findings
came out of this exercise: the throughput/latency comparison itself, and a
second, more interesting one about read/write timeouts and this runtime's
carrier model, found by *trying* to run this benchmark with the obviously
"correct" server code and watching it fail.

## Environment, honestly

- 12 logical CPUs (`nproc`), x86-64 Linux, both servers run on the same
  machine as the `hey` load generator (all loopback, `127.0.0.1` -- no
  network hop, which the numbers below should be read with in mind).
- `uptime` immediately before the runs: `load average: 6.04, 4.33, 3.00`.
  This machine was **not** idle -- this is a shared development host and
  other work (other agents' builds/tests in sibling git worktrees of the
  same repository) was plausibly running at the same time. This is the
  single biggest caveat on the absolute numbers below: they are internally
  comparable (same machine, same minute, same method, for both servers) but
  should not be read as "m31 serves exactly 53,000 req/s on dedicated
  hardware." A quieter machine might show higher throughput for both; a
  busier one, lower for both -- what matters for the comparison is that
  both servers were measured under the same conditions, back to back.
- Go 1.23.8 (`net/http`, standard library only, `GOMAXPROCS` left at its
  default, which is `nproc`). m31's green-thread scheduler also defaults its
  carrier count to `nproc` (`LANG_NUM_CARRIERS`, unset here) -- both sides
  run with "one scheduling unit per core" as their default, which is the
  fairest default-vs-default comparison available without hand-tuning
  either one.
- This is **one trivial endpoint** (`GET` anything -> `200`, 14-byte body,
  no routing, no work done per request) under a synthetic load generator on
  loopback. It is a measurement of the HTTP server loop and the concurrency
  model underneath it, not of either language or any realistic workload. Do
  not extrapolate to "m31 is as fast as Go" or "Go is as fast as m31" in
  general -- extrapolate only "for this one endpoint, on this one machine,
  neither was meaningfully faster than the other."

## Method

Load generator: [`hey`](https://github.com/rakyll/hey) v0.1.5 (Go, HTTP load
testing, the standard tool for this -- `wrk`/`bombardier`/`ab` were checked
first and none were installed or reachable with a package manager on this
host; `hey` was `go install`-able with no elevated privileges, so no custom
load generator was needed). Each server was built fresh, started on its own
port, warmed up with a readiness poll (a successful TCP connect, not a
timed warm-up period), driven for a fixed **15 second** duration (`hey -z
15s`) at four concurrency levels, **2 runs per concurrency level per
server**, server restarted between every run. `hey`'s default keep-alive
behavior was left on for both (persistent HTTP/1.1 connections reused for
the worker's whole run, which is the realistic case for both a browser and
a load balancer's backend connection pool).

```
# built once each, before the runs below
cd apps/httpserver && bash build.sh                               # ./httpserver
cd apps/httpserver/goserver && go build -o goserver main.go       # ./goserver

# one example run (repeated at c=1,10,50,100, 2x each, both servers,
# a fresh port and a fresh server process every time):
./httpserver --port 19001 --host 127.0.0.1 &
hey -z 15s -c 10 http://127.0.0.1:19001/
kill %1

./goserver -port 19009 -host 127.0.0.1 &
hey -z 15s -c 10 http://127.0.0.1:19009/
kill %1
```

Concurrency levels: **1** (no contention at all), **10** (at, not past, the
carrier/GOMAXPROCS count), **50** and **100** (well past it, the "100+" the
task asked for). Collected: requests/sec, and latency p50/p90/p99 (`hey`
also reports p10/p25/p75/p95; the full text is in
`apps/httpserver/bench_raw/`, alongside this file, one `.txt` per run,
exactly as `hey` printed it).

## Results

Requests/sec, both runs, both servers:

| concurrency | m31 run 1 | m31 run 2 | go run 1 | go run 2 |
|---:|---:|---:|---:|---:|
| 1 | 6,920 | 6,562 | 7,396 | 7,577 |
| 10 | 35,803 | 34,959 | 33,309 | 33,301 |
| 50 | 52,289 | 50,804 | 52,500 | 50,248 |
| 100 | 53,428 | 54,820 | 53,132 | 52,337 |

Latency (seconds), run 1 of each (run 2 is within a few percent in every
cell -- see `bench_raw/`):

| concurrency | server | p50 | p90 | p99 |
|---:|---|---:|---:|---:|
| 1 | m31 | 0.0001 | 0.0002 | 0.0002 |
| 1 | go | 0.0001 | 0.0002 | 0.0002 |
| 10 | m31 | 0.0003 | 0.0004 | 0.0006 |
| 10 | go | 0.0003 | 0.0004 | 0.0007 |
| 50 | m31 | 0.0008 | 0.0016 | 0.0041 |
| 50 | go | 0.0007 | 0.0018 | 0.0044 |
| 100 | m31 | 0.0015 | 0.0035 | 0.0077 |
| 100 | go | 0.0014 | 0.0041 | 0.0077 |

**Zero errors in every one of these 16 runs, both servers, every
concurrency level.** Run-to-run variance within each (server, concurrency)
pair is a few percent, consistent with ordinary measurement noise on a
shared, non-idle machine -- not a sign either server behaved differently
between its two runs.

## Interpretation

At this one endpoint, on this one machine, **the two servers are
indistinguishable to within run-to-run noise**, at every concurrency level
tested, from 1 connection to 100. Neither is faster; the small differences
between columns above (a few percent here or there) flip direction between
run 1 and run 2 of the same configuration about as often as they don't,
which is the signature of noise, not a real effect. The honest conclusion
is not "m31 matches Go's HTTP performance" as a general claim -- it is
narrower and more useful than that: **tonight's concurrency fix produced a
server whose request-handling loop is not the bottleneck here**; whatever
is limiting throughput at 100 concurrent connections on this box (likely
`hey`'s own client-side cost and loopback syscall overhead, shared by both
measurements) is limiting it equally for both servers, which is exactly
what "the race fix works" should look like in a benchmark this simple: the
new runtime machinery gets out of the way instead of becoming the ceiling.

## A second finding: timeouts and the carrier ceiling

The server actually benchmarked above (`apps/httpserver/main.src`)
deliberately does **not** call `set_read_timeout`/`set_write_timeout` on
its connections. That was not the first thing written -- the first version
did set both, to 30 seconds, matching `http.serve`'s own default
(`TIMEOUT_MS`/`deadline()`), because a server that cannot time out an idle
or hostile connection is a known bad default. That version **failed this
exact benchmark**, reproducibly, and the reason is worth recording because
nobody else had exercised sustained, concurrent, kept-alive HTTP traffic
against this runtime before tonight.

**What happened.** With the timeout set, `hey -c 20` (or higher) against
the default 12-carrier build reliably produced `context deadline exceeded`
errors and multi-second stalls -- not corruption, not a crash, just
requests that took seconds to answer instead of microseconds. `hey -c 10`
(at, not past, the carrier count) was always clean. The threshold tracked
the carrier count exactly: rebuilding with `LANG_NUM_CARRIERS=4` moved the
break point from "between 10 and 12" to "between 4 and 5." Two clean runs
of each, reproduced here for the record (`apps/httpserver`'s shipped binary
never has this problem; this table is the *other* version, built only to
demonstrate it):

```
hey -z 10s -c 10 http://127.0.0.1:PORT/     # <= carrier count (12 here)
hey -z 10s -c 20 http://127.0.0.1:PORT/     # > carrier count
```

| concurrency | run | requests/sec | errors | wall time |
|---:|---:|---:|---:|---:|
| 10 (<=12 carriers) | 1 | 35,955 | 0 | 10.0s (as asked) |
| 10 (<=12 carriers) | 2 | 35,002 | 0 | 10.0s (as asked) |
| 20 (>12 carriers) | 1 | 11,960 | 8 | 29.9s (hey's own retry/timeout stretched it) |
| 20 (>12 carriers) | 2 | 12,653 | 8 | 29.9s |

**Root cause, read directly out of `lib/net.src`'s own doc comments** (not
a guess -- the file says this plainly, around `Conn.wait_read_timeout`):
once Phase 3.5 made every socket non-blocking, the kernel's own
`SO_RCVTIMEO`/`SNDTIMEO` stopped being able to time anything out (a
non-blocking read never waits in the kernel at all). So an *unbounded*
wait after `EAGAIN` (`read_timeout_ms == 0`, the type's own default) goes
through `__wait_io`, which parks the green thread on the epoll/kqueue
reactor and **frees its carrier immediately** -- the cooperative, scalable
path tonight's fix is about. But a *bounded* wait (any
`set_read_timeout`/`set_write_timeout` greater than zero) goes through
`__poll` instead, which is a real, synchronous `poll(2)` **on the calling
carrier's own OS thread**. That call does not return until data arrives or
the timeout elapses, and for that whole span the carrier is unavailable to
the scheduler for anything else -- not another connection's request, not
even the accept loop's own next `accept()`. The blocking-FFI monitor that
exists for exactly this class of problem (`try_handoff`,
`runtime/scheduler.c`) does not help here: reading its code shows it
rescues a stuck carrier's *already-queued local backlog* by moving it to
other carriers, but it does not spin up a replacement OS thread to restore
lost scheduling capacity -- so once enough connections are simultaneously
idle-but-timed between requests to occupy every carrier in its own
`poll()` call at once, nothing is left to dispatch anybody else until one
of those calls happens to return.

This is **not** the race tonight's main session found and fixed (that one
was a data race, confirmed and closed under TSan; this is a scalability
ceiling in a documented, deliberate design trade-off, with no incorrect
behavior -- no wrong response, no corruption, no crash, just serialized
throughput once oversubscribed). It is also not specific to
`apps/httpserver`'s own code: `http.serve`'s own default
(`deadline()`/`TIMEOUT_MS = 30000`) sets exactly this kind of timeout on
every connection it accepts, so **any** server built the straightforward,
recommended way (`http.serve` plus a spawn-per-connection accept loop, once
that becomes possible some other way than a module-constant handler) would
hit the same ceiling today. Worth flagging to whoever owns
`lib/net.src`/`runtime/scheduler.c` next:

- The ceiling is exactly `n_carriers` simultaneously-idle, timeout-bearing
  connections, independent of how many green threads exist in total --
  verified by changing `LANG_NUM_CARRIERS` and watching the break point
  move with it.
- A fix would need either (a) a real "spin up a replacement OS thread while
  this one is blocked in `poll()`" rescue (the thing `try_handoff` is
  named like it should be but is not), or (b) teaching `wait_read_timeout`/
  `wait_write_timeout` to use `__wait_io` plus a software deadline (a timer
  green thread, or checking elapsed time against the reactor's own wake)
  instead of a real blocking `poll()`, so a bounded wait is exactly as
  carrier-cheap as an unbounded one.
- Until then, a server that wants both real concurrency *and* protection
  against a silent/slow peer needs a different mitigation than
  `set_read_timeout` -- for instance, a `max_requests` cap (already a
  `serve_conn` parameter) plus a watchdog green thread that closes a
  connection externally after some wall-clock budget, which never calls
  `__poll` at all.

`apps/httpserver`'s own choice (leave the timeout at its default, i.e. none)
is the right one for *this* app -- a benchmark server with no real adversary
-- and is exactly why the numbers in the first half of this file are real
concurrent numbers rather than numbers from a server quietly falling over
above 12 connections. It would not be the right default for a server
exposed to the public internet without one of the two fixes above or the
watchdog mitigation.

## Reproducing this

```
cd apps/httpserver
bash build.sh                                          # ./httpserver
(cd goserver && go build -o goserver main.go)           # ./goserver

go install github.com/rakyll/hey@latest                # if not already installed
                                                         # (needs Go >= 1.24 toolchain;
                                                         #  GOTOOLCHAIN=auto go install ... works
                                                         #  with an older local `go`)

./httpserver --port 19001 --host 127.0.0.1 &
hey -z 15s -c <1|10|50|100> http://127.0.0.1:19001/
kill %1

./goserver -port 19009 -host 127.0.0.1 &
hey -z 15s -c <1|10|50|100> http://127.0.0.1:19009/
kill %1
```

Raw `hey` output for all 16 runs behind the tables above is in
`apps/httpserver/bench_raw/*.txt` (named `<server>_c<concurrency>_run<N>.txt`).
