# Concurrency and refcounting

Status: **research complete, decision open.** Nothing here is implemented.
This exists so the decision gets made once, on evidence, before anything
depends on it — concurrency is one of the few things that genuinely cannot
be bolted on afterwards.

All figures below were verified from primary sources in September 2026.

---

## 1. Why this has to be decided early

Three commitments are already made and they interact:

- **Refcounting, no GC** (README) — so every shared object has a counter, and
  the counter is the thing threads race on.
- **The C backend** — so the C compiler owns the stack, and we cannot move
  stacks or enumerate pointers into them.
- **Freeze early** — so "we'll revisit the memory model" is not available.

The question is not "which concurrency library". It is **whether two threads
can ever hold the same object**, because the answer decides whether every
`rc_inc` in the language is a plain increment or an atomic one.

## 2. The cost of getting it wrong

Measured, not estimated.

**Refcount operations themselves** — Choi, Shull & Torrellas, *Biased
Reference Counting*, PACT '18 (DOI 10.1145/3243176.3243195), measuring real
Swift programs:

- RC operations are **32% of execution time** on average (42% client, 15% server)
- Making them non-atomic, changing nothing else, cuts average execution time
  by **25%**

**The atomic instruction** — Travis Downs, *Atomics and Concurrency Costs*
(2020), measured on Skylake/Ice Lake/Graviton:

| operation | cost |
|---|---|
| plain increment, uncontended | ~2 ns |
| `lock xadd`, uncontended | ~7 ns (3–5×) |
| `lock xadd`, 2–4 threads on the same cache line | **110–180 ns (50–90×)** |

The contended number is the one that matters. It is not the `lock` prefix; it
is cache-line transfer between cores, ~70+ cycles minimum. A hot shared object
in a refcounted language is a cache line that every core wants.

So: **atomic-everywhere is roughly a 25% tax, and a shared hot object is a
50–90× cliff.** That is the budget this decision spends.

## 3. What everyone else actually does

| runtime | RC atomicity | how they get away with it |
|---|---|---|
| **Swift** | atomic always | Nothing. Pays the 32%/25% above. |
| **Nim** ARC/ORC | **non-atomic** by default | Move semantics on cross-thread transfer (`Isolated[T]`). `--mm:atomicArc` exists for when you need it. |
| **Inko** | **non-atomic** for most values | Compile-time single ownership eliminates most refcounts entirely; runtime non-atomic count is only a *borrow-count backstop* on single-owned values. True atomics reserved for exactly two types: processes and Strings. |
| **Pony** | no shared counter at all | Actors own their refcounts privately; dropping a remote reference **sends a DEC message** rather than touching a shared counter. |
| **Erlang/BEAM** | none for ordinary terms | Messages are **deep-copied** between private heaps. The one genuinely shared thing — refc binaries >64 bytes — uses a real atomic. |
| **Rust** | both, chosen at compile time | `Rc` is non-atomic and `!Send`; `Arc` is atomic and `Send`. The type system enforces the split at zero runtime cost. |

Two things stand out.

**Swift is the cautionary tale.** Fully general aliasing plus fully general
threading forces atomics everywhere, and there is no shipped escape: Biased
Reference Counting never landed in mainline, no escape-analysis pass exists,
and — importantly — **actor isolation does not license non-atomic RC**.
SE-0371 says so directly: releasing child objects "can be done from any
thread", because `unowned` and non-isolated holders can retain from anywhere.
Isolation protects the state, not the counter.

**Inko is the interesting outlier**, because it does the opposite of the
obvious thing. It has work-stealing and explicitly *refuses* to pin processes
to threads ("Pinning processes to OS threads" is a named non-goal), yet still
gets non-atomic refcounts for nearly everything. Not through pinning —
through **compile-time single ownership**, so most refcounts do not exist at
runtime at all.

### The rule underneath all of them

> Non-atomic refcounting survives concurrency exactly when **only one thread
> can reach an object at a time**, and that has to be guaranteed statically,
> not hoped for.

Ownership-transfer sends preserve it (one owner at any instant). Aliasing
sends destroy it immediately. And **work-stealing of a task itself** is the
sharpest break, because it silently moves captured non-atomic state to
another core — which is exactly why Tokio has `LocalSet`, a whole separate
API existing only to host `!Send`/`Rc` futures that the work-stealing
scheduler cannot.

## 4. The finding that reframes the question

The user asked: *can we do green threads in C?* The more useful question
turned out to be **do we need stacks at all?**

**Pony's actors are stackless.** Verified in `pony_actor_t` (`actor.h`): it
contains a message queue, a per-actor heap, GC bookkeeping and flags — and
**no stack pointer, no context, no coroutine state**. `ponyint_actor_run()`
is an ordinary loop on the scheduler thread's own native C stack, and
`handle_message()` calls the behaviour body as a plain function call that
runs to completion and returns. No `setjmp`, no `ucontext`, no fiber library
anywhere in the runtime.

That matters enormously here, because **the entire difficulty of green
threads in emitted C is stack switching**, and a run-to-completion actor
never switches stacks.

The cost is real and should not be glossed: run-to-completion means a
behaviour **cannot block or yield in the middle**. You write message-driven
code, not Go-style blocking code. Go's whole ergonomic pitch is that
`conn.Read()` looks synchronous; Pony's is that nothing ever blocks.

## 5. The three models

| | A. Stackful green threads | B. Stackless actors | C. OS threads + channels |
|---|---|---|---|
| Feels like | Go | Erlang, Pony | Java, C++ |
| Blocking-looking code | yes | **no** | yes |
| Stack switching in C | **the hard problem** | none needed | none needed |
| Cheap to spawn | yes (~KB) | yes (~100s of bytes) | no (~MB, ~10µs) |
| Refcount atomicity | atomic, unless pinned | non-atomic if messages move | atomic |
| Effort with the C backend | high | **low** | low |

**A** is what was asked for, and is the one the C backend fights.
**B** sidesteps the problem entirely and is what Pony and Erlang prove out.
**C** is the boring fallback and composes with either later.

## 6. Can we have Go-style green threads from emitted C?

**Not while using the C stack.** This is the clearest result of the whole
investigation, and it comes from surveying every C-emitting language that
tried.

> Every C-emitting language that got **real** green threads abandoned the C
> call stack entirely as its execution stack. Every one that kept the ordinary
> C stack per logical thread ended up with OS threads instead.

| language | what it actually does |
|---|---|
| **CHICKEN Scheme** | Cheney on the M.T.A. The C stack pointer *is* a bump allocator for the nursery; CPS code never returns; a stack-limit check Cheney-copies live data to the heap and `longjmp`s to a trampoline. Green threads are cooperative, single OS thread, **no multicore**. |
| **Cyclone Scheme** | One full Cheney-on-the-MTA engine **per pthread** — genuine parallel green threads, shared heap, concurrent Doligez-Leroy-Gonthier collector, write barrier relocating objects before sharing. The most complete answer anyone has. MIT. |
| **Gambit** | Continuations kept entirely separate from the C stack. Manual is explicit: *"at most one OS thread"* — no multicore. (Worth noting against the "millions of threads across cores" folklore.) |
| **GHC via C** | The STG stack is a heap object — `move_STACK()` relocates a thread's stack during GC, which is only possible because it is not the C stack. M:N TSOs onto Capabilities. |
| **V (vlang)** | `go` and `spawn` are literally the **same AST node**, and both codegen to `pthread_create`. Its docs claim "a lightweight thread managed by the V runtime"; the source falsifies that. A real M:N scheduler exists in `vlib/goroutines/` but is **not wired to the compiler** — the wiring existed in 0.3.5 behind `-use-coroutines` and was dropped mid-2026 during a self-hosted rewrite. |
| **Haxe/hxcpp**, **Ur/Web** server, **Bigloo** | Plain OS threads. |

Note the CHICKEN detail that inverts the usual assumption: Cheney-on-the-MTA
does **not** depend on the C compiler performing tail-call optimisation. The
opposite — ordinary non-optimised C calls are *allowed* to grow the stack,
because the stack growth **is** the allocation.

### The third pattern: stackless CPS in the compiler

Nim, Vala and Zig's late stage1 all took a different route — transform async
functions into heap-allocated state machines, no second stack at all:

- **Nim**: `async` is a macro that rewrites the proc into a
  `iterator {.closure.}` and turns every `await` into a `yield`. Chronos uses
  the identical technique.
- **Vala**: each async method becomes a `g_slice_new0`'d `<Method>Data` struct
  with an `int _state_` field, `yield` emits `_state_ = N; return FALSE;
  _state_N:`, and entry is a `switch (_state_)` dispatcher. Duff's device,
  driven by the GLib main loop.

Portable and cheap, and the complaints are consistent and documented: **function
colouring** (named explicitly on the Nim forum — *"not only blue/red function
[but] all color spectrum"*) and **useless async stack traces** (dom96, Nim's
own async author, agreeing they are "usually useless"; Araq's suggested
workaround at the time was "add echo statements"). Still unresolved — the
issue was reopened in October 2025.

**Zig is worth watching.** async/await is permanently gone as syntax
(issue #6025 closed July 2025), but 0.16.0 (April 2026) ships `std.Io` with
`Io.Threaded` feature-complete and **`Io.Evented` experimental**, described in
Zig's own release notes as *"userspace stack switching with work stealing,
also known as M:N threading, 'green threads,' or stackful coroutines."* That
is the closest live experiment to what we would be attempting.

### What this costs us

To get Go-style green threads on the C backend, we would have to stop using
the C stack as the execution stack — meaning a CPS/trampoline architecture
through the *entire* compiler, not a library we link. That is a different
compiler, decided now, affecting every function we ever emit.

**Copying stacks are impossible for us either way.** Go moved from segmented to
contiguous copying stacks in 1.3 (to fix the "hot split" problem), but
copying a stack requires rewriting every pointer into it, which requires
knowing where those pointers are. The C compiler owns the stack layout and
exposes no pointer map. So the options are fixed-size stacks with a guard
page, or mmap reserve-and-commit — **not** growth by copying.

For scale: **Inko uses 512 KiB fixed mmap'd stacks with a guard page**
(`rt/src/config.rs`; note its design doc still says 1 MiB and is stale —
changed Feb 2024, commit `a9df553c`). Growable stacks were explicitly
rejected there for masking runaway recursion and complicating FFI.

**Debugging degrades, and the fixes are known.** This is the part usually
discovered too late:

- **gdb and `perf` backtraces run off the end of a switched stack.** The fix
  is one line of CFI at the fiber entry point: `.cfi_undefined rip`
  (`rbp`/`lr`/`x30`/`ra` per architecture), telling the DWARF unwinder to
  stop. Boost.Context does this in its asm trampolines; Seastar does it
  per-architecture in `thread.cc`. It is a real production fix —
  scylladb/scylla#1909 was `perf record --call-graph dwarf` producing *no
  backtrace for the vast majority of samples*.
- **gdb has no fiber-aware mode.** `libthread_db` only understands pthreads.
  Runtimes that want real support ship a Python extension; Go's
  `runtime-gdb.py` is 703 lines and exists to implement `info goroutines`.
- **ASan and TSan have fiber APIs in both GCC and Clang** —
  `__sanitizer_start_switch_fiber`/`__sanitizer_finish_switch_fiber`, and
  `__tsan_create_fiber`/`__tsan_switch_to_fiber`. But Boost.Context's own
  docs say ASan support works **only with its slow `ucontext` backend, not
  the fast assembly one**. Seastar doubles its stack size under ASan and
  zeroes every stack on allocation to avoid false positives.
- **Valgrind's own manual** says `VALGRIND_STACK_REGISTER` is *"unreliable
  and best avoided"* — while Boost.Context and Seastar both use it anyway,
  because there is no alternative. libaco documents that Memcheck produces
  many false positives with shared coroutine stacks.
- **Signals are a live hazard.** libaco documents that a signal arriving
  while the stack pointer points into a heap-allocated shared stack corrupts
  state, and notes you can trigger it from gdb.

## 7. IO, which the concurrency model is useless without

**io_uring cannot be a requirement in 2026.** This was the most clear-cut
finding:

- Google (2023, still live): io_uring was used in **all** kCTF submissions
  that bypassed their mitigations; ~$1M paid for io_uring bugs alone.
  Disabled on ChromeOS, blocked from Android apps, **disabled on all
  production Google servers**. "We currently consider it safe only for use by
  trusted components."
- **containerd removed io_uring syscalls from the default seccomp profile**
  (PR #9320, Nov 2023) and the current `main` branch still has zero
  `io_uring*` entries. That means Docker's default and most managed
  Kubernetes block it unless a container runs `Unconfined`.
- **gVisor**: `io_uring_register` is *unimplemented*, returns invalid
  syscall. GKE Sandbox cannot run a runtime that requires registered buffers.

So io_uring is an optional accelerant behind capability detection, never a
dependency. It is also genuinely best at **storage** IO; for networking even
Rust's ecosystem hedges — `tokio-uring`'s README still describes itself as
"very young" and tokio has not made it the default transport.

**The boring answer is tractable.** A real edge-triggered epoll/kqueue
reactor with timers and a wakeup path is **200–1000 LOC per backend**,
consistently, across four independent production codebases:

| | epoll | kqueue |
|---|---|---|
| libdill | 316 | 350 |
| State Threads | 403 | 455 |
| mio | 246 | 920 |
| Go netpoll | 189 | 337 |

(Plus a backend-agnostic core: libdill's is ~620 LOC, Go's `netpoll.go` is 733.)

**And it needs a blocking thread pool regardless.** Regular-file IO and
`getaddrinfo` have no async primitive. libuv's model is the template:
**4 threads by default**, `UV_THREADPOOL_SIZE`, capped at 1024. Go instead
grows the OS-thread pool on demand — `sysmon` polls on a backoff starting at
**20 µs**, doubling after 50 idle cycles to a 10 ms ceiling, and retakes a P
whose thread has been in a syscall for more than one tick.

## 8. Recommendation

**Decide the sharing model now; implement in stages.**

1. **Commit to move-on-send.** Values transferred between concurrent units
   are moved, not aliased. This is what buys non-atomic refcounting, and it
   is what Nim, Inko, Pony and Erlang each arrived at independently. It is a
   *type system* decision, so it must precede the type system.
2. **Keep `rc_inc`/`rc_dec` IR-level, never inlined by hand into a backend.**
   Already a rule in the README. It is what makes atomic-vs-non-atomic a
   lowering switch rather than a rewrite.
3. **Ship OS threads + channels first** (model C). Weeks, composes with
   anything, and unblocks real programs.
4. **Then choose A or B**, with the bar being: does the language want
   blocking-looking IO badly enough to pay for stack switching in emitted C,
   plus the debugging degradation in §6?

My reading: **model B, stackless actors**, is the strongest fit, and §6
strengthened rather than weakened that.

Model B is the only option that gets real concurrency **without** either of
the two costs every other C-emitting language paid:

- it does not abandon the C stack, because a run-to-completion behaviour never
  needs to be suspended mid-call (what CHICKEN, Cyclone, Gambit and GHC each
  had to do, at the price of restructuring their entire compiler);
- it does not colour functions, because nothing yields in the middle of a
  function (what Nim and Vala pay, with a decade of documented complaints
  about it and about the stack traces it produces).

It also makes non-atomic refcounting sound by construction, and it matches
decisions already made — errors as values, no unwinding, one way to do each
thing.

The honest argument against it stands: run-to-completion is a real ergonomic
difference from Go, and Go-like ergonomics may be exactly the point. If so,
the choice is not "green threads or not" but **"CPS through the whole
compiler, or wait for Cranelift"** — and that is the strongest argument yet
for bringing Cranelift forward, because its prologue stack-limit check is
precisely the primitive emitted C cannot provide.

What is *not* on the table is a middle path where we keep the C stack and add
green threads as a library. Nobody has done it, and V is the cautionary tale
of claiming to.
