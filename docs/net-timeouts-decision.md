# Bounded waits: they held a carrier, and now they park

Written after investigating whether `lib/net.m31`'s read, write and accept
deadlines can be implemented through the reactor in language source alone.
**They could not**, and sections 1 to 3 are that investigation, kept as it was
(they describe the code *before* the change, in the past tense where they
say so). Section 4 is what was then built, with the user's approval of the
runtime (C) work: a deadline heap in the reactor and one primitive. Section 5
is what shipped, how it is tested, and what is measured before and after.

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

## 4. What was built

One primitive and one reactor feature, as proposed.

- `int rt_wait_io_timeout(int fd, int events, int timeout_ms)` (runtime/rt.c)
  returns 0 (ready), 1 (timed out) or `-errno` in the sys layer's numbering;
  `prim int __wait_io_timeout(int fd, int events, int timeout_ms)` in
  `lib/net.m31` and `lib/timer.m31`. Same wrapper trick as `rt_wait_io`: it
  undoes the compiler's `rt_enter_blocking` around the park and re-arms it
  after, or the blocking-FFI monitor races the carrier's local run queue.
- `rt_reactor_wait_timeout` (runtime/reactor.h): `timeout_ms < 0` is exactly
  `rt_reactor_wait`; `0` is a `poll(2)` that never parks; `> 0` parks only the
  green thread. `fd < 0` with no events waits on nothing, which is a sleep.
- `runtime/reactor_timers.h`, shared by both backends: an intrusive min-heap
  of deadlines. A bounded wait owns one node and **the node lives on the
  waiting green thread's own stack**, which is exactly as alive as the wait, so
  a timer costs no allocation and has nothing to free. Each node knows its
  heap slot, so a wait that ends by readiness is removed in O(log n) at once;
  with a 30 s timeout and thousands of short requests a second the heap
  would otherwise carry tens of thousands of dead timers.
- `runtime/reactor_epoll.c`: the reactor thread's `epoll_wait` timeout is the
  time to the nearest deadline, rounded *up* (down would wake just short and
  spin). A new earliest deadline writes an `eventfd` so the sleeping thread
  re-aims. `runtime/reactor_kqueue.c`: the same through `kevent`'s timeout and
  a second `EVFILT_USER` knote (ident 2; ident 1 stays shutdown).
- Then, in m31 only: `Conn.wait_read_timeout`, `wait_write_timeout`,
  `Listener.wait` and `Conn.wait` call `__wait_io_timeout`; the `__poll`
  declaration is gone from `lib/net.m31`. `http.serve`'s accept back-off is
  `timer.sleep_ms`. `__poll` itself stays (`lib/term.m31` uses it for a
  keyboard timeout, which waits on a terminal, not a socket).

### The race, and how it is closed

Readiness and the deadline are two *different* unparkers for one park. Both
run on the reactor thread and both **claim the wait under the one lock that
guards the waiter map**: whichever gets there first writes the node's outcome
(`READY` or `EXPIRED`), removes the node from the heap and releases the
registration; the second finds nothing to claim and does nothing. So exactly
one wakes the thread, with no second state machine. Consequences worth
stating:

- The waiter does not return until the outcome is decided. A *stale* unpark
  (`rt_sched_park` may return early, as reactor_kqueue.c documents) finds the
  node still `PENDING` and parks again.
- `rt_sched_unpark` is called **after** the lock is released: it can block on
  a full shared queue, and a carrier may be waiting for that lock to register
  its own timer. Calling it inside would be a deadlock. The price is a
  narrow window where a spuriously woken waiter returns before the real
  unpark and leaves one harmless stale notification, absorbed by the next
  park like any other.
- Expiry releases the fd's registration only if it is still *this* wait's
  (`w->node == n`); a later wait on the same fd must not lose its own.
- A timeout of 0 never parks and registers nothing, so it cannot leak.
- A bad fd is returned as `-errno` (the registration happens under the same
  lock hold as the heap insert, and is undone if it fails), not trapped as
  `rt_wait_io` does: for a caller with a deadline a descriptor that has gone
  is an ordinary event.
- A descriptor closed *while* parked produces no event on either backend, so
  the wait ends at its deadline. With no deadline it never ends, as before.

### Still true

**One waiter per fd** (reactor.h's stated limitation) is unchanged: a second
wait on a descriptor takes over its registration, and the first then ends at
its own deadline (or never, if it had none). This matters more than it did,
because `Listener.wait` used to be a `ppoll` that never touched the reactor
and is now a reactor wait: a green thread in `wait` and another in `accept`
on the same listener at once will interfere. One thread per listener or
connection is the supported shape. A bounded wait on a *regular file* is
refused by epoll (`-EPERM`).

## 5. What shipped, and the numbers

### The stall, before and after

`runtime/net_timeout_stall_demo/main.m31`, run on two carriers
(`LANG_NUM_CARRIERS=2`) by `runtime/net_timeout_stall_test.sh`: N silent
connections each with a 400 ms read timeout, then one live connection that
sends a line. The number is the time from the start of the case to the live
line being read; the case needs about 120 ms by itself (two 60 ms settling
pauses).

| silent clients | before (`__poll`) | after (`__wait_io_timeout`) |
|---|---|---|
| 2 | 521 ms (+401) | 121 ms (+1) |
| 16 | 3327 ms (+3207) | 122-124 ms (+3) |

"Before" is this commit's runtime with the old `lib/net.m31`. These are larger
than section 1's Python-client table (301 ms and 1501 ms) because here the
accept loop and this program's own main also wait behind the pinned carriers.
The silent connections themselves time out at 400-401 ms in both. In the
reactor-level test (runtime/phase3_test.c, `stall_case`) the ready waiter wakes
0.07-0.2 ms after the write with 2 or 16 silent waiters. The gates assert
`ideal + 150 ms`, which a loaded machine meets and the stall (400 ms and up)
cannot.

### Tests

- `runtime/phase3_test.c`: ready before the deadline; timeout with no data
  (elapsed within bounds); a late write finding the registration released;
  `0` (poll, on empty, readable, writable and nothing), negative, and sleep-only
  forever (`-EINVAL`); a closed fd (`-errno`, no leak); an fd closed while
  parked (ends by deadline); stray `rt_sched_unpark`s not ending a wait;
  150 trials of a write aimed at the deadline (every wait returns exactly once;
  both outcomes occur); **1000 concurrent timers on two carriers** (all fire, none
  early, heap empty) plus 200 fd waits half answered early; plain and bounded
  waiters sharing one reactor; the stall at 2 and 16. Every test ends with
  `rt_reactor_timers_pending() == 0`. Clean under TSan (`phase3_tsan.sh`, 5/5)
  and the ASan/UBSan build.
- `corpus/modules/stdlib-net-timeout-park` (150 silent connections beside a
  live one; data before the deadline; none; `wait(0)`, `wait(60)`, `wait(-1)`,
  on a connection and a listener) and `corpus/modules/stdlib-timer`.
  `runtime/parked_waits_carriers_test.sh` runs both on one and on two
  carriers against the corpus `.out`: on one carrier the old design could not
  have passed the first.

### `timer.sleep_ms(ms)`

The same wait with no descriptor (`__wait_io_timeout(-1, 0, ms)`), in a new
`timer` module (`lib/timer.m31`; `date` is calendar, and says a clock module
will come). It parks the green thread only. It sleeps at least `ms` (the
deadline is rounded up, never down), `sleep_ms(0)` returns at once without
parking and without yielding, and a negative argument traps. 1000 sleepers of
300 ms on two carriers are all awake in one interval, not five hundred.
No cancel and no sub-millisecond resolution; a thread that must stop waiting
early waits on a channel instead.

### Not done, and not run

- **`reactor_kqueue.c` was written and read carefully but never executed**:
  there is no macOS or BSD host here. It does compile as far as a type check
  against a stub `<sys/event.h>` can show, and it reuses the backend's
  existing seq-number protection (a late firing of an expired wait's knote
  finds no matching entry). The first CI run on macOS is its real test.
- No connect or accept deadline yet (possible now: `ready` on the connecting
  fd); not added, as it is a new API.
- `Conn.wait`/`Listener.wait` with a timeout used to count `POLLNVAL` as
  ready; now a closed descriptor is an error (`-EBADF` through `from_errno`).
