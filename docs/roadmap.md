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
| Corpus | 182 programs — 49 behaviour, 95 diagnostics, 16 traps, 7 Go twins, 15 multi-module |
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
erased before the IR so they cost nothing. Monomorphised generics on types,
functions and methods (`T Wrap<T>.get()`, `T Picker.pick<T>(..)`), with type
arguments inferred from the arguments and from where the value goes, and
usable across modules.

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
exact error-type matching, and a discarded `Result` as a compile error.
`Result<void, E>` for a failure with nothing to return, and `trap(msg)` for
a program to stop on its own bugs. The
one thing left is what `E` should be in a standard library, which
`docs/errors-decision.md` says to settle last -- once there is a library to
say what actually fails.

**Conversions and parsing.** `to_str` on every built-in type, `parse_int`
and `parse_float` returning `Option`, and `byte_at` so a library can do its
own textual work.

**Modules.** A file is a module; `pub` to export, private by default;
acyclic imports enforced with a chain-printing diagnostic.

**Conversions.** `print(v)` and `str(v)` on a user type go through its
`to_str`; `to_X` generally is an ordinary method plus a structural
interface, with no compiler support needed.

**Static methods.** `static Point Point.origin()` -- a method on the type
rather than on a value, which is what a conversion dispatching on its
TARGET needs.

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

### 1. Errors — all but one question, done

**`docs/errors-decision.md`.** Built: `Option<T>` and `Result<T, E>`, `?`
with exact error-type matching, and a discarded `Result` as a compile error.
Remaining: **what `E` should be in a standard library**, which the record
says to settle last, once there is a library to say what actually fails.
`int.parse` and `float.parse` wait on it and nothing else -- static methods,
the mechanism they need, already exist.

The original framing:

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

### 2. Conversions — done

`print(v)` and `str(v)` on a user type call its `to_str`, found by name the
way `add`, `eq` and `cmp` are. That closes the hole where a user type was
refused with "there is no way for a type to say how it prints".

**No blessed `ToStr`.** It turned out none was needed: interfaces are
structural, so a program declares `interface ToStr { str to_str(); }` itself
and every type with the method satisfies it. Blessing one would buy nothing
and freeze a name -- and `Option`/`Result` had a hard reason that this does
not, namely that a built-in method cannot return a user-defined type.

`to_int`, `to_float` and `to_bool` follow the same shape and need no compiler
support at all: declare the interface, write the method.

**Parsing is the other half and is still open.** It dispatches on its TARGET,
so it is a static method -- `int.parse(s)` -- and static methods now exist.
What it waits on is what `E` should be, the last open question in
`docs/errors-decision.md`.

### 3. Modules — built, and then substantially repaired

`docs/modules-decision.md`, built. A file is a module named by its basename,
private by default with `pub`, qualified imports with no wildcards or
aliases, cycles refused with the whole chain printed, one entry file declared
on the command line.

A fourth adversarial review found the first cut badly broken, and the
headline bug was one this roadmap had already guessed at and shipped anyway:
**each file interned its own type arena and the loader concatenated them
without rebasing the indices.** `Ty::User` is an index, so every module but
the first silently took the first one's types. A program calling its own
method got the library's, compiled clean under all four builds, and printed
the wrong answer. Fixed by parsing every file into ONE arena.

It also found that **privacy was enforced for functions only** — a private
type, its fields, its methods and a private enum's variants were all
reachable from another module, and a module could declare methods on another
module's private type. And that every post-parse diagnostic was blamed on the
entry file, quoting an innocent line at the same number.

Since repaired: declarations are interned module-qualified, so two modules
may each have a private `helper` and their own `Point`; and `lib.P` works
anywhere a type may be written.

Still open from that review:

  - A type with a static method cannot be embedded — embedding harvests
    promoted methods out of the signature table, which holds statics too.
  - A local may shadow an imported module name, though shadowing a type is a
    hard error.

Two things the implementation settled that the record left implicit:

  - Only an IMPORTED module needs a name that is an identifier. The entry
    file is named on the command line and never in source, so the corpus's
    `011-types.src` keeps working.
  - Every declaration records which module declared it, because privacy has
    to survive the point where all the files are concatenated into one
    program.

### 4. Standard library

**Designed: `docs/stdlib-decision.md`**, which answers the last open question
from the errors record: **`E` is whatever the library says it is.** No blessed
error type, because `match` is exhaustive with no `default`, so a single
global error enum could never gain a variant without breaking every program
that handles errors. An `Error` interface is worse still — with no
downcasting it is write-only.

Done: the three primitives a library cannot write from inside —
`str.byte_at`, `str.parse_int`/`parse_float` returning `Option`, and `to_str`
on `int`, `float` and `bool`.

Next: `io`, then `math`, then `text`. One of the three features this line
used to say were waiting on function references was not: `sort()` orders a
list of a user type by the type's own `cmp`, a reserved method name the
runtime reaches through the type's metadata (docs/reference.md 4.4a). A
`Hashable` for user-type Map keys should go the same way. What still waits
on a function reference is a `sort` that takes the comparison as an
argument; `spawn` taking a closure should not happen at all
(docs/closures-decision.md).


The stated goal is Oro's and Go's: a standard library good enough that most
programs need nothing else. Needs modules to live in and errors to report
with. Minimum: strings, sorting, a file and stdin API, time, math, and a `Map`
that takes a user type as a key.

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

**Self-hosting: rewrite the compiler in this language.**

Not a rewrite in C -- a rewrite in the language itself, which is the path Go
took from C and Rust took from OCaml.

How it works, since it is easy to picture wrongly. The compiler emits C, so
a C compiler never leaves the chain:

    stage 0:  src/*.rs      --rustc-->            langc0
    stage 1:  compiler.src  --langc0--> .c --cc--> langc1
    stage 2:  compiler.src  --langc1--> .c --cc--> langc2
    stage 3:  compiler.src  --langc2--> .c --cc--> langc3

`langc2` and `langc3` must be BYTE-IDENTICAL. Not 1 and 2: stage 1 was built
by a different compiler and may legitimately generate different code. Stages
2 and 3 are both built from this language's own source, so identical input
must give identical output -- and that check only means anything because
emission is already reproducible, which the gate added after the
HashMap-iteration bug guarantees.

Prerequisites, all of which are on the pre-freeze list anyway:

  - **modules** -- 10,900 lines is not going in one file;
  - **file I/O** -- it has to read a `.src` and write a `.c`, and there is no
    I/O of any kind today;
  - **command-line arguments** -- it has to know which file.

So this is not a separate project; it is what falls out of modules plus a
standard library with I/O. That is also why it is worth doing: it is the
thing that PROVES the standard library is good enough, rather than a claim
that it is.

After the freeze, not before. Self-hosting welds the compiler to the
language, so every later language change becomes a bootstrap problem. Freeze
the surface, then self-host against something that is not moving.

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

## Settled, and worth saying out loud

Decisions that would otherwise look like accidents.

**`spawn` is fire-and-forget.** A spawning scope does not wait for its
children; the program waits for all of them before exiting, and that is the
only synchronisation there is. Structured concurrency -- a scope that joins
its children -- is the Loom-shaped alternative and it was considered and not
taken. It is a surface decision, so it is made now rather than discovered
later: a channel is how a spawned thread reports back, and adding a joining
scope on top later is additive in a way that removing one would not be.

**`List<int> xs = List<int>();` stays.** The repetition is real and was
looked at: a bare `List<int> xs;` auto-initialising, a context-typed `= []`,
and Java's diamond `List()` were all considered.

Bare declaration was the one to reject outright. It reintroduces a zero
value, which is what makes null impossible here, and it only has an answer
for two types: `Array` needs a length and a fill, a struct has no empty, and
an enum has no default variant. `Array` and `List` sitting next to each other
with different rules is the worst version of that.

The redundancy is also narrower than it looks. `T x = T()` is the
empty-collection shape specifically; most declarations initialise from a
call, where the type is doing real work. Four characters on one shape did not
justify new surface that has to be frozen.

`= []` remains the option worth revisiting, and only alongside real
collection literals -- `[1, 2, 3]` is the part that would earn it.

**A local may not take a function's name, builtins included.** Asked for
by the standard library's authors: a private helper `digits` blocked every
local called `digits` in its module, and the builtin `close` blocked a local
`close`. The case for allowing it is real -- today a function is only ever
*called*, so `digits` and `digits(..)` cannot be confused by the compiler,
and Java keeps methods and variables in separate namespaces for exactly that
reason. It was declined anyway, on three grounds. The rule's own rationale
covers it: "a name means one thing where a reader can see it" is about the
reader, and a function listing `digits` and `digits(x)` side by side means
two things by one name (docs/types.md §5b lists "not a function" among what
nothing shadows, on purpose). Closures are on this roadmap (§5), and the day
a function can be a value, `digits` alone becomes genuinely ambiguous --
so allowing it now is a breaking change waiting to happen, where allowing it
later, if closures never come, is additive. And the cost is a rename, which
is the cost the no-shadowing rule always charges. The builtin names are the
sharpest edge, so the builtin set stays deliberately small: `print`,
`concat`, `clone`, `send`, `recv`, `close`, `trap`.

**`str.size()` counts BYTES, not characters.** `"héllo".size()` is 6, and
`substr` takes byte offsets. Chosen, not defaulted into -- and since
2026-09-21 a `str` is also always valid UTF-8, an offset inside a
character traps, and code points are `int`s through `s.chars()` and
`str.from_chars(xs)`: Rust's model rather than Go's. Grapheme clusters,
case mapping and normalisation are a `unicode` module once constant arrays
can hold its tables. See `docs/text-decision.md`.

---

## Open questions

Written down because they are unresolved, not because they are unimportant.

  - Cancellation. libdill's model — killing a thread makes every blocking
    call in it return an error — fits errors-as-values and needs no unwinder.
  - Does `spawn` keep taking a function plus arguments, or a closure?
  - Constraints on type parameters. Today a bad instantiation fails when it
    is checked, naming the instantiation, which is C++'s error experience.
  - Integer width. `int` is 64-bit and deliberately unqualified so the IR can
    choose per target. Whether sized types ever become spellable is open.
  - Whether `float` ever gets a `%`-shaped remainder (`fmod`), and under
    what name. It is left out today because `%` means integer remainder.

---

## Known bugs

None recorded.

The vtable-slot bug that sat here is fixed. It was accurately described as
unreachable — an agent tried twenty-two delivery paths and assignability
blocked every one — but looking for a way in found **three other routes to
the same bad cast**, all reachable, and one of them silent. See
`docs/reference.md` §3.4 and `corpus/core/048`.
