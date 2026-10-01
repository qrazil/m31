# Module state: the decision so far

Two questions about what may live at the top level of a module besides types
and functions.

  - **Immutable state — module constants.** Decided 2026-09-21 and
    implemented; §1 below. Normative text: `docs/reference.md` §4.4.
  - **Mutable state — module variables.** **Decided: there is none, and
    there will be none.** §2 lays out the problem, the collision with the
    concurrency model, the options with evidence, and the recommendation
    that was *not* taken. §2a records what was done instead, and it is the
    rule to build on: state a library needs between calls lives in an object
    the program holds.

What `const` means (deep immutability, frozen objects) is its own record:
`docs/const-decision.md`.

---

## 1. Module constants — decided

```c
pub const int MINUTE = 60 * SECOND;
const Array<int> POW10_HI = [-1228264617323800998, ...];
const Map<str, int> KEYWORDS = {"if": 1, "else": 2};
```

### Rules, with the reason for each

| rule | why |
|---|---|
| `[pub] const <type> NAME = <constant expression>;`, in any module, the entry file included | A constant is a declaration, not a statement, so an imported module may hold one; the entry file is not special. |
| At the top level `const` always declares a module constant | One spelling, one meaning. Every top-level `const` in the corpus before this (`012`, `015`, `201`) was already a constant expression, so none had to change. |
| The type is mandatory | The language writes every name's type. A constant is read far from its declaration, which is where a written type pays most. |
| No naming rule | Nothing else in the language is case-checked (types may be lowercase, `017`). The standard library uses `UPPER_SNAKE_CASE` by convention, which keeps constants out of the way of locals — useful because a local may not take a constant's name. |
| The initialiser is a constant expression: literals, other constants, operators, collection literals | Evaluated **by the compiler** (`src/lower/consts.rs`). No calls: a compile-time call needs the whole language at compile time — a second implementation that can disagree with the first. |
| Arithmetic follows the run-time rules; what would trap is an error; a float result must be finite | A constant and the same expression at run time must not disagree. A non-finite constant follows the rule a float literal already follows (§1.5). |
| Constants may reference each other in any order; a cycle is an error printing the whole chain | Declarations are order-independent everywhere else. The chain format is the import cycle's. |
| Types: `int`, `float`, `bool`, `str`, `bytes`, and `Array`/`List`/`Map` of those, nested | Everything the compiler can lay out as static data. `List` and `bytes` are allowed because a `const List<int>` local is legal and `const` means one thing (`docs/const-decision.md`). A user type waits for a use. |
| A constant map may not repeat a key | In a hand-written table a duplicate is a mistake nothing would ever show at run time. |
| At most 2^20 elements per collection | The collection is written out in the emitted C; the limit bounds compile time and file size, not run time. |
| Private unless `pub`; reached as `mod.NAME`; one namespace with the module's functions and types; no local or parameter may take the name | The rules every other module-level name follows (§2.1, §4.1). A parameter is included because it would silently hide the constant for the whole body. |
| Deeply immutable: compile-time refusal of visible writes, run-time trap for the rest; `clone` for a mutable copy | `docs/const-decision.md`. |
| Readable from any thread, and may be passed to `spawn`/`send` without being moved | It is immortal and immutable: no thread ever writes its count or its contents. Checked under ThreadSanitizer (corpus/core/744: eight threads reading the same tables, clean). |

### Representation

A collection constant is **static, immortal data** in the emitted C, laid out
exactly as the runtime lays out the same collection built at run time, with
a count of `RC_IMMORTAL` (`src/emit_c.rs`, `emit_statics`):

  - `Array` — an anonymous struct `{ Obj hdr; int64_t len; int64_t data[N]; }`
    with the same layout as the runtime's `Arr` (whose flexible array member
    cannot be initialised statically in standard C);
  - `List` and `bytes` — the runtime's own `Lst` and `Bytes`, pointing at a
    static buffer;
  - `Map` — the runtime's `Map` and a static `MapSlot` table, **hashed by the
    compiler** with the runtime's hash and probe. That makes the hash part of
    the compiler–runtime contract; both copies say so, and corpus/core/742
    looks up every key of maps large enough to have grown.

All of it is `static const`, so it lands in read-only memory: a write that
got past both the compiler and the runtime check would fault rather than
corrupt a table every thread reads. Retain and release return early on
`RC_IMMORTAL`, so nothing ever writes the header. No initialisation code
runs, so there is no order in which constants come into existence and no
cost at start-up. A scalar constant is folded into every use; a `str`
constant is a string literal.

### The float-formatting table, converted

`lib/__floatfmt.m31` kept 684 128-bit powers of ten as hex text — eight
entries to a string literal, found by a binary search over 86 literals and
decoded a hex digit at a time on every lookup — because there was no constant
array. It is now two `const Array<int>` of 684 64-bit words each, and a
lookup is `POW10_HI[e + 342]`. The generator script checked the new table
entry for entry against the old strings before the corpus ran; the corpus
(which prints floats everywhere) passes unchanged.

Measured on a program printing one million floats spread over the whole
exponent range (i7-8750H, release compiler, best of 5):

| | before | after | |
|---|---|---|---|
| `m31c --emit-c` | 10.0 ms | 11.5 ms | +1.5 ms: 1368 constant elements to evaluate (one loaded-machine run; ~7 ms on a quiet one) |
| emitted C | 175,230 bytes, 7,113 lines | 162,442 bytes, 5,682 lines | −7% bytes (this includes the per-type copy functions `const` snapshots added) |
| `gcc -O2 -c` of it | 0.320 s | 0.290 s | −10% |
| `clang -O2 -c` of it | 0.264 s | 0.237 s | −10% |
| run, 1M floats printed (gcc -O2) | 0.539 s | 0.435 s | **−19%** |
| output | md5 `78deecd3…` | md5 `78deecd3…` | byte-identical |

"Before" is the C the original compiler emitted, against the original
runtime; the two were run interleaved, best of 7, on a quiet machine. An
earlier run on a loaded machine gave the same shape (0.79 s → 0.675 s,
−15%).

---

## 2. Mutable module state — the case, as it stood

Everything in §2 is the record of the argument, kept as it was written. The
answer is in §2a; read that first if you only want the rule.

### The problem

Two library modules want state that outlives one call, and cannot have it:

  - **io.** `io.stdin()` returns a new `File` with its own read-ahead buffer
    each call, so its documentation has to say *call it once and keep it* —
    two of them each read ahead and each hold input the other never sees.
    `io.read_line()` cannot keep a buffer at all, so to leave the rest of
    the input for whoever reads next it reads **one byte per system call** on
    a pipe or a terminal (and two calls per line on a regular file, by
    reading ahead and seeking back). Both are written in `lib/io.m31` with
    the comment "a module cannot hold state".
  - **random.** The system generator cannot buffer the kernel's randomness,
    so every draw is one `getentropy` call where Oro reads 256 octets at a
    time and hands them out (`lib/random.m31`).

Every other library module so far is stateless by nature.

### Why it collides with the concurrency model

`docs/concurrency-decision.md`: values are **moved, never shared**, and
refcounts are **plain, non-atomic** — sound because only one thread can reach
a value at a time. A mutable module variable is the one thing every thread
can reach by name. So:

  - two threads assigning it is a data race on the variable; and
  - two threads merely **reading** a reference out of it is a race on the
    referenced object's **count** — `rc_inc` from two threads loses an
    increment and frees a live object. Under a tracing collector (Go) a race
    is a wrong answer; under non-atomic refcounting it is heap corruption.

Making those counts atomic for "global" objects is not available either: an
object read out of a global flows into locals, fields and collections
everywhere, so either every count is atomic (the cost the concurrency record
refused: atomicity alone was 25% of run time in Swift's measurement) or the
object must never leave the global — which is the actor/owner model below.

Module constants escape all of this because they are immutable **and
immortal**: nobody writes the count. That is why they were cheap to add, and
why nothing about them generalises to variables.

### The options

**Go — package variables, races allowed, a race detector.** Any package may
declare `var x T`, every goroutine can reach it, and the memory model calls a
racy program incorrect without preventing one. `go test -race`
(ThreadSanitizer) finds races only on paths that run, at 5–10× time and
memory. Go gets away with it because its collector is tracing: a racy read
of a pointer is a stale pointer, not a freed one — though a torn read of an
interface or slice header can still crash. With non-atomic refcounts every
race on a reference is a use-after-free. **Not viable here.**

**Rust — `static`, `static mut` is `unsafe`, safe state through types.** A
`static` must be `Sync`; `static mut` can only be touched in `unsafe`, and
the 2024 edition denies taking a reference to one by default. The safe
spellings are `static M: Mutex<T>` (const-constructible since 1.63),
atomics, `OnceLock`/`LazyLock` for lazy initialisation, and `thread_local!`
with `Cell`/`RefCell` for per-thread state. It works because `Mutex<T>`
owns `T` and hands out a guard, and because shared ownership across threads
uses `Arc` — an atomic count, chosen per object. **This language has no
`unsafe`, no `Mutex`, and one kind of count.** A `Mutex<T>` here would have
to move `T` out to the locker and back in on unlock, which is a channel of
capacity one.

**Swift 6 — global mutable state must be isolated.** Under strict concurrency
a global or static `var` is an error ("not concurrency-safe because it is
nonisolated global shared mutable state") unless it is isolated to a global
actor (`@MainActor var x`), is an immutable `Sendable` `let`, or is marked
`nonisolated(unsafe)` — an explicit, greppable opt-out. Swift pays for
atomic counts everywhere; isolation is about data races, not counts. The
lesson that transfers is the shape: **mutable global state is legal only
with an owner** that serialises access.

**Zig — globals plus a `threadlocal` keyword.** `var x: T = ...;` at file
scope is a global with no protection; `threadlocal var` gives each OS thread
its own copy; `std.Thread.Mutex` is a library type. Manual, like C. The
`threadlocal` half is the per-thread option below; the unprotected half is
Go's problem without Go's collector.

**Erlang and Pony — none.** Erlang has no global variables: state is a
process's loop argument (or its private process dictionary), and shared
tables (ETS) are owned by a process and accessed through it;
`persistent_term` is a global that is cheap to read and very expensive to
write (a write can trigger a global GC). Pony has no globals at all: state
lives in actors, and only `val` (deeply immutable) data may be shared — the
same line module constants draw here. Both have real parallelism with
per-process or non-atomic counts, because nothing mutable is shared.

**Per-thread module state.** Each thread gets its own copy of a module
variable, initialised on first use (Zig's `threadlocal`, Rust's
`thread_local!`, C's `_Thread_local`). No race is possible and counts stay
plain, because each copy is reached by one thread. The costs are semantic
and scale:

  - a spawned thread does **not** see what its parent set — the classic
    thread-local surprise;
  - for stdin it is the wrong answer: a process has **one** standard input,
    and per-thread read-ahead buffers reproduce today's bug across threads;
  - with green threads (stage 3 of the concurrency record, targeting
    millions of threads) it means per-green-thread storage: a pointer to
    switch on every context switch and a table allocated per thread that
    touches a module variable. Java met exactly this with virtual threads:
    `ThreadLocal` works on them but is discouraged, and `ScopedValue`
    (JEP 446, previewed from JDK 21 and finalised in JDK 25) exists because
    millions of threads × thread-locals, with inheritance, did not scale;
  - Go deliberately has no goroutine-local storage at all, for the same
    reasons.

It is right for the entropy buffer (a per-thread pool is what Rust's
`thread_rng` and Go's runtime generator, kept per OS thread, do) and wrong
for stdin.

**No mutable module state; pass state explicitly.** What the language does
today. Every piece of state is a value somebody holds and hands on, so the
move rule covers it with no new concept. Go's own answer for buffered stdin
is this shape — `bufio.NewReader(os.Stdin)`, an explicit object, while
`os.Stdin` itself is unbuffered. The cost is exactly the two problems above:
a library cannot give a convenient `read_line()` that is also efficient, and
cannot pool randomness.

### Recommendation

**Keep "no mutable module state" as the language rule, and move the
process's two genuine singletons — the stdin read-ahead buffer and the
entropy pool — into the runtime, behind primitives.**

The reasoning: every option that adds module variables to the language
either breaks the non-atomic-count guarantee (Go, Zig), or needs a new
ownership concept — a lock type, an actor, per-green-thread storage — whose
only customers today are two library functions. Neither customer actually
wants *language-level* state:

  - **stdin is a process-wide resource**, like the descriptor it buffers.
    Its buffer belongs where the descriptor does. The runtime already holds
    exactly one such thing — `print`'s output buffer, `out_buf` behind
    `out_lock` in `runtime/rt.c` — for exactly this reason, and it is C
    memory, not a language object, so no refcount is involved. A
    `__stdin_read(buf, off, n)` primitive reading through a runtime-owned
    buffer under a mutex lets `read_line()` read in chunks and makes every
    `io.stdin()` share one buffer, so the "call it once" warning goes away.
  - **the entropy pool is naturally per carrier thread** and invisible:
    `rt_entropy` keeps a `_Thread_local` 256-octet buffer and refills it.
    Nothing in the language changes.

**Cost:** no compiler change and no language change. Roughly 100–150 lines
of C in `runtime/rt.c` (a mutex-protected stdin buffer with read, and a
thread-local entropy buffer), one new primitive in `runtime/rt.h`,
`lib/io.m31` rewired to use it (`read_line`, `stdin()` and the stream
fill), `lib/random.m31` unchanged, a `sys_test` case for the new primitive,
and corpus tests for mixed `read_line` / `io.stdin()` reading and for
reading stdin from two threads. A day's work.

**What it costs in principle:** it bends `docs/stdlib-seam.md`'s rule that a
primitive returns a raw OS fact and the library does everything else in
source — the stdin buffer's logic moves to C. That is the honest price, and
it is the same price `print` already pays.

**If user programs later need their own module state** — a cache, a
counter, a registry — the next step is **per-thread module state with lazy
initialisation**, explicitly declared (`threadlocal` or similar), because it
is the only option that keeps plain counts without new ownership machinery.
Its cost: a new top-level declaration in the parser and formatter; lowering
each access to a runtime call returning the calling thread's slot object
(`rt_tls_get(slot, init)`, initialising on first use by calling the
declared initialiser); a per-thread slot table in the runtime, released
(each slot's value `rc_dec`'d) when the thread ends so the leak check
stays exact; one pointer switched per green-thread context switch in stage
3; about 400 lines across compiler and runtime, plus the documentation of
the no-inheritance rule. It should wait for a program that needs it.

**Rejected outright:** Go-style unprotected globals (a race is heap
corruption under plain counts), and a `Mutex`/`static mut`-style escape
hatch (the language has no `unsafe`, and a lock type that moves its value
in and out is a channel with a different name — channels already exist).

---

## 2a. Mutable module state — decided: there is none

**The recommendation in §2 was not taken.** Moving the two singletons into
the runtime would have bought convenience by putting library logic in C and
giving the language two answers to "where does state live". The rule instead
is the one the language already had, made explicit:

> **There is no mutable module state and there will be none. State a library
> needs between calls lives in an object the program holds.**

Both customers in §2 were answered that way, and neither needed a language
change:

  - **io.** `io.Buffer` (`lib/io.m31`) is "bytes I have fetched and not yet
    handed out": a read position, compaction when the dead prefix is half the
    buffer, so reading from the front is amortised O(1). `io.File` holds one
    for its own read-ahead, and `io.Buffer` is a `Stream` in its own right.
    `io.read_line()` was **removed** rather than made stateful: its contract
    — never read past the line — was exactly what made it one system call per
    byte on a pipe, and it cannot be kept and made fast. Line reading is
    `io.read_line_of(stream, limit)` over `io.Stream`, and the program holds
    the stream (`io.stdin()`, called once).
  - **random.** `random.system()` answers a `System` holding an `io.Buffer`
    pool, filled 256 octets at a time — the largest block `getentropy`
    answers in one call, so a refill is one system call. The module-level
    `random.integer(1, 6)`, `random.uuid4()` and the rest were **removed**
    for the same reason `io.read_line` was: they could not hold the pool, and
    a convenience that is quietly 256 times the work of the spelling beside
    it is worse than no convenience. Measured on 10^6 one-octet draws:
    1,000,000 `getentropy` calls and 0.87 s before, 3,907 calls and 0.14 s
    after.

**What this costs, honestly.** Two things get longer to write:
`random.system().uuid4()` instead of `random.uuid4()`, and a held
`io.stdin()` instead of `io.read_line()`. Both now say where the state is,
which is the point — and both are what Go writes too (`bufio.NewReader(
os.Stdin)`, an explicit object, beside an unbuffered `os.Stdin`).

**What is still true from §2:** if user programs ever need module state, the
next step is per-thread module state with lazy initialisation, for the
reasons given above. Nothing here makes that easier or harder; it removes the
two library customers that were the whole case for doing it now.
