# Closures and function references — the decision

Written **2026-09-22**, before building, in the shape of
`docs/concurrency-decision.md` and `docs/errors-decision.md`: what is
decided, the evidence for it, the alternatives that were rejected and why,
and what it costs.

`docs/stdlib-decision.md` ends by naming three features waiting on one
mechanism — `sort` by a comparison, a `Hashable` so a `Map` can key on a user
type, and `spawn` taking a closure — and says that is "worth knowing before
deciding it is a post-freeze concern". It is. The first finding of this
record is that the sentence is wrong in a useful way: **two of those three are
not waiting on closures at all**, and the third should not happen. What is
left is the case for closures on their own merits, which is a real case and a
different one.

---

## What the language already has

Everything a closure would be made of is already here. This is not a
prediction; it was checked against the compiler as it stands.

**A one-method interface works today**, including generically, including
with captured state, including across a thread boundary. All three of these
compile and run with `__rc_live=0`:

```c
interface Less<T> { bool lt(T a, T b); }
type IntAsc {}
bool IntAsc.lt(int a, int b) { return a < b; }

void isort<T>(List<T> xs, Less<T> c) { ... c.lt(xs[j], xs[j - 1]) ... }
isort(ns, IntAsc());                  // works
```

```c
type Job { Chan<int> out; int n; }    // a closure, written by hand:
void Job.run() { send(out, n * 2); }  // `out` and `n` are its captures
void worker(Task t, Chan<int> done) { t.run(); send(done, 1); }
spawn worker(Job(out, 21), done);     // works; prints 42
```

**The dispatch is already indirect.** `CallIface` (`src/ir.rs:261`) carries a
**constant** slot index, and the C backend emits exactly the cast-and-call a
closure invocation would need (`src/emit_c.rs:990`):

```c
v16 = ((bool (*)(Obj *, Obj *, Obj *))v1->ty->vtable[0])(v1, v12, v15); /* .lt */
```

**Slots are keyed by a method's name *and its IR shape*** (`src/ir.rs:431`,
`src/lower.rs:2937`), which is precisely the keying a function type needs:
`fn bool(A, B)` and `fn bool(C, D)` erase to the same shape and can share one
slot, exactly as `int area()` and `Price area()` already do.

**An interface value is a bare `Obj *`** — `struct Obj { long rc; const
TypeInfo *ty; }` (`runtime/rt.h:94`) — because the object knows its own type.
`IrTy` has four variants and only `Ref` is managed (`src/ir.rs:9`).

So the gap is not a mechanism. The gap is that **no expression in the
language denotes a function**, and no *type* spells one.

---

## Settled

### 1. A function is a value, and its representation is the one-method object

Both halves matter. A function *is* a value — there is a type `fn R(A, B)`,
there are lambdas, a named function's name is a value — **and** the thing
that value points at is an ordinary object with a `TypeInfo` and a vtable,
i.e. the one-method interface the compiler writes for you instead of you
writing it.

The two answers the question seemed to force a choice between turn out to be
the same answer at different levels. That is the whole design, and everything
below is a consequence.

#### Why closures earn their place over the interface written by hand

The hand-written version works (above), so the case has to be made on what it
costs, not on what it cannot do at all.

  - **Every callback is a top-level declaration, and burns a global name.**
    A predicate that is three tokens long becomes a `type`, a method, and a
    construction at the call site — and because there is no shadowing (§4.1)
    and no overloading, `ByX` is spent for the whole program. The reader who
    wants to know what the comparison is has to leave the function and find
    the method.
  - **It is manual defunctionalisation.** The programmer writes, by hand,
    the environment struct the compiler would write: `type Job { Chan<int>
    out; int n; }` is not a domain type, it is a closure with the sugar
    removed. Every field is a capture the author had to notice, name, and
    thread through a constructor.
  - **A generic higher-order function needs a generic interface per shape.**
    `Less<T>`, then `Pred<T>`, then `Fn1<A, R>`, `Fn2<A, B, R>`. The
    definitive evidence is Java, which took this exact route: `java.util.function`
    ships more than forty interfaces that exist only because Java had no
    function type, plus `@FunctionalInterface` to mark them, plus lambdas in
    Java 8 whose sole purpose is to construct them concisely. Java arrived at
    "a closure is a one-method object" from the other end and had to add the
    syntax anyway.
  - **Accidental satisfaction gets worse as the zoo grows.** Interfaces here
    are structural (§3.4), and the reference already names the hazard: a
    `Shape.draw()` silently satisfying a `Cowboy.draw()`. A library of
    single-letter interfaces named `Pred` and `Op` is that hazard multiplied.
  - **A no-capture callback still allocates.** `isort(ps, ByX())` builds a
    zero-field object at every call. A closure with no captures is a single
    static, immortal instance — free (see §5).

What the interface does better, and therefore keeps: **it has a name and it
can have more than one method.** `interface Sink { void write(bytes b); void
flush(); }` is a contract worth naming; `fn int(Point, Point)` is not. The
line between them is exactly that:

> **A function type when the contract is one unnamed operation. An interface
> when it has a name worth writing, or more than one method.**

Nothing is taken away from interfaces, and no interface has to be rewritten.

### 2. Two of the three blocked features are not blocked by this

This is the finding that changes the order of work, so it is stated
carefully.

**`ps.sort()` on a user type with `cmp`.** The diagnostic says it plainly
today (`src/lower.rs:993`):

```
`sort` orders `int`, `float` and `str`; P would need the compiler to call its
own `cmp`, which is not possible yet
```

What the runtime needs is to call a *known method of a known type* through
the object it already holds. It has the object; the object has a `TypeInfo`;
the `TypeInfo` has a vtable; the slot index is a compile-time constant. The
only missing piece is that no interface in the program declares `cmp`, so no
slot exists for it. **Reserve one.** The compiler assigns three fixed slots
at the head of `iface_slots`, before any interface's, for the three shapes
the runtime wants to call:

| slot | method | IR shape |
|---|---|---|
| 0 | `cmp` | `int64 (Obj *, Obj *)` |
| 1 | `eq`  | `bool (Obj *, Obj *)` |
| 2 | `hash`| `int64 (Obj *)` |

The runtime then calls `((int64_t (*)(Obj *, Obj *))o->ty->vtable[0])(a, b)`
with the index hard-coded, which is one more integer in the
compiler/runtime contract that `runtime/rt.c:1225` already describes for map
hashing. `rt_sort` gains a comparison path; nothing else changes.

**A `Map` keyed on a user type.** The same three slots answer it. `hash_key`
and `key_eq` (`runtime/rt.c:1254-1277`) switch on a `key_is_str` flag today;
they gain a third case that dispatches through slots 2 and 1. And there is no
need for a `Hashable` interface *at all*: the rule is the one §6.2 already
uses for operators —

> A `Map` key is an `int`, a `str`, or a type that declares `int hash()` and
> `bool eq(T)`.

`eq` already exists and already means what it must (§6.2 desugars `==` to
it), so **`hash` is the only new name.** That is strictly less language than
an interface declaration, and it is the same shape of rule as everything else
about user types: define the method and the feature works; do not and the
compiler names the method it wanted.

One honest caveat: a **module constant** `Map<P, int>` stays refused. The
compiler lays constant maps out as static data and must therefore compute
each key's hash itself (`src/lower/consts.rs`, `map_hash`), and it cannot run
a user's `hash` method at compile time. That is a small, well-bounded
refusal with a clear message, and it is additive to relax if constant
evaluation ever grows up.

**So `sort()` and user-type map keys should ship before closures, and
independently of them.** They are a week of work against a reserved slot
convention, not a language feature. The claim in `docs/stdlib-decision.md`
that they are "the function-reference problem again" is true only in the
sense that a vtable entry is a function reference — one the C emitter already
writes statically, needing nothing from the IR and nothing from the surface
language.

### 3. `spawn` keeps taking a function and arguments

`docs/roadmap.md` and `docs/concurrency-decision.md` both leave this open.
It should be closed: **`spawn f(x, y);` stays exactly as it is, and `spawn`
never takes a closure.**

The reason is the move rule, not the implementation. A capture is a +1 held
by the closure object (§4 below). The spawning frame still holds its own
reference to whatever it captured. So the closure's graph has two references
into it, and `rt_check_unique` traps. This is not speculation; it is what the
hand-written equivalent does today:

```c
str shared = concat("ab", "cd");
spawn worker(Job(shared, done));      // Job is fresh, `shared` is not
```
```
trap: value crossing a thread boundary is still referenced elsewhere;
clone() it, or drop the other reference first
```

A closure-taking `spawn` would trap in the ordinary case — every capture of a
local the spawner still has in scope — which makes it a feature that looks
convenient and fails at run time. The alternatives are both worse:

  - **Move-capture** (Rust's `move`, C++'s `[x = std::move(x)]`): a second
    capture mode, so a capture list to select between them, so two spellings
    for one idea. The language has one meaning for `=` and should keep it.
  - **Make capture a move always**: then a comparator that captures a list
    kills the caller's list, which is absurd everywhere except `spawn`.

Meanwhile `spawn f(x, y)` is *better* than a closure for this job: the
arguments are the captures, they are moved explicitly, the move checker sees
them by name, and the reader sees at the call site exactly what crosses.
That is the design `docs/concurrency-decision.md` arrived at for its own
reasons, and closures do not improve on it.

A closure can of course be *passed* to a spawned function — it is an ordinary
value — and then the existing rules apply unchanged: the closure must be an
owned local of the current block, and its whole captured graph must be
unreachable from anywhere else. Nothing new to specify.

Worth recording, because it is counter-intuitive: a closure-taking `spawn`
would *simplify* the backend. The emitter generates one argument struct and
one trampoline per spawned function name (`src/emit_c.rs:294-345`); with a
closure there would be one trampoline for the whole program. The reason to
refuse it is semantic, not mechanical.

---

## Syntax

### The type is `fn R(A, B)`

```c
fn int(Point, Point) by;              // a comparison
fn void() task;                       // no parameters, no result
fn bool(str) match;
Map<str, fn Response(Request)> routes;
```

The return type comes first and the parameter types follow in parentheses,
with no names: it is a function declaration with the name removed, which is
the one form the reader already knows.

**Why the `fn` keyword, when `int(Point, Point)` would say the same thing.**
The parser decides declaration-versus-expression with **two tokens**, and
says so (`src/parser.rs:1482`):

```
// Declaration. Three spellings, all decidable with two tokens:
//   const <ty> x = ..    a type keyword or type name after `const`
//   int x = ..           a type keyword at statement position
//   Point p = ..         a known type NAME followed by an identifier
```

`int(Point, Point) by = ...;` breaks that. `int(x)` is a conversion and
`Point(1, 2)` is a construction, so a statement beginning `int (` could be
either, and the parser would have to scan to the matching `)` and look one
token further to find out. That is C's most vexing parse, arriving by the
same route. One keyword removes it: `fn` at statement position means a
declaration, full stop, and the two-token rule survives.

The keyword also costs nothing to reserve — `fn` appears as an identifier
nowhere in `lib/`, the corpus or the examples — and it is in character for a
language that spells `type`, `interface`, `enum`, `distinct`, `const`,
`static`, `prim` and `spawn` out loud.

Rejected:

| | |
|---|---|
| `int(Point, Point)` | the parse above. |
| `fn(T, T) bool` | the spelling sketched in `docs/types.md` §8. Return type last, which is backwards for a type-first language; every other declaration in the language leads with the result. |
| `fn R(A) -> B` | an arrow is a second way to say what the parentheses already say. |
| a named `fn` declaration — `fn int Compare(Point, Point);` then `Compare by` | a new item kind and a new namespace, and it puts back the ceremony that motivated the feature: every callback needs a declaration, which is what the one-method interface already made you write. It would also have to decide whether two identically-shaped names are the same type, and there is no good answer. |

A function type is **invariant**: the return type and every parameter type
must match exactly. No covariance, no contravariance. Java's array covariance
and C#'s delegate variance are both remembered mainly for the holes they
opened, and the language has no subtyping between concrete types to build a
rule on anyway.

**A function with an optional parameter is not a value.** §4.2 says
mandatory parameters are positional and optional ones are named; a call
through a function type has no names to give, so `fn int(int, int) f =
scale;` for `int scale(int v, int by = 2)` is refused, naming the parameter.
Dropping the defaults silently would make `by` positional at one call site
and named at another, which is the one thing §4.2 exists to prevent.

### The lambda is `(int a, int b) => a + b`

Parameter types are written. The body is **one expression**, and the
lambda's return type is that expression's type. A lambda therefore has a
complete type of its own and needs nothing from its destination.

```c
fn int(int, int) add = (int a, int b) => a + b;
sort(ps, (Point a, Point b) => a.x - b.x);
fn void() hello = () => print("hi");
```

`(` followed by a type and an identifier is a lambda; `()` followed by `=>`
is a lambda; anything else after `(` is a parenthesised expression. Two
tokens again, using the same test the statement parser already has. `=>` is
free: no program can contain it today, because `=` followed by `>` does not
parse.

**Why written parameter types**, against Oro's `x => expr` and against every
other language with lambdas:

  - **Every binding in this language writes its type.** `int x = 5;`,
    `case Circle(int r):`, `for (int x in xs)`, every parameter, every field.
    There is no `var`. An inferred lambda parameter would be the first place
    in the language where a name is introduced without its type beside it,
    and a reader of the body would have to go and find the callee's signature
    to learn what `a` is. The §3.8 diagnostic that refuses `make().pick(xs)`
    — "bind it to a local first", so the receiver's type is written down —
    is the same instinct.
  - **Generic inference keeps working unchanged.** §3.8 requires each type
    parameter to be reached by a mandatory parameter whose argument has a
    *written* type. A lambda with written parameter types qualifies, so
    `sort(ps, (Point a, Point b) => a.x - b.x)` infers `T` from either
    argument and no inference rule has to be touched.
  - Oro's `x => expr` is the right answer for Oro, which is dynamically
    typed and has no type to write. The evidence does not transfer.

The cost is stated plainly: **it is verbose.** `xs.map((int x) => x * 2)` is
not as good as `xs.map(x => x * 2)`, and everyone who has used a language
with inferred lambda parameters will notice. The mitigation is that
**relaxing this later is additive** — every program written with the types
still compiles when the types become optional — so the restrictive choice is
the reversible one, and it is the one to make at a freeze.

**Why one expression and no block body.** `=> x * 2` and `=> { return x * 2; }`
are two spellings of one lambda, which the language does not do. An
expression body also makes the return type fall out of one expression rather
than a flow analysis over `return` statements, and it keeps lambdas small,
which is the only thing that makes an inferred capture list readable (§4).
Anything longer is a named function, and a named function's name is a value,
so nothing is inexpressible. Adding a block body later is additive.

**`?` is not allowed in a lambda body.** `?` returns from the enclosing
function (§6.3), and inside a lambda there is no enclosing function in any
sense the reader can act on — the lambda is called from somewhere else
entirely, possibly on another thread. It is refused with that said. The
workaround is a named function, which is where a fallible multi-step body
belongs anyway.

### A named function's name is a value; a method's name is not

```c
int compare(Point a, Point b) { return a.x - b.x; }
sort(ps, compare);                    // yes
```

This is safe *because a decision was already made for it*. `docs/roadmap.md`
records refusing to let a local take a function's name, and gives this as one
of three reasons:

> Closures are on this roadmap (§5), and the day a function can be a value,
> `digits` alone becomes genuinely ambiguous — so allowing it now is a
> breaking change waiting to happen.

The door was deliberately held open; this walks through it. A bare identifier
that names a function denotes that function, and nothing else can be named
`compare`.

**A method is not a value.** `p.area` is refused. The reason is §4.3's own
hazard: `p.area` is a field read, and making it mean a bound method when the
type has no such field would mean that *adding a field* later silently
changes what an existing expression denotes. Inside a method it is worse —
fields and sibling methods are both reached by bare name, so `area` alone
would be ambiguous between them. The workaround is one lambda,
`(Point p) => p.area()`, and adding bound methods later is additive.

`Type.method` as a static reference is left out for the same
add-it-later reason: nothing needs it yet.

### Only an identifier can be called

`f(x)` calls `f` when `f` is a function or a local of function type.
`obj.handler(x)` is always a *method* call, and `xs[0](y)` is not a call at
all. To call a closure that lives in a field or an element, bind it first:

```c
fn int(int) h = router.handler;
print(str(h(3)));
```

Same reason as above — otherwise adding a method named `handler` would
change what `obj.handler(x)` means — and the same remedy §3.8 already
prescribes for a receiver it cannot see: bind it to a local, with a
diagnostic that says so. It also keeps the grammar's `atom = IDENT [ args ]`
exactly as it is.

### What a closure cannot do

`==`, `print`, `str()`, `clone` and being a `Map` key are all refused on a
function type. None of them has an answer worth defending: there is no
identity comparison in the language to build `==` on, there is nothing useful
to print, and a second closure with the same captures is a copy nobody has
asked for. Each is additive to allow later.

---

## Capture

### One mode, and it is what `=` already means

A lambda's captures are the names from the enclosing scope its body mentions.
Capturing does exactly what binding does everywhere else in the language: an
`int`, `float` or `bool` is copied, and a reference is aliased and retained.

That is not a new rule — it is §7.2's existing one, reached by construction.
**The closure literal is a construction (§6.4) of a type the compiler
synthesised, whose fields are the captures**, and "a value stored into a
field is retained by the container" is already the protocol. Every
consequence below follows from rules that already exist, which is the main
reason to build closures this way.

**No capture list.** C++ needs `[=]`, `[&]` and `[x]` because it has two
capture modes and two lifetimes; a dangling `[&]` capture is one of the
best-known bugs in the language. With one mode there is nothing to list. The
cost is that a reader cannot see at a glance what a closure holds — mitigated
by the one-expression body, which puts every capture in view on the same
line.

### A capture cannot be reassigned; the object it names can be changed

Swift and JavaScript let a closure assign a captured local and have the
outside see it; Kotlin does too and pays for it by boxing the variable in a
`Ref` object; Rust needs `move` plus a `RefCell` to get the same effect
safely. We need none of that machinery, because the question does not arise:
a lambda body is one expression, so there is no assignment inside a lambda at
all.

What remains is the ordinary aliasing the language already has. `(int v) =>
xs.push(v)` captures `xs` by taking a reference to it; pushing changes the
one list, and the outside sees it, because that is what a second name for a
list means everywhere (§3.2). So:

> **The binding cannot be changed. The object can.**

This also means each closure made in a loop captures that iteration's value,
which is Java's effectively-final semantics and dodges the classic
JavaScript `var i` loop-closure bug by construction. Go had to change loop
variable scoping in 1.22 to fix the same bug; it cannot occur here.

### No shadowing applies, unchanged

A lambda's parameters are declarations, so §4.1 applies: they may not shadow
a local, a parameter, `this`, a type name, a function, a builtin, an import
or a module constant that is visible at that point. `xs.map((int x) => x * 2)`
inside a function that already has a local `x` is an error telling you to
rename one.

This is a real cost, and it lands exactly where it is most annoying: short
lambda parameter names collide in precisely the functions that have short
locals. It is not worth an exception. The rule's justification is about the
reader, and a reader looking at `x` in a lambda body inside a function with
another `x` is the case the rule was written for. The fix is a rename, which
is the price the no-shadowing rule always charges.

### `const`, frozen values, and destructors: nothing new

Because a closure is an ordinary object of a compiler-written type, it gets a
generated `drop_T`, `walk_T` and `copy_T` like every other type
(`src/emit_c.rs:66,129,161`), and every existing rule reaches it for free:

  - **Freezing.** `const fn int(int) f = ...;` freezes the closure and, deeply,
    its captures — the ordinary §4.1 snapshot, with the ordinary deep copy if
    anything else holds a part of the graph. Calling a frozen closure is a
    read, so it works.
  - **Resources.** A closure that captures an `io.File` cannot be bound to a
    `const`. `resource_in` walks fields, and a closure's captures *are*
    fields, so the existing compile-time refusal applies with no new code,
    and `rt_snapshot`'s runtime backstop catches what the type cannot show.
  - **Destructors.** A closure holding a `File` releases it when the closure's
    count reaches zero, and the `File`'s destructor runs then. A closure type
    cannot declare a `drop` of its own — it is anonymous, there is nowhere to
    write one — which is the right answer anyway.

### Cycles: the honest cost, and no weak references

`docs/roadmap.md` §5 gives this as the reason closures are late, and it is
the right worry: a closure stored in a field of an object it captures is a
cycle, and **a cycle leaks** (§7.1) — and since `docs/destructors-decision.md`
it leaks resources too, not just memory. Swift's `[weak self]` exists for
exactly this pattern, and `docs/concurrency.md` already records Nim's version
of it: a closure and an iterator referencing each other, and "nothing would
be released" — much of why Nim shipped a cycle collector.

**The recommendation is to accept it and not add weak references.** The
reasoning:

  - A weak reference is a second reference shape or a second header bit, a
    null check on every read, and therefore an `Option`-returning
    dereference — a second memory model, added for one pattern.
  - The exposure is narrower than it looks. **A closure with no captures is
    immortal** (§5) and can never be in a cycle, and that is the majority
    case: `sort(xs, compare)`, a handler table of top-level functions. A
    capturing closure that is only ever an argument dies with the call.
  - The language's answer to cycles is already "avoid the shape", and this
    adds one shape to the list rather than a new category of problem.

So the rule to write down is the pattern, not a mechanism: **do not store a
closure in a field of an object it captures.** Revisit only if real programs
hit it; weak references remain additive.

---

## Representation

### The claim, checked

> A closure is an object with a `TypeInfo` and a code pointer plus captured
> fields, so it is an ordinary `Obj *` and the IR needs no new reference
> shape.

The claim holds, and is stronger than stated. Checked against `src/ir.rs` and
`docs/ir-v0.md`:

  - `IrTy` is `I64 | F64 | I1 | Ref` (`src/ir.rs:9`), and `docs/ir-v0.md:38`
    states the invariant: "`ref` is the only managed type". A closure is a
    `Ref`. Nothing to add.
  - The code pointer does **not** live in the object. It lives in the type's
    vtable, which is a static array the C emitter already writes
    (`src/emit_c.rs:216`). So a closure object is header plus captures, and
    every closure made at one lambda site shares one `TypeInfo`.
  - The invocation is `CallIface` with a constant slot — the instruction that
    already exists. The synthesised method is named **`__call`**, which no
    program can declare: §10.1 reserves the `__` prefix in every identifier.
    That closes the accidental-satisfaction hole for free; a user type can
    never be structurally a function by having a method called `call`.
  - Slot keying by name *and IR shape* (`src/ir.rs:431`) gives one slot per
    distinct closure shape in the program, automatically.

**So the IR gains nothing. The C backend gains nothing.** No
`call_indirect`, no function-pointer IR type, no per-closure struct emitted by
hand, no new drop function — `drop_T`, `walk_T`, `copy_T`, the vtable and the
`TypeInfo` are all generated per type today and a closure type is a type.
`README.md` §3 predicts "one new IR op (`call_indirect`), a function type,
and a heap environment"; the first is not needed and the third is a
`TypeDecl`.

What changes is the **front end**: each lambda site becomes a synthesised
type plus a synthesised method, emitted before monomorphisation. Each
referenced named function becomes a synthesised zero-field type whose `__call`
forwards to it.

```
(int a, int b) => a + b + n        with `int n` captured, becomes:

type __closure$7 { int n; }
int __closure$7.__call(int a, int b) { return a + b + n; }
```

which is a program the language can already compile — which is the point, and
the best available evidence that the representation is right.

### A function reference with no captures is a special, immortal object

A zero-capture closure has no fields, so every instance is identical, so the
compiler emits **one static instance per site** with `rc = RC_IMMORTAL`
(`runtime/rt.h:21`), exactly as a string literal is emitted. The consequences
are all good and all follow from rules already written down:

  - no allocation, ever;
  - `rc_inc`/`rc_dec` on it are a compare and return (`runtime/rt.c:37`), so
    passing one around a hot loop is free;
  - `RC_IMMORTAL` is all-bits-set, so it reads as frozen, which is correct —
    there is nothing in it to change;
  - `reach_add` skips immortals (`runtime/rt.c:1704`), so **a bare function
    reference crosses a thread boundary at zero cost and can never trap the
    uniqueness check.**

So `sort(xs, compare)` costs one static struct in the binary and nothing at
run time, and a capturing closure costs one allocation of header-plus-captures.
Compare: Nim's closure is a fat `(proc, env)` pair, which is the second
reference shape this design avoids; Rust's is unboxed and free but
unnameable, which is why `Fn`/`FnMut`/`FnOnce`, `impl Fn` and `Box<dyn Fn>`
all exist; Go's is a heap object much like ours.

This does need one capability the compiler does not have yet: **emitting a
static instance of a user type.** `docs/const-decision.md` records the same
gap from the other side — "only a user-type module constant is still refused,
because the compiler cannot yet lay one out statically". The closure case is
the easy subset (a struct with nothing but a header), and doing it opens the
harder one.

### Cost of a call

A closure call is two loads and an indirect call — `o->ty`, then
`vtable[k]`, then the call — which is what every interface method call in the
language already costs, and one load more than Go's fat pointer. It cannot be
inlined, so a `sort` driven by a closure will be several times slower than the
built-in `sort()` on `int`s, which compares inline. That is the price of a
function type that can be written down, and it is the price Java and Go pay.
Devirtualising a call whose closure is known at the site is an optimisation,
addable after the freeze, which is what Lobster does aggressively and what
Rust gets by construction.

---

## Generics and monomorphisation

`void sort<T>(List<T> xs, fn int(T, T) by)`.

**The comparison is `fn int(T, T)`, not `fn bool(T, T)`** — negative, zero or
positive, the same `cmp` §6.2 already requires for the ordering operators.
The reference's own argument applies: one implementation is a total order and
several can disagree. Having the callback and the method spell the comparison
the same way means a type's `cmp` can be passed directly.

**Lambdas monomorphise; function types do not.** The desugaring happens in
the AST, before `src/mono.rs`, so a lambda written inside a generic function
becomes a synthesised generic type and is instantiated per instantiation like
any other — no new machinery, because `mono.rs` is an AST→AST pass that
already does this for nested generic types.

A `fn int(T, T)` *parameter*, though, does not multiply code: `sort<Point>` is
instantiated once and takes any comparator of that shape, dispatched
dynamically. The alternative is Rust's — monomorphise the callee per closure
type — which is faster and is not available here, because it requires closure
types that cannot be written down, which is exactly the thing this design
refuses. Once `fn int(Point, Point)` is a type a program can write, its
values must be interchangeable, and interchangeable means dispatched.

**The closure parameter's type is written at the declaration; the caller
writes nothing.** A lambda argument carries its own parameter types, so
inference reaches `T` from it under §3.8's existing rule, and a named
function argument does the same. There is no `sort<Point>(...)` spelling and
there never will be (§3.8).

---

## Iterators, `map` and `filter`

Wanted eventually; not blessed by the language, now or probably ever.

Oro's chains are the author's own prior art and they are genuinely good, but
they are good because Oro's collection protocol makes a chain **one pass**
with early exit. Reproducing that needs a lazy sequence protocol — an
interface, a generic, a closure per stage — and a decision about whether
`map` allocates. Without the protocol, `xs.map(f).filter(g)` allocates two
intermediate lists to save one `for` loop, which is a bad trade in a language
with no GC and a visible allocation cost.

The right shape: **once closures exist, `map` and `filter` become ordinary
generic functions a program can write**, and a `seq` module can be built and
judged on its merits, in the language, after the freeze. That is the whole
point of the feature — it moves higher-order code from the compiler's
privilege to the program's. Blessing particular methods is a separate
decision that nothing is waiting on.

---

## The freeze

Almost all of this is additive. A program written today compiles unchanged
under every decision above, and closures could land after the freeze without
breaking one. That is the useful headline: **the language can freeze without
closures.**

What must be decided **now**, because deciding it later would break programs:

  1. **`fn` becomes a keyword.** Reserving a keyword is not additive — a
     program with a variable called `fn` would break. It costs nothing today
     (nothing in `lib/`, the corpus or the examples uses it) and cannot be
     had for free later.
  2. **`hash` becomes a reserved-meaning method name**, and `cmp` and `eq`
     gain a second meaning (the runtime may call them). A program that has a
     method called `hash` meaning something else would change behaviour. Free
     today; not free later.
  3. **A local may not take a function's name.** Already in force
     (`docs/roadmap.md`) and it must stay in force, or a bare function name
     can never become a value.
  4. **`spawn f(args);` is final.** Closing the open question in
     `docs/concurrency-decision.md` and `docs/roadmap.md` means nothing else
     has to wait for it.
  5. **The synthesised call method is `__call`**, under the existing reserved
     `__` prefix — so no user type can ever be accidentally callable, and no
     ordinary name is spent.

What can arrive later without breaking a program: the `fn` type syntax
itself, `=>` and lambdas, function references, block-bodied lambdas,
inferred lambda parameter types, bound method values, `==`/`print`/`clone` on
closures, weak references, and devirtualisation.

---

## The recommended design, in one place

| | |
|---|---|
| **Is a function a value** | Yes — and its representation is the one-method object the language already dispatches |
| **Type** | `fn R(A, B)`; invariant; no defaults, so a function with an optional parameter is not a value |
| **Lambda** | `(int a, int b) => a + b` — written parameter types, one expression, return type from the body, no `?` inside |
| **Named function as a value** | Yes, a bare identifier. A method, no. |
| **Calling** | Only through an identifier; bind a field or element to a local first |
| **Capture** | Inferred, one mode, exactly what `=` means: an `int` copied, a reference retained. No capture list |
| **Mutation** | The binding never; the captured object as freely as any other alias |
| **Representation** | `Obj *` with a `TypeInfo`; captures are fields; code pointer in the vtable; invoked by `CallIface` on a synthesised `__call` slot |
| **No captures** | One static, immortal instance per site — no allocation, no refcount traffic, free across threads |
| **Generics** | Lambdas monomorphise with their enclosing function; a `fn` parameter dispatches dynamically |
| **IR** | No new instruction, no new type, no new reference shape |
| **`spawn`** | Unchanged: `spawn f(args);` |
| **Cycles** | Possible, not collected, no weak references; the pattern is named instead |
| **`sort()` / `Map` keys** | Not closures at all — three reserved vtable slots for `cmp`, `eq`, `hash` |

---

## Rejected alternatives

| | Why not |
|---|---|
| **One-method interfaces only, no function type** | Works (proved, today), and costs a top-level type and a global name per callback, a generic interface per shape, and manual construction of the environment. Java shipped exactly this and had to add lambdas and forty-odd interfaces anyway. |
| **`int(Point, Point)` as the type spelling** | Breaks the parser's two-token declaration rule (`src/parser.rs:1482`) and reintroduces C's most vexing parse. |
| **`fn(T, T) bool`** (the `docs/types.md` §8 sketch) | Return type last in a type-first language. |
| **A named `fn` type declaration** | A new item kind, and it puts back the per-callback ceremony the feature exists to remove. |
| **Inferred lambda parameter types** (`x => x * 2`) | The first binding in the language without a written type, and it breaks §3.8's inference precondition. Additive later, so the restrictive choice is the reversible one. |
| **Block-bodied lambdas** | A second spelling of a one-expression lambda; needs a return-type analysis; additive later. |
| **Explicit capture lists** (C++) | Only needed when there are two capture modes. There is one. |
| **Capture by reference** (C++ `[&]`, Go, Swift) | Dangling captures in C++; in a refcounted language it means either a box per captured local (Kotlin) or promotion to the heap (Go). By-value capture of an already-reference-counted reference gives the useful half with no machinery. |
| **`move` capture** (Rust) | A second mode, wanted only for `spawn`, which does not need it. |
| **`spawn` taking a closure** | Captures alias, so the uniqueness check traps in the ordinary case. `spawn f(x)` already expresses the captures as moved arguments. |
| **A `call` field in `TypeInfo`** | Eight bytes on every type and a new IR instruction, to replace a slot mechanism that already exists and already handles several shapes. |
| **A fat `(code, env)` closure value** (Nim, Go's interface) | A second reference shape in the IR — the exact thing `docs/types.md` §8 chose the object header to avoid. |
| **Monomorphising the callee per closure type** (Rust) | Requires closure types that cannot be written down, and then `Fn`/`FnMut`/`FnOnce`/`impl Fn`/`Box<dyn Fn>` to talk about them. Available later as devirtualisation, which is an optimisation and not a language feature. |
| **Weak references** | A second memory model added for one pattern; no-capture closures cannot cycle and argument closures die with the call. |
| **Blessing `map`/`filter` in the language** | Without a lazy protocol they allocate per stage; with one they are a large surface. Once closures exist a program can write them, which is the point. |
| **A `Hashable` interface** | `eq` already exists for `==`; only `hash` is new. A structural method rule matches §6.2 and needs no declaration. |

---

## Order of work

**Stage 0 — before the freeze, independent of everything else.**
Reserve `fn` as a keyword and `=>` as a token; reserve the three vtable
slots. Half a day for the lexer and keyword list; the slot reservation is
mechanical in `src/lower.rs:2329`. *Must* be pre-freeze.

**Stage 1 — `sort()` and `Map` on user types. ~1 week.**
Fill slots 0–2 from any type declaring `cmp`, `eq` or `hash`; teach
`rt_sort`, `hash_key` and `key_eq` to dispatch through them; replace the two
"not possible yet" diagnostics with the new rule, and add one refusing a
module-constant map with a user-type key. Unblocks two of the three features
`docs/stdlib-decision.md` names, with no language surface beyond one method
name. This is the highest value per unit of work in the whole record.

**Stage 2 — function types and function references, no lambdas. ~1–2 weeks.**
The `fn R(A, B)` type through the parser, the AST, `mono.rs` and assignability;
a bare function name as a value, desugared to a synthesised zero-field type
with a `__call` forwarder; the static immortal instance (which is the easy
half of the static-user-type-object work `docs/const-decision.md` wants);
calls through an identifier lowered to `CallIface`. At the end of this stage
`sort(xs, compare)` works and `docs/stdlib-decision.md`'s `sort` module can be
written.

**Stage 3 — lambdas and capture. ~1–2 weeks.**
Lambda parsing, capture analysis over the body, desugaring to a synthesised
type with capture fields before `mono.rs`, the no-shadowing check on
parameters, and the refusals (`?` inside a lambda, a method as a value,
calling a non-identifier, a defaulted function as a value). Corpus coverage
for the interactions: a closure in a `const`, a closure holding a `File`, a
closure crossing `spawn` inside a struct, a deliberate cycle in
`corpus/traps`.

**Stage 4 — the standard library catches up. Days.**
`sort(List<T>, fn int(T, T))` written in the language. Whatever else wants a
callback.

**After the freeze, at any time.** Devirtualisation of a statically known
closure; block bodies; inferred lambda parameter types; bound method values;
`map`/`filter` as a `seq` module; weak references if a real program needs
them.

---

## What this costs

  - **One keyword, one token, one type form, one expression form.** The
    grammar grows by four lines; §9's "closures, function values, lambdas"
    entry comes out of the not-in-the-language list.
  - **A synthesised type and method per lambda site**, so more `TypeInfo`s,
    more vtable slots, and a larger binary. Every type's vtable array grows by
    one entry per distinct closure shape in the program.
  - **An uninlinable indirect call per invocation**, two loads deep. A
    closure-driven sort of `int`s will be several times slower than the
    built-in one.
  - **A new way to leak.** A closure stored in a field of an object it
    captures is a cycle, and cycles are not collected — and since destructors,
    a leaked cycle holds resources as well as memory.
  - **Verbosity at the call site.** `(int a, int b) => a + b` is the price of
    the language's own rule that every binding writes its type, and it is
    charged at exactly the places where a lambda is most attractive.
  - **Short lambda parameter names will collide** with locals under the
    no-shadowing rule, and the fix is a rename.
  - **`hash`, `cmp` and `eq` are spoken for**, and a module constant map
    cannot be keyed on a user type.

None of these is new in kind. Every one of them is a cost the language has
already decided to pay somewhere else, which is the best sign available that
this is the design that belongs in this language rather than a good design
borrowed from another one.
