# Concurrency and refcounting

Status: **research complete; the decision is made in
`docs/concurrency-decision.md`.** This document keeps the evidence and the
rejected alternatives, because the reasoning is worth more than the verdict
and one section of it was wrong once already. Nothing here is implemented.
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
| **Koka** | **sign-bit hybrid** | `rc > 0` non-atomic, `rc < 0` thread-shared and atomic, `INT32_MIN..` sticky/immortal. Escaping to another thread promotes the reachable graph once. |

### Read Koka before writing any of this

**Koka is a precisely reference-counted language (Perceus) whose compiler
emits C.** That is our exact design, already shipped. Its answer to the
atomicity question is one bit:

```c
//  > 0        : non-thread-shared reference  (plain ++/--)
//  < 0        : thread-shared                (atomic)
//  INT32_MIN..: sticky (overflow) -- never freed
static inline bool kk_refcount_is_thread_shared(kk_refcount_t rc);
```

A decrement already needs a zero test, so the marginal cost of the sign test
is near nil. `kklib/include/kklib.h` and `kklib/src/refcount.c` are the
blueprint.

Pair it with **Nim's `--mm:atomicArc` fast path** (merged Aug 2026): do an
acquire-load first and skip the read-modify-write entirely when the count is
already unique. That took Nim's atomic overhead from **+33% to +1.5%** on
gcbench.

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

## 4. One way to dodge the problem entirely

Before §6 establishes that stackful green threads *are* achievable from
emitted C, it is worth recording the option that needs no stacks at all —
because it remains the cheapest thing to build.

**Pony's actors are stackless.** Verified in `pony_actor_t` (`actor.h`): it
contains a message queue, a per-actor heap, GC bookkeeping and flags — and
**no stack pointer, no context, no coroutine state**. `ponyint_actor_run()`
is an ordinary loop on the scheduler thread's own native C stack, and
`handle_message()` calls the behaviour body as a plain function call that
runs to completion and returns. No `setjmp`, no `ucontext`, no fiber library
anywhere in the runtime.

That matters because stack switching is the whole of the difficulty, and a
run-to-completion actor never switches stacks. Every item in §6's debugging
list — CFI, ASan fake stacks, TLS caching, shadow stacks — simply evaporates.

The cost is real and should not be glossed: run-to-completion means a
behaviour **cannot block or yield in the middle**. You write message-driven
code, not Go-style blocking code. Go's whole ergonomic pitch is that
`conn.Read()` looks synchronous; Pony's is that nothing ever blocks.

## 5. The three models

| | A. Stackful green threads | B. Stackless actors | C. OS threads + channels |
|---|---|---|---|
| Feels like | Go | Erlang, Pony | Java, C++ |
| Blocking-looking code | yes | **no** | yes |
| Stack switching in C | **possible, fixed stacks only** | none needed | none needed |
| Cheap to spawn | yes (~KB) | yes (~100s of bytes) | no (~MB, ~10µs) |
| Refcount atomicity | atomic, unless pinned | non-atomic if messages move | atomic |
| Effort with the C backend | ~35 engineer-weeks | **low** | low |

**A** is what was asked for, and §6 establishes it is genuinely available —
at the price of permanently fixed stacks and about eight months of work.
**B** sidesteps the problem and is what Pony and Erlang prove out.
**C** is the boring fallback and composes with either later.

## 6. Can we have Go-style green threads from emitted C?

**Yes — and an earlier draft of this document said no. That was wrong, and the
correction matters, so it is recorded rather than quietly edited away.**

The first survey found that CHICKEN, Cyclone, Gambit and GHC-via-C all
abandoned the C call stack, and generalised that into "you must abandon the C
stack to get green threads." The deeper pass falsifies the generalisation:

- **Go 1.0–1.4 shipped a complete M:N scheduler written in C**
  (`src/runtime/proc.c`) with three small assembly functions. `runtime·gogo`
  is fourteen instructions.
- **Inko's context switch is thirteen instructions of assembly**; nothing else
  in its work-stealing scheduler depends on LLVM.
- **Gambit ships M:N green threads from generated C today.**

Those Scheme implementations abandoned the C stack to get *unbounded recursion
and cheap first-class continuations*, not because green threads demanded it.

### The real constraint, and it is absolute

> **Fixed-size stacks, forever.** You can never have growable or copying
> stacks.

Copying a stack means rewriting every pointer into it, which means knowing,
for every pointer-sized word of every frame, whether it is a pointer. Go gets
this from `FUNCDATA_LocalsPointerMaps`, emitted by the Go compiler. With
gcc/clang you control neither frame layout, spill slots, callee-saved spills,
the red zone, nor register-resident derived pointers.

WG21 P1364R0 states it directly: pointer adjustment is *"possible ... in a
programming language with precise garbage collector, such as Go, but
unfeasible in more traditional languages ... Recognizing that limitation Rust
developers went with the virtual memory and guard page approach."*

Note the supporting detail from Go's own release notes: Go 1.5 removed its
in-tree C compiler, which existed *"in part to guarantee the C code would work
with the stack management of goroutines."* Go needed a **custom C compiler**
to make C coexist with growable stacks. We would be using gcc.

Segmented stacks are not a way out either. `gcc -fsplit-stack` is x86-Linux
only, needs the deprecated gold linker, breaks at every uninstrumented call
boundary — and reintroduces the **hot split** problem that made Go abandon
segmented stacks in 1.3 (a call at a segment boundary inside a loop pays an
allocate/free pair every iteration). Rust hit the same wall and also fled.

### What that costs, concretely

Real stack sizes in shipping systems: **Inko 512 KiB**, libdill 256 KB, State
Threads 128 KB, V 256 KB. Go's 2 KB is unavailable to us precisely because it
depends on copying.

And the ceiling nobody mentions: **the binding limit is VMA count, not
memory.** One `mmap` plus one `PROT_NONE` guard page is **2 VMAs per green
thread**, against a `vm.max_map_count` that defaults to **65530** — so
**~32,000 green threads**, then `mmap` starts failing. (This machine ships
1,048,576, so it is distro-dependent.) Budget for **10⁵ green threads with
64–256 KB stacks, given a raised limit** — not Go's millions. Any project
claiming otherwise on a default kernel is wrong, and that should be in our
docs from day one rather than discovered by a user.

**One advantage we have that no library runtime does:** because we emit the C,
we can emit a stack probe at function entry and skip guard pages entirely,
packing stacks densely in one slab — 1 VMA for thousands. CHICKEN has shipped
exactly this for 25 years; its generated C contains
`if(!C_stack_probe(&a)){ ... }` on every function entry, using the address of
a local as a stack-pointer proxy. Gambit's `___POLL_TRIGGER` does stack
overflow, GC request **and preemption** in one compare-and-branch.

### Mechanism

| | switch cost | portability | health |
|---|---|---|---|
| **own asm switch** | **~9 ns / 19 cycles** | ~40–60 lines per arch | — |
| **Boost.Context** `fcontext` | 9 ns | widest arch coverage anywhere | **alive**, commits Sept 2026, BSL-1.0 |
| `ucontext` `swapcontext` | **547 ns** | removed from POSIX 2008; **absent from musl entirely** | obsolescent |
| Windows fibers | 49 ns | Windows only | stable |
| libaco / libco / libdill / libmill | ~6–10 ns | narrow | **all dead**; libdill.org now redirects to a domain squatter |

The 60× `ucontext` gap is fully explained: glibc's `swapcontext.S` performs an
`rt_sigprocmask` syscall on every switch. Boost's rationale says it in one
line — *"Context switches do not preserve the signal mask on UNIX systems."*
And **musl has no `ucontext` implementation at all** — the header declares the
functions, there is no source directory. You get a link error, so Alpine is a
hard no.

libaco's 10.29 ns claim independently verifies against Boost's 9 ns for the
same six-register save, but it is x86-only and dead since 2022. Contrary to
common belief it has **no CFI directives** — and neither does Boost's
`jump_fcontext`. The folklore that these libraries handle the debugger problem
for you is wrong.

### The correction to the stackless-CPS section

Everything said earlier about function colouring stands, and one consequence
deserves more weight than it got: **for a language whose stdlib is written in
itself, colouring forks the stdlib permanently.** The moment `read_line_of` or
`Socket.recv` can suspend, they are coloured, and so is every function that
transitively calls them — so there is no uncoloured `sort` that takes a
comparator which might do IO. That is the "two ecosystems" complaint Nim users
have, structurally.

`musttail` does not rescue it. It is real now (Clang 13, **GCC 15**, even
MSVC) and works through function pointers at `-O0`, but on
riscv64/arm32/ppc64le/mips64/loongarch64 it produces *"fatal error: error in
backend: failed to perform tail call elimination"* — a backend abort you
cannot feature-detect from C source. And it solves control transfer, not frame
allocation. (Also worth knowing: CPython's headline "10–15%" from tail calls
was an artefact of a poisoned Clang baseline; their docs now say **3–5%**.)

**Zig is the decisive data point.** A systems language with no GC and a
full-time team shipped stackless `async`/`await`, **deleted it** (issue #6025,
closed as not planned), spent five years, and in **0.16.0, April 2026**,
landed `Io.Evented` — *"userspace stack switching with work stealing (M:N
threading / green threads / stackful coroutines)."* They arrived at stackful.

### Debugging: real, fixable, and one silent-corruption class

- **gdb/perf backtraces** run off a fresh mmap'd stack. Fix: `.cfi_undefined
  rip` in the trampoline **and** zero the return-address slot (libgcc honours
  either; gdb needs the CFI). glibc's `clone.S` does exactly this. Parked
  green threads are in no thread's register set, so plan a Go-style
  `goroutine` gdb command regardless.
- **ASan is not cosmetic here.** `detect_stack_use_after_return` is **on by
  default on Linux**, locals move to a per-*thread* fake-stack arena, and two
  fibers on one carrier will hand each other the same fake frames — **genuine
  cross-fiber corruption, not a false positive.** Call
  `__sanitizer_start_switch_fiber`/`__sanitizer_finish_switch_fiber` around
  every switch, and keep an annotated slow path beside the fast asm one.
- **TSan actually works** — `__tsan_create_fiber`/`__tsan_switch_to_fiber` give
  each fiber its own vector clock, and upstream tests migrate a fiber between
  two pthreads 1000 times cleanly. Caveat: fiber states skip stack discovery,
  so **pooled and reused stacks produce false races**; add a no-pooling TSan
  mode.
- **Intel CET shadow stacks** fault on the first `ret` in a resumed coroutine
  unless you allocate and swap a parallel shadow stack.
- **Do not disable `-fstack-protector`** to make this work. Nim's `std/coro`
  disables `_FORTIFY_SOURCE` *for the entire program* as a workaround — a
  permanent hardening regression imposed on every user by a green-thread
  feature.

**The one that will silently cost a week** is TLS caching. P1364R0, verbatim:
a compiler may cache a TLS address in a callee-saved register, and after the
fiber migrates, *"if we are lucky, the original thread is still alive, and we
simply corrupt the value of the thread local of a different thread ...; if we
are unlucky, the thread could have quit, and its memory could have been reused
resulting in use after free."* Microsoft's answer was a compiler flag "off by
default and rarely turned on."

**We can fix this where C++ fiber libraries structurally cannot**, because we
emit the C: make every yield point an opaque barrier that clobbers the
relevant registers, and never emit a cached TLS base across one. This is why
Go reloads `g` from TLS after any call that can switch stacks. It is a
silent-corruption class, so it has to be designed in, not debugged later.

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

**Decide the sharing model now; build in stages; do not wait for Cranelift.**

### The Cranelift question is settled, and the answer is "it would not help"

This was the assumption worth checking, because an earlier draft suggested
deferring green threads until Cranelift. Cranelift offers two relevant things,
and neither unblocks anything:

1. **A prologue stack-limit check** (`FunctionStencil::stack_limit`). Real,
   and more precise than C — Cranelift knows the true frame size. But CHICKEN
   has emitted `if(!C_stack_probe(&a))` on every function entry for 25 years,
   and with fixed stacks plus a guard page we may not want the check at all.
   Nicer, not enabling.
2. **Precise stack maps** (`declare_value_needs_stack_map`). These are
   **GC-reference maps for a moving collector** — they let the collector
   relocate *heap objects* the frames point at. They are **not** Go's
   `FUNCDATA_LocalsPointerMaps`, which classify every pointer-sized word in
   every frame so the *stack itself* can be relocated.

So **even on Cranelift we would not get copying stacks** — and being reference
counted with no tracing GC, we do not need precise roots. The thing we would
be waiting for does not exist in the thing we would be waiting for.

Meanwhile the entire design below is backend-independent. Nothing in Inko's
scheduler touches LLVM, and Go shipped all of it in C in 2012.

### Order of work

1. **Commit to move-on-send.** Values transferred between concurrent units are
   moved, not aliased. This is what buys non-atomic refcounting, and Nim, Inko,
   Pony and Erlang each arrived at it independently. It is a *type system*
   decision, so it must precede the type system.
   **Note the negative result:** Swift proves a `Sendable`-style isolation
   system does **not** buy non-atomic refcounts — SE-0430 states flatly that it
   "does not change how any existing code is compiled". Isolation proves
   non-concurrent *access*; you need *ownership* (Inko's `uni`, Nim's
   `Isolated[T]`, Pony's `iso`).
2. **Do static refcount elision before worrying about atomicity.** Swift's
   optimiser already removes up to 97% of RC operations and RC is *still* 32%
   of runtime. That ordering matters more than the atomic/non-atomic choice.
   Then add Koka's sign bit and Nim's unique-check fast path (§3).
3. **Keep `rc_inc`/`rc_dec` IR-level, never hand-inlined into a backend.**
   Already a rule in the README, and it is what makes atomicity a lowering
   switch rather than a rewrite.
4. **Ship OS threads + channels first** (model C). Weeks, composes with
   anything, unblocks real programs.
5. **Then choose A or B.**

### If model A (stackful green threads)

The shape, all of it verified in shipping systems: **own ~40–60 lines of
assembly per architecture** (x86-64 SysV and aarch64 first; read Boost.Context's
`.S` files for the per-arch quirks), **fixed mmap'd stacks with a guard page**,
64 KB default, power-of-two aligned so `sp & -SIZE` finds the green thread with
no TLS lookup (Inko's trick), **one carrier per core with work stealing**,
**cooperative preemption via an epoch counter checked at loop back-edges** —
not signal-based, because every precondition Go checks for that is per-PC
compiler metadata we cannot emit from C.

Rough estimate for Linux + macOS on x86-64 + arm64: **~35 engineer-weeks**, of
which the two riskiest line items are not the obvious ones — the **park/unpark
↔ netpoll CAS state machine** (~3 weeks; it closes the lost-wakeup race where
data arrives between EAGAIN and parking) and the **TLS-caching discipline in
codegen** (~2 weeks, silent corruption if wrong). A Linux-x86-64-only proof of
concept is **6–8 weeks**.

**Our structural advantage, and it is the largest one available:** because we
emit the C, we can wrap every FFI call site in compiler-emitted
`enter_blocking()`/`exit_blocking()` — Go's cgo model — and *warn at compile
time* when an unannotated foreign function is called from a green thread. No
library runtime can do that. Erlang requires manual annotation; Java's Loom
explicitly refuses to compensate for native-frame pinning; async-std tried
automatic detection without compiler support and it never merged.

### If model B (stackless actors)

Cheaper by roughly an order of magnitude, needs none of §6's debugging work,
and makes non-atomic refcounting sound by construction. The cost is
run-to-completion ergonomics.

### My reading

**Model B remains my recommendation**, but on narrower grounds than before. It
is no longer "A is impossible" — A is possible, and Go/Inko/Gambit prove it.
It is that A costs about eight months plus a permanent ceiling of ~10⁵ threads
with fixed stacks, and B gets concurrency now with neither.

If Go-like ergonomics are the point, **build A, and build it now rather than
after Cranelift** — the verdict there is unambiguous, and waiting would also
mean retrofitting concurrency into a stdlib written without it, which means
rewriting the stdlib.

One thing to design in either way: **async machinery creates reference
cycles**, and a refcounted language feels that specifically. Chronos's
maintainer on Nim's stdlib: the closure and the iterator "would reference each
other ... nothing would be released until the (extremely slow) mark and sweep
pass would run." Nim shipped a cycle collector largely because of this. Make
the callback edge weak, or make the green thread itself the continuation —
which stackful does for free.

### Deferred, with reasons

- **io_uring** — see §7. Blocked by default in Docker *and* Podman (I checked
  both seccomp profiles: 428 and 479 allowed syscalls respectively, **zero**
  io_uring entries), disabled on all production Google servers, and libuv
  spent eighteen months adding it and then disabled SQPOLL by default. Go has
  declined for seven years.
- **Windows** — libuv spends 27,032 lines on Windows against 26,464 for
  thirteen Unix variants. When it happens, use AFD readiness emulation, not
  native IOCP: IOCP forces buffer pinning into the memory model, and Go's
  cancellation path blocks *uninterruptibly* waiting for the kernel to return
  a buffer. Ignore Windows IoRing entirely — its opcode list contains no
  socket operations at all.
- **Anything above ~10⁵ green threads.** Say so in the docs rather than
  letting a user discover `vm.max_map_count`.
