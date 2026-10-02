# m31

A statically typed, compiled, high-level language. Go altitude, not embedded.
Refcounted, no GC. Small and opinionated, intended to freeze.

The name is held in one file, `config.sh`. Run `./rename.sh <name>` to
rename it again; it rewrites `config.sh` and moves the corpus files with it.

## Status

**The walking skeleton is green.** `m31c` compiles the corpus language to C,
which gcc and clang both build clean at `-O0` and `-O2`.

```
./build.sh examples/tour.m31 && ./tour    compile and run a program
./target/debug/m31c fmt <file>           format in place
./target/debug/m31c fmt --check <file>   exit 1 if it is not formatted
./gates.sh                                every gate
./run.sh                                  the corpus alone
```

`examples/tour.m31` is a tour of every feature, and is also in the corpus so
it cannot rot.

- 722 corpus programs, 0 failing
- 130 unit tests
- 0 dependencies, 0 `unsafe`, clippy clean at `-D warnings`

Beyond the core language: a standard library (`lib/`, 27 modules) covering
collections, JSON, CSV, HTML, a full HTTP/1.1 client and server
(`lib/http.m31`), filesystem and OS access, and a constant-time crypto stack
(X25519, Ed25519, SHA-256/512, ChaCha20-Poly1305). On top of that, four real
applications in `apps/`: an interactive `git` client, an `ssh` client, a
`tui` framework, and a `markdown` renderer.

## What the language does today

```c
type Point {
    int x;
    int y;
}

int sum(Point p) {
    return p.x + p.y;
}

// No `main`. Statements at the top level are the program, as in Python
// or Oro; declarations are order-independent, so this could come first.
const int scale = 3;
Point p = Point(x: 3, y: 4);
print(sum(p) * scale);

str s = concat("hel", "lo");
if (s.size() > 3) {
    print(s);
}
```

**There is no `main`** — statements at the top level are the program, in
source order. Writing one is an error rather than a silent no-op, because it
is a habit worth catching.

Types `int`, `bool`, `str`, `void`, and user-defined `type` declarations.
Functions, recursion, `if`/`else if`/`else`, `while` with `break`/`continue`,
`return`, locals, `const`, assignment, field access and field assignment, the
C operator set with C precedence, short-circuiting `&&`/`||`, string literals
with escapes and UTF-8. Builtins: `print` (int, bool or str) and `concat`; `size()` and `contains()` are methods on every collection and on `str`.

**Generics are monomorphised** and erased before lowering, which is why the
IR has never needed to know about them. `Wrap<int>` becomes a plain type named
`Wrap$int`. Type arguments are explicit on types and inferred on function
calls.

**User types are reference types** — refcounted, heap allocated, aliased by
assignment, constructed by the argument rule below. A type holding no references gets no
drop function at all; one that does gets a generated one, and `rc_dec` checks
for `NULL` so the common case is a branch rather than a call. **A type may
declare a destructor**, `void T.drop()`, which runs at the exact moment the
count reaches zero, before the fields are released — so an `io.File` closes
itself on every path out of a scope, `?` included.

**Methods are declared by qualified name**, outside the type body, so they
can be added to any type and a type declaration stays a list of fields.
**Fields and sibling methods are reached by bare name; the receiver as a
whole is `this`.**

```c
type Rect { int w; int h; }

int Rect.area() {
    return w * h;
}

void Rect.scale(int f) {
    w = w * f;
}
```

That is safe only because of the next rule.

**Collections** come in two shapes, and the difference is layout:

```c
Array<int> a = Array<int>(4, 0);   // fixed length, elements INLINE
a[2] = 7;

List<str> xs = List<str>();        // growable, separate buffer
xs.push("one");

for (str x in xs) {
    print(x);
}
```

`Array<T>` is one allocation with the elements stored directly after the
header — one pointer chase, no capacity slack, never reallocates. `List<T>`
keeps a separate buffer that `push` may grow. Indexing is bounds-checked and
traps, like integer overflow.

An array takes a **fill value** because there is no null: a reference element
has to start as *something*, and only the caller can say what.

```c
Map<str, int> counts = Map<str, int>();
counts.set("a", 1);
print(counts.get("a"));
print(counts.contains("b"));
```

`Map<K, V>` is open-addressed with linear probing — one allocation for the
whole table. Keys are `int` or `str`; hashing a user type would need a
`Hashable` interface that does not exist yet. `get` on a missing key **traps**,
like an out-of-range index: there is no null to return, so the honest choices
are to trap or to force every read through a check, and `contains` is the check.

`clone(x)` is a **shallow** copy — we chose reference types, so `=` aliases
and this is the explicit way to get a second object.

`float` is an IEEE double and is a separate type from `int` with **no
implicit conversion** — `1.5 + 2` is an error, and `float(n)` / `int(x)` are
how you cross. `int` traps on overflow; `float` gives you an infinity or a
NaN, because those are IEEE's defined answers rather than faults.

**Distinct types** are the same representation as their base with a
different identity — erased before the IR, so they cost nothing:

```c
distinct int Price;
distinct int UserId;

Price p = Price(250);
print(p + Price(100));   // inherits int's operations: it IS an int
print(p + 2);            // error: convert one of them
int n = int(p);          // explicit, emits nothing
```

`distinct int Price` is an `i64` at runtime — no object, no header, no
refcount — and a `UserId` field stores as a plain `i64`, not a wrapper. A
distinct type inherits every operation of its base, and the result keeps the
distinct type; mixing with the base needs a conversion, which is the point.

There are **no type aliases**. An alias is a second name for the same type,
so it gives documentation and no safety — you could still pass an `OrderId`
where a `UserId` was wanted. Go added them for cross-package migration during
refactors, which is a module problem we do not have.

**Concurrency** is `spawn` plus channels, and values crossing a thread are
**moved**:

```c
void worker(Chan<int> out, int id) {
    send(out, id * 10);
}

Chan<int> results = Chan<int>(8);
spawn worker(results, 1);
spawn worker(results, 2);
print(recv(results) + recv(results));
```

Sending a reference moves it: the sender gives up its reference and the
receiver acquires it, with no retain or release in between. Using a moved
local afterwards is a compile error. That is what keeps `rc_inc`/`rc_dec`
non-atomic — only one thread can reach a value at a time.

Only a value you **own** can be moved: a temporary, or a local declared in
the block doing the move. A parameter is borrowed — the caller still holds
it — so handing one to another thread is refused, and so is moving a local
declared in an enclosing scope, because a loop or an `if` arm would move it
more than once. The fix in both cases is `clone(x)`, which hands over a
copy and leaves the original alone.

Retaining instead of moving would not help. Two threads on one *non-atomic*
counter is the bug; a `rc_inc` before the handoff just makes the race start
at 2. So the rule is compile-time refusal, backed at run time by
`rt_check_unique`: a value crossing a thread boundary with a refcount above
one traps rather than corrupting the heap, which catches the aliasing the
compiler cannot see (two locals reaching the same object through a field).

A **channel is exempt**, because it is how threads share; it is aliased
rather than moved, and is immortal for now (see `rt_chan_new`).

**`spawn` means a green thread**, M:N over one carrier OS thread per core
(`docs/concurrency-decision.md`) -- not an OS thread, and not conditionally
one or the other: a value crossing between them is still moved, exactly as
above, and the channel surface above did not change when green threads
replaced OS threads, as promised. Getting here safely took real work:
building and wiring the runtime surfaced three genuine data races under
real concurrent socket I/O (a compiler TLS-caching bug in the stack-overflow
probe, a scheduler local-buffer race, and a reactor-thread shutdown race),
each found with ThreadSanitizer and a dedicated reproducer, fixed, and
re-verified -- the full account, including exact verification numbers, is
in `docs/concurrency-decision.md`'s "Phase 3.5" section.

**Embedding** is composition in place of inheritance: an anonymous field,
named after its type, whose fields and methods are promoted.

```c
type Animal { str species; int legs; }
int Animal.count_legs() { return legs; }

type Dog {
    Animal;          // embedded
    str name;
}

Dog d = Dog(Animal("dog", 4), "Rex");
print(d.species);       // promoted field
print(d.count_legs());  // promoted method
```

Methods are promoted by synthesising forwarders, so direct calls, vtables and
interface satisfaction all work unchanged. A method the outer type defines
itself always wins. There is no inheritance and no subtyping — polymorphism
is the interface above, reuse is this.

**Interfaces are structural** — having the methods is the proof, with no
`implements` clause, so a type written before the interface existed can
satisfy it:

```c
interface Shape {
    int area();
    str name();
}

void report(Shape sh) {     // any type with area() and name()
    print(sh.name());
}
```

An interface value is just an `Obj *`: the object knows its own type through
the header, as in Java, so `ref` stays the only reference shape in the IR and
there are no fat pointers. Dispatch costs one extra load. A call on a
*concrete* receiver stays a direct call, so interfaces cost nothing where
they are not used.

**Operators desugar to methods**, so a user type can define them:

```c
Money Money.add(Money other) { return Money(cents + other.cents); }
int   Money.cmp(Money other) { return cents - other.cents; }
bool  Money.eq(Money other)  { return cents == other.cents; }

print(a + b);  print(a < b);  print(a != b);
```

Comparison goes through a single `cmp` returning an int rather than four
methods, so one implementation gives a total order and `<` and `>=` cannot be
defined inconsistently. `str` has `+` and `==` built in.

**Nothing shadows anything, anywhere.** Not an outer local, not a parameter,
not a function, not a type, not a field of the receiver. It is a compile
error; rename one of them. This is what lets fields and sibling methods go
bare, with `this` only for the whole receiver — a bare name can only ever
mean one thing, so `this.x` is refused as a second spelling — and it deletes
the class of bugs where a reader and the compiler disagree about which `x` is
meant. Reusing a name in *sibling* scopes is fine, since neither is visible to
the other.

**Arguments follow Oro's rule, for calls and construction alike: a parameter
with no default is positional, one with a default is named.** Never both.

```c
type Point { int x; int y; str label = "unnamed"; }
int volume(int w, int h = 1, int depth = 1) { return w * h * depth; }

Point b = Point(3, 4, label: "corner");
print(volume(5, depth: 3, h: 2));    // named arguments need no order
```

Naming a mandatory parameter is an error, as is passing an optional one
positionally. The two halves never overlap, so there is no question of which
form to use and no question of what order optional arguments come in.

A callback is an ordinary **one-method interface**, and a function's name or
a lambda may be written wherever one is expected — there is no function type
(`docs/closures-decision.md`). Concurrency is green threads
(`docs/concurrency-decision.md`), Phases 0-3 of which are done, verified
under real concurrent load, and load-bearing -- see Concurrency above.

## Decisions made

| Decision | Choice | Note |
|---|---|---|
| Syntax | **type-first, semicolons, braces** (C/Java) | Parseable without C's lexer hack because type names are keywords and there are no pointer declarators. `IDENT IDENT` with two tokens of lookahead is how Java manages the same grammar. |
| `int` | **64-bit everywhere, traps on overflow** | The IR may use i32/i64/i128 as hardware suits, *provided observable behaviour is identical* — that makes width an optimisation needing no spec. What must not vary is the promise. An arch-dependent `int` is the mistake C is still paying for, and would make a freeze promise nothing. |
| Memory | refcount, **non-atomic** | Single-threaded today. Atomic later is a lowering change — *provided* refcount ops stay IR-level and are never hand-inlined into a backend. See `docs/concurrency.md`. |
| Ownership | **arguments borrowed, returns owned (+1)** | Swift's default. Passing a value you already hold to a function that only reads it costs nothing. |
| Errors | values, not unwinding | Needs nothing from the IR. Unwinding plus refcounts means every unwind path must decrement correctly; that bug class never fully closes. |
| Generics | **monomorphisation**, implemented later | Deciding the strategy now keeps generics out of the IR entirely: instantiation happens in the frontend, so the IR only sees concrete types. Deferring the *decision* is what cost Go ten years. |
| Backend | **C emitter only**, for now | Zero dependencies, `gcc` already present, every target including 32-bit, and it hands us two oracle layers free. Cranelift becomes the second backend when cross-compilation gets real or `gcc`-per-build gets painful. |
| Concurrency | **stackful green threads, moved not shared** | Uncoloured — one kind of function, no sync/async split, because colouring forks the stdlib permanently. Values crossing threads are moved, which is what keeps refcounts non-atomic. Fixed stacks, but no guard pages: the compiler emits a stack probe instead, so stacks pack densely and the ~32k VMA ceiling disappears. `docs/concurrency-decision.md`; evidence in `docs/concurrency.md`. |

## The oracle

Four layers, all of which must agree. This matters more than usual: a new
language has no external check on its own correctness, and three engines
written by the same person would only ever prove they agree with each other.

| Layer | Catches | Where |
|---|---|---|
| `gcc` vs `clang` | emitted UB — the C emitter's signature failure mode | `run.sh` matrix |
| `-O0` vs `-O2` | optimiser-visible UB, and our own bugs | `run.sh` matrix |
| Go twin programs | frontend and IR-pass bugs — real external truth | `corpus/twin/` |
| refcount invariant | leaks, double-free, use-after-free | `runtime/rc_debug.h` |

Plus `-Wall -Wextra` treated as failure: a warning in generated code is a
defect in the emitter, and usually the early form of a UB bug.

### The rule that makes it an oracle

**`corpus/core/*.out` is hand-authored by a human. Never generated by the
compiler under test.**

The moment expected output is produced by running the compiler, the oracle is
only checking that the compiler agrees with itself. Oro's README already names
this trap: self-generated fixtures *"can never catch Oro being wrong from the
start."*

- **`corpus/twin/`** — anything computational. Truth comes from Go. Should hold
  most of the corpus, because it scales without hand-auditing.
- **`corpus/core/`** — only what Go cannot express: our own semantics, refcount
  behaviour, output formatting. Small, deliberate, every `.out` hand-checked.
- **`corpus/traps/`** — programs that must abort, with their trap message and
  exit 134. Overflow lives here rather than `twin/` because Go wraps: a Go twin
  would print `-9223372036854775808` and be confidently wrong about us. The
  refcount invariant is not checked here, because `abort()` skips `atexit`.
- **`corpus/errors/`** — programs that must be rejected, with their expected
  diagnostic. Diagnostics rot silently without this.
- **`corpus/fmt/`** — formatter layout: `name.m31` must format to the
  hand-written `name.want`. Checked by `gates.sh`, not `run.sh`; a `name.out`
  lets the meaning gate run the program too.

The unit tests in `src/tests.rs` cover what the corpus structurally cannot
see. A redundant retain/release pair, an unreachable block or a stray
temporary all still produce correct output, so the refcount tests assert on
the IR directly.

### Normative engine

When layers disagree and it is not obvious which is wrong, **the Go twin wins**
for anything with a universal answer; otherwise the hand-authored `.out` wins.
Write the reasoning into the test rather than adjusting the expectation to
match the compiler.

## Layout

```
config.sh              name, binary, extension — the only place they appear
rename.sh              renames the language and moves the corpus with it
build.sh               compile a source file to an executable
gates.sh               every gate, run after every commit
run.sh                 the differential corpus runner
src/                   the compiler
  lexer.rs             hand-written; type names are keywords
  parser.rs            recursive descent + Pratt, C precedence
  mono.rs              monomorphisation; generics are erased here
  lower.rs             typecheck and lower in ONE pass, deliberately fused
  ir.rs                docs/ir-v0.md made real
  emit_c.rs            post-refcount IR to C
  fmt.rs               the formatter: one layout, no options
  tests.rs             unit tests, mostly IR assertions
runtime/
  rt.h rt.c            the runtime — a SEPARATE translation unit, see §7.1
  rc_debug.h           refcount invariant, compiled in under -DRC_DEBUG
  greenthread.h/.c     slab stack allocator, fiber state, ctx-switch glue
  ctx_switch_x86_64.S  the x86-64 context switch itself
  scheduler.h/.c       the M:N scheduler — not yet wired to spawn/Chan
  reactor.h/.c         epoll reactor, park/unpark, blocking-FFI handoff
lib/                   the standard library, written in the language itself
  net.m31 http.m31 json.m31 fs.m31 os.m31 ...     27 modules total
  x25519.m31 ed25519.m31 sha256.m31 sha512.m31
  chacha20poly1305.m31                            constant-time crypto
apps/                  real programs built on the language and stdlib
  git/                 an interactive git client
  ssh/                 an SSH client — see docs/ssh-decision.md
  tui/                 a terminal UI framework
  markdown/            a markdown renderer
docs/
  reference.md             the language, stated normatively -- start here
  roadmap.md               what exists, what the freeze needs, what is out
  errors-decision.md       errors as values; what is settled, what is open
  modules-decision.md      file = module, private by default, no cycles
  stdlib-decision.md       what an error is, and what the library will hold
  ir-v0.md                 the IR specification
  concurrency-decision.md  the concurrency decision, and the phased build-out
  concurrency.md           the evidence behind it, and what was rejected
  ssh-decision.md          SSH scope: client-only, publickey-only, exec-only
  types.md                 type system: proposal, plus the forks left open
corpus/{core,twin,traps,errors,fmt}/
examples/tour.m31      every feature in one file
examples/enums.m31     enums and match, including Option, Result and JSON
```

## Three layers, so they do not get confused

1. **The language** — C/Java shaped: type-first declarations, semicolons,
   braces. The angle brackets on generics are Java's `List<String>`, not
   Rust's.
2. **The IR** — internal, sixteen instructions, SSA with block parameters.
   `--emit-ir` prints it; nothing else exposes it.
3. **The backend** — emits C and hands it to `cc`. `build.sh` does this in a
   temp directory and deletes it. **You never see or keep the C.** It is
   replaceable — Cranelift is the planned second backend — which is exactly
   why nothing about the language depends on it.

## The formatter

`m31c fmt` has **one canonical layout and no options**. That is the other
half of the braces decision: the case for braces over significant indentation
was that a formatter gives you one correct layout without putting whitespace
in the grammar, so the language owes you the formatter.

It **keeps declarations in the order written, except that a method joins its
type**. Declaring methods by qualified name lets them scatter through a file,
which was accepted on exactly this basis; nothing else moves, so a section
divider stays in its section. A comment block touching the declaration below
it travels with it; one with a blank line after it stays where it is. String
literals are printed as they were spelled. **Lines are not wrapped** — gofmt's
choice, not rustfmt's; see the header of `src/fmt.rs`.

Two properties are gated rather than asserted, because a formatter that
quietly alters a program is worse than no formatter:

- **it preserves meaning** — every corpus program emits byte-identical C
  before and after formatting
- **it is idempotent** — formatting twice matches formatting once

and the layout itself is pinned by hand-written fixtures in `corpus/fmt/`
(`name.m31` formats to `name.want`).

## Two rules that look like details and are not

**The runtime is a separate translation unit, and `-flto` is off.** Either
change lets the C compiler see a `free()` whose argument is a static string
literal object, and it warns with `-Wfree-nonheap-object`. The `RC_IMMORTAL`
guard makes that path unreachable at runtime, but the compiler cannot prove
it. `gates.sh` enforces both. Full reasoning in `docs/ir-v0.md` §7.1.

**Refcount operations stay IR-level.** No backend may inline `rc_inc`/`rc_dec`
by hand. That is what keeps atomic-vs-non-atomic a lowering switch instead of
a rewrite when concurrency lands.

## Provisional

The diagnostic format in `corpus/errors/*.err` is observable surface — the
corpus compares it byte for byte — but it has had no design pass. Expect to
revisit it.

## Next

1. **A real profiling pass on the reactor's I/O dispatch path.**
   `apps/httpserver/SCALING.md` found that `apps/httpserver` tracks an
   equivalent Go server closely up to ~1,000-2,500 concurrent connections,
   then degrades measurably faster from 5,000 upward (throughput and tail
   latency) with no errors anywhere up to 20,000. The leading hypothesis,
   from reading the code rather than a profiler: a single dedicated OS
   thread discovers every I/O-ready event and serially takes three separate
   global locks before any carrier can run the resulting work, a cost that
   scales with request rate. Confirming this (and fixing it, likely by
   batching the dispatch or sharding event discovery across carriers) is
   the next concrete runtime item
2. **A smaller default green-thread stack**, or real growable/relocatable
   stacks. The same scaling test measured real resident-memory divergence
   at high connection counts (1.56 GB vs. a comparable Go server's 460 MB at
   20,000 connections) — the fixed, demand-paged 1 MiB-per-thread stack
   this runtime uses in place of guard pages. Tuning the default down is
   cheap to try; true growable stacks need compiler-level pointer maps first
3. A second macOS CI remote, so `macos-13` (Intel) can be re-enabled
   alongside `macos-14` (Apple Silicon) — see `.github/workflows/macos.yml`
4. aarch64 and macOS CI both need a human to push and watch a real run
   confirm clean before either platform is more than "written and reasoned
   correct" — see `docs/concurrency-decision.md`'s Phase 3.5/Phase 4 entries
5. A C-emitter second look, now that there is enough language to benchmark
