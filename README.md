# lang (placeholder name)

A statically typed, compiled, high-level language. Go altitude, not embedded.
Refcounted, no GC. Small and opinionated, intended to freeze.

The name is a placeholder held in one file. Run `./rename.sh <name>` when
there is one; it rewrites `config.sh` and moves the corpus files with it.

## Status

**The walking skeleton is green.** `langc` compiles the corpus language to C,
which gcc and clang both build clean at `-O0` and `-O2`.

```
./build.sh examples/tour.src && ./tour    compile and run a program
./gates.sh                                every gate
./run.sh                                  the corpus alone
```

`examples/tour.src` is a tour of every feature, and is also in the corpus so
it cannot rot.

- 77 corpus programs, 0 failing
- 66 unit tests
- 0 dependencies, 0 `unsafe`, clippy clean at `-D warnings`

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
if (len(s) > 3) {
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
with escapes and UTF-8. Builtins: `print` (int, bool or str), `len`, `concat`.

**Generics are monomorphised** and erased before lowering, which is why the
IR has never needed to know about them. `Box<int>` becomes a plain type named
`Box$int`. Type arguments are explicit on types and inferred on function
calls.

**User types are reference types** — refcounted, heap allocated, aliased by
assignment, constructed by the argument rule below. A type holding no references gets no
drop function at all; one that does gets a generated one, and `rc_dec` checks
for `NULL` so the common case is a branch rather than a call.

**Methods are declared by qualified name**, outside the type body, so they
can be added to any type and a type declaration stays a list of fields.
**Fields are reached by bare name — there is no `this`.**

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

A **channel is exempt**, because it is how threads share; it is aliased
rather than moved, and is immortal for now (see `rt_chan_new`).

These are OS threads. The channel surface does not change when green threads
replace them.

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
error; rename one of them. This is what removes the need for `this` — a bare
name can only ever mean one thing — and it deletes the class of bugs where a
reader and the compiler disagree about which `x` is meant. Reusing a name in
*sibling* scopes is fine, since neither is visible to the other.

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

Not yet: `for`, closures, modules. Concurrency is OS threads for now;
green threads are stage 3 of `docs/concurrency-decision.md`.

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
  tests.rs             unit tests, mostly IR assertions
runtime/
  rt.h rt.c            the runtime — a SEPARATE translation unit, see §7.1
  rc_debug.h           refcount invariant, compiled in under -DRC_DEBUG
docs/
  ir-v0.md                 the IR specification
  concurrency-decision.md  the concurrency decision
  concurrency.md           the evidence behind it, and what was rejected
  types.md                 type system: proposal, plus the forks left open
corpus/{core,twin,traps,errors}/
examples/tour.src      every feature in one file
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

1. `for` — sugar over `while` now that the loop machinery and its merge
   points exist
2. **The type system** — `docs/types.md`. The move checker is the only part
   concurrency is waiting on, and it is the smallest part: one bit per local
   over the CFG we already build
3. Closures and function values — one new IR op (`call_indirect`), a function
   type, and a heap environment. Note that closures plus refcounting is the
   most common source of reference cycles, which makes weak references a
   near-term need rather than a deferred one
4. User-defined types
5. A C-emitter second look once there is enough language to benchmark
