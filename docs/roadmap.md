# Features and roadmap

What the language has, what it needs before it can be frozen, and what it is
deliberately never going to have.

The plan is C's and Oro's: get to a small, complete surface, then **freeze**
it. A frozen language is one people can write against for a decade. Freezing
the wrong thing is worse than not freezing, so the list below is ordered by
what a freeze actually requires, not by what is fun.

`docs/reference.md` is the normative statement of the current surface.
Anything marked done there is tested; the corpus is the proof.

---

## Where it stands

| | |
|---|---|
| Corpus | 119 programs — 38 behaviour, 62 diagnostics, 12 traps, 7 Go twins |
| Oracle | gcc and clang, each at -O0 and -O2, all four must agree |
| Leaks | every behaviour program asserts `__rc_live=0` at exit |
| Warnings | emitted C must be clean under `-Wall -Wextra` |
| Dependencies | none — `Cargo.lock` names one package |
| `unsafe` | none, in the compiler or the runtime |

---

## Done

**Types.** `int`, `bool`, `str`, `void`. Structs with per-field defaults.
Structural interfaces dispatched through a vtable in the object header.
Embedding with forwarder methods synthesised to a fixpoint. `distinct` types,
erased before the IR so they cost nothing. Monomorphised generics on types
and functions.

**Collections.** `Array<T>` fixed-length, `List<T>` growable, `Map<K, V>`
open-addressed with tombstones. One name per question across all of them and
across `str`: `size()` and `contains()`. Plus indexing, `push`, `pop`,
`insert`, `remove_at`, `clear`, `reverse`, a stable `sort()`, and `keys()` /
`values()` on a map.

**Functions.** Default arguments, with one calling rule: mandatory
parameters are positional, optional ones are named. Methods on any type, with
the receiver's fields in scope bare. Operator methods — `add`, `sub`, `mul`,
`div`, `rem`, `eq`, and a single `cmp` behind all four orderings.

**Statements.** `if` / `else if` / `else`, `while`, `for ... in`, `break`,
`continue`, `return`, `const`. No shadowing.

**Memory.** Non-atomic reference counting with every retain and release
inserted by the compiler. Arguments borrowed, returns owned. Immortal string
literals. No GC — and therefore no cycle collection: a cycle leaks.

**Concurrency.** `spawn` and `Chan<T>` on OS threads, with `send`, `recv`,
`close`. Values crossing a thread boundary are moved, checked at compile time
and backed at run time by a transitive uniqueness check: the whole graph
reachable from a moved value must be unreachable from anywhere else.

**Tooling.** A C emitter, a formatter with one canonical layout and no
options, single-line diagnostics with the source line echoed, and `gates.sh`.

---

## Before the freeze

These are the things whose absence would make a frozen language not worth
freezing. Roughly in order.

### 1. Enums with payloads, and pattern matching

One feature, not two: an enum you cannot match on is useless.

```c
enum Option<T> { None; Some(T); }
enum Result<T, E> { Ok(T); Err(E); }
```

This moved ahead of errors because `Option` and `Result` are not
primitives -- they are ordinary enums, and errors-as-values is a *use* of
them rather than a separate mechanism. Building errors first would mean
building a one-off `(T, error)` shape and then rebuilding it.

It is also the answer to null. The language has none and is not getting any:
with null every reference might be absent and nothing makes you check, while
with `Option<T>` only the things typed that way can be, and the compiler will
not let you skip it. JSON's `null` is then a *variant* of a JSON sum type, not
a pointer that can blow up -- the same for an empty CSV field and a nullable
database column. Absence needs representing; it does not need a hole in every
type.

Origin, since it comes up: not Rust's. ML had `option` with `NONE`/`SOME`
around 1973, Haskell has `Maybe` and `Either`, and Rust took both from that
family -- its contribution was making them ordinary in a systems language and
adding `?` so propagation is not painful. The null they replace is older:
Tony Hoare put it in ALGOL W in 1965 and later called it his billion-dollar
mistake.

### 2. Errors

The largest hole. Today every fault traps: a bad index, a missing key, a
closed channel. That is fine for a bug and wrong for a condition a program
should handle — a file that is not there is not a bug.

The decision to make is errors-as-values (Go, Rust) against exceptions. The
shape that fits refcounting and a C backend is values: no unwinder, no
landing pads, nothing to interact with the release points the compiler has
already placed. What it needs from the type system is multiple returns or a
sum type, which is why the next item is entangled with this one.

**Blocks the freeze.** A language that cannot report a recoverable failure
cannot have a standard library worth the name.

Once enums exist, errors are a `Result<T, E>` and a propagation operator,
not a new mechanism. The open question is only how much sugar to put on
propagation, and whether a trap can ever be turned into an error.

### 3. A string library

`size()` and `concat` are the whole of it today. A usable language needs
`substr`, `index_of`, `contains`, `starts_with`, `ends_with`, `split`,
`join`, `trim`, case conversion, and conversion between `int` and `str`.

None of it is hard. It is deliberately after errors, because `to_int("abc")`
has to return something, and what it returns is the errors decision. These
land as methods on `str`, which now has method dispatch.

### 4. Modules

One file is the whole program today. That is tolerable for a corpus and not
for anything else. Needs: a unit of compilation, a visibility rule, and a
name resolution order. Kept behind errors because a module system that has to
be revised once errors land is a module system written twice.

### 5. Standard library

The stated goal is Oro's and Go's: a standard library good enough that most
programs need nothing else. Needs modules to live in and errors to report
with. Minimum: strings, sorting, a file and stdin API, time, math, and a
`Hashable` interface so `Map` takes a user type as a key.

### 6. Closures

Also gates a nicer `spawn`. The reason they are late is that closures plus
reference counting is the most common way to build a cycle, and a cycle leaks
in a language with no collector. Worth doing only with an answer to that.

---

## After the freeze

Implementation work that does not change the surface, so it can land at any
time — including after the language is frozen. That is the point of freezing
a *language* rather than a *compiler*.

**Green threads.** Stage 3 onward of `docs/concurrency-decision.md`: context
switch, slab stacks with probes, a work-stealing scheduler, an epoll reactor,
blocking-FFI handoff. `spawn` and `Chan<T>` already have the surface they
will keep — that was the reason to ship threads before scheduling them.

**Atomic refcounts**, if the move rule ever proves too strict. Two runtime
functions, not an IR change.

**A second backend.** The C emitter is deliberately replaceable; Cranelift is
the placeholder for a direct one. Cross-compilation is a requirement, and the
C emitter already satisfies it.

**Mortal channels.** Channels never free today, because a channel is the one
value exempt from the move rule and so the one refcount that would genuinely
race. Fixable with an atomic count on that single type.

**Performance.** No optimiser of our own — the C compiler is the optimiser.
Worth measuring before assuming otherwise.

---

## Not going to happen

From `docs/reference.md` §9, with the reasons:

  - **A garbage collector.** Refcounting is the decision. Cycles leak, and
    that is the price.
  - **Inheritance and method overriding.** Embedding plus interfaces covers
    the reuse; the fragile base class does not come with it.
  - **Function overloading.** One name, one function. `print` picks a runtime
    helper by static type, which is not user-visible.
  - **Operators beyond the fixed set**, and no changing them on a built-in.
  - **Null.** Every declaration initialises, there is no zero value, and
    absence is spelled `Option<T>` once enums exist. Not a gap -- the
    replacement is strictly better, because the compiler enforces the check.
  - **Type aliases.** An alias that does not enforce is a comment with
    syntax; `distinct` is the version that enforces.
  - **Shadowing.** A name means one thing where a reader can see it.
  - **`unsafe`, raw pointers, manual allocation.**
  - **Reflection and downcasting from an interface.**

---

## Open questions

Written down because they are unresolved, not because they are unimportant.

  - Does a spawning scope wait for its children? Structured concurrency is a
    real improvement on Go's fire-and-forget, and it is a surface decision,
    so it has to be settled before the freeze.
  - Cancellation. libdill's model — killing a thread makes every blocking
    call in it return an error — fits errors-as-values and needs no unwinder.
  - Does `spawn` keep taking a function plus arguments, or a closure?
  - Constraints on type parameters. Today a bad instantiation fails when it
    is checked, naming the instantiation, which is C++'s error experience.
  - Integer width. `int` is 64-bit and deliberately unqualified so the IR can
    choose per target. Whether sized types ever become spellable is open.

---

## Known bugs

  - `rc_debug.h`'s `__rc_live` is a plain non-atomic `static long` mutated
    from every spawned thread, so the leak oracle can report a wrong count on
    a threaded program. The oracle checking the refcounts is itself racy.
  - Vtable slots are assigned per method *name*, while each call site casts
    the slot to the signature it computed. Two interfaces declaring the same
    method name with different signatures share a slot. Not reachable today —
    assignment demands an exact per-method signature match and refuses
    interface-to-interface assignment — but it becomes type confusion through
    a function-pointer cast the day interface embedding arrives.
