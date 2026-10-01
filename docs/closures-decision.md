# Callbacks, function references and lambdas — the decision

Written **2026-09-22**, revised **2026-09-23** after the author read the
first draft and rejected its central choice. In the shape of
`docs/concurrency-decision.md`, `docs/errors-decision.md` and
`docs/const-decision.md`: what is decided, the evidence for it, the
alternatives that were rejected and why, and what it costs.

Every claim below marked *checked* was compiled and run against the compiler
as it stood on 2026-09-23. The programs are small and are quoted where they
matter. Source line numbers are from that snapshot; the files are moving, so
treat them as pointers to a function, not to a line. A bare `§N.M` is a
section of `docs/reference.md`; sections of this record are named.

---

## What changed, and why this is a rewrite rather than an edit

The first draft answered "is a function a value?" with *yes, and it has a type
you can write: `fn int(Point, Point)`*. The author read it and rejected the
type syntax outright:

> **There is no function type syntax. `Fn<...>` and the `fn` keyword are both
> rejected. A callback's type is an ordinary one-method interface, which this
> language already has and dispatches structurally.**

That is not a smaller version of the first draft; it moves the feature out of
the type system and into a mechanism that already exists and is already
frozen. The first draft's own representation section had quietly proved the
point — it concluded that a closure *is* a one-method object, that the IR
needs nothing, and that the desugaring output is "a program the language can
already compile". If the desugaring target is already writable, the type
spelling was never buying a mechanism. It was buying a shorter way to write a
type, at the price of a keyword, a new type form, a new parse ambiguity, an
invariance rule, a variance-free subtyping story, and a second thing in the
language that means "one operation".

So: **the interface stays, and the sugar is on the value side only.**

```c
interface Less { int cmp(Point a, Point b); }

Point smallest(List<Point> ps, Less order) {
    Point best = ps[0];
    for (Point p in ps) { if (order.cmp(p, best) < 0) { best = p; } }
    return best;
}

int by_x(Point a, Point b) { return a.x - b.x; }

smallest(ps, ByX());                                 // works today
smallest(ps, by_x);                                  // new: a function's name
smallest(ps, (Point a, Point b) => a.y - b.y);       // new: a lambda
```

All three lines pass the *same kind of value*: an object with a `TypeInfo`
whose vtable holds the code. The first is written by hand; the compiler
writes the other two.

Kept from the first draft, unchanged in substance and re-checked here: the
finding that `sort` and user-type map keys need no callbacks at all, the
representation analysis, the capture semantics, the `spawn` analysis, and the
cost accounting. Removed: the `fn` type, the `fn` keyword, invariance as a
rule needing statement, "only an identifier can be called", `__call`, and the
whole "function type versus interface" boundary — there is no boundary now.

---

## What the language already has — checked

Everything this design needs is present, and more of it than the first draft
credited.

**A one-method interface works, including generically.** *(checked)*

```c
type Point { int x; int y; }
interface Less<T> { int cmp(T a, T b); }
type ByX {}
int ByX.cmp(Point a, Point b) { return a.x - b.x; }

void isort<T>(List<T> xs, Less<T> order) { ... order.cmp(xs[j], xs[j-1]) ... }

List<Point> ps = [Point(3,1), Point(1,2), Point(2,3)];
isort(ps, ByX());          // prints 1 2 3, __rc_live=0
```

**A generic interface is not a future feature. It compiles today**, it
instantiates per type argument, and one program may hold several
instantiations of the same interface (`Less<Point>` and `Less<int>` side by
side, both dispatching correctly — checked). The first draft left this as an
open question; it is closed, and it closes affirmatively.

**An interface value goes everywhere a value goes.** *(checked, one program)*
a local, an element of a `List<Less>`, a field of a struct, a function's
return, a `Chan<Less>` payload, and the receiver of a method call on a call's
result (`pick().cmp(a, b)`). None of it needed a new rule.

**Dispatch is already indirect and already constant-folded.** `CallIface`
(`src/ir.rs`, around line 261) carries a **constant** slot index, and the C
backend emits exactly the cast-and-call a callback invocation needs
(`src/emit_c.rs:990`):

```c
v16 = ((int64_t (*)(Obj *, Obj *, Obj *))v1->ty->vtable[0])(v1, v12, v15); /* .cmp */
```

**Slots are keyed by a method's name *and its IR shape*** (`src/ir.rs:440`,
`src/lower.rs:2334`). Two interfaces declaring `cmp` with the same shape
share one slot; two declaring `cmp` with different shapes do not. Checked
both ways: one type satisfying two identically-shaped interfaces works and
dispatches identically through each.

**An interface value is a bare `Obj *`** — `struct Obj { long rc; const
TypeInfo *ty; }` (`runtime/rt.h:94`) — because the object knows its own type.
`IrTy` has four variants and only `Ref` is managed (`src/ir.rs`).

**The function namespace and the value namespace are already disjoint.**
*(checked)* A local may not take a function's name:

```
`f` is already a function; shadowing is not allowed, rename one
```

and a top-level function `cmp` coexists with a method `ByX.cmp` with no
conflict at all, because methods live in a per-type namespace. This is the
single most important pre-existing fact for this design, and *Where the
target type is known* below is built on it.

So the gap is not a mechanism, and it is not a type. **The gap is that no
expression in the language denotes a function.** A bare function name is
`unknown variable by_x` today *(checked)*. That is the whole of what this
record adds to the surface language, plus one literal form for the unnamed
case.

---

## Settled

### 1. A callback's type is a one-method interface. There is no function type

`interface Less { int cmp(Point a, Point b); }` is the declaration. There is
no `fn int(Point, Point)`, no `Fn<int, Point, Point>`, no `fn` keyword.

The case for this is not that a function type is bad. It is that in *this*
language a function type would be a second spelling of something already
spelled, and every consequence follows from that:

  - **The mechanism is identical.** A `fn int(Point, Point)` value would be
    an `Obj *` with a vtable whose slot 0 holds the code — which is exactly
    what `Less` already is. The first draft's own representation section
    proved this and then added a type form on top of it anyway.
  - **A type form is not free.** `int(Point, Point) by = ...;` breaks the
    parser's two-token declaration rule (`src/parser.rs`, the comment at the
    head of the statement parser: *"Declaration. Three spellings, all
    decidable with two tokens"*), because `int(x)` is a conversion and
    `Point(1,2)` is a construction, so a statement beginning `int (` needs a
    scan to the matching paren. The `fn` keyword exists in the first draft
    only to repair that. A keyword spent to repair a syntax that a language
    feature did not need is a bad trade.
  - **It would need its own assignability rule.** The first draft had to
    declare function types invariant, argue against Java's array covariance
    and C#'s delegate variance, and rule on what happens to defaulted
    parameters. Interface satisfaction already answers all of that, and
    already answers it the same way — **exactly**, checked in three
    directions below, under *Signature matching*.
  - **It would create a second "one operation" concept.** The first draft
    drew a line: *a function type when the contract is one unnamed operation,
    an interface when it has a name worth writing*. That line is not
    decidable by a reader. `Less`, `Pred`, `Sink`, `Route` — every one of
    them is one operation *and* has a name worth writing. A rule that
    requires taste to apply is not a rule this language keeps.
  - **The interface reads better where it is used.** `Point smallest(List<Point> ps,
    Less order)` says what the second argument *is for*. `Point
    smallest(List<Point> ps, fn int(Point, Point) by)` says what it is made
    of. The reference already prefers the former everywhere else: there is
    deliberately no built-in `ToStr` type, and §6.6 tells a program to write
    `interface ToStr { str to_str(); }` itself.

What is given up honestly: **the declaration.** `smallest(ps, by_x)` needs a
`Less` to exist somewhere. In Java that cost produced `java.util.function`
and its forty-odd interfaces, and that is the real risk here — it is named
and answered under *Naming the interfaces the library will declare*, which
is where the one-way rule does the work.

What is *not* given up, and is the thing the first draft undervalued:
**a callback with configuration needs no new mechanism.**

```c
type ByAxis { bool on_x; }
int ByAxis.cmp(Point a, Point b) { if (on_x) { return a.x - b.x; } return a.y - b.y; }

smallest(ps, ByAxis(true));
```

`ByAxis(true)` is an ordinary construction of an ordinary type, and it is a
`Less` because it has the method. Under a function-pointer design this needs
a closure; under a `fn` type it needs a closure; here it is the *base case*,
and the lambda is the abbreviation. A design where the general case is the
simple one is the right way round.

### 2. A function's name is a value where a one-method interface is expected

```c
int by_x(Point a, Point b) { return a.x - b.x; }
smallest(ps, by_x);
```

The compiler synthesises a zero-field type whose one method has the target
interface's method name and forwards to the function:

```
type   __ref$by_x$Less {}
int    __ref$by_x$Less.cmp(Point a, Point b) { return by_x(a, b); }
```

`__` is already a reserved prefix in every identifier (§10.1; checked — `int
__call(int a)` is refused with *"`__call` is reserved: a name may not begin
with `__`"*), so the synthesised name can never collide and can never be
written by a program.

This is safe *because a decision was already made for it*. `docs/roadmap.md`
records refusing to let a local take a function's name, and gives this as one
of three reasons:

> Closures are on this roadmap (§5), and the day a function can be a value,
> `digits` alone becomes genuinely ambiguous — so allowing it now is a
> breaking change waiting to happen.

The door was deliberately held open; this walks through it, and the rule
stops being precautionary and becomes load-bearing. It is the first item on
the freeze list for that reason.

**A method's name is not a value.** `p.area` is refused, and is refused
today with *"type `P` has no field `area`"* *(checked)*. §4.3's hazard is the
reason: `p.area` is a field read, and making it mean a bound method when the
type has no such field would mean that *adding a field later silently changes
what an existing expression denotes*. Inside a method it is worse — fields
and sibling methods are both reached by bare name. The workaround is one
lambda, `(Point p) => p.area()`, and bound method values are additive later.

`Type.method` as a static reference is left out for the same
add-it-later reason: nothing needs it.

### 3. A lambda is the same thing, unnamed

```c
smallest(ps, (Point a, Point b) => a.y - b.y);
```

The lambda has **no type of its own**. It takes its method name, and the
signature it is checked against, from the interface it is passed to. The
compiler synthesises a type whose fields are the captures and whose one
method is the target's:

```
(Point a, Point b) => a.y - b.y + bias        against `Less`, capturing `int bias`

type __lambda$7 { int bias; }
int  __lambda$7.cmp(Point a, Point b) { return a.y - b.y + bias; }
```

Parameter types are written; the body is one expression; the return type is
the interface's, not the body's. Detail and the diagnostics are under
*Lambdas*.

`=>` is free: `int y = x => 2;` fails to parse today with *"expected `;`,
found `=`"* *(checked)*, so no program can contain the token.

### 4. Calling a callback is a method call

`order.cmp(a, b)`. Never `order(a, b)`.

This is the rule the author stated as *two spellings of one thing is what
this language refuses*, and it is worth recording what it buys beyond
consistency:

  - **There is nothing to disambiguate.** Under the first draft, `f(x)`
    could be a call to a function `f` or a call through a local `f` of
    function type, `obj.handler(x)` had to be a method call and never a call
    through a field, and `xs[0](y)` had to be refused with a "bind it to a
    local first" diagnostic. Three rules, all of them about a spelling. Here
    `xs[0].cmp(y)` and `router.handler.cmp(y)` just work, because they are
    ordinary method calls on ordinary values — checked, including
    `pick().cmp(a, b)` on a function's result.
  - **The grammar does not move.** `atom = IDENT [ args ]` and
    `postfix = atom { "." IDENT [ args ] | "[" expr "]" | "?" }` are
    untouched. The
    first draft needed a rule saying only an identifier can be called; that
    rule and its diagnostic are gone.
  - **The reader sees the operation's name at the call site.**
    `order.cmp(p, best)` says what is being asked. `order(p, best)` says only
    that something is being invoked.

Today `l(1, 2)` on a `Less` local reports *"unknown function `l`"*
*(checked)*. That message should be improved once this lands, to name the
interface and its method: see the diagnostics table under *Lambdas*.

### 5. Two of the three blocked features are not blocked by this

`docs/stdlib-decision.md` ends by naming three features waiting on one
mechanism — `sort` by a comparison, a `Hashable` so a `Map` can key on a user
type, and `spawn` taking a closure — and says that is "worth knowing before
deciding it is a post-freeze concern". It is, and the sentence is wrong in a
useful way: **two of the three are not waiting on callbacks at all**, and the
third should not happen.

**`ps.sort()` on a user type with `cmp`.** The diagnostic says it plainly
today (`src/lower.rs:996`, checked):

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
compiler/runtime contract that `runtime/rt.c:1228` already describes for map
hashing. `rt_sort` gains a comparison path; nothing else changes.

**A `Map` keyed on a user type.** The same three slots answer it. `hash_key`
and `key_eq` (`runtime/rt.c:1263,1274`) switch on a `key_is_str` flag today;
they gain a third case that dispatches through slots 2 and 1. And there is no
need for a `Hashable` interface *at all* — which matters more now that
interfaces are the callback mechanism, because it keeps one interface out of
the library. The rule is the one §6.2 already uses for operators:

> A `Map` key is an `int`, a `str`, or a type that declares `int hash()` and
> `bool eq(T)`.

`eq` already exists and already means what it must (§6.2 desugars `==` to
it), so **`hash` is the only new name.** That is strictly less language than
an interface declaration, and it is the same shape of rule as everything else
about user types: define the method and the feature works; do not and the
compiler names the method it wanted. The current refusal is
`src/lower.rs:6986`, checked:

```
a map key must be int or str, found K; hashing a user type would need a
Hashable interface, which does not exist yet
```

— and the answer is that the interface should not exist, then or now.

One honest caveat: a **module constant** `Map<P, int>` stays refused. The
compiler lays constant maps out as static data and must therefore compute
each key's hash itself (`src/lower/consts.rs`, `map_hash`), and it cannot run
a user's `hash` method at compile time. Small, well-bounded, with a clear
message, and additive to relax if constant evaluation grows up.

**So `sort()` and user-type map keys should ship before any of this, and
independently of it.** They are a week of work against a reserved slot
convention, not a language feature — one that adds no surface at all beyond
the name `hash`. The claim in `docs/stdlib-decision.md` that they are "the
function-reference problem again" is true only in the sense that a vtable
entry is a function reference: one the C emitter already writes statically,
needing nothing from the IR and nothing from the surface language.

*(Another agent is implementing this now. Nothing in the rest of this record
depends on it, and nothing in it depends on the rest of this record.)*

### 6. `spawn` keeps taking a function and arguments

`docs/roadmap.md` §5 and `docs/concurrency-decision.md` both leave this open.
It should be closed: **`spawn f(x, y);` stays exactly as it is, and `spawn`
never takes a callback object as the thing being spawned.**

The reason is the move rule, not the implementation, and the hand-written
equivalent demonstrates it precisely. Two programs, both checked:

```c
type Job { Chan<int> out; int n; }
void Job.run() { send(out, n * 2); }
void worker(Task t) { t.run(); }

Chan<int> out = Chan<int>(1);
spawn worker(Job(out, 21));           // prints 42, __rc_live=0
```

```c
str shared = concat("ab", "cd");
spawn worker(Job(out, shared), out);  // `shared` is captured AND still live
print(shared);
```
```
trap: value crossing a thread boundary is still referenced elsewhere;
clone() it, or drop the other reference first
```

The distinction is sharper than the first draft stated it, and worth getting
right. A **local passed directly** to `spawn` is *moved* at compile time —
`spawn work(shared, out); print(shared.bias);` is refused with *"`shared` was
moved and cannot be used again"* *(checked)*. What traps at run time is a
value **inside** the spawned graph that the spawning frame also holds by
another name. A capture is exactly that: the closure holds a +1, and the
spawner's own local holds another. So a closure-taking `spawn` would trap in
the ordinary case — every capture of a local the spawner still has in scope —
which makes it a feature that looks convenient and fails at run time, at a
place the type system cannot warn about.

The alternatives are both worse:

  - **Move-capture** (Rust's `move`, C++'s `[x = std::move(x)]`): a second
    capture mode, so a capture list to select between them, so two spellings
    for one idea.
  - **Make capture a move always**: then a comparison that captures a list
    kills the caller's list, which is absurd everywhere except `spawn`.

Meanwhile `spawn f(x, y)` is *better* than a closure for this job: the
arguments are the captures, they are moved explicitly, the move checker sees
them by name, and the reader sees at the call site exactly what crosses.
That is the design `docs/concurrency-decision.md` arrived at for its own
reasons, and callbacks do not improve on it.

A callback can of course be *passed to* a spawned function — it is an
ordinary value — and then the existing rules apply unchanged. Checked:
`spawn work(Asc(), out);` with `void work(Less o, Chan<int> out)` runs and
leaves `__rc_live=0`.

Worth recording, because it is counter-intuitive: a closure-taking `spawn`
would *simplify* the backend. The emitter generates one argument struct and
one trampoline per spawned function name (`src/emit_c.rs:294-345`); with a
closure there would be one trampoline for the whole program. The reason to
refuse it is semantic, not mechanical.

---

## Where the target type is known, and where it is not

This is the section the new design makes necessary, because a bare function
name and a lambda are the first expressions in the language whose meaning
depends on what is expected of them. The question has three parts, and two of
them turn out to be non-questions.

### The function/method collision is not a collision

*What if a type has a method matching the interface and a function of the
same name is in scope?*

Nothing happens, because the two names are never in the same position.
Checked, in one program that compiles and runs:

```c
type Point { int x; int y; }
int cmp(Point a, Point b) { return a.x - b.x; }     // a module function
type ByX {}
int ByX.cmp(Point a, Point b) { return a.x - b.x; } // a method on ByX
interface Less { int cmp(Point a, Point b); }

Less l = ByX();
print(str(l.cmp(Point(1,0), Point(2,0))));   // -1, the method
print(str(cmp(Point(1,0), Point(2,0))));     // -1, the function
```

A method is reached only through a receiver and is resolved in the receiver's
type. A function is reached only by a bare name and is resolved in the
module. The new rule adds one place a bare name may appear — as an argument
whose parameter is a one-method interface — and in that position a *method*
name cannot appear at all, because *A function's name is a value* refuses
`p.cmp` as a value. So there is exactly one candidate, always.

The one thing that does become possible is a function *accidentally*
satisfying an interface it was not written for: `int cmp(Point, Point)` will
match any `Less`-shaped interface in the program. That is the hazard
`docs/types.md` already names and accepts for types — a `Shape.draw()`
silently satisfying a `Cowboy.draw()` — and it is *smaller* for functions
than for types, because a function only ever satisfies an interface at a site
where the programmer wrote its name. A type satisfies silently, everywhere.

### Two interfaces with differently-named methods

*If a function could satisfy either `Less { int cmp(..) }` or `Rank { int
score(..) }`, which does `smallest(ps, by_x)` mean?*

Whichever `smallest` declared. **The method name comes from the target, and
the target is a declared type, never an inference.** There is no case where a
bare function name or a lambda has to choose between two interfaces, because
it never sees more than one.

A type may satisfy both at once, and that is fine and needs no rule —
checked: one type with `int A.cmp(int,int)` satisfies two interfaces
declaring the same shape under *different* names, and two interfaces
declaring the same name and shape share one slot and dispatch identically.
Satisfaction is not exclusive. A synthesised wrapper is per (function,
interface) pair, so `smallest(ps, by_x)` and `rank(ps, by_x)` synthesise two
zero-field types; both are static, both are immortal, and neither costs a
run-time anything (*Representation*).

### Where the target *is* known

Every position in the language where a value lands has a written type,
because every binding in the language writes its type. All of these are
checked as interface positions today and all of them therefore accept a
function name or a lambda:

| position | the target comes from |
|---|---|
| an argument to a parameter of interface type | the parameter's declaration |
| a `const`/plain local declaration | the declared type |
| an assignment to a local, a field or an element | the declared type of the destination |
| a `return` | the function's return type |
| an element of a collection literal | the declared type of the binding it initialises |
| `send(c, ..)` | the channel's element type |
| a field of a construction | the field's declared type |
| a default for a parameter or a field | the declared type |

So *yes* to every part of the author's question: a bare function name may be
assigned to a local of interface type, stored in a `List<Less>`, returned,
sent over a channel, and captured by a lambda — because in each case the
destination's type is written down. Checked in one program, with an
ordinary hand-written type standing in for the synthesised one: local, list
element, struct field, function return, channel round trip, all working with
`__rc_live=0`.

### Where the target is *not* known

Three positions, and the diagnostic is the same shape in all three: name the
expression, say why the type is not known, and say what to write instead.

**1. An argument to an unconstrained generic parameter.**

```c
void show<T>(T v) { ... }
show(by_x);                 // T could be any interface, or none
```
```
by_x.m31:9:6: `by_x` is a function; it becomes a value only where a
one-method interface is expected, and `T` here is not one — bind it to a
local of the interface type first
```

This is not hypothetical: the analogous case is refused today for a
hand-written type. `void f<T>(Less<T> o); f(ByX());` gives *(checked)*

```
cannot infer type parameter `T` of `f` from its arguments or from where its
value goes; bind an argument, or the result, to a local with a written type
first
```

because structural satisfaction cannot be run backwards to recover `T`. Which
gives the general rule, and it is the most important one in this section:

> **Information flows from the target interface to the callback, never back.**
> A function name and a lambda are *checked* against a type the compiler
> already knows. Neither is ever a source of type inference.

`isort([Point(3), Point(1)], ByX())` works because `T` is `Point` from the
list *(checked)*, and `isort([1, 2], ByX())` is refused naming the
instantiated interface — *"expected `Less<int>`, found `ByX`"* *(checked)*.
A lambda argument behaves identically: the interface is resolved first, and
the lambda is then checked against `Less<Point>`. *Generics* says what this
costs.

**2. A builtin whose parameter is not an interface.** `print(by_x)`,
`str(by_x)`, `clone(by_x)`. `print` already refuses an interface
value — *"cannot print a value of type `Less`; give it a method `str
Less.to_str()`"* *(checked)* — so the function-name case should be refused one
step earlier, at the point the name is resolved, with the message above.

**3. An expression statement.** `by_x;` on its own. Refused as an expression
with no effect, which is what it is.

### Signature matching: exact, and the receiver does not count

Interface satisfaction is exact today, in both directions. Checked, three
programs, all refused:

| the method | the interface wants | result |
|---|---|---|
| `int A.cmpx(int, int)` | `int cmp(int, int)` | `` `A` needs a method `int cmp(..)` to satisfy `Less` `` |
| `float A.cmp(int, int)` | `int cmp(int, int)` | ``...`int cmp(..) with a matching signature` `` |
| `int A.cmp(int)` | `int cmp(int, int)` | ``...`int cmp(..) with a matching signature` `` |
| `void T1.take(Sq)` | `void take(Shape)` — `Sq` satisfies `Shape` | refused: **no contravariance** |
| `Sq T2.make()` | `Shape make()` — `Sq` satisfies `Shape` | refused: **no covariance** |

So the answer to *exact match only, or are subtypes allowed?* is **exact
only**, and it is not a new decision: it is what the compiler already does,
and it is right, because the only "subtyping" in the language is interface
satisfaction, which is a *structural* relation with no runtime coercion —
a `Shape` slot holds an `Obj *` whose vtable is laid out for `Shape`'s slots,
and a `Sq` passed where a `Shape` is wanted works because the slot index is
global, not because a conversion happens. Allowing variance would require the
conversion the design does not have. The first draft needed a paragraph
arguing against Java's array covariance and C#'s delegate variance to reach
the same place; here it falls out.

A function name is matched the same way, with one clarification that has to
be written down:

> **The receiver is not a parameter.** A function of *n* declared parameters
> matches an interface method of *n* declared parameters, with the same
> types, in order, and the same return type. The synthesised wrapper supplies
> the receiver and it carries nothing.

The consequence is that a function can never satisfy an interface whose
method takes its data through `this`. `interface ToStr { str to_str(); }`
cannot be satisfied by any function, because `str show(Point p)` has one
parameter and `to_str` has none. That is correct — `ToStr` is a property of a
*type*, not an operation over arguments — and the diagnostic should say so:

```
`show` takes 1 parameter; `ToStr.to_str` takes none and gets its value from
the receiver, so no function can satisfy it — give `Point` a `to_str` method
```

**A function with an optional parameter cannot be a callback**, and this too
is already the behaviour, for free. Checked:

```c
interface Scale { int go(int v); }
type S {}
int S.go(int v, int by = 2) { return v * by; }
Scale s = S();
```
```
type mismatch: expected Scale, found S: `S` needs a method
`int go(..) with a matching signature` to satisfy `Scale`
```

The first draft had to invent a rule for this ("`fn int(int, int) f = scale;`
is refused, naming the parameter"). Here arity already differs, so the
existing check catches it. The reason it *should* be caught is §4.2's: an
optional parameter is named at the call site, and a dispatch site has no name
to give, so accepting it would make `by` positional at one call and named at
another.

**A generic function cannot be a callback**, because an interface cannot
declare a generic method. Checked: `interface Any { int f<T>(T a); }` is a
parse error, *"expected `(`, found `<`"*. This is right — a vtable slot is a
single code address and a generic method has one per instantiation — but it
means `int first<T>(List<T> xs)` can never be passed anywhere. The workaround
is an instantiating wrapper, `int first_int(List<int> xs) { return
first(xs); }`, and a diagnostic should say that rather than repeating "needs
a matching signature".

---

## Lambdas

### The form

```c
(Point a, Point b) => a.x - b.x
() => print("tick")
(int x) => x * 2
```

Parameter types are written. The body is **one expression**. The method name,
the parameter count, the parameter types and the return type are all taken
from the target interface and the lambda is *checked* against them.

**A lambda may appear anywhere a one-method interface is expected** — the
table under *Where the target is known* — including at the top level of a
declaration:

```c
Less f = (Point a, Point b) => a.x - b.x;       // yes: `Less` is written
```

which is the case the author asked about, and it works for the same reason
every other position does: the destination's type is written down. Note what
it does *not* mean: `f` is an ordinary `Less` value from that point on, and
it is called `f.cmp(p, q)`.

### The diagnostics

Each of these is a case the design creates, and each needs a message that
says what the compiler wanted rather than what it found.

| the mistake | the message |
|---|---|
| a lambda where no interface is expected — `print((int x) => x)`, `show((int x) => x)` for `show<T>(T v)` | `` a lambda takes its method name from the interface it is passed to, and nothing here expects one; declare an interface and bind it to a local of that type `` |
| the target interface has **two or more** methods | `` `Sink` declares 2 methods (`write`, `flush`); a lambda supplies one, so write a type with both `` |
| **arity mismatch** — `(int a) => a` against `int cmp(int, int)` | `` `Less.cmp` takes 2 parameters, this lambda takes 1 `` |
| **parameter type mismatch** | `` `Less.cmp` takes (Point, Point), this lambda takes (int, int) `` |
| **return type mismatch** — the body's type is not the method's | `` `Less.cmp` returns int, this lambda's body is a str `` |
| the target interface method returns `void` and the body has a value | accepted, and the value is discarded — same as an expression statement |
| the target interface method returns non-`void` and the body is a `void` call | `` `Step.of` returns str, but `print(x)` has no value `` |
| `?` in the body | `` `?` returns from the enclosing function, and a lambda has no enclosing function a reader can act on; write a named function `` |
| a parameter shadows anything visible | the existing §4.1 message, unchanged |

The arity and type messages all name the *interface method*, not the lambda,
because that is where the truth is.

### Why written parameter types

Against Oro's `x => expr` — the author's own prior art — and against every
other language with lambdas.

  - **Every binding in this language writes its type.** `int x = 5;`, `case
    Circle(int r):`, `for (int x in xs)`, every parameter, every field. There
    is no `var`. An inferred lambda parameter would be the first place in the
    language where a name is introduced without its type beside it. The §3.8
    diagnostic that refuses `make().pick(xs)` — *"bind it to a local first"*,
    so the receiver's type is written down — is the same instinct.
  - **It keeps the checking one-directional.** With types written, the lambda
    is checked against the interface exactly as a hand-written type's method
    is, by the code that already does that. Without them, the compiler must
    *push* the interface's parameter types into the body before it can type
    the body — a second, inward direction of inference in a language that has
    only ever had the outward one. It is a small amount of code and a large
    amount of rule.
  - Oro's `x => expr` is the right answer for Oro, which is dynamically typed
    and has no type to write. The evidence does not transfer.

The cost is stated plainly: **it is verbose.** `seq.map(xs, (int x) => x * 2)`
is not as good as `xs.map(x => x * 2)`, and everyone who has used a language
with inferred lambda parameters will notice. Two mitigations, both real:
relaxing this later is **additive** — every program written with the types
still compiles when the types become optional — and the interface's method
signature is right there in the declaration, so the types were never secret.

### Why one expression and no block body

The first draft argued this on "two spellings of one lambda". That holds, but
the stronger argument is the evidence the author asked for, and it points the
same way.

  - **Oro**, the author's own frozen language, has `x => expr` and nothing
    else — no block lambda — and has a standard library of a dozen modules
    written in it (`io`, `json`, `http`, `csv`, `args`, `fs`, `date`,
    `random`, …) plus a real deployed application. If a block body were
    necessary for library-scale code, that is where it would have shown.
  - **Java** shipped both (`x -> expr` and `x -> { return expr; }`) and got
    the predictable result: two spellings of most lambdas, a style guide in
    every project about which to use, and `return` meaning something
    different at two nesting depths.
  - **C#** shipped both, then had to add expression-bodied *methods*
    (`int F() => x;`) so that the two forms would stop being a lambda-only
    distinction. The block form did not simplify anything; it multiplied.

Here, the one-expression rule also does three jobs beyond consistency: the
return type comes from one expression rather than a flow analysis over
`return` statements; the lambda stays short enough that an inferred capture
list is readable; and **there is no assignment inside a lambda**, which is
what makes the capture rule below a non-question rather than a design.

Anything longer is a named function, and a named function's name is now a
value, so nothing is inexpressible — the workaround is one line and it has a
name. Adding a block body later is additive.

---

## Capture

Restated for lambdas-as-interface-objects. Each claim below was verified by
compiling the hand-written equivalent — a type with fields and a method — and
that is exactly the point: **the captures are the synthesised type's fields,
so every existing rule reaches them with no new code.**

### One mode, and it is what `=` already means

A lambda's captures are the names from the enclosing scope its body mentions.
Capturing does what binding does everywhere else: an `int`, `float` or `bool`
is copied, and a reference is aliased and retained.

That is §7.2's existing rule, reached by construction. **The lambda is a
construction (§6.4) of a synthesised type whose fields are the captures**,
and "a value stored into a field is retained by the container" is already the
protocol.

**No capture list.** C++ needs `[=]`, `[&]` and `[x]` because it has two
capture modes and two lifetimes; a dangling `[&]` capture is one of the
best-known bugs in the language. With one mode there is nothing to list. The
cost is that a reader cannot see at a glance what a lambda holds — mitigated
by the one-expression body, which puts every capture on the same line.

**Inside a method, the receiver is one capture.** *(Added when Stage 4
landed; the draft above left it undecided.)* A bare field name, a bare
sibling call and `this` are the same thing — §4.3 says so in as many words,
*"a method of the receiver is called by its bare name, as a field is
read"*, and refuses `this.f` and `this.m()` because the bare form is the
only spelling. They are all `this.`, unwritten. So a lambda that mentions
any of them captures **the receiver**, once, as a reference — which is
exactly what `=` does with `this`.

```c
type Counter { int base; List<int> seen; }
int Counter.twice() { return base * 2; }
Get Counter.both() { return () => base + twice(); }   // ONE capture: this
```

The consequence to state plainly, because it is the one place capture is not
a snapshot of anything: a field read inside a lambda reads the **live**
object, so reassigning the field is visible through the lambda. That is not
an exception to "the binding cannot be changed, the object can" — it is that
rule, with `this` as the binding. Capturing each mentioned field's value
instead was the alternative, and it was rejected because a lambda that
mentions a field and calls a method would then hold a stale copy of one and
the live object for the other.

### The four claims, each checked

**1. Refcounting applies unchanged.** A capture is a field store, so it takes
a +1, and the synthesised type gets a generated `drop_T`, `walk_T` and
`copy_T` like every other type (`src/emit_c.rs:73,135,165`). Checked
indirectly by every program in this record ending `__rc_live=0`.

**2. `const` applies unchanged, including the deep copy.** Checked:

```c
type Env { List<int> xs; int n; }            // the captures, as fields
int Env.call(int v) { return xs[0] + n + v; }

void go() {
    List<int> src = [10, 20];
    const Env e = Env(src, 5);               // `src` is held outside
    print(str(e.call(1)));                   // 16
    src.push(99);
    print(str(src.size()));                  // 3
    print(str(e.xs.size()));                 // 2  <- the snapshot is a copy
}
```

`rt_snapshot` saw that `src` was reachable from outside the value being
frozen and took a **deep copy**, exactly the middle row of
`docs/const-decision.md`'s table. So `const Less f = (int a, int b) => a +
n;` will freeze the lambda *and its captures*, deeply, with the ordinary
copy when a capture is shared — at that line, with the cost visible in the
reference (§4.1) rather than in the syntax.

**3. Freezing is deep and the runtime enforces it.** Checked:

```c
type Env { List<int> xs; }
void Env.bump() { xs.push(1); }
void go() { const Env e = Env([1, 2]); e.bump(); }
```
```
trap: cannot modify a constant; clone() it for a copy that can be changed
```

A frozen lambda's captured list cannot be pushed to, through the lambda or
through anything else, because the flag is on the object (§ "constness
belongs to the object, not the name").

**4. The resource rule applies unchanged, at compile time.** Checked:

```c
type Res { int fd; }
void Res.drop() { print("closed"); }
type Env { Res r; }
void go() { const Env e = Env(Res(3)); }
```
```
`const` cannot hold `Env`: it can hold `Res`, which owns a resource (it has a
destructor); a constant can neither freeze a resource nor copy one, so bind it
without `const`
```

`resource_in` (`src/lower/consts.rs:371`) walks fields, and a lambda's
captures *are* fields, so **a lambda that captures an `io.File` cannot be
bound to a `const`**, with that message, with no new code. `rt_snapshot`'s
runtime backstop catches what the type cannot show, through an interface.

**5. Destructors apply unchanged.** Checked: a holder with a `Res` field runs
`Res.drop` when the holder's count reaches zero, at the right moment, before
the next statement. So a lambda holding a `File` releases it when the
lambda's count reaches zero. A synthesised type cannot declare a `drop` of
its own — it is anonymous, there is nowhere to write one — which is the right
answer, and it is already enforced from the other side: §4.4 says an
interface may not declare a method named `drop`.

### A capture cannot be reassigned; the object it names can be changed

Swift and JavaScript let a closure assign a captured local and have the
outside see it; Kotlin does too and pays for it by boxing the variable in a
`Ref` object; Rust needs `move` plus a `RefCell` to get the same effect
safely. None of that machinery is needed, because the question does not
arise: **a lambda body is one expression, so there is no assignment inside a
lambda at all.**

What remains is the ordinary aliasing the language already has. `(int v) =>
xs.push(v)` captures `xs` by retaining it; pushing changes the one list and
the outside sees it, because that is what a second name for a list means
everywhere (§3.2). So:

> **The binding cannot be changed. The object can.**

This also means each lambda made in a loop captures that iteration's value,
which is Java's effectively-final semantics and dodges the classic JavaScript
`var i` loop-closure bug by construction. Go had to change loop variable
scoping in 1.22 to fix the same bug; it cannot occur here.

### No shadowing applies, unchanged

A lambda's parameters are declarations, so §4.1 applies: they may not shadow
a local, a parameter, `this`, a type name, **a function**, a builtin, an
import or a module constant visible at that point. Checked, in its existing
form: `int f(int a) {...} void g() { int f = 3; }` gives *"`f` is already a
function; shadowing is not allowed, rename one"*.

This is a real cost and it lands where it is most annoying: short lambda
parameter names collide in precisely the functions that have short locals. It
is not worth an exception — a reader looking at `x` in a lambda body inside a
function with another `x` is the case the rule was written for — and the fix
is a rename, which is what the no-shadowing rule always charges.

### Cycles: the honest cost, and no weak references

`docs/roadmap.md` §5 gives this as the reason closures are late, and it is
the right worry. Checked, and it behaves exactly as feared:

```c
interface Handler { void run(); }
type Node { List<Handler> hs; int n; }
void Node.run() { print(str(n)); }
void go() { Node n = Node([], 7); n.hs.push(n); n.hs[0].run(); }
go();
```
```
7
done
__rc_live=2
```

Two objects leak, and the language's own leak check sees it. Since
`docs/destructors-decision.md` a leaked cycle holds resources too, not just
memory. Swift's `[weak self]` exists for exactly this pattern, and
`docs/concurrency.md` records Nim's version of it — a closure and an iterator
referencing each other, "nothing would be released" — much of why Nim shipped
a cycle collector.

**The recommendation is to accept it and not add weak references:**

  - A weak reference is a second reference shape or a second header bit, a
    null check on every read, and therefore an `Option`-returning
    dereference — a second memory model, added for one pattern.
  - The exposure is narrower than it looks. **A callback with no captures is
    immortal** (*Representation*) and can never be in a cycle, and that is
    the majority
    case: `sort.by(xs, by_x)`, a handler table of top-level functions. A
    capturing lambda that is only ever an argument dies with the call.
  - The language's answer to cycles is already "avoid the shape". This adds
    one shape to the list rather than a new category of problem — and note
    that the shape is *already reachable today*, by hand, as the program
    above shows. Lambdas make it easier to write, not newly possible.

So the rule to write down is the pattern, not a mechanism: **do not store a
callback in a field of an object it captures.** Revisit only if real programs
hit it; weak references remain additive.

---

## Representation

### The claim, checked

> A callback is an object with a `TypeInfo` whose vtable holds the code, plus
> captured fields, so it is an ordinary `Obj *` and the IR needs no new
> reference shape.

The claim holds, and is stronger than stated.

  - `IrTy` is `I64 | F64 | I1 | Ref` (`src/ir.rs`), and `docs/ir-v0.md:38`
    states the invariant: "`ref` is the only managed type". A callback is a
    `Ref`. Nothing to add.
  - The code pointer does **not** live in the object. It lives in the type's
    vtable, a static array the C emitter already writes
    (`src/emit_c.rs:216-241`). So the object is header plus captures, and
    every instance made at one lambda site shares one `TypeInfo`. Checked in
    the emitted C for the `ByX` program:
    ```c
    static const AnyFn vt_T6[] = { (AnyFn)fn_e1____ByX___cmp };
    static const TypeInfo ti_T6 = { NULL, vt_T6, NULL, copy_T6, NULL };
    ```
  - The invocation is `CallIface` with a constant slot — the instruction that
    already exists, emitted today as the cast-and-call quoted in §"What the
    language already has".
  - Slot keying by name *and* IR shape gives one slot per distinct callback
    shape in the program, automatically.

**So the IR gains nothing. The C backend gains nothing.** No
`call_indirect`, no function-pointer IR type, no per-closure struct emitted by
hand, no new drop function — `drop_T`, `walk_T`, `copy_T`, the vtable and the
`TypeInfo` are all generated per type today and a synthesised type is a type.
`README.md` §3 predicts "one new IR op (`call_indirect`), a function type, and
a heap environment"; **none of the three is needed** — the first does not
exist, the second is now explicitly rejected, and the third is a `TypeDecl`.

What changes is the **front end only**: each lambda site becomes a
synthesised type plus a synthesised method, and each referenced function name
becomes a synthesised zero-field type whose method forwards. Both are emitted
before monomorphisation, and both are programs the language can already
compile — which is the best available evidence that the representation is
right.

**Note what the new design removes from this section.** The first draft had
to reserve the method name `__call` to close an accidental-satisfaction hole:
a user type must never become structurally a function by having a method
called `call`. Under the interface design there is no "callable" notion to
satisfy accidentally, so **`__call` is not needed and is not reserved.** The
synthesised method simply *is* the target interface's method, with the target
interface's name. Only the synthesised **type** name needs to be unwritable,
and the existing `__` prefix reservation already provides it, at no cost.

### A callback with no captures is a static, immortal object

A callback with no captures — every function name, and every lambda that
mentions nothing from its scope — has no fields, so every instance is
identical, so the compiler emits **one static instance per (function,
interface) pair** with `rc = RC_IMMORTAL` (`runtime/rt.h:21`), exactly as a
string literal is emitted. The consequences all follow from rules already
written down:

  - no allocation, ever;
  - `rc_inc`/`rc_dec` on it are a compare and return, so passing one around a
    hot loop is free;
  - `RC_IMMORTAL` is all-bits-set, so it reads as frozen, which is correct —
    there is nothing in it to change;
  - `reach_add` returns immediately for an immortal (`runtime/rt.c:1705`) and
    `rt_check_unique` returns immediately for one (`runtime/rt.c:1763`), so
    **a bare function reference crosses a thread boundary at zero cost and
    can never trap the uniqueness check.**

This is a genuine improvement over the hand-written interface, and it is
measurable today: `isort(ps, ByX())` **allocates**. Checked in the emitted C —

```c
v11 = rt_alloc(sizeof(T6), &ti_T6);          /* ByX(), a type with no fields */
```

— one heap object per call, for a value with nothing in it. `sort.by(ps,
by_x)` will cost one static struct in the binary and nothing at run time.

This does need one capability the compiler does not have: **emitting a static
instance of a user type.** `docs/const-decision.md` records the same gap from
the other side — "only a user-type module constant is still refused, because
the compiler cannot yet lay one out statically" — and it is still refused
*(checked: `const Env e = Env(src, 5);` at module level gives "a constant must
be an int, float, bool, str or bytes, or an Array, List or Map of those")*.
The callback case is the easy subset: a struct with nothing but a header.
Doing it opens the harder one.

Compare: Nim's closure is a fat `(proc, env)` pair, a second reference shape;
Rust's is unboxed and free but unnameable, which is why `Fn`/`FnMut`/`FnOnce`,
`impl Fn` and `Box<dyn Fn>` all exist; Go's is a heap object much like this
one but reached through a fat interface value.

### Cost of a call

Two loads and an indirect call — `o->ty`, then `vtable[k]`, then the call —
which is what every interface method call already costs, and one load more
than Go's fat pointer. It cannot be inlined, so a sort driven by a callback
will be several times slower than the built-in `sort()` on `int`s, which
compares inline. That is the price of a callback that can be written down,
and it is the price Java and Go pay. Devirtualising a call whose callback is
known at the site is an optimisation, addable after the freeze.

---

## Generics

### Generic interfaces work today

`interface Less<T> { int cmp(T a, T b); }` compiles, instantiates and
dispatches, in a generic function and with several instantiations live at
once *(checked, both)*. So **nothing has to change for `sort<T>` to take
one.** The first draft listed this as an open question; the answer is that it
was never open.

Three properties worth recording, because the design leans on all three:

  - The interface is monomorphised with everything else. `Less<Point>` and
    `Less<int>` are different types and take different slots, because the
    slot key includes the shape.
  - Satisfaction is checked **after** the type arguments are resolved. `f([1,
    2], ByX())` reports *"expected `Less<int>`, found `ByX`"* — the
    instantiated interface, named *(checked)*.
  - A type parameter may be constrained by an interface and nothing else
    (`docs/types.md` §8, "Constraints: interfaces only"), and §9 of the
    reference lists "constraints on type parameters" as not in the language
    at all. Neither matters here: `sort<T>(List<T>, Less<T>)` needs no
    constraint on `T`, because everything it does to a `T` it does through
    the `Less<T>` it was handed. `docs/types.md` §8's own sketch —
    `T pick<T>(T a, T b, fn(T, T) bool)` — becomes
    `T pick<T>(T a, T b, Less<T> order)`, and that sketch's `fn` spelling
    should be struck from that document when this lands.

### Inference runs one way, and that is a real constraint

The rule from *Where the target type is known* — information flows from the
target interface to the
callback, never back — has a consequence for library design that the first
draft got backwards. It claimed *"a lambda argument carries its own parameter
types, so inference reaches `T` from it under §3.8's existing rule"*. That is
not how the compiler works and should not be made so:

  - §3.8 requires each type parameter to be reached by a mandatory parameter
    whose argument has a **written type** — a literal, a construction, an
    enum variant, a local or a parameter. A callback argument is none of
    these, and an interface-typed argument already fails to reach a type
    parameter *(checked, g1 above)*.
  - Making a lambda an inference source would mean matching its written
    parameter types back against an uninstantiated interface method
    signature, which is unification, which is the thing §3.8 was written to
    avoid.

So: **a type parameter must be reachable without the callback.** For
`void sort.by<T>(List<T> xs, Less<T> order)` that is free — `T` comes from
`xs`. For a two-parameter shape it is not, and §3.8's second rule takes over:
what the arguments leave open is inferred from where the value goes. Checked,
with a hand-written `map`:

```c
interface Step<A, B> { B of(A x); }
List<B> mapl<A, B>(List<A> xs, Step<A, B> f) { ... }

List<str> ss = mapl(ns, Dbl());     // works: A from `ns`, B from the declaration
print(str(mapl(ns, Dbl()).size())); // refused
```
```
cannot infer type parameter `B` of `mapl` from its arguments or from where its
value goes; bind an argument, or the result, to a local with a written type first
```

**Which means a chain cannot infer.** `xs.map(f).filter(g)` is not merely
undesirable in this language; under the current rules it is *uncallable*,
because the intermediate result has nowhere to take its type from. That is
new evidence, and it lands squarely on §"Iterators" below.

### Monomorphisation

A lambda written inside a generic function becomes a synthesised generic type
and is instantiated per instantiation of its enclosing function — no new
machinery, because `src/mono.rs` is an AST→AST pass that already does this
for nested generic types, and the desugaring happens in the AST before it.

An interface *parameter* does not multiply code: `sort.by<Point>` is
instantiated once and takes any `Less<Point>`, dispatched dynamically. The
alternative is Rust's — monomorphise the callee per callback type — which is
faster and is not available here, because it requires callback types that
cannot be written down, which is the opposite of this design.

---

## Iterators, `map` and `filter`

Wanted eventually; not blessed by the language, now or probably ever. The
first draft's reasoning still holds and there is now a second, harder reason.

**The first reason, unchanged.** Oro's chains are the author's own prior art
and they are genuinely good, but they are good because Oro's collection
protocol makes a chain **one pass** with early exit. Reproducing that needs a
lazy sequence protocol — an interface, a generic, a callback per stage — and a
decision about whether `map` allocates. Without the protocol,
`xs.map(f).filter(g)` allocates two intermediate lists to save one `for` loop,
which is a bad trade in a language with no GC and a visible allocation cost.

**The second reason, found while checking this record.** A chain does not
typecheck. `mapl(ns, f).size()` is refused because the element type of the
intermediate has nowhere to come from, and no blessing of `map`/`filter` as
methods fixes that without adding backward inference. So the syntax that
makes chains attractive is the syntax the inference rules refuse, and the
form that *does* work —

```c
List<str> names = seq.map(users, (User u) => u.name);
List<str> short = seq.filter(names, (str s) => s.size() < 8);
```

— is a local per stage, which is a `for` loop with extra allocation. That is
an argument against the feature, not for it.

The right shape stays: **once callbacks exist, `map` and `filter` become
ordinary generic functions a program can write**, a `seq` module can be built
and judged on its merits in the language after the freeze, and the language
blesses nothing. That is the whole point of the feature — it moves
higher-order code from the compiler's privilege to the program's.

---

## Naming the interfaces the library will declare

This is the cost the design takes on, and the place it can go wrong. Java's
`java.util.function` is what happens when interfaces are named after their
*shape* — `Function`, `BiFunction`, `Supplier`, `Consumer`, `BiConsumer`,
`Predicate`, `BiPredicate`, `UnaryOperator`, and then the primitive
specialisations. Forty-odd names, none of which says anything about the
problem being solved.

The rule that prevents it here is the language's own: **name the interface
after what the operation means, never after its arity or its types, and
declare it in the module that needs it.** If two modules want the same
meaning they share one; if they want different meanings they get different
names even when the shapes match. A shape with no meaning does not get an
interface at all — it gets a `for` loop.

Applying that:

### Comparison — the one the library needs now

```c
// lib/sort.m31
pub interface Order<T> { int cmp(T a, T b); }

pub void       by<T>(List<T> xs, Order<T> order);        // sort in place
pub T          max<T>(List<T> xs, Order<T> order);
pub T          min<T>(List<T> xs, Order<T> order);
pub Option<int> search<T>(List<T> xs, T want, Order<T> order);  // on a sorted list
```

used as `sort.by(ps, by_x)`, `sort.max(ps, (Point a, Point b) => a.y - b.y)`.

  - **`Order`, not `Less` or `Comparator`.** It is a total order, not a
    predicate, and the method returns three-way. `Less` would mislead a
    reader into returning a `bool`, which is the single most common mistake
    in every language with a comparison callback.
  - **The method is `cmp`, which §6.2 already means.** A user type's natural
    order is `int T.cmp(T other)` — receiver plus one parameter — and an
    external order is `int cmp(T a, T b)` — two parameters. Different shapes,
    so different slots, no collision *(the slot key includes the shape —
    checked)*. One name, one meaning, two receivers. A reader learns `cmp`
    once.
  - **Negative, zero or positive**, matching §6.2 exactly, so that a
    hand-written order and the built-in ordering never disagree about what a
    comparison returns.
  - The division of labour is clean and needs no explaining: **`xs.sort()`
    uses the type's own `cmp`** (the reserved slot above); **`sort.by(xs,
    order)` uses the order you hand it.** One is the natural order, one is a
    chosen order, and there is exactly one spelling of each.

### Hashing — nothing

The other agent's work leaves **no interface at all**. A `Map` key is an
`int`, a `str`, or a type declaring `int hash()` and `bool eq(T)` — structural
method rules of the same shape as `to_str` and the operators. The temptation
to declare `interface Hashable { int hash(); bool eq(T other); }` now that
interfaces are the callback mechanism should be refused: the methods are
about a *type*, not about an operation over arguments (the receiver rule
under *Signature matching*
makes that concrete — no function could ever satisfy it), a `Map` is a
runtime structure that dispatches through a fixed slot rather than an
interface value, and declaring it would spend a public name on a thing no
program ever writes down.

### Iteration, if `seq` is ever wanted — three names, post-freeze

Proposed so the shape is on record, **not** adopted:

```c
// lib/seq.m31, after the freeze, if at all
pub interface Step<A, B> { B of(A x); }        // map
pub interface Keep<T>    { bool ok(T x); }     // filter
pub interface Fold<A, B> { B step(B acc, A x); } // reduce
```

  - `Step`, `Keep`, `Fold` are named for what the caller is doing, not for
    `A -> B`. `Step.of`, `Keep.ok`, `Fold.step` read at the use site:
    `f.of(x)`, `p.ok(x)`, `f.step(acc, x)`.
  - **There is deliberately no `Each<T> { void visit(T x); }`.** A visitor
    for iteration with side effects is a second way to write `for (T x in
    xs)`, which the one-way rule refuses outright. If a library wants to hand
    elements to a caller, it returns a `List` and the caller writes `for`.
  - Three is the ceiling. A fourth name is the signal that the zoo has
    started and the design should be re-examined instead.

### The existing precedent

`interface ToStr { str to_str(); }` is already the reference's own
recommendation (§6.6, "There is deliberately no built-in `ToStr` type"), and
it is a one-method interface declared by a program. This design does not
introduce the pattern; it makes the pattern's values easier to produce.

---

## The freeze

Almost all of this is additive. A program written today compiles unchanged
under every decision above, and all of it could land after the freeze without
breaking one. That is the useful headline: **the language can freeze without
any of it**, and the new design makes that *more* true than the first draft
did, because it no longer needs a keyword.

What must be decided **now**, because deciding it later would break programs:

  1. **A local may not take a function's name.** Already in force
     (`docs/roadmap.md`; checked — *"`f` is already a function; shadowing is
     not allowed, rename one"*). It was precautionary; it is now
     **load-bearing**, because a bare function name is a value and the two
     namespaces must stay disjoint for it to be unambiguous. Relaxing it
     would have to be undone.
  2. **`cmp`, `eq` and `hash` are structurally significant method names.**
     `hash` is new; `cmp` and `eq` gain a second meaning (the runtime may
     call them through a reserved slot, and the library's `Order` means
     `cmp`). A program with a method called `hash` meaning something else
     would change behaviour. Free today; not free later. (`to_str` is already
     in this set, by §6.6.)
  3. **An interface method signature may not declare a default.** *New, and
     found by experiment.* The grammar permits it today —
     `sig = type IDENT "(" [ params ] ")"` and `param = type IDENT [ "=" expr ]`
     — and it **misbehaves**: `interface Scale { int go(int v, int by = 2); }`
     compiles, and then `s.go(3)` through the interface reports *"takes 2
     positional argument(s), found 1"* while `s.go(3, by: 3)` reports *"`by`
     is mandatory, so it is positional"*. Both contradict §4.2, and a direct
     call on the concrete type honours the default correctly. Dispatch has no
     name to pass, so a default cannot work through a vtable; the signature
     should be refused where it is written, and the grammar's `sig` should
     take a defaultless parameter list. This is freeze-critical because it is
     a behaviour change to a program that compiles today, and it is the rule
     that makes "a function with an optional parameter cannot be a callback"
     (*Signature matching*) coherent rather than accidental.
  4. **`spawn f(args);` is final.** Closing the open question in
     `docs/concurrency-decision.md` and `docs/roadmap.md` means nothing else
     has to wait for it.
  5. **`=>` is reserved as a token.** It costs nothing — `x => 2` does not
     parse today *(checked)* — and it belongs in the frozen grammar even if
     the lambda form itself lands later. *(Done with Stage 4: `Tok::FatArrow`,
     lexed greedily beside `==`, and the only token in the grammar with one
     use — which is what lets the parser decide a lambda by what follows the
     closing parenthesis.)*

**What is no longer on this list, and is the headline of the revision:
`fn` is not reserved.** It is an ordinary identifier, available to every
program, and the language's keyword list does not grow. `Fn`, `Func` and
`Proc` are likewise free as type names.

`__call` is also not reserved. Nothing is reserved for the synthesised method
name, because the synthesised method's name comes from the target interface.
The synthesised *type* names sit under the existing `__` prefix reservation
(§10.1), which costs nothing new.

What can arrive later without breaking a program: a bare function name as a
value, `=>` and lambdas, block-bodied lambdas, inferred lambda parameter
types, bound method values, weak references, and devirtualisation.

---

## The recommended design, in one place

| | |
|---|---|
| **A callback's type** | an ordinary one-method `interface`, structurally satisfied. There is **no function type syntax** |
| **Declaring one** | `interface Less { int cmp(Point a, Point b); }` — the mechanism the language already has |
| **Passing a named function** | `smallest(ps, by_x);` — a bare identifier, where a one-method interface is expected |
| **Passing a lambda** | `smallest(ps, (Point a, Point b) => a.y - b.y);` — the method name comes from the target interface |
| **Passing an object** | `smallest(ps, ByAxis(true));` — unchanged, and the general case the others abbreviate |
| **Calling one** | `order.cmp(a, b)`, always. Never `order(a, b)`. No new grammar |
| **Where a name or lambda may appear** | anywhere a one-method interface is written: a parameter, a declaration, an assignment, a `return`, a field, a collection element, a channel payload. Not where the type is inferred |
| **Matching** | exact: same parameter count, same types in order, same return type. No variance. **The receiver is not a parameter** |
| **Not callbacks** | a method's name (`p.area`), a generic function, a function with an optional parameter, a two-method interface |
| **Lambda form** | written parameter types, one expression, no `?`, return type from the interface |
| **Capture** | inferred, one mode, exactly what `=` means: an `int` copied, a reference retained. No capture list. The captures are the synthesised type's fields, so `const`, freezing, the resource refusal, destructors and refcounting all apply with no new code |
| **Mutation** | the binding never; the captured object as freely as any other alias |
| **Representation** | `Obj *` with a `TypeInfo`; captures are fields; code in the vtable; invoked by the existing `CallIface` on the target interface's slot |
| **No captures** | one static, immortal instance per (function, interface) pair — no allocation, no refcount traffic, free across threads |
| **Generics** | generic interfaces work today. Inference flows target→callback only; a type parameter must be reachable without the callback |
| **IR** | no new instruction, no new type, no new reference shape |
| **Keywords** | none added. `fn` stays an ordinary identifier |
| **`spawn`** | unchanged: `spawn f(args);` |
| **Cycles** | possible, not collected, no weak references; the pattern is named instead |
| **`sort()` / `Map` keys** | not callbacks at all — three reserved vtable slots for `cmp`, `eq`, `hash` |
| **The library** | `sort.Order<T> { int cmp(T a, T b); }`, and nothing else until something earns it |

---

## Rejected alternatives

| | Why not |
|---|---|
| **A function type, `fn R(A, B)`** (the first draft) | A second spelling of a one-method interface, whose value is the same object dispatched the same way. It costs a keyword, a new type form, an invariance rule, a rule for defaulted parameters, a "function type versus interface" boundary no reader can apply, and three grammar rules about what may be called. The interface answers all of it already, and answers it identically. |
| **`Fn<int, Point, Point>`** | Worse than `fn R(A, B)` on every count: the return type is positional and unlabelled, it reads as a generic type that is not one, and it needs the same keyword-free parse story without the readability. It also invites `Fn0`…`Fn9` the moment arity varies. |
| **`int(Point, Point)` as the type spelling** | Breaks the parser's two-token declaration rule and reintroduces C's most vexing parse — which is what forced the `fn` keyword in the first draft. |
| **`fn(T, T) bool`** (the `docs/types.md` §8 sketch) | Return type last in a type-first language. Superseded entirely: that line should now read `Less<T> order`. |
| **A named `fn` type declaration** — `fn int Compare(Point, Point);` | A new item kind and a new namespace, to produce exactly what `interface Compare { int cmp(Point, Point); }` already produces with an item kind that exists. |
| **`order(a, b)` as a second call spelling** | Two spellings of one thing. It also forces three disambiguation rules — what `f(x)` means, what `obj.handler(x)` means, whether `xs[0](y)` is a call — each of which becomes a diagnostic a user has to learn. `order.cmp(a, b)` needs none of them and names the operation. |
| **Auto-generating an interface per lambda shape** (the "invisible `Fn2<A,B,R>`" route) | Puts Java's zoo in the compiler where nobody can see it, and then needs a name to print in diagnostics anyway. |
| **Inferred lambda parameter types** (`x => x * 2`) | The first binding in the language without a written type, and it requires pushing types *into* an expression, a second direction of inference. Additive later, so the restrictive choice is the reversible one. |
| **Block-bodied lambdas** | A second spelling of a one-expression lambda; needs a return-type flow analysis; Java and C# both shipped both forms and both regret the split. A named function is one line away and its name is a value. Additive later. |
| **Explicit capture lists** (C++) | Only needed when there are two capture modes. There is one. |
| **Capture by reference** (C++ `[&]`, Go, Swift) | Dangling captures in C++; in a refcounted language it means either a box per captured local (Kotlin) or promotion to the heap (Go). By-value capture of an already-counted reference gives the useful half with no machinery. |
| **`move` capture** (Rust) | A second mode, wanted only for `spawn`, which does not need it. |
| **`spawn` taking a closure** | Captures alias, so the uniqueness check traps in the ordinary case — demonstrated above. `spawn f(x)` already expresses the captures as moved arguments, and the move checker sees them by name. |
| **A `call` field in `TypeInfo`** | Eight bytes on every type and a new IR instruction, to replace a slot mechanism that already exists and already handles several shapes. |
| **A fat `(code, env)` callback value** (Nim, Go's interface) | A second reference shape in the IR — the exact thing `docs/types.md` §8 chose the object header to avoid. |
| **Monomorphising the callee per callback type** (Rust) | Requires callback types that cannot be written down, and then `Fn`/`FnMut`/`FnOnce`/`impl Fn`/`Box<dyn Fn>` to talk about them. Available later as devirtualisation, which is an optimisation, not a language feature. |
| **Weak references** | A second memory model added for one pattern; no-capture callbacks cannot cycle and argument lambdas die with the call. |
| **Blessing `map`/`filter` in the language** | Without a lazy protocol they allocate per stage; with one they are a large surface; and a chain does not typecheck under §3.8 anyway. Once callbacks exist a program can write them, which is the point. |
| **A `Hashable` interface** | `eq` already exists for `==`; only `hash` is new. A structural method rule matches §6.2, needs no declaration, and no function could satisfy it anyway (the data comes through the receiver). |
| **An `Each<T>` visitor for iteration** | A second way to spell `for (T x in xs)`. |

---

## Order of work

**Stage 0 — before the freeze, independent of everything else. ~1 day.**
Reserve `=>` as a token. Refuse a default on an interface method signature
(freeze item 3) and fix the grammar's `sig`. Reserve the three vtable slots
(mechanical, at the `iface_slots` push in `src/lower.rs`). *Must* be
pre-freeze. Note what is **not** here: no keyword to reserve.

**Stage 1 — `sort()` and `Map` on user types. ~1 week.**
Fill slots 0–2 from any type declaring `cmp`, `eq` or `hash`; teach `rt_sort`,
`hash_key` and `key_eq` to dispatch through them; replace the two "not
possible yet" diagnostics with the new rule, and add one refusing a
module-constant map with a user-type key. Unblocks two of the three features
`docs/stdlib-decision.md` names, with no language surface beyond one method
name, and no dependency on any later stage. Highest value per unit of work in
the record. *(In progress.)*

**Stage 2 — a static instance of a zero-field user type. ~2–3 days.**
Standalone and useful on its own: it is the easy half of the
static-user-type-object work `docs/const-decision.md` wants, and it is what
makes Stage 3 free at run time. Emit one immortal static per zero-field type
and construct from it instead of calling `rt_alloc`. This also makes
`ByX()` — the hand-written form, which allocates today — free, so it pays
before any new syntax exists.

*Done.* `TypeDef::is_immortal_singleton` (`src/ir.rs`) decides it and the
emitter writes one `static T{i} imm_T{i} = { { RC_IMMORTAL, &ti_T{i} } };`
per such type; `Inst::Alloc` on one becomes `&imm_T{i}.hdr`. Four kinds of
type are excluded because they are not really field-less — an interface, a
channel and a distinct type have no object at all, and an enum's empty
`fields` hides a tag and payload slots — and one because sharing would be
observable: **a field-less type that declares a DESTRUCTOR keeps
allocating**, since an immortal is never released and its `drop` would
silently never run. Refusing that combination was the alternative and was
rejected: a field-less guard whose whole content is its effect is a
legitimate shape, and a synthesised callback type never has a destructor,
which is the case this exists for. Nothing else changes, because `==` on a
user type is its `eq` method and never identity, `rt_snapshot` already
returns an immortal unchanged (`RC_IMMORTAL` is all bits set, so it reads as
frozen), and `rt_check_unique` already skips one. Measured on a loop of 10M
constructions at `-O2`: **0.17 s → 0.04 s**.

**Stage 3 — a function's name as a value. ~1 week.**
Resolve a bare identifier that names a function, in the positions listed in
*Where the target type is known*, against the expected one-method interface;
synthesise the zero-field
wrapper type and its forwarding method before `mono.rs`; emit it as the
Stage 2 static. The signature check is the existing satisfaction check with
the receiver rule added. Diagnostics: the four above (target not
known, receiver-only method, generic function, defaulted function). At the
end of this stage `sort.by(xs, by_x)` works and `docs/stdlib-decision.md`'s
`sort` module can be written.

*Done*, with two corrections to the plan above.

First, the synthesis happens **after** `mono.rs`, not before. It is the
better place and not a compromise: by the time the lowering runs, a generic
interface has already been instantiated, so `Less<Point>` is the ordinary
concrete declaration `Less$Point` with concrete method signatures, and the
wrapper is checked against that with no generic machinery at all. It also
means one hook covers every position, because `lower_expr_as` — the
lowering's own "here is the type that is wanted" entry point — is what every
row of the table under *Where the target is known* already goes through.

Second, **privacy is judged at the reference site and the wrapper belongs to
the function's module.** The site is checked by the same rule, and with the
same words, as a call of the same name: a bare name means this module's
declaration, a qualified one needs `pub`. Having passed that, the wrapper is
declared in the module that declared the function, so its forwarding call is
an ordinary same-module call — which is what lets a module hand out an
interface over its *own* private function, exactly as it may over its own
private method, while nobody else can.

The wrapper is `__ref$<function>$<interface>`, one per pair for the whole
program, cached — so the same name written twice is one type, one method and
one object. `spawn` is untouched. Measured: a loop of 10M `Less o = by_x;`
runs in **0.04 s**, the same as the hand-written `ByX()` after Stage 2, and
the emitted C contains no `rt_alloc` for either.

**Stage 4 — lambdas. ~1–2 weeks.**
Lambda parsing (`(` type IDENT … `)` `=>` expr, decided on two tokens by the
test the statement parser already makes); resolution against the target
interface for the method name, arity, parameter types and return type;
capture analysis over the body; desugaring to a synthesised type with capture
fields before `mono.rs`; the no-shadowing check on parameters; the refusals in
the *Lambdas* diagnostics table. Corpus coverage for the interactions: a
lambda in a
`const` (deep copy of a shared capture), a lambda capturing an `io.File`
(refused under `const`, released by the destructor otherwise), a lambda
crossing `spawn` inside a struct, and a deliberate cycle in `corpus/traps`.

*Done*, with four corrections to the plan above and one rule the plan did
not have. All of it is `src/`: the IR, the C emitter and the runtime are
untouched, as *Representation* predicted.

**First, the parse is decided by the `=>`, not by two tokens.** The
statement parser's test would have had to read `(int(x))` — a parenthesised
conversion — as a parameter list, because both begin `(` `int`. The token
after the CLOSING parenthesis settles it instead, and settles it completely:
`=>` has exactly one use in the grammar, so nothing else can be followed by
one. Scanning to the matching `)` costs a pass over the parenthesised text
and buys the good diagnostic for `(a) => a`, which is refused by naming the
parameter's missing type rather than as a puzzling expression.

**Second, the synthesis happens after `mono.rs`, for Stage 3's reason.** By
the time `lower_expr_as` sees a lambda the target is the concrete
`Less$Point`, and a lambda inside a generic function has already been
duplicated per instantiation with its written parameter types substituted —
so two instantiations are two ordinary concrete lambdas and no generic
machinery reaches the synthesis. One hook covers every position, because
every row of the table under *Where the target is known* goes through
`lower_expr_as` already.

**Third, the lambda IS a construction rather than being lowered like one.**
`lower_lambda` declares the type, then hands the ordinary lowering an
`Expr::New` of it whose arguments are the capture expressions. That is the
whole of why the four claims under *Capture* needed no code: the retain, the
release, `const`'s deep copy, the resource refusal and the destructor are
not re-implemented for captures, they are reached by writing a construction.
Every one is checked by a test rather than assumed (below).

**Fourth, the rule the plan did not have: what a lambda in a METHOD
captures.** The draft said "the names from the enclosing scope its body
mentions", which leaves a bare field name and a bare sibling call
undecided — and the language has already decided them, in §4.3's own words:
*"a method of the receiver is called by its bare name, as a field is read"*,
with `this.f` and `this.m()` both refused because the bare form is the only
spelling. They are `this.`, unwritten. So:

> **A lambda that mentions `this`, a field of the receiver or a method of
> the receiver captures the receiver — once, and as a reference, which is
> what `=` does with `this`.**

The body is rewritten against that one capture: `this` becomes `$self`, a
bare field read becomes `$self.f`, a bare sibling call becomes
`$self.m(..)`. Three consequences, all of them the point:

  - a lambda's body means inside the lambda what it means outside it, which
    is the property that makes the rewrite invisible;
  - a field read stays LIVE — reassigning the field is seen through the
    lambda, because the lambda holds the object, not a copy of the slot
    (`corpus/core/1102`);
  - the alternative, capturing each mentioned field's value, was rejected
    for making a lambda that mentions a field and calls a method hold a
    stale copy of one and the live object for the other. One capture, one
    rule.

A lambda in a lambda needs nothing extra and was not special-cased: the
inner one is lowered while the outer one's method is being lowered, so the
outer's parameters are locals and its captures are fields of the receiver,
and the same two rules apply. An inner `$self` is a field of the outer
lambda, so this rewrite turns it into `$self.$self` and reaches the original
receiver through the outer lambda it captured (`corpus/core/1103`).

No shadowing is checked at the site the lambda is WRITTEN, for every lambda
nested inside it as well as for its own parameters. Without that, an inner
lambda's parameter could take the name of a local of the enclosing function,
which is invisible by the time the inner one is lowered
(`corpus/errors/1142`).

**The diagnostics, as shipped.** Each is the drafted message or better, and
each has a corpus test:

| written | said |
|---|---|
| `(Point a) => a.x` for `int cmp(Point, Point)` | `` `Less.cmp` takes 2 parameters, this lambda takes 1 `` |
| `(int a, int b) => a - b` for the same | `` `Less.cmp` takes (Point, Point), this lambda takes (int, int) `` |
| `(int a, int b) => "smaller"` | `` `Less.cmp` returns int, this lambda's body is a str `` |
| `(int x) => print(x)` for `str of(int)` | `` `Step.of` returns str, but this lambda's body has no value `` |
| a two-method target | `` `Sink` declares 2 methods (`write`, `flush`); a lambda supplies one, so write a type with all of them and pass one of those `` |
| `print((int x) => x)`, `(int a) => a;` | `` a lambda takes its method name from the interface it is passed to, and nothing here expects one; declare an interface and bind it to a local of that type `` |
| `int y = (int a) => a;` | `` a lambda takes its method name from the one-method interface it is passed to, and `int` is not one; … `` |
| `?` in the body | `` `?` returns from the enclosing function, and a lambda has no enclosing function a reader can act on; write a named function `` |
| `(a) => a` | `` a lambda's parameters are written with their types, like every other binding in this language -- write `(int a) => ..` `` |
| `(int a = 2) => a` | `` `a` cannot have a default: a lambda is called through an interface, which dispatches with no name to give an optional argument `` |
| a parameter shadowing anything | the existing §4.1 message, unchanged |

One drafted message is **not** what a lambda handed to an unconstrained type
parameter gets. `show((int x) => x)` for `void show<T>(T v)` reports
*"cannot infer type parameter `T` of `show` from its arguments or from where
its value goes; bind an argument, or the result, to a local with a written
type first"*, because monomorphisation runs first and that is where the
failure is. Checked: **a function's name gets exactly the same message in
exactly the same position** (`show(by_x)`), so the two forms agree, and the
message names the fix. Diverging would have meant making a lambda an
inference source, which is the thing the design refuses.

**The interactions, each checked rather than argued.** The four claims under
*Capture* are the load-bearing ones, and the tests are named so a later
reader can see which program proves which:

| claim | checked by |
|---|---|
| an `int` is copied, a reference is aliased and retained | `corpus/core/1101` |
| a change through a captured reference is seen outside | `corpus/core/1101` |
| a lambda made in a loop holds that turn's value | `corpus/core/1101` |
| a lambda in a method holds the receiver, live | `corpus/core/1102` |
| a lambda in a lambda, and one in a method | `corpus/core/1103` |
| no captures: one immortal static, in a `const`, across a thread | `corpus/core/1104`, `src/tests.rs` |
| `const` deep-copies a capture held outside | `corpus/core/1105` |
| a captured destructor runs when the lambda dies, and not before | `corpus/core/1105` |
| a frozen lambda's captured list cannot be changed | `corpus/traps/1121` |
| a `const` holding a captured resource is refused | `corpus/traps/1120` |
| a capturing lambda cannot cross a thread | `corpus/traps/1122` |
| generic interfaces, and a lambda inside a generic function | `corpus/core/1109` |
| a lambda across modules, over private names | `corpus/modules/lambda-across-modules`, `lambda-privacy` |

Two of those want a word. **The resource-in-a-`const` refusal is the runtime
one, never the compile-time one**, and that is not a gap: a lambda's static
type is always an interface, and `resource_in` stops at an interface because
it cannot see through one. So `const Get g = () => r.id_of();` with a `Res`
captured is caught by `rt_snapshot`'s backstop, in the words the compiler
would have used. The compile-time rule still fires for every hand-written
type that holds one, unchanged.

And the **`spawn` diagnostic is the existing one**: *"value crossing a
thread boundary is still referenced elsewhere; clone() it, or drop the other
reference first"*. It is accurate and actionable, and it does not say "the
lambda's capture" because the check is a runtime reachability walk with no
idea which field it came through. Saying more would mean the runtime naming
a capture, which is a change to `runtime/rt.c` for a message; the shape of
the mistake is in the record instead.

**One thing the corpus cannot hold.** The plan asked for a deliberate cycle
in `corpus/traps`, and there is none: a cycle does not trap, it leaks, and
every corpus program must end `__rc_live=0` while a traps program must abort
with a message. The leak is real and is the documented cost; the program
that shows it is the one quoted under *Cycles* above, which the corpus
harness has no category for.

**Measured.** A captureless lambda emits `v = &imm_T{i}.hdr` and no
`rt_alloc` — asserted on the emitted C in
`src/tests.rs::a_lambda_with_no_captures_never_allocates`, since no program
can observe the difference from inside the language. A capturing one is an
ordinary construction, which the same test's twin asserts from the other
side.

**Found on the way, and fixed here:** `l(1, 2)` on an interface value said
*"unknown function `l`"*, which *What is open* named as the mistake
newcomers would now make. It says what to write instead.

**Stage 5 — the standard library catches up. Days.**
`lib/sort.m31`: `Order<T>`, `by`, `max`, `min`, `search`. Whatever else has
by then earned a callback.

*Done.* Ordinary source -- no `prim`, nothing added to the runtime, more
comment than code: `pub interface Order<T> { int cmp(T a, T b); }` and four
functions over `size()`, `[i]`, `[i] =` and `push`.

**`by` is a stable bottom-up merge sort** — the same algorithm
`rt_sort_with` runs for `xs.sort()`, written in the language instead of in
C. Stability is promised by `xs.sort()` already and the two must not differ
in a property a program builds on, so it is tested rather than asserted:
`corpus/core/1107` checks that an all-ties sort is the identity at every
length from 0 to 11, that ties keep their order in a list long enough to
need several merge passes, and that sorting by a minor key and then a major
one produces the compound order — which is the use stability exists for.

**`search` returns `Option<int>`, and it is `index_of` for a sorted list.**
The decision, and the reason, in one line: `xs.index_of(v)` already answers
"where is this value" with `Option<int>` and the FIRST index holding it, so
`search` answers the same question with the same type and the same index,
in O(log n) instead of n. Two functions that disagree about the answer to
one question are a defect, so `search` finds the first of a run of equal
elements — a lower bound, one comparison more than stopping at any match —
and `corpus/core/1108` checks the two against each other.

It is deliberately **not** an insertion point. Java's `binarySearch` and
C#'s return `-(insertion point) - 1` for an absent value: two answers in one
integer, and a sign convention every caller has to remember. "Where would
this go" is a different question from "where is this"; if a program needs
it, it earns a name of its own rather than a second meaning for this result.

**`max` and `min` are defined by an identity**, which is what settles the
only question they have: `max(xs, o)` is the element `by(xs, o)` leaves
last and `min` the one it leaves first. So `max` keeps the LAST of several
equal elements and `min` the first — a "first wins" rule for both would make
`max` and a sort disagree about which of two equal elements a program gets.
An empty list **traps**, which is the answer `xs[0]` gives to the same
question: there is no element to return, and an `Option` would put a `match`
at every call site for a case most callers have already ruled out.

**No runtime primitive, and one difference from `xs.sort()` that follows.**
`rt_sort_with` marks the list `RC_SORTING` so that a `cmp` which changes the
list being sorted traps. `sort.by` cannot: marking is a runtime operation,
and adding a primitive to the seam (`docs/stdlib-seam.md`) to catch a
caller's bug would be a poor trade. What happens instead is bounded and
stated in the module: a `cmp` that shortens the list makes an ordinary index
trap, one that lengthens it leaves the new elements unsorted, and memory is
never at risk because every access is a checked `[i]`.

**A `List`, not an `Array`.** Parameter types are written and there is no
overloading, so covering arrays means a second name for each of the four --
eight public names for four operations. `xs.sort()` already orders an array
by the element type's own `cmp`; a chosen order over an array waits until
something needs it badly enough to spend the names. Recorded because it is
the first place the no-overloading rule costs the library something real.

**One wart found, and worked around rather than papered over.** `T best =
xs[0];` inside a generic function in `sort` is refused when `T` turns out to
be a type the CALLING module keeps private: a local's type is checked for
visibility after monomorphisation has substituted it, so the check asks
whether `sort` can name the caller's private type, and it cannot. `max` and
`min` therefore track the best INDEX, which needs no such local (and costs
one retain less per improvement). The underlying rule — privacy judged on a
substituted type rather than on what the source wrote — is not a callback
question and is left where it was found, noted here because it will bite the
next generic library function that wants a `T` local.

**After the freeze, at any time.** Devirtualisation of a statically known
callback; block bodies; inferred lambda parameter types; bound method values;
`seq` as a module, judged on its merits; weak references if a real program
needs them.

Stages 1, 2 and 3 are independent of each other and can be done in any order
or in parallel. Only Stage 4 depends on Stage 3, and only for the synthesis
machinery.

---

## What this costs

  - **One token and one expression form.** `=>` and the lambda. No keyword,
    no type form. The grammar grows by two lines — `lambda = "(" [ params ]
    ")" "=>" expr` and an `atom` alternative — and the reference's §9 entry
    "closures, function
    values, lambdas" entry comes out of the not-in-the-language list, replaced
    by a sentence saying what a callback is instead.
  - **Every callback contract needs a declaration.** This is the real price
    of dropping the function type, and it is the price Java paid. `interface
    Less { int cmp(Point a, Point b); }` before `smallest` can take one. The
    mitigations are that the declaration is one line, that it has a name the
    reader can look up, and that the naming rule above is written down
    *before*
    the library starts.
  - **A synthesised type and method per lambda site and per (function,
    interface) pair**, so more `TypeInfo`s, more vtable slots, and a larger
    binary. Every type's vtable array grows by one entry per distinct
    interface-method shape in the program.
  - **An uninlinable indirect call per invocation**, two loads deep. A
    callback-driven sort of `int`s will be several times slower than the
    built-in one.
  - **A new way to leak.** A callback stored in a field of an object it
    captures is a cycle, and cycles are not collected — and since destructors,
    a leaked cycle holds resources as well as memory. Reachable by hand
    today; easier to write after this.
  - **Verbosity at the call site.** `(Point a, Point b) => a.x - b.x` is the
    price of the language's own rule that every binding writes its type, and
    it is charged at exactly the places where a lambda is most attractive.
  - **Short lambda parameter names will collide** with locals under the
    no-shadowing rule, and the fix is a rename.
  - **`hash`, `cmp` and `eq` are spoken for**, and a module constant map
    cannot be keyed on a user type.
  - **A chain of higher-order calls does not typecheck.** Each stage needs a
    local with a written type. Stated as a cost, though *Iterators, `map` and
    `filter`* argues it is also
    a benefit.

None of these is new in kind. Every one is a cost the language has already
decided to pay somewhere else, which is the best sign available that this is
the design that belongs in this language rather than a good design borrowed
from another one.

---

## What is open, and one thing that is wrong today

  - ~~**The interface-method default bug**~~ *(fixed.)* The declaration is
    refused where it is written: *"`go` is an interface method, so `by` may
    not have a default: a call through an interface passes positions, not
    names, and the default could never be used"* *(checked)*. It is the rule
    that makes "a function with an optional parameter cannot be a callback"
    coherent rather than accidental, and a lambda parameter is refused a
    default with the same reasoning in its own words.
  - ~~**The diagnostic for calling an interface value**~~ *(fixed with
    Stage 4, since that is when the mistake became likely.)* `l(1, 2)` now
    says *"`l` is a `Less`, not a function: a callback is called through its
    method, so write `l.cmp(..)`"* — and a value of a type that is not a
    one-method interface gets the first half alone
    (`corpus/errors/1144`).
  - **A user-type module constant**, which Stage 2 opens the door to but does
    not finish. Still open.
  - ~~**Whether `sort.Order` should also cover `search` and a stable-sort
    guarantee.**~~ Both, and Stage 5 says why: `search` is `index_of` for a
    sorted list and returns the same `Option<int>` and the same first index;
    stability is promised because `xs.sort()` promises it and the two must
    not differ.
  - **Devirtualisation.** Deferred, and the only performance lever that
    matters. An interface call is two loads and an indirect call, and a
    callback-driven sort of `int`s is several times slower than the built-in
    one — now measurable, since both exist.
  - **Privacy is judged on a substituted type.** Found writing `lib/sort.m31`:
    a generic function in one module cannot declare a local of type `T` when
    the caller instantiated `T` with a type it keeps private. Not a callback
    question, and worked around there by tracking an index instead of an
    element, but it will bite the next generic library function.
