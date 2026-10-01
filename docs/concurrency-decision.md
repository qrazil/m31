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
      (`runtime/scheduler.{h,c}`), standalone, not yet wired to `spawn`.
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

**Known gap, carried forward rather than fixed in Phase 3**: ThreadSanitizer
itself intermittently segfaults (never a real race report, never in this
project's code) when a green thread is suspended by one carrier and resumed
by a different one under TSan specifically — the same class of gap already
noted for ASan's fiber-switching above, just a hard crash here instead of a
warning. Mitigated by narrowing the two affected tests to 1 carrier under
TSan only; every plain build and the ASan/UBSan build exercise full
multi-carrier counts with no issue. The real fix — `__tsan_switch_to_fiber`
annotations in `ctx_switch_x86_64.s` — is real, scoped, not-yet-started
follow-up work before this is fully hardened under every tool.

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

**Phase 2 — the scheduler itself. Done** (standalone, not yet wired to
`spawn` — a deliberate, separate decision, not done here).
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
known, carried-forward TSan gap.
- epoll reactor
- park/unpark, closing the lost-wakeup race against netpoll (the CAS state
  machine flagged under "What this costs" above)
- Blocking-FFI handoff (`enter_blocking`/`exit_blocking`, monitor thread,
  compile-time warning on unannotated foreign calls)

**Phase 4 — portability.**
- aarch64 context switch
- kqueue (macOS/BSD)
- Windows: AFD readiness emulation only, never native IOCP (forces buffer
  pinning into the memory model) — no timeline yet

Deferred with reasons: **io_uring** (blocked by default in Docker *and*
Podman, disabled on Google's production fleet, libuv reverted it).

---

## Open

- Channel syntax and typing — waits on the type system
- **Decided 2026-10-01, deferred until it's real friction, not a capability
  gap: `spawn` takes a named function plus explicit arguments, not a
  closure.** This is not a workaround — anything a closure would capture can
  be passed as an explicit argument instead, so it is exactly as capable,
  just more typing at the call site. Closures stay deliberately unbuilt:
  they are the most common source of reference cycles in a refcounted
  language with no GC to collect them later, and that problem does not have
  an answer yet. Separate from this, and lower-risk: an anonymous,
  **non-capturing** function literal — reusing the existing
  `fn name(params) { body }` syntax with the reserved name `_` (a compile
  error to reference, not merely discouraged by convention the way
  Python's `_` is), so a one-off function passed straight to `spawn` never
  needs an invented name. This has nothing to do with closures — no
  captured environment, no cycle risk — and is pure sugar: `fn _(x) {...}`
  compiles to exactly what a top-level named function with a
  compiler-generated name would. Safe to build whenever naming one-off
  spawn workers becomes real friction; not needed for `spawn`/`Chan` to be
  usable today.
- **Deferred until it's real friction, not built speculatively: `select`
  (wait on multiple channels/events, proceed with whichever is ready
  first).** This is a genuine capability gap, not just ergonomics — unlike
  the closure question above, `spawn`+`Chan` cannot fake it: watching N
  channels by spawning N watcher threads leaks every losing watcher, since
  the ones that do not win stay blocked forever. A real `select` needs the
  same shape of mechanism the epoll reactor already has — register
  interest in several things, get exactly one wakeup, cleanly withdraw
  from the rest. Not needed for the immediate next goal (an HTTP server):
  `http.serve`'s `timeout_ms:` and `net.m31`'s own
  `set_read_timeout`/`set_write_timeout` already solve per-connection
  timeouts at the socket level, without needing general multiplexing.
- Structured concurrency: does a spawning scope wait for its children? Loom
  says yes and it is a genuine improvement over Go's fire-and-forget
- Cancellation. libdill's model — killing a thread makes every blocking call
  in it return an error — fits "errors are values" and needs no unwinder
