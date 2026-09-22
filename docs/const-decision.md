# `const`: the decision

Decided **2026-09-21/22**, on the author's two directions: **`const` means
one thing everywhere — the value never changes, deeply** — and **a `const`
binding takes a frozen snapshot of its value, and is never refused** -- with
one exception added the next day: a value that owns a resource (below). No
second keyword (no `imm`). This record is how that is implemented, why this
way, and what it costs. The normative rules are `docs/reference.md` §4.1, §4.4 and §7.3.

---

## What changed

Before: `const` existed only on locals and only forbade reassignment.

```c
const List<int> a = [1, 2];
a[0] = 5;          // compiled, and changed the list
```

After: the same program is refused, and so is every way of reaching the
list through another name, at compile time where the compiler can see it and
at run time where it cannot. A module-level `const` (§4.4) follows the same
rule, and is additionally computed by the compiler and emitted as static,
immortal data.

## Constness belongs to the object, not the name

Under reference counting `=` aliases. `List<int> b = a;` is a second name
for the same list, and so is a parameter, a field, an element. A rule about
the *name* `a` — "no mutation through `a`" — says nothing about `b`, which
is exactly how `a` would be changed in practice: passed to a function that
changes its argument.

So the **object** is marked. Binding a `const` *freezes* the value: the
object and everything reachable from it get a flag, and every operation that
changes an object checks the flag. The compiler refuses the changes it can
see; the runtime traps on the rest.

## The mechanism

**The flag** is the highest non-sign bit of the refcount word
(`RC_FROZEN = LONG_MAX / 2 + 1`, `runtime/rt.h`), not a new header field:

  - the header stays two words, so no object grows;
  - `rc_inc` is unchanged, and `rc_dec`'s zero test masks the flag — one
    AND, on a path that already loads and stores the word;
  - `RC_IMMORTAL` is -1, every bit set, so an immortal object reads as
    frozen with no extra test. That makes a module constant's static data
    frozen from birth, which is the rule, and makes string literals and
    channels "frozen" too, which is harmless: a string is immutable, and a
    channel never passes through a mutation path.

The alternatives were a third header word (8 bytes on every object for one
bit), or a tag bit in the `TypeInfo` pointer (every vtable dispatch and
every `is_list` test would have to mask it).

**The snapshot** (`rt_snapshot`, `runtime/rt.c`). Every `const` binding of
a reference type other than `str` calls it after the local has taken its
+1; it takes that +1 and returns one on the value the name will hold:

| the value at the binding | result | cost |
|---|---|---|
| already frozen: another `const`, a module constant, a path through one | itself | none |
| reachable from nothing but itself: a literal, a fresh construction, a call's result nobody kept | itself, **frozen in place** | a walk of the graph |
| anything in it held from outside: `const T a = b;`, `[p]` while `p` names the Point | a **deep copy**, frozen; the original released and left mutable | a walk, plus a copy of every unfrozen object reached |

"Reachable from nothing but itself" is the count `rt_check_unique` already
makes at a thread boundary — the references into each object from within
the graph must equal its refcount — walking with the per-type `WalkFn`. A
`str` and anything already frozen are left out of the count and out of the
copy: nobody can change them, so sharing them changes nothing.

**Nothing is refused** -- except a value that owns a resource, below. The first version refused `const T a = b;` at
compile time ("bind `clone(b)`") and trapped on a fresh value with a shared
part. The author replaced both: a `const` names a value that will not
change, and whether that needs a copy is the runtime's business, not the
programmer's. The price is that the cost is not in the syntax — **binding a
`const` from a shared value costs a deep copy**, at that line — and the
reference says so plainly (§4.1).

**The deep copy** is a real graph copy. A `CopyFn` joins `DropFn` and
`WalkFn` in `TypeInfo`; the compiler emits one per user type (`copy_T*`,
`src/emit_c.rs`) and the runtime copies its own collections itself. Every
copy is registered in an old→new map **before** its children are copied, so
a child already copied is shared — a diamond stays a diamond — and a cycle
closes onto the copy instead of recursing forever. The objects the copy made
are exactly the map's values, and those are what get frozen.

Two consequences, found while testing:

  - **A snapshotted cycle can never be freed.** Breaking a cycle is a
    change, and the copy is frozen; reference counting does not collect
    cycles (reference §7.1). So `const Node c = n1;` on a cyclic `n1` leaks
    the copy for the life of the program. The test for it
    (corpus/traps/768) has to report through a trap's message, because the
    leak check could never pass. This is inherent to "frozen + refcounted +
    no cycle collector", not to the implementation.
  - **The copy recurses on the C stack**, one frame pair per level of
    nesting, so a very deep chain of objects (a linked list of a few million
    nodes) could overflow it. The freeze walk is iterative; the copy could be
    made so with an explicit work list, at the cost of registering children
    before filling their parents. Left as it is until something deep enough
    appears.

Diamond and cycle behaviour were also checked directly in the emitted C: the
snapshot of a `Pair(box, box)` has `left == right`, and the snapshot of a
two-node cycle traverses `1 2 1 2`.

**Compile-time refusal** (`Lowerer::refuse_const_write`): assignment to the
name, an index store, a field store, or a changing method of a built-in
collection or `bytes`, on any chain of fields and indexes that starts at a
`const` local or a module constant. A user type's method is not refused,
because nothing in its signature says whether it changes `this`.

**Run-time backstop**: every mutation entry point checks
`rt_check_mutable(o)`, a static inline in `rt.h` (a load, a test, a cold
branch): `rt_index_set`, the List mutators (`push`, `pop`, `insert`,
`remove_at`, `clear`), `reverse` and `sort`, the Map mutators (`set`,
`remove`, `clear`), the bytes mutators (`set`, `push`, `pop`, `clear`,
`extend`), the two primitives that write into a caller's buffer (`__read`,
`__listdir`), and — emitted by the compiler — every field store into an
object that already exists (a `p.f = v` statement, and a method assigning a
field of its receiver). Stores that initialise a new object (construction,
`clone`) are not checked; the object cannot be frozen yet.

## A value that owns a resource: refused

Added **2026-09-22**, after an adversarial review. The rule and its
reasons are in docs/destructors-decision.md, "A resource cannot be copied";
in short: **a `const` cannot hold a value that owns a resource** -- an
object whose type has a destructor, or anything that can hold one.

The snapshot above was designed for data, and a resource is not data. Its
deep copy duplicated an `io.File`, and the copy's destructor closed the
caller's descriptor (`const io.File g = f;` in a function taking `f`); a
destructor that logged `this` through a function binding a `const` copied
`this` on every call, forever; and freezing a fresh `Lease` froze the pool
its destructor gives its slot back to. Freezing a fresh resource in place
would avoid the copy but leave it half usable -- a frozen File can be
written but not read or closed, by accident of which of its methods store a
field -- and would make its destructor the one change a constant allows.

So the binding is refused, fresh or shared:

  - **at compile time** whenever the declared type, after monomorphisation,
    can hold a type with a destructor (`Lowerer::refuse_const_resource`,
    `resource_in` in src/lower/consts.rs: a walk over fields, variant
    payloads and collection element types, stopping at interfaces and
    channels);
  - **at run time** through an interface, where the type cannot show it:
    `rt_snapshot` checks every object of the unfrozen graph -- the same walk
    it already made for the uniqueness count -- against a new `TypeInfo`
    field, `resource` (the type's name, or NULL), and traps before freezing
    or copying anything. The cost is one load and test per object walked,
    on a path that already visits each one.

This is the one place the author's "never refused" gives way, and it gives
way to the destructor's own guarantee -- a resource is released exactly
once -- which a snapshot cannot keep. `clone` is refused for a type with a
destructor for the same reason. A consequence worth stating: **no frozen
object ever has a destructor**, so the drop function's count-to-1 dance,
which also clears the frozen bit, never runs on a frozen object.

## What a frozen value can still do

Everything but change: it is read, passed, returned and stored; it is freed
when its last reference goes (the flag is not a pin, and the leak check
counts it like anything else); `clone` of it is an ordinary, unfrozen copy —
shallow, so the elements of a cloned frozen `List<Point>` are still frozen;
and it crosses a thread boundary under the ordinary move rule.

`rt_check_unique` was deliberately **not** relaxed for frozen graphs. A
frozen object is immutable, but its count is still a plain, non-atomic
count; two threads retaining and releasing one shared frozen object is the
refcount race the move rule exists to prevent. Only immortal objects — module
constants — are shared across threads, because their count is never
written.

## The one-meaning rule, applied to module constants

Because a `const List<int>` local is legal, a module-level `const List<int>`
is too, and so is `const bytes`: the first draft refused both ("a List that
cannot grow is an Array"), which would have made `const` mean different
things at different depths of the file. Only a user-type module constant is
still refused, because the compiler cannot yet lay one out statically.

## Cost, measured

Machine: Intel i7-8750H, gcc 14.2 / clang 18.1, `-O2`, best of 7 runs.

| benchmark | with checks | without | |
|---|---|---|---|
| 100M iterations of three field stores in a method + one direct store, gcc | 0.181 s | 0.185 s | no measurable cost |
| the same, clang | 0.213 s | 0.183 s | **+16%** |
| 50M `push` + index store on a List (runtime-side checks), gcc | 0.367 s | 0.379 s (original runtime) | noise |
| the same, clang | 0.361 s | 0.351 s | +3%, near noise |

The first gcc measurement was **+70%** (0.54 s against 0.32 s). That was not
the check: gcc counted the call to the trap against the size of every
function that stores a field and **stopped inlining the method** into the
loop. Declaring `rt_frozen_trap` `__attribute__((cold))` restored the
inlining and removed the cost. Deduplicating the checks within a basic block
(one check instead of three in the method) changed nothing under clang, so
the remaining clang cost is code layout around the branch, not the number of
checks.

This is a worst case — a loop that does nothing but store fields. Any real
work in the loop dilutes it. If it matters later, in order of cost:

  1. **Hoist**: check an object once per function rather than per store. It
     is sound — an object held by a live frame can never be frozen, because
     the frame's reference would be outside the graph and the freeze would
     trap — but it needs dominance to place the check, so it waits for an
     optimisation pass.
  2. **Read-only parameter types**, so a callee that cannot change its
     argument needs no checks at all. That is the "second keyword" the author
     ruled out for now, and it would still leave the check on any `T`
     parameter.
  3. Nothing cheaper exists that keeps the guarantee through aliases.

## What is open

  - A user-type module constant (a static struct or enum).
  - The hoisting above.
  - Whether `spawn f("literal")` should be accepted the way `spawn
    f(CONSTANT)` now is: a string literal is immortal for the same reason.
    Today it is refused as borrowed; unchanged here.
  - The frozen bit assumes a two's-complement `long` of at least 32 bits.
    On a 32-bit `long` the count has 30 bits, which a program would need a
    billion references to one object to exhaust.
