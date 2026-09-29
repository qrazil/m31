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

## Scheduler queues: per-carrier arrays, work-stealing, not one shared queue

Decided **2026-09-29**. Fills in what "work-stealing scheduler" in the order
of work actually means, mechanically.

**State: a byte per green thread, indexed by id, never scanned.** A green
thread's state is an enum (`Runnable`, `Running`, `Parked-on-I/O`,
`Parked-on-channel`, `Parked-on-timer`, `Dead`), not a single bit, so a byte
table beats a hand-packed bitmap — simpler code, and the memory difference
is noise: a million threads is 1 MB of state against 4–64 **GB** of stack
for the same million threads, at the 4–64 KB slab-stack sizes already
decided. This table is read and written in O(1) by id. It is never scanned
to find work — see below for why that distinction matters more than the
byte-vs-bit choice that prompted it.

**Each carrier — the OS thread, one per core — owns its own run queue**,
not each green thread and not one shared queue. A fixed-size array used as
a ring buffer (Go's own is 256 slots), not a linked list: contiguous memory
is cache-friendly where a linked list's scattered nodes are not, there is no
per-node allocation, and — the part that matters most — stealing a chunk of
another carrier's queue is a couple of compare-and-swaps on array indices,
where a linked list would need a lock or a considerably harder lock-free
list algorithm to do the same thing safely.

**One shared queue, with every carrier pulling from it, was Go's own first
design (pre-1.1, 2012) and it did not scale**: one lock, every core
fighting over the same cache line to pop the next goroutine, contention
that gets *worse* as cores are added rather than better. Go 1.1 (2013)
replaced it with per-carrier local queues and work-stealing, which is the
design here. Independently, Java's `ForkJoinPool` (2011, built for the same
kind of fine-grained parallel scheduling) landed on the identical
local-queue-plus-stealing shape without copying Go — two unrelated,
heavily-used systems converging on the same answer is strong evidence on
its own. The reason a shared queue loses is not really about the words
"push" versus "steal": a shared queue is *every* carrier touching one piece
of memory on *every* dequeue, which is constant cross-core contention
regardless of what the access pattern is called. A local queue is touched
by its own carrier alone in the common case — zero contention — and only
reached into from outside when a sibling is actually idle, which is rare by
comparison.

**Stealing takes a batch from the opposite end, not one item at a time.**
Go steals half a victim's queue in one operation: a thief that had to steal
again immediately for every single item would just move the contention
problem rather than solve it. Taking the opposite end from where the owner
works (the owner's own push/pop needs no synchronization at all; only the
far end, touched by thieves, needs an atomic operation) is what makes the
owner's own fast path free of stealing's cost.

**A small global queue stays as a fallback, not a redesign.** Go did not
eliminate its global queue in 1.1 — it demoted it from "the only queue" to
the rare case: overflow when a local queue is full, and a way to stop one
carrier's long local queue from starving a thread that just became
runnable elsewhere. Same shape here.

**A carrier-sized bitmap, not a green-thread-sized one, is where the
bitmap instinct behind the byte-per-thread state table actually belongs.**
A thief needs to find *which* sibling carrier has stealable work; with one
bit per carrier (tens, at most — one per core), a bitmap scan is exactly
the regime it is fast in, matching the real Linux O(1) scheduler's own
technique (pre-2.6.23, before CFS replaced it): a bitmap over a *small*
number of buckets, `find_first_bit` in O(1). The same technique over a
million green threads instead of a handful of carriers is the mistake
`select()` made and `epoll()` was built to fix: `select()` hands the kernel
a bitmap and makes it scan the whole thing every call, cost scaling with
*total* descriptors; `epoll()` hands back only what is actually ready, cost
scaling with the *ready* count alone. Scanning a million-entry table on
every scheduling tick would be exactly `select()`'s mistake, restated in a
different subsystem — sized to carriers instead of threads, the same
bitmap idea is simply the right tool.

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
- [ ] Context switch, slab stacks with probes
- [ ] Scheduler, work stealing, probe-based preemption
- [ ] epoll reactor, park/unpark
- [ ] Blocking-FFI handoff

**Known v0 simplification: channels are immortal.** A channel must be
reachable from several threads at once, so it is exempt from the move rule --
and that exemption is exactly what would make its own refcount race. Rather
than make one refcount atomic ahead of the general answer, a channel is never
freed. A program creates few, so the leak is bounded by that count.

## Order of work

1. **Type system with ownership** — `moved` has to be expressible and checked.
   Blocks everything else.
2. OS threads + channels, to get the channel semantics right against a simple
   scheduler.
3. Context switch (asm, x86-64 then aarch64) + slab stack allocator + probes.
4. Scheduler: per-carrier queues, work stealing, probe-based preemption --
   see "Scheduler queues" above for the exact mechanism.
5. epoll reactor, then park/unpark.
6. Blocking-FFI handoff.
7. kqueue.

Deferred with reasons: **io_uring** (blocked by default in Docker *and*
Podman, disabled on Google's production fleet, libuv reverted it), **Windows**
(use AFD readiness emulation when it happens, never native IOCP — it forces
buffer pinning into the memory model).

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
