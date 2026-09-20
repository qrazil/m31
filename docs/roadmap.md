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
| Corpus | 152 programs — 44 behaviour, 86 diagnostics, 15 traps, 7 Go twins |
| Oracle | gcc and clang, each at -O0 and -O2, all four must agree |
| Leaks | every behaviour program asserts `__rc_live=0` at exit |
| Warnings | emitted C must be clean under `-Wall -Wextra` |
| Dependencies | none — `Cargo.lock` names one package |
| `unsafe` | none, in the compiler or the runtime |

---

## Done

**Types.** `int`, `float`, `bool`, `str`, `void`. Structs with per-field defaults.
Built-in `Option<T>` and `Result<T, E>`, with `Map.get` and `index_of`
returning an `Option`. **Enums with payloads**, generic, matched exhaustively with no fallthrough
and no `default` -- which is what makes `Option<T>` and `Result<T, E>`
ordinary library types rather than language primitives.
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

**Errors.** Built-in `Option<T>` and `Result<T, E>`, the `?` operator with
exact error-type matching, and a discarded `Result` as a compile error. The
one thing left is what `E` should be in a standard library, which
`docs/errors-decision.md` says to settle last -- once there is a library to
say what actually fails.

**Strings.** `size`, `substr`, `contains`, `index_of`, `starts_with`,
`ends_with`, `split`, `trim`, `to_upper`, `to_lower`, `repeat`, and `join` on
a collection of them. Byte-oriented and ASCII-only where case is involved,
said so in the reference. Still missing: `int`/`float` to and from `str`,
which waits on the conversion interfaces and on what an error's type is.

**Tooling.** A C emitter, a formatter with one canonical layout and no
options, single-line diagnostics with the source line echoed, and `gates.sh`.

---

## Before the freeze

These are the things whose absence would make a frozen language not worth
freezing. Roughly in order.

### 1. Errors

**Designed: `docs/errors-decision.md`.** Settled there: errors are values
and not exceptions; `Option<T>` and `Result<T, E>` are built in, because a
built-in method cannot return a user-defined type and without them
`index_of` and `parse_int` cannot exist at all; and no trap that exists
today becomes an error, because a trap is for a bug in the program and an
error is for the world. Open there, with recommendations: whether `Map.get`
starts returning `Option<V>`, what propagation looks like, and whether an
ignored `Result` is an error.

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

Enums exist now, so `Result<T, E>` is already writable -- see
`examples/enums.src`. What is missing is the ergonomics and the decisions
around them:

  - a propagation operator, so a chain of fallible calls is not a staircase
    of `match`;
  - whether the standard library ships one blessed `Result`, or every module
    declares its own;
  - whether any of today's traps become recoverable errors, and which. An
    index out of range should probably stay a trap; a file that is not there
    should not be one.

### 2. Conversions: every `to_X` is an interface

The rule is one rule, and it turns on which side of the conversion varies.

**Conversion dispatches on the SOURCE, so it is an interface.** All of them,
uniformly:

```c
interface ToStr   { str to_str(); }
interface ToInt   { int to_int(); }
interface ToFloat { float to_float(); }
```

A type implements whichever make sense -- `Price` has `to_int` and `to_str`,
`Point` has only `to_str`. Two things make this cheap: interfaces are
structural, so a type with a `to_str` method satisfies `ToStr` with no
`implements` clause, which lets `print` find the method by name AND lets a
user write `void show(ToStr x)` from the same declaration. And it matches the
precedent operator methods already set -- `eq`, `cmp` and `add` are found by
name too.

It also closes a hole that exists today: `print` refuses a user type with the
diagnostic "there is no way for a type to say how it prints". This is that
way.

**Parsing is a different operation and gets a different name.** `"42"` to an
int reads text and can fail; the source is always `str` and it is the TARGET
that varies, which single dispatch cannot express. So `str.parse_int()`
returning a `Result`, not `to_int`. Calling both `to_int` is what made them
look like one inconsistent family; they are two consistent ones.

Parsing therefore waits for errors. Conversion does not.

**Numeric conversion between built-in types stays built in.** `int` to
`float` and back has no user extension point and no failure worth a
`Result`, so `float(x)` and `int(f)` fit the syntax `distinct` already uses.
`int(f)` truncates, and traps on NaN or a value outside the range -- the C
cast is undefined there, which is exactly the kind of thing the gcc/clang
differential would catch later rather than sooner.

### 3. Modules

One file is the whole program today. That is tolerable for a corpus and not
for anything else. Needs: a unit of compilation, a visibility rule, and a
name resolution order. Kept behind errors because a module system that has to
be revised once errors land is a module system written twice.

### 4. Standard library

The stated goal is Oro's and Go's: a standard library good enough that most
programs need nothing else. Needs modules to live in and errors to report
with. Minimum: strings, sorting, a file and stdin API, time, math, and a
`Hashable` interface so `Map` takes a user type as a key.

### 5. Closures

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
  - Whether `float` ever gets a `%`-shaped remainder (`fmod`), and under
    what name. It is left out today because `%` means integer remainder.
  - **`str.size()` counts bytes, not characters.** `"héllo".size()` is 6.
    Go does the same and it is defensible, but it has to be decided and
    written down before the freeze rather than discovered after it.

---

## Known bugs

  - Vtable slots are assigned per method *name*, while each call site casts
    the slot to the signature it computed. Two interfaces declaring the same
    method name with different signatures share a slot. Not reachable today —
    assignment demands an exact per-method signature match and refuses
    interface-to-interface assignment — but it becomes type confusion through
    a function-pointer cast the day interface embedding arrives.
