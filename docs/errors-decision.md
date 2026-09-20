# Errors — the decision, and what is still open

Written before building, in the shape of `docs/concurrency-decision.md`: the
parts that are settled, the parts that are not, and the reasoning for both.

Errors are the last thing blocking the freeze. Everything after them —
modules, the string library, the standard library — is written *in terms of*
them, so getting this wrong means writing all of it twice.

---

## Settled

### Errors are values, not exceptions

No `throw`, no unwinding, no stack of handlers. A function that can fail says
so in its return type.

This is not a taste call. Unwinding would have to interact with every
`rc_dec` the compiler has already placed — each frame needs a landing pad
that releases exactly the locals that are live at that point, which is the
one part of the compiler that is currently mechanical and would stop being
so. The C backend has no unwinder to borrow either. Values cost nothing new:
a `Result` is an enum, and enums already work.

It also matches the concurrency decision. `docs/concurrency-decision.md`
notes that libdill-style cancellation — a killed thread makes every blocking
call return an error — fits errors-as-values and needs no unwinder. Two
decisions pointing the same way is worth something.

### `Option<T>` and `Result<T, E>` are built in

Both, blessed by the language rather than declared per program.

The reason is not convenience, it is that **a built-in method cannot return a
user-defined type**. The compiler has to know what `xs.index_of(v)` returns.
Without a blessed `Option`, `index_of` cannot exist at all, `"42".parse_int()`
cannot exist, and every fallible thing in a standard library is stuck.

The second reason is composition. If two libraries each declare their own
`Result`, a function cannot propagate one through the other without a
conversion at every boundary. One shared vocabulary is most of the value.

The cost is two types frozen into the language for good. That is acceptable
for these two specifically: their shape is settled across the whole ML
lineage and has not needed revision in fifty years.

They stay ordinary enums — the same `match`, the same exhaustiveness, no
special syntax for construction. Only their *declaration* is built in.

### No existing trap becomes an error

The line is: **a trap is for a bug in the program; an error is for the
world.**

Staying traps — every one of them is a defect at the call site, and a
`Result` would only let it be ignored:

  - an index outside `0 .. size-1`
  - `pop` on an empty list
  - division or remainder by zero, and integer overflow
  - `int(f)` on a NaN or an out-of-range float
  - a uniqueness violation at a thread boundary

Becoming errors — none of these exist yet; they arrive with the standard
library, already shaped as `Result`:

  - a file that is not there, or cannot be read
  - text that does not parse as a number
  - anything involving a network

So this decision changes no code that exists today. That is the point of
making it now rather than after the standard library is written.

---

## Open, with a recommendation

### 1. Does `Map.get` keep trapping? — DECIDED: no

Implemented. `get` returns `Option<V>`, and `index_of` exists at last.
`Option` grew exactly two methods (`is_some` and `or`) so that a
lookup is one line rather than a four-line `match`; without them the change
would have been a downgrade. No `unwrap`, deliberately.

The original question, kept because the reasoning is the record:

Today `get` traps on a missing key and `contains` is the check. With `Option`
built in, `get` could return `Option<V>` instead, which cannot be forgotten
and does the lookup once rather than twice.

**Recommendation: change it.** Trapping was the honest answer when there was
no way to express absence; there is one now. The `contains`-then-`get`
pattern is also a double hash of the same key.

It is a breaking change, which is exactly why it should happen before the
freeze rather than after.

### 2. What does propagation look like? — DECIDED: postfix `?`

Implemented. Parsed as part of the one postfix chain rather than a loop of
its own, which is what lets `m.get(k)?.size()` work.

The original reasoning:


Without sugar, every fallible call is a staircase:

```c
Result<int, str> r = half(n);
match (r) {
    case Err(str e): { return Result<int, str>.Err(e); }
    case Ok(int v): { ... }
}
```

Three candidates:

| | |
|---|---|
| `int x = half(n)?;` | Rust's. Terse. `?` is free here — there is no ternary and there never will be (§9 of the reference). |
| `int x = try half(n);` | Zig's. Reads aloud, but `try` means exceptions to anyone from Java or C++, and this is the opposite of that. |
| nothing | Go's. Honest and very noisy, and Go needs it because it has no sum type to shorten. We do. |

**Recommendation: postfix `?`.** The objection to `?` is that it hides a
return, which is a fair thing to dislike — but it hides *one specific*
return, always in the same place, and the type system will not let you forget
the function returns a `Result`. Go's verbosity buys visibility this language
already gets from the signature.

`?` on an `Option` in a function returning `Option` should work the same way.

### 3. Do the error types have to match exactly? — DECIDED: yes

Implemented, with a diagnostic naming both types.

The original reasoning:


`?` in a function returning `Result<T, E1>`, applied to a `Result<U, E2>`.

Rust converts via `From`. We have no such mechanism, and inventing one for
this is a large feature hiding inside a small one.

**Recommendation: require `E1` and `E2` to be the same type, for now.**
Restrictive, honest, and relaxing it later is additive — a conversion
interface can arrive with the `to_X` family (`docs/roadmap.md` §2) and `?`
can start using it without any existing program changing meaning.

### 4. What is `E` in practice?

If the standard library returns `Result<T, str>`, errors are strings: easy,
and lossy — a caller cannot branch on *which* failure without parsing text.

If it returns `Result<T, SomeError>` with a blessed error enum, callers can
branch, and the enum has to be right on the first try because it is frozen.

**Recommendation: decide this last**, once the standard library exists and
can say what failures it actually has. Nothing about `?` or the built-in
`Result` depends on it.

### 5. Must a `Result` be used? — DECIDED: yes, and it is an error

Implemented. `Option` stays exempt.

The original reasoning:


A function returning `Result` whose caller ignores it is the classic quiet
bug — C's `fclose` problem. Rust warns via `#[must_use]`.

**Recommendation: make it an error, not a warning**, and only for `Result`.
The language has no warnings today and should not grow a category for this.
`Option` is exempt: ignoring an `Option` is often reasonable.

---

## Order of work

1. ~~Blessed `Option<T>` and `Result<T, E>`.~~ **Done.** Declared by the
   compiler, usable with today's `match`, no new syntax.
2. ~~`Map.get` returns `Option<V>`; `index_of` arrives at last.~~ **Done**,
   along with the three `Option` methods that keep a lookup to one line.
3. ~~The `?` operator, with exact error-type matching.~~ **Done.**
4. ~~Unused-`Result` is an error.~~ **Done.**
5. `str.parse_int` and friends. Still blocked on question 4 — what `E` is —
   which is now the only open one.

Steps 1 and 2 landed on their own, as planned: they are useful immediately
and they are what the string library was actually waiting for.
