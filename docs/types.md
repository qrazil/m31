# The type system

Status: **proposal.** §1–4 are the part concurrency forces and I would commit
to them. §5 onward is a sketch with real forks left open — those are language
design, not consequences, and they are yours.

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

**Proposal: all user-defined types are reference types, refcounted.** Like
Java, not like Go or C.

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

**The fork you may want instead:** value structs (`Point` copied by value,
heap types marked somehow). That is what Go does and it is a real performance
difference for numeric code. It is also a second concept, a second set of
rules for assignment and argument passing, and a `&`-shaped thing eventually.
I lean to reference-only, but this is a genuine decision and not mine.

## 6. Errors — sketch

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

Go shipped multiple returns at 1.0 and had no generics for a decade; the
stdlib was fine. That is the pragmatic evidence, and I would follow it.

## 7. Absence — sketch

§6.6 of Oro's feasibility doc said **non-nullable by default**, and that
should hold: there is no `null` for ordinary types.

Absence needs an optional — `str? name` — plus narrowing so that after
`if (name != null)` the value is a plain `str`. Flow-sensitive narrowing is
contained and worth it; it is the same CFG walk as the move checker.

## 8. Polymorphism — sketch

Two separate things, and they do not have to arrive together:

- **Interfaces** — needed for a stdlib that reads and writes different things.
  Structural (Go) or declared (Java)? Go's structural interfaces are pleasant
  and make the stdlib composable without a type hierarchy.
- **Generics** — strategy already fixed as monomorphisation, which keeps them
  out of the IR entirely. Decide *whether* before 1.0, since Go's single worst
  compatibility scar came from deferring exactly this.

---

## 9. The forks that are actually yours

1. **Value structs, or reference-only?** (§5) — performance vs one concept.
2. **Error shape** (§6) — multiple returns now, or generics first.
3. **Interfaces: structural or declared?** (§8)
4. **Generics before or after 1.0?** — the decision, not the strategy.
5. **Mutability**: is there a `const`, or is everything mutable? The move
   checker does not need immutability, so this is free to decide later — but
   it is cheaper to add `const` before the freeze than after.

## 10. Order of implementation

The move checker (§4a) is the only part concurrency is waiting on, and it is
the smallest: one bit per local over the existing CFG. Everything else in this
document can follow at its own pace.

1. `type` declarations and field access — the thing every other feature needs
2. Move checking, with the diagnostic in §4a
3. `send`/`recv` and the runtime uniqueness check
4. Optionals and narrowing
5. Errors, in whatever shape §6 resolves to
6. Interfaces
7. Generics
