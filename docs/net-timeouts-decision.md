# Bounded waits: why they hold a carrier, and what it would take not to

Written after investigating whether `lib/net.m31`'s read, write and accept
deadlines can be implemented through the reactor in language source alone.
**They cannot.** This records the measurement, where the blocking happens, the
approaches that were tried on paper and why each fails, and the smallest
runtime change that would do it. Nothing was implemented, because every path
needs a runtime (C) change and the standard library is written only in m31.

## 1. The symptom, measured

A server that does `spawn handle(conn)` per connection, where `handle` calls
`conn.set_read_timeout(400)` and then `conn.read(16)`; clients are a separate
process (Python) so nothing in the measurement shares a scheduler with the
server. `k` silent connections are opened, then one good client sends a byte;
the number is how long the good client waits for its answer.

| carriers | k = 0 | 2 | 4 | 8 | 16 |
|---|---|---|---|---|---|
| 2 | 4 ms | 301 ms | 701 ms | 706 ms | 1501 ms |
| 4 | 4 ms | 0 ms | 300 ms | 300 ms | 1148 ms |

(`LANG_NUM_CARRIERS`; timeout 400 ms; the good client connects 100 ms after
the silent ones.) A good client waits as soon as the silent ones reach the
carrier count, and then in steps of about one timeout per `carriers` silent
clients: roughly `k * timeout / carriers`. A server with no timeout set does
not have the problem, because it parks on the reactor; it has a different one
(a silent client holds a green thread forever).

## 2. Where the blocking happens

- `lib/net.m31` `Conn.wait_read_timeout` / `wait_write_timeout`: with a
  timeout set they call `__poll(fds, events, revents, timeout_ms)`.
  With none set they call `__wait_io(fd, events)`, which parks.
- `rt_poll` (runtime/rt.c) is `sys_poll` -- a real `ppoll(2)` on the carrier's
  OS thread. The compiler wraps every `prim` call in
  `rt_enter_blocking()/rt_exit_blocking()`, so the blocking-FFI monitor sees
  the carrier as stuck after 20 ms and moves the green threads *queued behind*
  it to other carriers. That rescues the siblings; it does nothing for the
  carrier itself, which is gone for the whole timeout. Once `carriers`
  green threads are in `ppoll`, nothing can run.
- `Listener.wait` and `Conn.wait` (explicit `wait(timeout_ms)`) are the same
  `__poll`, by design: they are the "ask the kernel" calls.
- `http.serve`'s accept back-off (`back_off`, new) is `poll` with an empty
  table. It holds a carrier for 10 ms, only on the failure path.
- Hazard worth knowing: a *non-blocking* client program that waits with
  `Conn.wait` or a read timeout while the thing it waits for is a green thread
  on the same carrier set can starve it. `corpus/modules/stdlib-net-accept`
  avoids timeouts for exactly this reason and still passes with
  `LANG_NUM_CARRIERS=1`.

## 3. Approaches that stay inside m31, and why none works

The language has `spawn`, `Chan` (`send`/`recv` park), and the `prim`s in
`lib/*.m31`. It has **no sleep that parks, no timer, no `select`, no yield, no
way to wait on two things** (docs/concurrency-decision.md lists `select` as
deliberately deferred, and notes watcher threads leak).

1. **`__wait_io` with a deadline.** It takes `(fd, events)` and returns 0 when
   ready; there is no third argument and no way to wake it other than the fd
   becoming ready.
2. **A watchdog green thread that wakes the waiter.** The only wake-up the
   parked thread understands is readiness of *its* fd. Waking it means making
   that fd readable -- `shutdown(SHUT_RD)` -- which destroys the connection,
   and a timeout here is documented as *retryable* ("a read that fails has
   consumed nothing"). Writing into the peer's side corrupts the stream. A
   second fd to wait on needs a multi-fd wait, which does not exist.
3. **A single timer thread** sleeping in `poll` for the nearest deadline and
   woken through a self-made socket pair. It holds one carrier forever (so a
   one-carrier runtime deadlocks), and it still has no way to wake the waiter
   without approach 2's problem.
4. **Spin: `__poll(..., 0)` in a loop with a yield.** There is no yield. Parking
   on an always-ready descriptor as a yield would work mechanically, and burns
   a reactor round trip per spin per silent client: it trades a blocked
   carrier for a busy one. Not done; it is a hack and it makes every idle
   connection cost CPU.
5. **Channel with timeout.** `recv` has none.

## 4. The minimal runtime change

One new primitive and one reactor feature:

- `int rt_wait_io_timeout(int fd, int events, int timeout_ms)` returning 0
  (ready), 1 (timed out) or `-errno`; `prim int __wait_io_timeout(...)` in
  `lib/net.m31`. Same wrapper trick as `rt_wait_io`: undo the compiler's
  `rt_enter_blocking` on entry and re-arm before returning, or the monitor
  races the carrier's local run queue (the bug rt.c documents at length).
- In `runtime/reactor_epoll.c`: a min-heap of `(deadline, green_id,
  generation)` guarded by the same lock as the waiter map; the reactor
  thread's `epoll_wait` timeout becomes the time to the nearest deadline; on
  expiry it claims the waiter (clearing it from the map and disarming the
  ONESHOT registration) and `rt_sched_unpark`s the thread with a timed-out
  flag. `runtime/reactor_kqueue.c`: `kevent`'s timeout argument for the same
  loop, or `EVFILT_TIMER`. The header contract in `reactor.h` grows one
  function; neither backend touches `scheduler.c`.
- Then, in m31 only and small: `wait_read_timeout`, `wait_write_timeout`,
  `Listener.wait`, `Conn.wait` call it instead of `__poll`; an accept
  deadline becomes possible; and the same machinery yields a parking `sleep`
  (`rt_wait_io_timeout` on no fd), which `http.serve`'s back-off wants.

### Risk

- **The readiness/timeout race** is the whole difficulty: both are claimed
  under one lock and exactly one may unpark. The existing park/unpark CAS
  already tolerates an unpark before, during or after the park; what is new is
  two *different* unparkers for one park.
- **A stale unpark.** `reactor_kqueue.c` already documents that a fired event
  can call `rt_sched_unpark` for a green id that has moved on to something
  unrelated. A timer that outlives its wait is the same hazard; the generation
  field above exists for it.
- **One waiter per fd** (reactor.h's stated limitation) is unchanged; a
  timeout must not clear a registration a *later* wait on the same fd made.
- Both backends, the TSan lanes (`phase3_tsan.sh`) and the Phase 3 tests need
  new cases. Estimate: a day or two in C plus tests; the m31 side is an hour.

## 5. What this changes today

Nothing in behaviour. The practical guidance, until the primitive exists:
for a server that must tolerate silent clients, prefer *no* per-read timeout
plus an application-level idle policy (close a connection that has not
completed a request after N seconds, driven by something that is not itself a
blocking poll), and size `LANG_NUM_CARRIERS` above the number of silent
clients you are willing to carry. `http.serve` (one connection at a time)
keeps its `timeout_ms` argument: there the held carrier is the one doing the
serving anyway.
