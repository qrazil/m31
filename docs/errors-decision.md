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
`Option` grew exactly three methods (`is_some`, `is_none`, `or`) so that a
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

#### DECIDED 2026-09-26: an error is an id, and carries no payload

The library now exists, so the question can be answered from evidence rather
than taste. **An error value is one 64-bit id. No variant carries a payload,
nothing is allocated, and the text lives in a static table indexed by the
id.**

The word split:

    class : 32 bits    which error set — 0 core, 1 OS, 2.. per module
    code  : 32 bits    which error within that set

##### Why: it is the only shape that fits in registers

This is not a micro-optimisation, it is the difference between an error path
that allocates and one that does not. The current shape, measured on zlib:

    Error  = tag (8) + two ints (16)              = 24 bytes
    Result = tag (8) + the larger of int / Error  = 32 bytes  -> returned through memory

    proposed:
    Error  = one id                               =  8 bytes
    Result = tag (8) + 8                          = 16 bytes  -> returned in two registers

16 bytes is the ABI cliff. `docs/value-enums.md` measured the same program
three ways — `Result` over heap enums at 51 MB/s, over value enums at
79 MB/s, and a hand-rolled error flag at 92 MB/s — and separately measured a
payload-free error at **36× the boxed version, beating the hand-rolled
flag**. The clean style is only free in the configuration this decision
makes universal.

##### Why an id and not a heap singleton

The first sketch was a reserved heap address per error, so every mention of
`NotFound` was the same object. An id is strictly better: there is no pointer
to load, no reference count to touch, and no object to keep alive. The
static table is at a link-time address, so reading the message is
`base + id * stride` — one indexed load.

Nothing may point into the *stack* for this. A stack address dies with its
frame, and returning is exactly what an error does. The table is static
data, which has no lifetime at all.

##### The OS block, in place of `Other(int)`

`io.Error.Other(int)` exists today to carry an errno, and it is the reason
`io.Error` has a payload at all. Instead, **class 1 is the OS, and the code
*is* the errno.** One reserved range replaces ~130 declarations and the
payload with them, and it still round-trips the exact number the kernel
returned.

##### Per-module sets, one uniform width

Each module keeps its own error type, so `match` over `io.Error` is still
checked exhaustive and a reader can still see what `read` can fail with.
Because every error type has the same one-word shape, any of them widens
into a universal `Error` for `?` to propagate across module boundaries. That
is Go's uniform `error` for propagation and Rust's per-module enums for
handling, without paying for either.

##### How many core errors

**Twelve to twenty, not fifty.** Once the OS range is mechanical, the
hand-written set is only the semantic ones. The evidence from neighbours is
that small wins: Go's sentinel set is about fifteen; Rust's `io::ErrorKind`
is about forty, is widely regarded as a mistake, and had to be marked
non-exhaustive because most variants are never matched. The starting list:

`NotFound`, `Denied`, `Exists`, `Invalid`, `Interrupted`, `WouldBlock`,
`BrokenPipe`, `Timeout`, `Overflow`, `CorruptData`, `Unsupported`, `Closed`.

##### The cost, stated plainly

**An error cannot carry context.** `CorruptData` cannot say "at offset 4213",
and a `NotFound` cannot name the file. This is a real loss and it is why
Rust boxes `io::Error`.

It is accepted because the caller has that context at the point it reports,
and formatting it there costs nothing on the success path — whereas a payload
costs an allocation on every error, in a language whose entire argument is
that it has no garbage collector. A failure that genuinely needs structured
detail returns a type of its own, which is what a `Result<T, E>` already
allows.

**This is a freeze-level commitment.** Payloads cannot be added later
without changing the size of every `Result` in every signature.

#### Implementation note, 2026-09-28: the OS-errno class needs a real mechanism, not just payload-freedom

Implemented for `json`, `base64`, `csv`, `text`, `http` and `apps/git`'s
`zlib`/`refs`/`object` — every variant payload-free, and it cost nothing
new: the existing value-enum rule already lays out a payload-free enum as a
bare `{ tag: int64 }`, 8 bytes, once every variant qualifies. No new 64-bit
id type was needed for these.

**`io.Error`, `net.Error` and `term.Error` were the exception, and they are
the ones that matter most in practice** — every file and socket operation
returns one. Their `Other(int)` errno-passthrough variant was left with a
real payload rather than folded into the class/code scheme this section
describes, so these three stayed 16-byte value enums and `Result<T,
io.Error>` stayed 24 bytes: still boxed, still returned through memory, the
exact ABI cliff this decision exists to clear.

The reason was mechanical, not a judgement call: an enum variant's tag is
always a compile-time constant equal to declaration order. The class/code
scheme needed the OS variant's tag to be a *runtime-computed* value, which
needed new construction and match-destructuring support for that one
variant shape — real, bounded, but touching match-arm codegen, which is
central and heavily exercised, so it was correctly scoped out of the same
change that swept the rest of the library rather than risked alongside it.

##### Done, 2026-09-28: the tag-packing mechanism

**`Other`'s payload now lives inside the tag word, not beside it.** The tag
is still the compile-time declaration index for every ordinary variant; for
the one variant this applies to, it is `OS_ERRNO_TAG_BASE | errno` instead,
with `OS_ERRNO_TAG_BASE = 1 << 32`. No ordinary enum has anywhere near
four billion declared variants, so the reserved high bit can never collide
with a real tag, and a plain `tag >= OS_ERRNO_TAG_BASE` test is exactly "is
this `Other`" — no separate discriminant is needed. This is the shape this
section originally proposed (`class << 32 | code`), narrowed to what was
actually missing: a single reserved range on ONE variant of a handful of
already-frozen enums, not a second error representation living alongside
per-module enums.

Three places needed to know about it, all gated on one new predicate,
`TypeDef::os_errno_variant(tag)` (`src/ir.rs`):

  - **Construction** (`Inst::EnumPack`, `src/emit_c.rs`): for this one
    variant, the tag is computed (`base | (uint32_t)errno`) instead of
    written as the constant every other variant's tag is.
  - **Reading the payload back** (`Inst::EnumPayload`, `src/emit_c.rs`): a
    mask (`tag & 0xFFFFFFFF`) instead of a union read.
  - **`match` dispatch** (`lower::lower_match`, `src/lower/stmt.rs`): the
    comparison this arm gets is `tag >= OS_ERRNO_TAG_BASE` instead of
    `tag == <constant>`, wherever in the arm list it appears — `net.src`'s
    `error_of` writes `Other` FIRST, `io.src`'s and `term.src`'s `to_str`
    write it LAST, and both compile to correct dispatch, because the range
    test and every ordinary equality test are mutually exclusive by
    construction. `to_str`/message formatting needed no change at all: it
    already just calls `code.to_str()` on the bound payload, and the
    payload now arrives the same value through a different read.

The layout change is one line: `emit_enum_body` (`src/emit_c.rs`) skips
generating a union member for this one variant, exactly as it already skips
one for every payload-free variant, so `io.Error` — whose ONLY
payload-carrying variant this is — gets no union at all: `{ int64_t tag; }`,
8 bytes, measured (§ below).

**Named by exact type, not detected structurally.** The predicate checks
`io#Error` / `net#Error` / `term#Error` by name, not "a value enum with one
`int`-payload variant and the rest payload-free" — the corpus has over a
dozen fixtures with exactly that shape for unrelated reasons
(`corpus/core/1281`, `1283`, `1285`, `1287`, `1289`, `1291`, ... — each
deliberately exercising the ordinary value-enum rule on an int payload), and
a structural rule would have reached into all of them. It would also have
been unsound in general: a general `int` payload can be negative or exceed
32 bits, and this packing silently corrupts either. It is exact only because
a Linux errno is always a small non-negative number, which is a property of
what `Other` specifically holds, not of "a variant shaped like this."
`is_immortal_variant` (the sibling mechanism this extends, for an ordinary
payload-free variant's single shared static instance) is untouched by this
and is never asked about `Other`: `os_errno_variant` requires `is_value`,
and `is_immortal_variant` requires `!is_value`, so the two are disjoint by
construction, matching that a computed-tag variant obviously cannot be
interned — its value varies by construction.

**Measured**, compiling a program against the real stdlib (`--emit-c`, then
`sizeof` under gcc, `-O2`, x86-64 System V):

    io.Error                 16 bytes -> 8 bytes
    Result<int, io.Error>    24 bytes -> 16 bytes

16 bytes is exactly the register-return line `docs/value-enums.md` §3
measured: `Result<int, io.Error>` now returns in two registers (`rax:rdx`)
instead of through a caller-allocated stack slot.

**Verified functionally**, not just by size: a program exercising all three
types' `from_errno`, a genuine kernel-triggered unnamed errno
(`ENAMETOOLONG` via a 300-character path component, through `io.open`), and
`match` on the computed variant in first, middle and last position produced
byte-identical output before and after this change (`gcc -Wall -Wextra`
clean both times). `./gates.sh` (all 17 gates) and the three application
suites (`apps/markdown`, `apps/git`, `apps/tui`) pass unchanged.

This decision is now fully realized: every error type in the stdlib,
`Other`/`Other`-shaped variants included, is an 8-byte value enum, and no
`Result` over one of them goes through memory on account of its error type.

Implemented. `Option` stays exempt.

The original reasoning:


A function returning `Result` whose caller ignores it is the classic quiet
bug — C's `fclose` problem. Rust warns via `#[must_use]`.

**Recommendation: make it an error, not a warning**, and only for `Result`.
The language has no warnings today and should not grow a category for this.
`Option` is exempt: ignoring an `Option` is often reasonable.

### 6. Can a program trap with its own message? — DECIDED: yes, `trap(msg)`

Implemented, as a builtin statement.

The standard library needed it first — `random.integer(3, 1)`, `os.exit(256)`
and declaring `--help` twice are bugs in the caller, and "a trap is for a bug
in the program" says they trap — and it met the need with a private prim.
But the need is not the library's: any program has preconditions and
invariants the language cannot see, and without a way to trap on them it has
two bad choices — a `Result`, which tells the caller a bug is an event in the
world to be handled, or a contrived index out of range. `trap` takes neither.
It adds no way to *recover*, so it changes nothing settled above: a trap is
still uncatchable, still for bugs, and a failure of the world is still a
`Result`.

Spelled `trap` because that is this language's word — the section of the
reference, the runtime's `trap: ` prefix — rather than borrowing `panic`,
which in Go comes with `recover`. It is a statement and never returns, so
the missing-return check knows a function may end in one. There is no
`assert`: it would be a second spelling of `if (!c) { trap(msg); }`.

---

## Order of work

1. ~~Blessed `Option<T>` and `Result<T, E>`.~~ **Done.** Declared by the
   compiler, usable with today's `match`, no new syntax.
2. ~~`Map.get` returns `Option<V>`; `index_of` arrives at last.~~ **Done**,
   along with the three `Option` methods that keep a lookup to one line.
3. ~~The `?` operator, with exact error-type matching.~~ **Done.**
4. ~~Unused-`Result` is an error.~~ **Done.**
5. ~~`str.parse_int` and friends, blocked on question 4 — what `E` is.~~
   **Question 4 is decided**: an error is an id and carries no payload. The
   parse methods are unblocked; reworking the existing error enums into that
   shape is item 3 of `docs/perf-board.md`.

Steps 1 and 2 landed on their own, as planned: they are useful immediately
and they are what the string library was actually waiting for.
