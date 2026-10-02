# Concurrency: the decision

Decided **2026-09-19**. Supersedes the open question in `docs/concurrency.md`,
which keeps the evidence and the rejected alternatives.

---

## The decision, in one place

| | |
|---|---|
| **Model** | Stackful green threads, M:N over a carrier thread per core |
| **Ergonomics** | Uncoloured. One kind of function. `read_file(p)` works anywhere. |
| **Sharing** | **Moved, never shared.** A value crossing between threads is given up by the sender. |
| **Refcounts** | **Plain, non-atomic.** Guaranteed sound by the move rule, not by hope. |
| **Communication** | Channels, buffered and unbuffered |
| **Stacks** | Fixed size, **no guard pages** — compiler-emitted stack probes instead |
| **Preemption** | Cooperative, via the same probe |
| **Target scale** | Millions of green threads, not tens of thousands |

The reference points are **Go and Java's Project Loom**, not Node. Loom and Go
have no function colouring; Node does, and that difference is the whole point.

---

## Why uncoloured, and what it rules out

The question that decides everything else is: *can a function stop in the
middle and resume later?*

- **Yes, invisibly** → green threads. One kind of function.
- **Yes, but visibly** → `async`/`await`, stackless state machines. Two kinds
  of function, and the distinction is contagious.
- **No** → actors, run-to-completion.

We choose the first. The second is rejected outright, and the reason is not
taste:

> **Colouring forks the standard library permanently.**

The moment `read_line_of` can suspend, it is coloured, and so is every function
that transitively calls it. There is no uncoloured `sort` that takes a
comparator which might do IO. The stdlib splits in two and never rejoins —
which is the complaint Nim users have, structurally, and the reason JavaScript
went callbacks → promises → `async/await` and is *still* coloured.

For a language whose stdlib we intend to write in itself, that is
disqualifying.

Actors (run-to-completion) were the other serious candidate and are rejected
for fit rather than principle. They are excellent when the problem is
event-shaped and awkward when it is *"do A, then with the result do B, then
C"* — because each `then` becomes a separate message handler with state
carried by hand. Every workload named for this language is the second shape:
read a file then parse it, download parts then combine then write, read a
request then query then respond. See `docs/concurrency.md` §4–5 for the worked
comparison.

---

## Moved, not shared

A value that crosses between concurrent units is **moved**: the sender gives
it up and cannot touch it afterwards. Nothing is aliased across threads.

This is not an ergonomic preference. It is what keeps refcounts cheap:

| | cost |
|---|---|
| plain increment | ~2 ns |
| uncontended atomic | ~7 ns |
| **contended atomic** | **110–180 ns** |

And whole-program, measured in Swift: refcount operations are **32% of
execution time**, of which **atomicity alone accounts for 25%** — paid to
protect the <7% of objects that ever leave one thread.

Because only one thread can reach a value at a time, `rc_inc`/`rc_dec` stay
plain increments. Inko and Pony reach non-atomic refcounting the same way, and
both have real parallelism.

**The negative result worth knowing:** Swift proves a `Sendable`-style
*isolation* system does **not** buy this. SE-0430 states plainly that it "does
not change how any existing code is compiled" — isolation proves
non-concurrent *access*, not non-*migration*. What is needed is **ownership**
(Inko's `uni`, Nim's `Isolated[T]`, Pony's `iso`). That requirement lands in
the type system, and is the reason the type system is the next piece of work.

---

## Taking a payload out of a `match`

Decided **2026-09-23**.

A server accepting connections and handing them to workers is *the* motivating
program for this design, and until this was settled it had no obvious
spelling. The natural one,

```
match (ln.accept()) {
    case Ok(net.Conn s): { send(conns, s); }
    case Err(net.Error e): { trap("accept"); }
}
```

was refused, because a `case` binding reads as **borrowed**: the `match` holds
the enum for the whole arm and the payload's `+1` belongs to the enum. The
diagnostic then advised `clone(s)` — which `net.Conn`, `net.Listener` and
`io.File` all refuse outright, because a type that owns a resource cannot be
copied (`docs/destructors-decision.md`). So the advice was always wrong for
exactly the types a server moves. Writing an alias first (`net.Conn t = s;`)
compiled and then *trapped*, because the binding still held a reference. The
only spelling that worked was laundering the value through a list.

**A `case V(T x):` binding may now cross a thread boundary.** The move is not
a retain: it **takes the payload out of the enum**. The slot is cleared
(`ir::Inst::EnumTake`), the `+1` the enum held goes to the receiver, and the
enum's release skips the slot it no longer owns. The binding is the arm's own
name for that payload and nothing else names it, so after the take the value
is genuinely unaliased — which is the whole of the move rule.

Two conditions, and both are load-bearing:

1. **The `match` must be the enum's only owner.** Statically, the scrutinee
   must be a *temporary* — the value of an expression, not a name — so no name
   outside the match reaches it. That alone is not proof (a method may return
   a retained reference to something it keeps), so `rt_check_unique` runs on
   the **enum**, not the payload: the whole graph reachable from it has to be
   private, and the payload is in that graph, so one check answers both
   questions. Matching a named local instead is refused at compile time, and
   the message says to match the call directly.
2. **The move must happen in the arm's own block.** Same reason `mark_moved`
   refuses a local declared outside the current block: the move set is one
   flat set of names with no control-flow graph, so a move inside a nested
   loop or branch would run a different number of times than it was checked,
   and the second run would take a slot that is already empty. The arm's own
   body is fine however many times the whole `match` runs — each time round it
   matches a fresh enum, which is exactly the accept loop above.

What is still refused, and honestly: a parameter, a field or `this` of a
resource-owning type. Those are held by somebody else, and no local alias
changes that — the alias is a second reference to the same resource. There is
no spelling, and the diagnostic no longer pretends there is one: it says a
resource can only be moved on by whoever made it, and names the `case`
binding as the way to be that owner.

## Channels

A channel is not a thread. A thread is a worker; a channel is the pipe workers
hand values across.

```c
Chan c = chan_new();
spawn worker(c);
str result = recv(c);   // parks until something arrives
```

`recv` on an empty channel parks the green thread; `send` on a full one parks
until there is room. So a channel carries both data and timing — it is how one
thread waits for another without any user-written locking.

**Unbuffered** is a handoff: the sender waits until a receiver takes it, which
synchronises the two. **Buffered** holds N and only blocks when full.

For us the channel is also the place the move rule is enforced: `send`
relinquishes ownership, `recv` acquires it. Go offers "share memory by
communicating" as advice; here it is a requirement, because the alternative
costs 25% of runtime.

---

## Stacks: fixed, but not limited

Copying stacks are impossible for us and always will be — relocating a stack
means rewriting every pointer into it, which needs a per-word pointer map the
C compiler will never emit. Go gets one from its own compiler; we get nothing.
So stacks are **fixed size, chosen at spawn**.

That is a real constraint. The *thread ceiling* it seemed to imply is not.

The obvious implementation — one `mmap` plus one `PROT_NONE` guard page per
stack — costs **2 VMAs per green thread**, against a `vm.max_map_count` that
defaults to 65530. That caps you at **~32,000 threads**, which is
unacceptable.

**So we do not use guard pages.** The guard page exists only to *detect*
overflow, and we can detect it better: the compiler emits a stack check at
function entry. Stacks then pack densely into slabs:

```
64 MB slab = 1024 × 64 KB stacks = 1 VMA      (not 2048)
```

A million green threads is then ~1000 VMAs. The ceiling disappears.

CHICKEN has shipped exactly this for twenty-five years — its generated C
carries `if(!C_stack_probe(&a))` at every function entry, using a local's
address as a stack-pointer proxy. Gambit's version does better still: **one
compare-and-branch serves as both the overflow check and the preemption
point**, by poisoning the limit when a thread should yield.

So the probe buys two things for roughly three instructions per call:
unbounded thread counts, and preemption we would otherwise need a separate
mechanism for. **No library runtime can do this; it requires owning codegen.**

That is also why signal-based preemption is rejected: every precondition Go
checks for it is per-PC compiler metadata we cannot emit from C. Cooperative
preemption at the probe is both cheaper and available.

---

## Scheduler queues: no stealing, one shared entry point, a tuned batch size

Decided **2026-09-30**, superseding the 2026-09-29 design below in full.
That design was never implemented; it was tested first, in simulation,
against synthetic load shaped like this language's own stated target
workloads (a server accepting connections and handing them to workers is
*the* motivating program — see "Taking a payload out of a `match`" above).
The simulation found the original design's load-balancing was being done
entirely by work-stealing — a mechanism borrowed from Go and Cilk's own
lineage, not something this design contributed — and that removing
stealing (a deliberate choice, to find out what this design's own
mechanism actually buys on its own) exposed a severe, structural failure
that a different fix, not stealing, turned out to solve.

**State: a byte per green thread, indexed by id, never scanned.** Unchanged
from the original reasoning: an enum state (`Runnable`, `Running`,
`Parked-on-I/O`, `Parked-on-channel`, `Parked-on-timer`, `Dead`) read and
written in O(1) by id, never scanned to find work.

**Sibling-to-sibling work-stealing is removed. Not tuned down — gone.** No
carrier may ever reach into another carrier's queue, under any
circumstances, batch or single-item. This is a firm design decision, not a
temporary simplification.

**Every spawn — local or external in origin — routes through one shared
global queue.** This reverses "each carrier owns its own run queue, spawns
land there directly." The reversal is load-bearing, not cosmetic. Once
stealing was removed, local-first spawn routing meant a coordinator fanning
out N tasks — the single most common concurrency pattern here, an accept
loop handing connections to workers — had all N tasks stuck on the
spawning carrier permanently, because nothing else could ever reach in to
help. Measured: throughput completely flat regardless of core count (4 vs.
64 cores gave zero difference). This held even when the spawned tasks later
yielded for I/O: a *resumed* task gets redistributed fine through the
global path, but its *first* dispatch is still gated on the same
bottleneck, so yielding rescues fairness, never fan-out throughput.
Routing every spawn through the shared queue instead recovered near-linear
core-count scaling on the worst-hit synthetic benchmarks, matching a real
work-stealing reference (Go) within single digits of a percent on most
load shapes, and beating it on some.

**The cost on ordinary, non-fan-out load is real but small** — a low
single-digit percent throughput tax on naturally-distributed bursty load,
and a load-balance cost when many tasks land at the same instant. Both
traced to the claim batch size below, not to the routing decision itself,
and both closed by shrinking that batch size.

**Carriers still keep a small local buffer — a claimed batch of work, not
a landing spot for new spawns.** A carrier that runs dry draws another
batch from the shared queue.

**The claim/batch size is the one parameter that actually matters. Call it
`fuel_size`**: how much work a carrier draws from the shared queue each
time it runs low, the same idea as topping off a tank rather than idling
on fumes. Every draw takes `min(available, fuel_size)`, with **no minimum
threshold** — even a single item is claimed immediately rather than
waiting for a full batch to accumulate, which is what fixes a real
starvation bug: a lone task arriving during otherwise-idle time must not
wait for company that may never come. Smaller `fuel_size` won on
throughput, load-balance, and latency simultaneously in every load shape
tested, no tradeoff found in the tested range — the mechanism is
monopolization, not per-claim overhead: a large batch lets whichever
carrier draws first vacuum most of the remaining backlog once the batch
size exceeds roughly `remaining backlog ÷ idle carriers`, starving every
other carrier that was also about to look. **Caveat that must be resolved
before picking a production default**: the simulation that found this
charges zero cost per draw regardless of size, so it cannot see the real
downside of tiny batches — an actual draw is a genuinely nonzero
synchronized operation (an uncontended atomic is ~7ns; a contended one is
110–180ns, the same cost cited above for refcounting). There is almost
certainly a real floor below which shrinking `fuel_size` stops helping and
starts hurting; the simulated result is a strong signal to tune from, not
a validated number to ship.

**The shared queue's own capacity does not matter, once it is at least
`fuel_size`.** Proven mathematically, not just measured: a draw is capped
at `fuel_size` independent of the queue's total capacity, and anything that
overflowed is re-admitted the instant any draw frees room — so the visible
queue plus the overflow queue always equal the true backlog, and capacity
only moves the line between "visible" and "in overflow," never what a draw
can take or when the next one happens. Confirmed across every load shape
tested, including the one that actually matters for a deployed system: a
live horizontal-scaling event, new carriers joining mid-run after a
realistic autoscaler-reaction lag. Even when an undersized queue genuinely
engages backpressure during that lag, recovery afterward was
indistinguishable from a generously-sized one — undersizing cost zero real
lost or delayed work. Pick any capacity `>= fuel_size`; it is a
monitoring/signaling knob, not a functional one.

**Both `fuel_size` and the shared queue's capacity must be
runtime-configurable, not compiled-in constants.** No single `fuel_size`
value won across every load shape tested, its real floor is unknown per
the caveat above, and different deployments will reasonably want different
backpressure-signaling capacities even though outcomes don't depend on the
choice.

**Mechanism: environment variables, read once at process startup** — the
same approach Go uses for `GOMAXPROCS`, which this whole document already
takes as its reference point. `LANG_FUEL_SIZE` and `LANG_GLOBAL_QUEUE_CAP`
(names open to bikeshedding later), parsed into runtime globals during
startup, falling back to a built-in default if unset or invalid. This
serves both purposes at once with no extra mechanism: the project's own
tuning against real hardware once Phase 2 exists, and an end user or
operator tuning for their own machine or workload without a rebuild.
Defaults: `fuel_size` starts at the simulated best value (4), explicitly
provisional pending the real-hardware tuning Phase 2 still owes (see
above); the queue capacity default can be anything `>= fuel_size` since
outcomes don't depend on it — pick something that reads sensibly for
monitoring (64 is as good as any).

**Resolved 2026-09-30: an idle carrier learns something landed in the
shared queue via a reshuffled random permutation, not a fixed order.**
The obvious candidate — notify the carrier that spawned a task, if it's
idle — is moot under all-spawns-to-the-shared-queue routing, since there
is usually no local spawn to react to. Simulation went through three
attempts before landing on a safe one, and the failures are as informative
as the fix:

- **Wake the spawning carrier only** (`wake_owner`): provides *zero* active
  notification for shared-queue arrivals — there is no owner to notify —
  so it degenerates to relying entirely on a periodic idle-retry check.
  Safe, but leaves real fairness on the table on ordinary traffic.
- **A persistent circular pointer**, advanced by one on every arrival,
  skipping non-idle carriers: fixed the fairness gap on ordinary traffic,
  but failed badly and deterministically on a workload shaped like
  sustained, per-task-cost-skewed but never queue-depth-skewed load, via
  two separate, compounding bugs. **Bug one**: the fixed, predictable
  sequence phase-locked onto whichever carrier it happened to be pointing
  at whenever that workload's periodic arrivals recurred. **Bug two**: a
  "who is actually idle" bookkeeping gap at startup — a carrier was marked
  idle only *after* it tried and failed to find work, so at t=0, when
  every carrier genuinely *is* idle, none of them were marked as such yet,
  and the wake mechanism found nobody eligible until something else
  (a periodic fallback check) eventually intervened — and when several
  carriers' fallback checks landed on the same instant, a **fixed
  tie-break order** among them handed the same carrier the first claim on
  every run, every time. Not bad luck — the flag didn't reflect reality,
  and the fallback path that covered for it had its own hidden bias.
- **The fix has two independent parts, confirmed separately, not
  bundled.** For bug one: a reshuffled random permutation — walk every
  carrier exactly once per round in a random order, reshuffling fresh each
  time the round completes. The completeness guarantee (everyone visited
  once per round — nobody starved) survives; the fixed, guessable sequence
  that let the lock-on happen does not, because no correlation formed in
  one round can persist into the next round's different order. For bug
  two, the direct, root-cause fix is simpler than it first looked:
  **initialize every carrier's idle/sleeping state to true at startup**,
  reflecting what is actually true (everyone genuinely is idle before
  anything has run), instead of leaving it false until a carrier
  discovers its own idleness the hard way. Verified directly: a diagnostic
  counting exactly how often this startup gap is hit went from `=
  num_carriers` (every single carrier, every run) to exactly `0` once this
  was fixed. Randomizing the fallback path's own tie-break order also
  works as a one-step-removed workaround for bug two and was tried first,
  but it treats the symptom; fixing the initial state directly is the
  smaller, more correct change and makes the tie-break randomization
  unnecessary as a load-bearing fix (cheap to keep anyway, as
  defense-in-depth, since it costs nothing).

This is not a novel technique — it is the same principle real production
schedulers already use at the analogous decision point. Go picks randomly,
every time, when a P looks for a stealable victim or checks its shared
queue, specifically to avoid a fixed order ever favouring the same core.
Erlang/BEAM avoids the race a different way, by assigning new work
round-robin up front rather than having schedulers race to claim from one
shared queue, and correcting imbalance only via a slow periodic rebalance
rather than a reactive one. The bug fixed here was a simulation forgetting
to randomize at exactly the spot Go already knows to.

**One honestly-disclosed, shared limitation, not unique to this design**: a
workload where per-task cost varies dramatically without a corresponding
change in queue length defeats *any* length-based scheduling signal — this
design's, and, confirmed directly, Go's and Erlang's identically. No fix
for this exists in any design tested; it is a real, open gap.

<details>
<summary>Superseded 2026-09-29 reasoning (per-carrier arrays, work-stealing) — kept for the record</summary>

Each carrier — the OS thread, one per core — was to own its own run queue,
a fixed-size array ring buffer (Go's own is 256 slots), not a linked list:
contiguous memory is cache-friendly where a linked list's scattered nodes
are not, there is no per-node allocation, and stealing a chunk of another
carrier's queue is a couple of compare-and-swaps on array indices, where a
linked list would need a lock or a considerably harder lock-free algorithm
to do the same thing safely.

One shared queue, with every carrier pulling from it, was Go's own first
design (pre-1.1, 2012) and it did not scale: one lock, every core fighting
over the same cache line, contention that gets worse as cores are added.
Go 1.1 (2013) replaced it with per-carrier local queues and work-stealing.
Java's `ForkJoinPool` (2011) landed on the identical shape independently.

Stealing was to take a batch from the opposite end of the queue, not one
item at a time, and a small global queue was to remain as a rare-case
fallback (overflow when a local queue is full), not the primary path.

A carrier-sized bitmap (one bit per core, not per green thread) was the
proposed mechanism for a thief to find a stealable sibling — matching the
Linux O(1) scheduler's pre-CFS technique, and deliberately avoiding
`select()`'s mistake of scanning cost proportional to *total* entries
rather than *ready* entries.

This entire mechanism is removed per the decision above. Kept here because
the underlying reasoning about arrays-vs-linked-lists and about scan cost
scaling with population size is still correct in general — it was the
per-carrier-ownership and stealing-as-load-balancer parts that didn't
survive contact with simulation, not the data-structure reasoning.

</details>

---

## Blocking FFI

`O_NONBLOCK` is a no-op on regular files, `getaddrinfo` is synchronous by
specification, and any foreign call might block. The answer is Go's cgo model,
and again it needs codegen:

- the compiler wraps every FFI call site in `enter_blocking()` /
  `exit_blocking()`
- a monitor thread notices a carrier stuck in a syscall and hands its queue to
  a backup thread
- unannotated foreign calls from a green thread produce a **compile-time
  warning**

Erlang requires manual annotation; Java's Loom explicitly refuses to
compensate for native-frame pinning; async-std tried automatic detection
without compiler support and it never merged. We can do better than all three
because we control the call site.

---

## What this costs

Roughly **35 engineer-weeks** for Linux and macOS on x86-64 and arm64; a
Linux-x86-64 proof of concept is 6–8 weeks. The two line items most likely to
overrun are not the obvious ones:

- the **park/unpark ↔ netpoll CAS state machine** (~3 weeks) — closes the
  lost-wakeup race where data arrives between `EAGAIN` and parking
- **TLS-caching discipline in codegen** (~2 weeks) — a compiler may cache a
  thread-local's address in a callee-saved register across a yield, and after
  migration that address belongs to another thread. Silent corruption, not a
  crash. Fixed by making every yield point an opaque barrier; this is why Go
  reloads `g` from TLS after any call that can switch stacks.

**Not waiting for Cranelift.** Its stack maps are GC-reference maps for
relocating heap objects, not per-word frame maps for relocating the stack — so
even there we would not get copying stacks, and being refcounted we do not
need precise roots. Nothing in this design is backend-specific; Go shipped all
of it in C in 2012.

---

## Progress

- [x] **Type system with ownership** — done
- [x] **OS threads + channels** — done: `spawn`, `Chan<T>`, `send`/`recv`/
      `close`, and the move checker. A slot is 64 bits and the element type
      is known statically, so an int rides in the slot and a reference rides
      as its pointer.
- [x] **Scheduler design** — decided and simulation-validated above
      (2026-09-30): no stealing, single shared queue, self-service draw with
      no minimum threshold, `fuel_size` tuning, configurable capacity, and
      the wake/notification mechanism (random permutation + correct initial
      idle state).
- [x] **Context switch, slab stacks with probes** — Phase 1, implemented and
      tested (`runtime/greenthread.{h,c}`, `runtime/ctx_switch_x86_64.s`).
      x86-64 only; aarch64 is Phase 4.
- [x] **Scheduler implementation, probe-based preemption** — Phase 2
      (`runtime/scheduler.{h,c}`).
- [x] **epoll reactor, park/unpark** — Phase 3 (`runtime/reactor.{h,c}`,
      park/unpark surface added to `runtime/scheduler.{h,c}`). The 3-state
      textbook CAS design (EMPTY/PARKED/NOTIFIED) was tried and found to
      have a real, reproducible lost-wakeup-adjacent bug — it can publish
      "externally resumable" before a context switch has actually finished
      saving state, letting an unparking carrier switch into a context mid-
      save (two OS threads on one stack). Fixed with a 4th state (ARMED)
      marking "decided to park" distinctly from "provably resumable."
- [x] **Blocking-FFI handoff** — Phase 3, `enter_blocking`/`exit_blocking`
      wrapped around every `prim` call site by the compiler, a monitor
      thread handing a stuck carrier's local buffer to a backup via the
      existing self-service draw path. The compile-time warning for an
      unannotated foreign call is a known-incomplete approximation (doesn't
      follow interface/virtual dispatch) — documented, not overclaimed.
- [x] **`spawn`/`Chan`/`net` wired to the real scheduler — Phase 3.5,
      wiring done, the critical race FIXED.** `spawn` always means a green
      thread now, unconditionally, and the wiring itself (Chan's
      multi-waiter park/unpark, net.src's non-blocking-plus-reactor
      conversion) is complete and correct in isolation. Building and
      stress-testing it surfaced a real, TSan-confirmed data race in the
      PRE-EXISTING Phase 1/2 fiber-switching mechanism (`rt_stack_limit`),
      reliably reproducible under genuine concurrent socket I/O across 2+
      real carriers; after two inconclusive investigation sessions, a third
      found the actual root cause (a compiler TLS-address-caching issue, not
      a scheduler or context-switch bug) and fixed it — see "Phase 3.5"
      below for the full account, the fix (`rt_stack_check`, `runtime/
      rt.h`/`rt.c`/`src/emit_c.rs`), and its verification. A SEPARATE real
      deadlock found along the way (`rt_sched_spawn`'s backpressure, below)
      is also fully fixed. Two smaller, separate races found by the same
      TSan runs — `try_handoff`/`carrier_main`'s unsynchronized touch of a
      carrier's own local buffer, and a shutdown-ordering race in
      `rt_sched_destroy` — are now ALSO both fixed; see "Phase 3.5" below
      for both accounts in full.

**Known gap, carried forward rather than fixed in Phase 3 — now CLOSED.**
ThreadSanitizer itself used to intermittently segfault (never a real race
report, never in this project's code) when a green thread was suspended by
one carrier and resumed by a different one under TSan specifically — the
same class of gap already noted for ASan's fiber-switching above, just a
hard crash here instead of a warning. Previously mitigated by narrowing the
two affected tests (`phase3_test.c`'s `test_park_unpark_race` and
`test_reactor_real_pipe_wakeup`) to 1 carrier under TSan only.

**The fix**: ThreadSanitizer's own documented fiber-switch interface --
`__tsan_create_fiber`/`__tsan_destroy_fiber` (called once per green-thread
stack lease, in `runtime/greenthread.c`'s `rt_stack_alloc`/`rt_stack_free`)
and `__tsan_switch_to_fiber` (called immediately before every
`rt_fiber_switch` in `runtime/scheduler.c` -- `carrier_dispatch`,
`rt_sched_yield`, `rt_sched_park`, `green_trampoline` -- naming whichever
fiber identity, the green thread's own or the carrier's native one,
execution is about to switch to) -- tells TSan explicitly what it was never
being told before: that control is now a different logical thread of
execution, on a different stack. All four symbols are declared behind the
same portable `__SANITIZE_THREAD__`/`__has_feature(thread_sanitizer)` check
this file's tests already use (see `runtime/greenthread.h`'s "A note on
ThreadSanitizer" comment for the full mechanism), so every one of them --
declarations, the `rt_stack_t` field holding each lease's fiber handle, and
every call site -- compiles to nothing outside a TSan build. Deliberately
NOT implemented in `ctx_switch_x86_64.s`: the annotations bracket
`rt_fiber_switch` from the C side, so the hand-written asm context switch
itself (`rt_ctx_switch`) is untouched, consistent with this project's
standing rule about not touching that file half-confidently.

**Verified**: `runtime/phase3_tsan.sh`, run at the ORIGINAL, unnarrowed
carrier counts (4 for `test_park_unpark_race`, 2 for
`test_reactor_real_pipe_wakeup`) -- 110 consecutive clean runs across two
batches (60 + 50), zero TSan-internal crashes, zero new false-positive race
reports, 4,950 checks total, 0 failures. Confirmed directly beforehand that
the pre-fix code still reproduces the gap at these same unnarrowed counts
(the un-narrowed binary hung inside a TSan-instrumented run, killed after
~5 minutes with no completion -- consistent with the previously-documented
SEGV-inside-TSan's-own-runtime signature, just manifesting as a hang rather
than a clean crash on this host/compiler-rt combination). `runtime/
scheduler_tsan.sh` (never narrowed in the first place -- its own tests
never hit this gap): 60/60 clean runs post-fix, 120 checks each, 0
failures, confirming the new thread-local fiber-identity bookkeeping in
`scheduler.c` introduces no regression there. `runtime/
spawn_wiring_tsan.sh`'s Chan tests (`corpus/core/1303`/`1304`) keep their
own, separate `LANG_NUM_CARRIERS=1` mitigation for a gap of their own
(documented in that script) -- out of scope for this fix and deliberately
left as-is; its `net_concurrent_clients` report case was re-built and
re-run in isolation (43 runs) and showed only the already-documented,
unrelated `try_handoff`/`c->local_len` race or a clean exit, exactly as
expected -- no new failure mode introduced.

Ordinary (non-TSan) builds are unaffected by construction, not just by
testing: every line this fix adds is behind `#if` on the same
TSan-detection macro, so a plain or ASan+UBSan build preprocesses all of it
away entirely. Confirmed directly: `runtime/rt.c` and `runtime/scheduler.c`
both still compile warning-free under `gcc -O2 -Wall -Wextra`, and the full
Phase 1/2/3 standalone suites (`runtime/greenthread_test.sh`, `runtime/
scheduler_test.sh`, `runtime/phase3_test.sh` -- every compiler/opt
combination each already covers, plus their own ASan+UBSan builds) pass
clean, exercising the exact modified functions (`rt_stack_alloc`/`free`,
`rt_fiber_switch`'s callers) under every one of those configurations.

**Known v0 simplification: channels are immortal.** A channel must be
reachable from several threads at once, so it is exempt from the move rule --
and that exemption is exactly what would make its own refcount race. Rather
than make one refcount atomic ahead of the general answer, a channel is never
freed. A program creates few, so the leak is bounded by that count.

## Phases

**Phase 0 — foundations. Done.**
- Type system with ownership
- OS threads + channels (`spawn`, `Chan<T>`, move checker)

**Phase 1 — runtime primitives. Done.**
- Context switch (asm, x86-64 first, then aarch64)
- Slab stack allocator with compiler-emitted probes (also the preemption
  point — see "Stacks: fixed, but not limited" above)
- Byte-per-thread state table

**Phase 2 — the scheduler itself. Done.**
- Per-carrier local buffers (a claimed batch of work, not a spawn landing
  spot)
- Single shared queue; every spawn — local or external — routes through it
- Self-service draw, no minimum threshold, `fuel_size` as a configurable
  batch cap
- Shared queue capacity as a separate configurable parameter (functionally
  inert above `fuel_size`, kept for backpressure signaling)
- No stealing anywhere
- Wake targeting: a reshuffled random permutation over carriers, not a
  fixed sequence, with every carrier's idle state initialized true at
  startup — resolved, see "Scheduler queues" above for the two failure
  modes this fixes and why. No longer blocking.
- **Still blocking on this phase, needs empirical tuning against real
  hardware, not simulation**: a production default for `fuel_size`, given
  the simulation that found "smaller wins" charges zero cost per draw and
  so cannot see where the real floor is. The algorithmic design above is
  otherwise ready to implement against.

**Phase 3 — I/O integration. Done**, see Progress above for the one
TSan gap this phase carried forward (now closed -- see "Known gap, carried
forward rather than fixed in Phase 3 — now CLOSED").
- epoll reactor
- park/unpark, closing the lost-wakeup race against netpoll (the CAS state
  machine flagged under "What this costs" above)
- Blocking-FFI handoff (`enter_blocking`/`exit_blocking`, monitor thread,
  compile-time warning on unannotated foreign calls)

**Phase 3.5 — wiring `spawn`/`Chan`/`net` to the real scheduler. Wiring
done; the one critical bug found along the way is now FIXED — see the end
of this section for the root cause, the fix, and its verification.**
Phases 1-3 built the whole runtime standalone, exercised only by hand-written
C test harnesses; this phase is what makes it real from a compiled `.src`
program. `spawn` now always means a green thread (`rt_sched_spawn` on a
process-wide scheduler, `runtime/rt.c`'s `rt_global_scheduler`) — never
conditionally an OS thread, which would have reintroduced the uncoloured
design's whole reason for existing. The program's own top level runs as
green thread 0 (`rt_run_program`), not on the raw OS thread, so a
`Chan`/`net` call made before any `spawn` has something to park against.

- `Chan.send`/`recv` park instead of blocking their carrier
  (`rt_sched_park`/`rt_sched_unpark`), with a FIFO linked-list queue per
  channel for however many green threads are waiting to send or to receive
  at once (plural matters — a condvar's wait queue did this for free under
  OS-thread `spawn`; parking needs its own). `close` wakes every waiter on
  both queues, not just one.
- `lib/net.src`'s sockets are non-blocking from the moment they are made
  (`SO_NONBLOCK`), and every wait that used to be the kernel blocking the
  calling thread — a connect finishing, more to read, room to write, a
  connection to accept — now goes through a new primitive, `__wait_io`,
  that parks the calling green thread on the epoll reactor instead
  (`rt_reactor_wait`). The one exception is a bounded wait: a
  `set_read_timeout`/`set_write_timeout` deadline is honored with `__poll`
  instead, because parking has no way to say "gave up after N ms" — SO_
  RCVTIMEO/SNDTIMEO stopped being able to do this once the socket became
  non-blocking (the kernel never waits long enough to time out), so this
  module's own two fields (`read_timeout_ms`/`write_timeout_ms`) replaced it.
- **A real deadlock was found, not merely a risk, and is now FIXED.**
  `rt_sched_spawn`'s own backpressure (`scheduler.c`'s `squeue_push`, when
  the shared queue is full) used to be a genuine OS-level
  `pthread_cond_wait` on the calling thread, unconditionally — correct when
  the caller is an ordinary OS thread (every Phase 2/3 test before this),
  wrong the instant the caller is a green thread running on the only
  carrier that could ever drain the queue: that carrier is now blocked
  waiting on itself, forever. With `LANG_NUM_CARRIERS=1` and the default
  queue capacity (64), spawning 300 workers in a tight loop from the top
  level hung forever. Initially shipped as a raised default
  `LANG_GLOBAL_QUEUE_CAP` (1,048,576) that only narrowed the window; that
  mitigation is now replaced with the real fix: `squeue_push` asks
  `tls_current_green` (via a small `rt_sched_in_green_thread` accessor)
  whether its caller is a green thread. If so, it parks the calling green
  thread (`rt_sched_park(RT_GT_PARKED_QUEUE)`) instead of blocking the
  carrier — the exact same FIFO-waiter-queue-then-park shape `Chan.send`/
  `recv` already use (`rt.c`), reimplemented locally in `scheduler.c`
  (`rt_squeue_waiter_t`/`sqwq_push`/`sqwq_drain`) rather than shared across
  the rt.c/scheduler.c layering boundary, matching this codebase's existing
  per-layer-own-copy convention (reactor.c's waiter map does the same).
  `squeue_draw`, which already frees room and broadcasts the OS-thread
  condvar, now also drains and `rt_sched_unpark`s every green-thread waiter
  whenever it draws anything — the green-thread-safe equivalent of that
  broadcast, not a weaker guarantee (every waiter re-checks its own
  `while (q->len == q->cap)` on resume, the same reason the broadcast was
  already correct for the condvar side). The raised default queue capacity
  is kept as a second line of defense (harmless, and it also helps batching),
  but the hang it used to merely narrow no longer exists: verified with
  `runtime/spawn_backpressure_demo/main.src` (300 spawns from the top level,
  `LANG_NUM_CARRIERS=1`, `LANG_GLOBAL_QUEUE_CAP=4` so the cap is hit almost
  immediately rather than only past a million) — hangs forever on the
  pre-fix code (confirmed directly, `timeout` kills it), completes and
  prints the correct sum on the fixed code, 40/40 consecutive runs clean at
  1/2/4 carriers, and 15/15 clean under ThreadSanitizer at both 1 and 2
  carriers (`runtime/spawn_backpressure_test.sh`). Not extended to
  `squeue_push`'s other call sites (`carrier_dispatch`'s own post-dispatch
  self-requeue, `try_handoff`'s handoff pushes) — those run with
  `tls_current_green` already cleared (no green thread is "currently
  running" by the time they execute, so there is nothing for
  `rt_sched_park` to park), so the same fix shape does not apply directly;
  they still rely on the raised queue capacity alone. Reaching the
  equivalent hang through one of those paths needs a carrier's own local
  buffer plus the shared queue both saturated at once, far past anything
  this phase's own tests or the demo above exercise — a real, scoped,
  smaller residual than the one this fix closes, not silently ignored.
- [ ] **kqueue reactor for macOS/BSD — Phase 3.5 extension (pulled forward
  from Phase 4's stub below, in parallel with the aarch64 context-switch
  port). Built and statically reviewed; NOT YET VALIDATED ON REAL
  HARDWARE.** `runtime/reactor.h`'s contract (`rt_reactor_create`/
  `rt_reactor_wait`/`rt_reactor_destroy`, same park/unpark semantics) now
  has two backends — `runtime/reactor_epoll.c` (Linux, the original Phase 3
  file, renamed, behavior unchanged and reconfirmed by the full x86-64
  regression suite below) and `runtime/reactor_kqueue.c` (macOS/BSD, new).
  `runtime/arch.sh` picks between them per `uname -s`, the same shape it
  already uses for the per-CPU-architecture context-switch file; every
  build/test script that used to hard-code `runtime/reactor.c` now sources
  `arch.sh` and uses `$RT_REACTOR_C` instead (~20 scripts). See
  `runtime/reactor_kqueue.c`'s own top comment for the full epoll→kqueue
  mapping and reasoning; the two points worth repeating here:
  - **EPOLLONESHOT → EV_ONESHOT is not a transparent swap.** kqueue splits
    read/write into independent per-filter knotes where epoll has one
    combined per-fd registration, which creates a spurious-wakeup hazard a
    line-for-line port would have introduced silently: one requested
    direction firing does not disarm the other, so a stale sibling knote
    can fire later and call `rt_sched_unpark` for a green_id that has moved
    on to something unrelated — and `rt_sched_unpark`'s own CAS word has no
    notion of "which episode" a notification is for (see scheduler.h), so
    that is a real corruption path, not a cosmetic one. Closed with a
    per-registration sequence number (not the green_id) carried in each
    kevent's `udata`; a firing is only acted on if it matches the fd's
    CURRENT registration. Reasoned through carefully and believed correct;
    not exercised under TSan on real hardware, which is exactly what the CI
    workflow below is for.
  - **eventfd → EVFILT_USER, not a self-pipe.** Chosen for no extra fd and
    no self-pipe draining logic, and because kqueue namespaces idents
    per-filter, so the shutdown signal can never collide with a real fd.
    Lowest-confidence detail in the whole port: the exact `fflags`
    control-bit convention for triggering it (`NOTE_TRIGGER` alone, no
    explicit `NOTE_FFCOPY`/etc.) is written from documented/recalled
    kevent(2) semantics, not confirmed against a real man page this session
    (no macOS/BSD box available) — if wrong, the failure mode is a hang in
    `rt_reactor_destroy`'s `pthread_join`, not silent corruption, so it
    would be loud and immediate on the first real run.
  **Verified so far**: the full x86-64/Linux regression suite (`gates.sh`,
  `runtime/phase3_test.sh`, `runtime/scheduler_test.sh`,
  `runtime/scheduler_tsan.sh`, `runtime/phase3_tsan.sh`) stays clean after
  the epoll backend's rename/refactor — this change touches no epoll-path
  logic, only its filename and the scripts that reference it. The kqueue
  backend itself has had a careful static read-through against kevent(2)'s
  documented semantics (see above) but has **never been compiled or run**
  — this project has no macOS/BSD hardware. A GitHub Actions workflow
  (`.github/workflows/macos.yml`) builds the runtime and runs the same test
  suites on real `macos-13`/`macos-14` GitHub-hosted runners; it has been
  reviewed for YAML correctness and its build commands dry-run locally
  against the Linux path, but **a human still needs to push a branch/tag
  and watch it run** before any claim here about the kqueue backend
  graduates from "reasoned to be correct" to "confirmed." Do not treat this
  checklist item as done until that run is green.

  **`gates.sh` on this branch currently reports 2 corpus failures
  (`modules/stdlib-http-client`/`modules/stdlib-http-server`, `clang -O2`
  only) -- confirmed, directly, to be the PRE-EXISTING `rt_stack_limit` race
  below (the same "trap: stack overflow" signature, the same gcc-passes/
  clang-fails compiler-dependent pattern already on record there), NOT
  anything this kqueue work touched.** Verified three ways before writing
  this: (1) `runtime/reactor_epoll.c` is a byte-for-byte functional copy of
  the original `runtime/reactor.c` (only the top comment changed -- `git
  diff` confirms no code line moved); (2) `gcc -O2` passes this exact test
  5/5 while `clang -O2` crashes 5/5 with "trap: stack overflow", matching
  the compiler-dependence already recorded below precisely; (3) building
  the ORIGINAL, completely unmodified `runtime/reactor.c` straight out of
  this branch's own base commit (`git archive`, no kqueue-work files
  involved at all) against the same test reproduces the identical crash
  (2/3 runs). This is the same bug a separate, concurrent session is
  already working on (see this branch's own history for `rt_stack_check`) --
  tracked there, not a new regression, and not this task's to fix.
- **A second, more serious bug was found by TSan and is NOT fixed: a
  genuine data race on `rt_stack_limit` between two carrier OS threads.**
  Found empirically while stress-testing real concurrent socket I/O (a
  server accept loop plus ~10-200 concurrent client/handler pairs, all
  doing real TCP through the reactor) under `clang -O1`/`-O2` with 2+
  carriers: the program crashes with a spurious "stack overflow" trap --
  confirmed via gdb to fire 3-4 frames into a FRESH green thread's call
  stack, nowhere near genuinely deep -- reproducing well over half the time
  with as few as 2 carriers and ~10 real connections. A TSan build of the
  identical program caught the mechanism directly: a WRITE to
  `rt_stack_limit` inside `rt_fiber_switch` (`runtime/greenthread.h`) on one
  carrier OS thread, racing a READ of the same address inside a different
  green thread's own compiler-emitted stack probe, running on a DIFFERENT
  carrier OS thread, at the same instant. `rt_stack_limit` is declared
  `_Thread_local` and every other access to it in this codebase is
  correctly scoped per-carrier, so this is a real, narrow, and so far
  unexplained violation of that isolation under genuine SMP concurrency --
  not a TSan tool artifact (unlike the already-documented gap below:
  that one is a sanitizer-internal segfault with no race report attached;
  this one IS a reported, categorized `ThreadSanitizer: data race`, and the
  user-visible crash reproduces identically with no sanitizer involved at
  all). Investigated at length before concluding it could not be safely
  fixed in the time available: ruled out the blocking-FFI monitor racing a
  carrier's local buffer (disabling the monitor entirely did not help),
  ruled out the slab allocator and the green-thread state table (both
  correctly locked), and ruled out naive TLS-address caching across the
  hand-written context switch (an explicit compiler memory barrier placed
  immediately after `rt_ctx_switch` did not help either) -- each a
  plausible, targeted hypothesis, each tested directly and disproved.
  Telling evidence gathered along the way: `runtime/scheduler_tsan.sh` and
  `runtime/phase3_tsan.sh` -- which already exercise multiple real carriers
  and the reactor's own park/unpark directly, via hand-written C, and
  normally run clean -- stayed clean at 25/25 runs each when re-run during
  this same investigation, so the bug is not in the standalone
  scheduler/reactor mechanism in any way those harnesses' own test shapes
  reach; it takes the SCALE and PATTERN of a real compiled program driving
  many concurrent green threads through many park/unpark cycles via
  `lib/net.src` to surface it, which is new coverage this task added and
  neither existing harness happened to provide. Deliberately NOT patched:
  this is hand-rolled, assembly-adjacent context-switching code this task
  was scoped to reuse as-is, a wrong fix here is worse than no fix (this
  project's own lesson from the ASan/fiber-annotation attempt noted
  earlier in this file applies just as much here), and misdiagnosing a
  genuine SMP race under real time pressure is exactly the overclaiming
  this whole effort was warned against. **Practical consequence, stated
  plainly: real concurrent socket I/O across more than one carrier is not
  yet safe.** `LANG_NUM_CARRIERS=1` avoids it entirely (confirmed: the
  demo below passes reliably with it, and removes genuine parallelism
  between carriers, which is also why the race cannot occur -- there is
  only ever one carrier OS thread to race against itself). See
  `runtime/net_concurrency_demo/` (moved out of the ordinary corpus
  specifically because it is not reliable there yet) and
  `runtime/spawn_wiring_tsan.sh` for the reproduction and the TSan
  transcript this was built from.

  **Follow-up investigation (separate session, after the above was
  written): re-confirmed everything above, found one more real bug along
  the way, found something genuinely strange that could not be resolved,
  and still did not fix the race.** Summary of what changed:

  - **Minimal repro, reconfirmed precisely**: `LANG_NUM_CARRIERS=2` against
    the full 200-connection demo reproduces the "stack overflow" trap
    reliably under plain `clang -O2`, no sanitizer at all — 15/15 and then
    20/20 consecutive runs crashed in independent batches. The SAME program
    built with `gcc -O2` did NOT crash in 5/5 runs. This compiler-dependence
    is new, confirmed information: it is consistent with a genuine race
    whose window happens to be wide enough to hit reliably under clang's
    codegen and narrow enough to miss under gcc's on this host, and is not
    itself evidence of a miscompilation in either compiler (a race's
    reproduction rate is expected to be codegen- and timing-sensitive; nothing
    about this project's probe or ctx-switch code differs between the two
    builds). Did not find a smaller reproducer than the full demo at
    `LANG_NUM_CARRIERS=2` — every attempt to shrink connection count traded
    reliability for size without actually isolating the mechanism further,
    so further work used the full demo directly rather than a weaker proxy
    for it.
  - **gdb, directly, on a stock (non-TSan) crash**: confirms the prior
    account precisely. The trap fires inside `Conn.close`'s own
    compiler-emitted probe, called from `client()` at the very first thing
    it does after `read_until` returns — `rt_ctx_trampoline` ->
    `green_trampoline` -> the compiled `client` function -> `Conn.close` ->
    `rt_stack_probe_slow` -> `rt_trap`. Four real frames, the second of
    which (`ctx_trampoline`'s own fake caller) is `0x0` — the normal,
    expected end of a backtrace for a green thread that has barely started,
    not a sign of a corrupted unwind. This is not a deep, genuine overflow
    by any reading of the call chain.
  - **The blocking-FFI monitor is independently reconfirmed NOT the cause**,
    by direct experiment this session (not just by re-trusting the earlier
    claim): built a variant with `rt_sched_start_blocking_monitor`'s call
    site in `init_global_scheduler` (`rt.c`) commented out entirely, so the
    monitor thread never starts and `try_handoff` is never reachable. The
    same crash still reproduced 20/20. (A real, independent, GENUINE data
    race WAS found and reproduced directly along the way, in
    `try_handoff`/`carrier_main`'s unsynchronized read of `c->local_len` —
    TSan flags it repeatedly, correctly, by the letter of the C/pthread
    memory model: `carrier_main` reads `c->local_len`/`c->local` with no
    lock, relying on the invariant that a carrier reported "stuck" in a
    blocking FFI call cannot concurrently be running its own dispatch loop,
    which `rt_wait_io`'s own undo/rearm dance, above, narrows but — per this
    session's reading of it — does not provably close for every ordinary
    `prim` call, only for `rt_wait_io` itself. This is flagged here as a
    real, separate, smaller bug worth a future look, NOT fixed in this
    session because the monitor-disabled experiment above proves it is not
    what is crashing this demo, and this session's remaining time went to
    the bigger question instead.)
  - **TSan's own race reports, read closely, point at something odder than
    "two carriers touched the same byte"**: re-running
    `runtime/spawn_wiring_tsan.sh`'s net case repeatedly surfaces several
    DIFFERENT race locations across runs (`rt_fiber_switch`, the
    try_handoff/carrier_main pair above, and — once — a probe inside
    `Conn.close` itself), not always the same one, consistent with the
    underlying corruption being real and then cascading into whatever code
    happens to touch adjacent memory next, rather than one single,
    isolated race site. For the specific, previously-documented
    `rt_fiber_switch` race: TSan reports "Location is TLS of thread T1" for
    a write made by T1 and a read made by a DIFFERENT real thread T2 — and
    this session confirmed, by printing `(void*)&rt_stack_limit` directly
    (not merely its value) at every fiber switch, that T1's and T2's own
    computed addresses for this `_Thread_local` variable ARE genuinely
    different, stable, never-colliding addresses throughout a run, exactly
    as `_Thread_local`/the `fs`-relative local-exec TLS model (confirmed by
    disassembly: `mov r14, -32; cmp QWORD PTR fs:[r14], ...`, a fixed
    compile-time offset from the thread pointer, not a dynamic
    `__tls_get_addr` call that could be miscached) guarantees they must be.
    So hypothesis (a) from this task's own framing — "it looks cross-thread
    to TSan but is really one thread's own stale read" — does not fit what
    TSan is reporting either, at least not in the simple form.
  - **A genuinely strange, NOT-fully-explained observation, reported
    honestly rather than resolved**: instrumenting the compiler-emitted
    probe directly (patching the generated C, not just the runtime) to
    print `pthread_self()` and `&rt_stack_limit` together at the exact
    moment a probe fires, in the same run, found the SAME `pthread_self()`
    value reported at two different moments paired with TWO DIFFERENT
    `&rt_stack_limit` addresses — which should be impossible for a real,
    live OS thread under this TLS model (the thread pointer, and therefore
    this fixed offset from it, cannot change for a living thread; nothing
    in this runtime ever touches `%fs`). Also found, separately, that the
    actual stack pointer at the moment a probe fires sits almost exactly
    `RT_STACK_SIZE` (1 MiB) below the `rt_stack_limit` value read at that
    same instant — not a few-byte race-window discrepancy, a clean,
    suspiciously round, one-slab-slot-sized one, which smells more like
    "running on the wrong green thread's stack slot entirely" than "read a
    value a few instructions stale." Neither of these was run down to a
    root cause: they could be a genuine, deeper bug in the slab allocator
    or the spawn/dispatch path that only manifests at this scale, OR an
    artifact of the ad hoc `fprintf`-based instrumentation itself (stdio
    locking and multi-threaded output ordering were not independently
    verified trustworthy for this purpose, and a printf-based probe changes
    timing, which matters for a timing-sensitive race). Reported here
    precisely so the next person does not have to rediscover either
    number, and does not mistake "I could not fully explain my own
    instrumentation's output" for "I found the root cause."
  - **Conclusion, stated plainly**: this session did NOT reach confidence
    sufficient to change `rt_fiber_switch`, `rt_ctx_switch`,
    `rt_stack_probe_slow`, or the slab allocator. Hypothesis (b) from this
    task's own framing — a narrow surviving window where the same green
    thread is briefly live on two carriers at once — remains the most
    plausible shape given the ~1 MiB stack-slot-sized discrepancy above,
    but no specific mechanism in the park/unpark CAS protocol or the
    reactor's waiter map (both re-read in full this session; see their own
    files) was found to actually permit that double-dispatch, and the
    try_handoff race, while real, is independently ruled out as the cause.
    The honest state of this bug, after two independent sessions of
    investigation, is: real, reproducible, NOT a TSan tool artifact, NOT
    the blocking-FFI monitor, NOT the try_handoff/carrier_main race, NOT a
    simple TLS-address collision — and still unexplained at the mechanism
    level. `LANG_NUM_CARRIERS=1` remains the only known-safe configuration
    for real concurrent socket I/O.

  **Third session: root cause found and FIXED.** Both leading hypotheses
  from the first two sessions were first disproven directly rather than
  re-argued: a diagnostic guard added to `carrier_dispatch` (a CAS claiming
  a green thread before `rt_fiber_switch`, releasing it after, `rt_trap`ing
  loudly on a failed claim) never fired across dozens of reproductions under
  `clang -O2`/2 carriers — ruling out hypothesis (b), the same green thread
  live on two carriers at once. A second guard on the slab allocator (an
  `in_use` bitmap per slot, checked on every `rt_stack_alloc`/`rt_stack_free`)
  also never fired — ruling out premature reuse of a stack slot. Forcing
  `-fno-optimize-sibling-calls` (in case `rt_ctx_switch` was being
  tail-call-optimized into a `jmp`, dropping a stack frame) made no
  difference either — 30/30 still crashed.

  The actual mechanism was found with a hardware watchpoint (GDB, plain
  `watch`/`commands` scripting — Python watchpoint callbacks proved too
  unstable at this hit rate and crashed GDB itself) on each carrier's own
  live `rt_stack_limit` TLS slot, logging every write's call site and value
  for an entire run. It proved every write to `rt_stack_limit` was correct,
  always — the crashing green thread's slot held exactly its own correct
  `g->stack.base` from the moment it was dispatched, with nothing writing to
  it again before the crash. That flips the whole investigation: the bug is
  not in `rt_stack_limit` at all, it is in what the probe compares it
  against. Reading the actual compiled comparison at the crash site
  (`objdump`) showed why:

  ```
  mov  %fs:0x0, %r15      ; compute &rt_stack_limit ONCE, early in the caller
  add  %rax, %r15
  mov  %r15, 0x18(%rsp)   ; spill that ADDRESS to the stack for later reuse
       ... connect() / write() / read_until() happen here ...
  mov  0x18(%rsp), %rcx   ; reload the SAME cached address for a later probe
  cmp  %rax, (%rcx)       ; compare against it -- this is the one that traps
  ```

  `clang -O2` (not `gcc -O2`, which happens not to in this exact shape)
  computes the THREAD-LOCAL ADDRESS of `rt_stack_limit` once, early in a
  long-lived function, and caches it in a spilled register across real,
  opaque calls in between — any of which may park the green thread and
  resume it on a *different carrier*. A later probe inlined into the same
  function (from a small callee like `Conn.close` that `-O2` inlines back
  in) then reads through that stale, wrong-carrier address: whatever that
  *other* carrier's own unrelated activity has since written there, which
  can be anything. This is ordinary, valid optimisation from the compiler's
  point of view — nothing in C's memory model says a thread-local variable's
  resolved address can change mid-function, and nothing told it this runtime
  violates that. It is not a bug in `rt_fiber_switch`, `rt_ctx_switch`, or
  the slab allocator at all, and never was.

  **The fix** (`runtime/rt.h`, `runtime/rt.c`, `src/emit_c.rs`): the probe
  is no longer inlined comparison text. `src/emit_c.rs` now emits a plain
  call, `rt_stack_check();`, to a new, `__attribute__((noinline))` function
  in `rt.c` that does the same comparison internally. A real, out-of-line
  call forces a fresh `%fs`-relative read inside it on every invocation,
  because the caller no longer contains any TLS access of its own to cache
  or hoist — there is nothing left for the optimiser to reuse across the
  intervening calls. `-flto` is already off project-wide (see the README's
  "Two rules that look like details and are not"), so this cannot be
  silently undone by cross-TU inlining later; the `noinline` attribute
  documents the requirement explicitly regardless.

  **Verified**: 100+ consecutive stock runs (`clang -O2`, 2 and 4 carriers)
  of the exact `net_concurrency_demo` reproducer, zero crashes (previously
  well over half failed). 80 further runs under ThreadSanitizer: zero
  recurrences of the `rt_stack_limit` race or the "stack overflow" trap:
  65 clean, 15 hit one of the two separate, pre-existing, unrelated races
  below (never this one). `runtime/spawn_wiring_tsan.sh`'s own Chan tests
  remain clean. Full `gates.sh`, including the sanitized corpus, passes
  (one test, `greenthread_test.sh`'s "emit_c.rs probe text" check, was
  updated to assert the new call-based codegen instead of the old inlined
  text it was written against).

  **Two smaller, separate races were found by the same TSan runs, neither
  responsible for the crash above. Both are now ALSO fixed** — see the two
  sessions below.

  **Fourth session: the `try_handoff`/`carrier_main` race, found and
  FIXED.** What it actually was, read precisely rather than re-summarized:
  `try_handoff` (the blocking-FFI monitor thread's rescue path) mutates a
  stuck carrier's `c->local`/`c->local_head`/`c->local_len` under
  `c->local_lock`, having re-checked `blocking_since_ns` under that same
  lock first — correct, by itself. But `carrier_main`'s own ordinary
  dispatch loop read and wrote those exact fields with NO lock at all: the
  zero-length check, the post-draw `local_head`/`local_len` reset (and the
  draw itself, which writes `c->local`'s contents directly), and `local_pop`
  were all lock-free, resting entirely on the invariant that try_handoff
  only ever runs while this carrier is PROVABLY not executing this loop —
  true for a genuine blocking syscall, but, as `rt_wait_io`'s own comment
  documents in full, NOT guaranteed for every `prim`: a `prim` that parks
  the green thread switches the carrier straight back into this very loop
  while `blocking_since_ns` can still be left looking set, which is exactly
  what lets the monitor believe the carrier is stuck while it is actually
  here, running. Either way, the result was a real, TSan-confirmed,
  unsynchronized concurrent read/write of a carrier's own run-queue
  bookkeeping from two different OS threads.

  **The fix** (`runtime/scheduler.c`, `carrier_main` and its struct's own
  comment; no change to `try_handoff`, `rt_wait_io`, `rt_fiber_switch`,
  `rt_ctx_switch`, the slab allocator, or `rt_stack_check`): `carrier_main`'s
  dispatch loop now takes `c->local_lock` around every one of its own
  touches of `local`/`local_head`/`local_len` — the emptiness check, the
  `squeue_draw`-and-reset, and `local_pop` — in one short critical section
  per loop iteration, released before `carrier_dispatch(c, g)` runs (so
  `carrier_dispatch`'s own later `local_push`, under the same lock, never
  nests it). This makes the carrier safe against `try_handoff` by actual
  mutual exclusion, not by an invariant about when try_handoff is allowed to
  run. No new deadlock risk: `try_handoff` always releases `local_lock`
  before its own `squeue_push` calls (unchanged), the new code never holds
  `local_lock` while blocked on anything (`squeue_draw` is non-blocking),
  and the two locks are always acquired in the same order wherever they
  nest (`local_lock` outer, the global queue's `q->lock` inner, never the
  reverse) — no AB-BA cycle is possible.

  **Verified**: 25/25 clean runs each of `runtime/scheduler_tsan.sh` and
  `runtime/phase3_tsan.sh`. `runtime/spawn_wiring_tsan.sh`'s `Chan` tests
  stayed clean (30/30 each). For the actual target — real concurrent socket
  I/O under TSan, `net_concurrency_demo` at `LANG_NUM_CARRIERS=2` — 180 total
  runs across two batches: ZERO occurrences of the `try_handoff`/
  `carrier_main` race in any of them (previously a real, repeatedly-observed
  TSan report in this exact configuration). The 100-run batch's full output
  was inspected run by run: 94/100 completely clean, 6/100 hit a data race,
  and every one of those 6 was the OTHER open race below (`rt_sched_destroy`
  shutdown ordering, confirmed by reading each report's full stack trace) —
  never the fixed race, and never a new one. Full `gates.sh` passes clean on
  top of this change.

  **Fifth session: the `rt_sched_destroy` shutdown-ordering race, found and
  FIXED.** Root cause, found by tracing `rt_sched_destroy`'s teardown
  sequence against every OS thread the scheduler subsystem can have
  running: `rt_sched_shutdown` correctly `pthread_join`s every carrier
  before `rt_sched_destroy` frees anything, but the epoll reactor's
  dedicated OS thread (`runtime/reactor.c`'s `reactor_loop`, started by
  `rt_reactor_create`) is a SEPARATE thread `rt_sched_shutdown` never
  touches — and `rt_reactor_destroy` (which exists, signals the reactor
  thread's shutdown eventfd, and joins it) turned out to never be called
  anywhere in this runtime at all. `rt_run_program` created the global
  reactor the first time any `net` call needed it and then never tore it
  down: the reactor thread keeps calling `epoll_wait` and, on any ready fd
  (plausible even this late — a peer's last FIN/RST can arrive concurrently
  with this process's own exit), calls `rt_sched_unpark`, which locks
  `s->registry.lock`, looks `g` up, and pushes onto `s->global` — exactly
  the objects `rt_sched_destroy` destroys and frees moments later.

  Reproduced directly: the real `net_concurrency_demo` did not reliably
  surface this specific race on its own (140 combined TSan runs, 100 at
  `LANG_NUM_CARRIERS=1` and 40 at `=2`, produced zero reports of it — its
  natural window at process exit is narrow). A dedicated, deterministic
  standalone reproducer was built instead (one green thread parks on a real
  pipe fd via `rt_reactor_wait`; a second OS thread makes that fd ready a
  couple of milliseconds after `rt_sched_shutdown` returns, timed to land
  while `rt_sched_destroy` is freeing scheduler state) and reliably
  reproduced the exact mechanism, 10/10 runs, confirmed by TSan as a
  heap-use-after-free: `registry_lookup_locked`/`rt_sched_unpark` on the
  reactor's own OS thread, reading memory the main thread had already freed
  inside `rt_sched_destroy` moments before.

  **The fix** (`runtime/rt.c` only — `runtime/scheduler.c` untouched,
  reactor/scheduler pairing is `rt.c`'s responsibility by this codebase's
  existing layering, and standalone harnesses call `rt_sched_destroy`
  directly with no reactor involved at all and must keep working
  unchanged): `rt_run_program`'s own teardown now calls a new
  `rt_global_reactor_destroy_if_created` between `rt_sched_shutdown` and
  `rt_sched_destroy` — joining the reactor thread (if `net` was ever used;
  a program that never calls it leaves the reactor uncreated, and this is a
  correct no-op) strictly after every carrier is already stopped and
  strictly before any scheduler memory is freed.

  **Verified**: the standalone reproducer, 120/120 clean runs under
  ThreadSanitizer with the fix applied (vs. 10/10 reproducing the
  heap-use-after-free without it). The real compiled program was re-run
  post-fix, 100 iterations at `LANG_NUM_CARRIERS=1` and 40 at `=2`, with no
  change in behavior from the pre-fix baseline. Full `gates.sh` passes
  clean with this change in place.

  **Honest residual, after all five sessions**: every race this section
  documents is now fixed and empirically verified against its own specific
  reproducer — not against every conceivable interleaving a formal proof
  would need to rule out one by one, which is the same epistemic standard
  the rest of this file already holds itself to. `LANG_NUM_CARRIERS` greater
  than 1 for real concurrent socket I/O is, as of this session, no longer
  known to be unsafe for any previously-documented reason.
- The blocking-FFI handoff monitor (Phase 3, built but opt-in) is now
  actually turned on, unconditionally, for the one process-wide scheduler
  every compiled program uses — it was built with exactly this moment in
  mind but had nothing to protect until `spawn` meant a green thread.
  Regular-file I/O, `os.run`'s process wait, DNS resolution and a terminal's
  canonical-mode read are still genuinely blocking and NOT converted (out
  of this phase's scope) — the monitor is this phase's safety net for
  those, not a fix for them: it rescues a stuck carrier's queued siblings,
  it does not make the stuck call itself faster.
- Every build line in the repository that links `runtime/rt.c` — which is
  all of them — now also links `runtime/scheduler.c`, one of
  `runtime/reactor_epoll.c`/`runtime/reactor_kqueue.c` (picked by
  `runtime/arch.sh`, per `uname -s` — see the Progress item above) and the
  per-architecture context-switch file, because `rt.c` calls into them
  unconditionally. The reactor choice is no longer an open question as of
  the Progress item above, but it is UNVALIDATED on the macOS/BSD side
  pending a real-hardware CI run.

**Phase 4 — portability.**
- aarch64 context switch
- ~~kqueue (macOS/BSD)~~ — pulled forward into Phase 3.5, see the Progress
  checklist above: built, statically reviewed, pending real-hardware CI
  validation (not yet checked off for that reason)
- Windows: AFD readiness emulation only, never native IOCP (forces buffer
  pinning into the memory model) — no timeline yet

Deferred with reasons: **io_uring** (blocked by default in Docker *and*
Podman, disabled on Google's production fleet, libuv reverted it).

---

## Open

- Channel syntax and typing — waits on the type system
- Whether `spawn` takes a closure or a function plus arguments (closures are
  not implemented yet, and closures plus refcounting is the most common source
  of reference cycles)
- Structured concurrency: does a spawning scope wait for its children? Loom
  says yes and it is a genuine improvement over Go's fire-and-forget
- Cancellation. libdill's model — killing a thread makes every blocking call
  in it return an error — fits "errors are values" and needs no unwinder
