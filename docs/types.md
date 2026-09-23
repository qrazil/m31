# The type system

Status: §1–5 and `const` **decided**; §5 implemented. §6–8 decided, not yet
implemented. Nothing here is open any more except where marked.

---

## 1. What concurrency actually demands

`docs/concurrency-decision.md` commits to *moved, not shared*, because that is
what keeps `rc_inc`/`rc_dec` plain increments instead of atomics. The type
system has to make that true, not merely encouraged.

The requirement, stated precisely:

> **When a value crosses between threads, no alias may remain behind.**

That is the whole demand. It is worth noticing how much smaller it is than
what Rust needs.

## 2. The insight that keeps this simple

Rust needs ownership *everywhere* because it has no fallback — every reference
must be statically justified. **We have refcounting as the fallback.** Inside a
single thread, aliasing is free and safe: there is no race, and the refcount
handles lifetime.

So ownership is not a general discipline here. It is a **boundary condition**,
and it applies at exactly two places:

- sending a value on a channel
- capturing a value into a `spawn`

Everywhere else the language behaves like Java or Go: values alias freely, the
refcount cleans up, and you never write a lifetime.

That means **no borrow checker, no lifetime annotations, no shared-vs-mutable
distinction, no `&`/`&mut`.** Those exist in Rust to solve a problem we solved
by choosing refcounting.

## 3. The model

**Inside a thread:** nothing. Values alias freely. This is the common case and
it costs the programmer no thought.

**At a thread boundary:** the value must be **unique** — exactly one reference
to it exists — and it is *moved*: the sender gives it up.

```c
str chunk = fetch(url);   // rc == 1, freshly built
send(c, chunk);           // moved; `chunk` is dead from here
print(chunk);             // compile error: `chunk` was moved
```

## 4. How uniqueness is checked

Two checks, deliberately split, because they catch different mistakes.

### 4a. Use-after-move — compile time

The compiler tracks which locals have been moved and rejects any later use.
This is the mistake people actually make, and it deserves a real diagnostic:

```
corpus/errors/0NN-use-after-move.src:7:11: `chunk` was moved here
  6 |     send(c, chunk);
    |             ----- moved on line 6
  7 |     print(chunk);
    |           ^^^^^ used after move
```

This is a flow-sensitive analysis over the IR and is genuinely small — it is
one bit per local, propagated through the CFG we already build. No inference,
no constraints, no solver.

### 4b. Uniqueness — runtime, for now

Use-after-move does **not** prove uniqueness, and it is worth being explicit
about why:

```c
str a = build();
str b = a;        // alias: rc == 2
send(c, a);       // `a` is moved -- but `b` still reaches the object
```

Statically ruling that out means tracking aliases, which is a borrow checker,
which is the thing we just avoided. So instead:

**`send` checks `rc == 1` and traps if not**, with a message that names the
problem rather than the symptom:

```
trap: value sent to another thread still has 2 references
```

In practice this nearly always passes, because values crossing a channel are
overwhelmingly freshly constructed by the worker that sends them. An alias
surviving a send is a bug in a moved-not-shared program regardless.

**Why this is the right call for now, and not a cop-out:** the check is one
branch on a value we already load, the surface syntax needs nothing, and
**it can be upgraded to a static analysis later without changing the
language.** A future escape analysis that proves uniqueness simply elides the
check. That is precisely the "freeze the language, not the implementation"
property we want — and it is the opposite of shipping `uni`/`iso` type
qualifiers now and being stuck with them.

The honest cost: a class of error that is a runtime trap instead of a compile
error. Mitigations available cheaply — warn at compile time when a sent value
is *visibly* aliased in the same function, and offer an explicit `copy(v)` for
when you genuinely want to keep one.

### 4c. What this buys

Because only one thread can reach a value at a time, every refcount operation
in the language stays a **plain, non-atomic increment**. That is the 25% of
runtime Swift pays and we do not.

---

## 5. User types — sketch

**Decided and implemented: all user-defined types are reference types,
refcounted.** Like Java, not like Go or C.

```c
type Point {
    int x;
    int y;
}

Point p = Point(x: 1, y: 2);
```

No `*`, no `&`, no value-vs-pointer distinction to teach or to choose between.
It also keeps `ref` as the only managed type in the IR, which is already the
design.

**The cost, stated plainly:** a two-field `Point` becomes a heap allocation,
where Go or C would put it in a register or on the stack. The answer is
compiler unboxing for provably non-escaping values — an optimisation, not a
language feature, and therefore addable after the freeze.

Value structs were considered and **rejected**: they are a second concept, a
second set of rules for assignment and argument passing, and eventually a
`&`-shaped thing. Having two answers to "does `=` copy or alias?" is a
permanent source of confusion in C# and Swift.

When a copy is genuinely wanted, it is **explicit: `clone(v)`.** That is one
visible operation rather than an invisible rule that depends on the type.

### Reconsidered 2026-09-23, and kept

The opposite was put again once `const` had landed: make plain `a = b` a deep
copy, delete `clone`, and spell aliasing with a keyword or not at all. It was
rejected on four counts, and they are worth keeping written down because the
question will be asked a third time.

  - **Cost.** Every assignment and every argument would copy a whole graph.
    Swift makes that bearable with copy-on-write, which is a uniqueness check
    before *every mutation* -- a cost on writes, paid forever, to make reads
    look cheap.
  - **Shared structure stops existing.** A parent link, a graph, a cache, two
    names watching one object: none of it is expressible when every copy is a
    new object. Putting a reference type back is the two-concept world this
    rejected in the first place.
  - **A resource cannot be copied at all** (`docs/destructors-decision.md`),
    so `File b = a;` would have to keep aliasing -- one rule for most types
    and another for the ones holding a file, which is the worst of both.
  - **Methods would stop working as read.** `p.move(1, 2)` mutates its
    receiver; if passing `p` copied it, the caller would never see it.

What the question was really after is already answered elsewhere: `const`
binds a **snapshot** (`docs/const-decision.md`), so a value that must not
change behind your back does not, without changing what `=` means.

## 5b. Methods, and no shadowing

Methods are declared by **qualified name**, outside the type body:

```c
type Rect { int w; int h; }

int Rect.area() { return w * h; }
void Rect.scale(int f) { w = w * f; h = h * f; }
```

Chosen over methods-inside-the-type because it **retrofits** — a method can
be added to any type, including one declared elsewhere — and because it keeps
a type declaration a list of fields. The cost, accepted deliberately, is that
a type's methods can scatter through a file; that is a formatter's job, not
the grammar's.

Also considered and rejected: an `impl` block (a new keyword and two places
to look per type) and Go's receiver-in-the-signature (`int (Rect r) area()`,
where the return type and the receiver fight for the front of the line).

### Fields are bare, because nothing shadows anything

Inside a method a field is reached by its bare name, and so is a sibling
method; `this.x` and `this.m()` are refused as second spellings. `this`
itself names only the **whole** receiver -- what a method needs to match on
an enum receiver, return itself, or pass itself on (reference §4.3). The bare
forms are only safe given the rule that pays for them:

> **Nothing shadows anything, anywhere.** Not an outer local, not a
> parameter, not a function, not a type, not a field of the receiver.
> Shadowing is a compile error; rename one of them.

A bare name can therefore only ever mean one thing, so there is nothing for a
`this.` prefix to disambiguate. The rule also deletes the whole class of bugs where a
reader and the compiler disagree about which `x` is meant — Java allows bare
field access *and* shadowing, and pays for it with a culture of `this.`
conventions and `m_` prefixes.

Reusing a name in **sibling** scopes is fine: neither is visible to the other.

## 5a. Arguments

**Oro's rule, applied to calls and construction alike: a parameter with no
default is positional; one with a default is named. Never both.**

```c
int volume(int w, int h = 1, int depth = 1) { ... }

volume(5)                    // ok
volume(5, h: 2)              // ok
volume(5, depth: 3, h: 2)    // ok -- named arguments need no order
volume(5, 2)                 // error: h is optional, so it is named
volume(w: 5)                 // error: w is mandatory, so it is positional
```

The two halves never overlap, which removes two questions at once: which form
to use for a given parameter, and what order the optional ones come in.

It also gives up something, and it is worth naming: `Point(3, 4)` can
transpose `x` and `y` silently, where the earlier name-everything rule could
not. That is the trade Oro already makes, and the exposure is limited to
adjacent parameters of the same type.

Positional arguments must precede named ones, so a reader never counts commas
to work out where a value lands.

## 5c. Distinct types, and no aliases

```c
distinct int Price;
distinct int UserId;
```

Same representation as the base, different identity to the type checker,
**erased before the IR** — the same trick monomorphisation uses, and the same
surface-versus-IR split that already lets `int` be an `i64`. There is no
object, no header and no refcount, and a distinct field stores unwrapped.

A distinct type **inherits every operation of its base**, because it is one:
`Price + Price` is a `Price`. Mixing with the base is an error and needs an
explicit `Price(n)` or `int(p)`, both of which emit nothing. That asymmetry
is the whole feature — `Price` and `UserId` cannot be confused, while
`Price * Price` still works for the cases where arithmetic is meaningful.

Nim makes a distinct type lose all of its base's operations, to be added back
one at a time. Rejected: the motivating cases here are `Price`, `Meters`,
`Celsius`, where the arithmetic is the point.

**Type aliases are rejected.** An alias is a second name for the same type,
so it reads like a safety feature and is not one — an `OrderId` would still
pass where a `UserId` was wanted. Go added them in 1.9 for moving a type
between packages during a refactor, which is a module-system problem we do
not have. Purely additive if it ever becomes one.

Note the trap this avoids: a wrapper struct, `type UserId { int v; }`, does
work today and costs a heap allocation, a refcount and a pointer chase to
hold one integer. A distinct type costs none of that.

## 6. Errors

Errors are values (already decided). The open question is what shape.

Expressing "an `int`, or a failure" needs either generics or a built-in, and
generics are deliberately deferred. Options:

- **Multiple return values**, Go style: `int, err read_int(str s)`. Needs no
  generics, familiar, and composes with monomorphised generics later. A bit
  alien in C-shaped syntax.
- **A built-in `Result`** with dedicated syntax, generalised later once
  generics land.
- **Wait for generics** and use `Result<T, E>` from the start — which means
  generics move from "later" to "now".

**Decided: multiple return values**, Go style. Needs no generics, and
composes with generics later rather than being replaced by them.

## 7. Absence

§6.6 of Oro's feasibility doc said **non-nullable by default**, and that
should hold: there is no `null` for ordinary types.

Absence needs an optional — `str? name` — plus narrowing so that after
`if (name != null)` the value is a plain `str`. Flow-sensitive narrowing is
contained and worth it; it is the same CFG walk as the move checker.

## 8. Polymorphism

**Interfaces are structural**: having the methods is the proof, no
`implements` clause.

The keyword comes first, as the kind word does for a struct:

```c
type Point { int x; int y; }
interface HasArea { int area(); }
```

Go writes `type X interface { .. }` because in Go a kind word *always*
follows `type` — `type X struct { .. }`. We dropped `struct`, so following
Go here would have left one form with a kind word and the other without.
`type` still earns the keyword for aliases later, where there is no `{`:
`type Id = int;` Chosen for retrofit — an interface can be satisfied by a
type written before the interface existed, including one in a library you do
not control. That is what makes a stdlib compose without a type hierarchy.

The known hazard is accidental satisfaction: a `Shape.draw()` silently
satisfying a `Cowboy.draw()`. Rare, and the cost is accepted.

**Generics arrive from day one**, monomorphised. **Implemented** — see
`src/mono.rs`. The reason is not
convenience, it is that **containers are generic and we need `Chan<T>`
immediately.** The alternative is special-casing channels, arrays and maps in
the compiler so users cannot write their own — which is what Go did for ten
years and then spent its worst compatibility scar undoing.

### Constraints: interfaces only

A type parameter may be constrained by an **interface** and nothing else.

```c
T max<T: Ordered>(T a, T b)         // NO -- would need `<` on T
T pick<T>(T a, T b, Less<T> order)  // yes -- pass the comparison
```

**No type sets.** Go had to invent `interface { ~int | ~float64 }` because a
method-based interface cannot express "supports `<`", and the result is two
different things sharing one keyword — the ugliest corner of Go generics, and
one they cannot now remove.

We avoid it because our motivation is containers, and containers need nothing
of their element type. `Chan<T>` only moves values. `List<T>` needs nothing.
Only `Map<K,V>` needs anything of `K`, and `List<T>.sort()` anything of `T`.

Neither turned out to need an interface either. Both are answered by
**reserved method names** (docs/reference.md §4.4a): a key type declares
`int T.hash()` and `bool T.eq(T other)`, a sorted element type declares
`int T.cmp(T other)`, and the compiler puts those methods in the type's
runtime metadata so the runtime can call them. No `Hashable`, no `Ord`, no
type set, and nothing to write at the use site. Where an operation is
genuinely the *caller's* to choose rather than the type's — a sort by some
other key — **pass a function**, which is Oro's style for `sort`.

### What is implemented, and what is not

Working: generic types and generic functions, any arity, nested
instantiations (`Wrap<Wrap<int>>`), and a worklist so an unused generic is never
instantiated — and therefore never type-checked against types it was not
written for.

**Type arguments on function calls are inferred, never written.** There is no
`f<int>(x)` syntax, deliberately: after a name that is *not* known to be a
type, `<` is ambiguous with comparison. That is the problem that pushed Go to
`f[int](x)` and Rust to the turbofish, and it is worth not inheriting. On a
type it is unambiguous — `Wrap<int>` works — because the parser already knows
every type name from its pre-pass.

Inference unifies structurally, so `Wrap<T>` against `Wrap<int>` binds `T`. Its
limit is that it reads argument types syntactically — literals,
constructions, and locals with a written type — so a *nested* call is opaque:

```c
print(unwrap(unwrap(nested)));   // cannot infer
Wrap<int> inner = unwrap(nested); // write the type once
print(unwrap(inner));            // fine
```

Lifting that needs a real type checker running before monomorphisation, which
is the right eventual architecture. Until then the diagnostic says exactly
what to do rather than guessing.

### Embedding, in place of inheritance

An anonymous field -- a bare type with no name -- is embedded, and takes the
type's own name. Its fields and methods are promoted onto the outer type.
Breadth-first, so a direct member always wins over a promoted one and a
shallower promotion wins over a deeper one; Go's rule.

Methods are promoted by **synthesising forwarders** rather than by teaching
every call site about embedding, which leaves direct calls, vtables and
interface satisfaction working unchanged.

Forwarder generation runs to a **fixpoint**. Transitivity does not fall out
for free: when `Puppy` embeds `Dog` which embeds `Animal`, `Dog.count_legs`
is itself a forwarder generated in the same pass, so it is invisible until
the round that created it has finished.

This is the whole of the inheritance replacement: polymorphism comes from
interfaces, reuse from embedding, and there is no subtyping between concrete
types.

### Dispatch lives in the object header

Monomorphised generics need no dispatch. Interfaces do, and the obvious answer
is Go's fat pointer: an interface value is `(data, vtable)`, two words, a
second shape of reference in the IR.

**We do not need that, because the header already has a slot.** Widen
`DropFn` into a `TypeInfo *` carrying the drop function *and* the method
table:

```c
struct Obj { long rc; const TypeInfo *ty; };
```

An interface value is then **just an `Obj *`** — the object knows its own
type, as in Java. `ref` stays the only reference shape in the IR, the word is
one we already spend, and the cost is one extra load on dispatch
(`obj -> ty -> method`) against Go's one. Worth it.

---

## 9. Settled

1. ~~Value structs or reference-only~~ — **reference-only**, with explicit
   `clone(v)` when a copy is wanted.
2. ~~Error shape~~ — **multiple return values**.
3. ~~Interfaces~~ — **structural**, for retrofit.
4. ~~Generics before or after 1.0~~ — **day one**, monomorphised,
   interface-only constraints, because `Chan<T>` needs them.
5. ~~**Mutability**~~ — decided: `const` exists, on locals. Assignment to a
   const is a compile error. Const fields are not yet a thing.

## 10. Order of implementation

The move checker (§4a) is the only part concurrency is waiting on, and it is
the smallest: one bit per local over the existing CFG. Everything else in this
document can follow at its own pace.

1. ~~`type` declarations and field access~~ — done
2. **Generics**, monomorphised — moved ahead of everything else because
   `Chan<T>` needs them and retrofitting generics into an existing stdlib is
   the expensive order
3. Move checking, with the diagnostic in §4a
4. `send`/`recv` and the runtime uniqueness check
5. `clone`
6. Interfaces, and the `TypeInfo` header change
7. Multiple returns, then errors
8. Optionals and narrowing
