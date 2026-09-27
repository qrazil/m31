# The performance board

What is agreed, in what order, and why. Opened **2026-09-26** after the real
programs in `apps/` gave the first honest numbers, and after the value-enum
work showed that the remaining costs are structural rather than diffuse.

Everything here is *implementation* work except where marked, so most of it
can land after the language freezes. Two items change the language and are
called out as such.

The number that started this: **SHA-1 in `apps/git` runs at roughly ten times
the time C takes.** Not because the arithmetic is slow — because every
element access is an out-of-line call and the no-LTO gate forbids inlining
it away. That single fact accounts for most of the gap on every
byte-processing workload we have.

---

## 1. Inline the accessors, and hoist the metadata

**The biggest single win, and no semantic change.**

`a[i]` is an out-of-line call today. It should be address arithmetic in the
caller. Two halves:

  - **Inline the accessor** so the call disappears. The wrapper exists for
    bounds checking and the frozen-flag check, and both survive inlining.
  - **Hoist the data pointer and the length** into locals once, ahead of a
    loop, instead of reloading them on every access.

The second half is the one that needs care: hoisting is only legal if
nothing in the loop can reallocate the buffer. After the first half, a
simple loop contains no opaque call and the compiler can see that for
itself. Loops that *do* call something opaque need the guarantee to come
from somewhere else — which is item 2.

Measured precedent: the git agent did this by hand and got **+19%** on one
loop.

## 2. `const` makes hoisting legal across opaque calls — **language change**

Recorded in `docs/const-decision.md`, second decision. A frozen value's
pointer and length cannot move, so they can be held in registers across a
call the compiler cannot see into. Two parts:

  - **`const` is required where the compiler can prove no mutation**, so the
    guarantee is present in the source rather than rediscovered — and stays
    present, because removing it breaks the build instead of quietly
    removing the optimisation.
  - **`const` gains a block form**, `const b { ... }`, so a buffer that is
    filled, read and refilled can be frozen for the read and mutable for the
    refill. This is the only way to get the guarantee without the snapshot's
    deep copy.

Note the barrier this creates for the check-hoisting pass — see that record's
"Interaction with check hoisting".

## 3. Errors are an id — **language change**

Recorded in `docs/errors-decision.md` §4. One 64-bit id, no payload, static
message table, OS errno as a reserved class.

`Result<int, Error>` goes from 32 bytes to 16, which is the ABI cliff:
returned in registers instead of through memory. Measured at **36× the boxed
version** in isolation, and it is the remaining gap between the clean style
and a hand-rolled flag in zlib. It also removes an allocation from every
error path.

Freeze-level: payloads cannot be added back later without changing the size
of every `Result` in every signature.

## 4. Inline storage for `Array`

An `Array` never grows, so its elements can sit immediately after the header
in one allocation instead of behind a pointer. One allocation instead of
two, and one less indirection on every access.

`bytes` cannot do this as-is because it grows; the variant that would work
there is a small-buffer optimisation, which is a separate and larger piece
of work.

## 5. Intern `Option.None` and every payload-free variant

A variant with no payload has nothing to distinguish one instance from
another, so one static immortal instance can serve forever. Identity is not
observable, so nothing can detect the sharing.

The field-less-type machinery from the closures work already does exactly
this, so the mechanism exists. The largest beneficiary is not errors but
`None`, which is constructed constantly.

Largely subsumed by item 3 for errors specifically; kept because it applies
to every enum, not only error enums.

---

## Later, with a decision attached

**Slices — `(owner, offset, len)`.** Sub-ranges without copying, which is
what every parser in `apps/` actually wants; `substr` allocates today. The
owner reference keeps the data alive, which a raw pointer could not do
safely across a reallocation. It costs one indirection per access, and that
indirection hoists away inside a `const` region — so items 1 and 2 pay for
it. Needs its own discussion before anything is built.

**Escape analysis.** The general form of item 1's second half, and what
would let a non-escaping object live on the stack outright. Wanted, large,
and deliberately after everything above — most of its benefit on our actual
workloads is available more cheaply through `const`.

## Considered and not doing

**Constant-size arrays as a stack-allocated language feature.** Escape
analysis subsumes it, and it would add a second array type to a language
trying to have one of each thing.

**Splitting objects into a stack header and a heap payload.** The header
holds the reference count; two names for one object would mean two counts
for one buffer and a double free. Making the stack part a handle to a shared
heap header reintroduces exactly the indirection it was meant to remove. The
salvageable part of the idea — caching the pointer and length in stack
locals for the duration of a function — is item 1.

**Bare `{}` mini-scopes**, for earlier drops. `{}` is already an empty map
literal, so a block at statement position is ambiguous; and a general scope
is a second way to do what a function already does. If earlier release turns
out to be needed in real code, an explicit `drop x` statement is the honest
spelling.
