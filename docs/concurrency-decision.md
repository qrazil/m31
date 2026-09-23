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
4. Scheduler: per-carrier queues, work stealing, probe-based preemption.
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
