# IR v0 — the walking skeleton

The smallest IR that can carry a real program end to end: allocate a
refcounted value, pass it across a call boundary, drop it, print an integer.

Its job is not to be the final IR. Its job is to be **built and run before
anything is locked**, because three implementations of a paper IR all break at
integration time simultaneously, and then you are fixing one design in three
places with three different mental models of what it should have been.

Everything here is provisional except the ownership protocol in §5, which is
the part that is expensive to change later.

---

## 1. Shape

SSA with **block parameters**, not phi nodes. Same choice Cranelift makes: a
jump carries its arguments, so there is no separate phi list to keep in sync
with predecessor order, and no way to express a malformed phi.

```
func <name>(<param>: <type>, ...) -> <type> {
block0(<param>: <type>, ...):
    <value> = <op> <operands>
    ...
    <terminator>

block1(<param>: <type>, ...):
    ...
}
```

Every block ends in exactly one terminator. Every value is assigned exactly
once. Values are function-scoped (`v0`, `v1`, …); blocks are `block0`,
`block1`, …

## 2. Types

Surface types are lowercase keywords, as in Oro. They are **not** the same
thing as IR types, and keeping the two apart is deliberate.

| Surface | IR (today) | C |
|---|---|---|
| `int` | `i64` | `int64_t` |
| `bool` | `i1` | `bool` |
| `str` | `ref` | `Obj *` |

Three IR types, and no more until something forces a fourth.

**`ref` is the only managed type.** That is what makes the refcount pass
mechanical rather than clever: it inserts `rc_inc`/`rc_dec` for `ref`-typed
values and ignores everything else. No raw untyped pointer in v0 — you do not
need one yet, and adding it later is additive.

### 2.1 What `int` promises, and what the IR may do with it

The IR is free to represent `int` as `i32`, `i64` or `i128` — narrowing where
it can prove the range fits, widening for intermediates — **on one condition:
observable behaviour must be identical.** Under that rule, width selection is
an optimisation, it needs no spec, and it can arrive at any time.

What must not vary is the surface promise. **`int` is 64-bit and traps on
overflow, on every target.** Not "whatever the hardware likes."

This distinction is worth being strict about, because the alternative is the
one mistake C is still paying for. If `int` is arch-dependent:

- the same program traps at different values on different machines, so the
  language has no single meaning and a freeze promises nothing;
- `corpus/core/*.out` becomes arch-dependent and the oracle stops being a
  fixed target;
- Go twins break, because Go's `int` is 64-bit on amd64/arm64 and 32-bit
  elsewhere — the twin would be comparing against a moving target too.

If an arch-native width is ever genuinely wanted, it should be a **separate
named type** (Rust spells it `isize`, Go spells it `int` and regrets the
collision), never a reinterpretation of `int`. Explicit `i32`/`i128` surface
types are additive and can come later; they do not change anything here.

## 3. Instructions

Sixteen.

### Constants

| Op | Signature | Notes |
|---|---|---|
| `iconst <n>` | → `i64` | |
| `bconst <b>` | → `i1` | |
| `sconst <s>` | → `ref` | string literal; **immortal**, see §5.4 |

### Arithmetic

| Op | Signature |
|---|---|
| `iadd a, b` | `(i64, i64) -> i64`, **traps on overflow** |
| `isub a, b` | `(i64, i64) -> i64`, **traps on overflow** |
| `imul a, b` | `(i64, i64) -> i64`, **traps on overflow** |
| `icmp <cond> a, b` | `(i64, i64) -> i1`, `cond ∈ {eq ne lt le gt ge}` |

**Overflow traps.** There is no wrapping variant — one way to do each thing.
A `wrapping_add` can be added the day something needs it, and until then its
absence is a feature.

Lowered to `__builtin_add_overflow` and friends, which are gcc 5+ and
clang 3.8+. The check is `static inline` in `rt.h` while `rt_trap` is
out-of-line and `_Noreturn`, so the compiler treats the trap edge as cold
and the hot path stays straight. Arithmetic is the hottest thing in the
language; a call per add would be indefensible.

Verified 2026-09-18: identical values and identical trap behaviour under gcc
and clang at `-O0` and `-O2`, exit status 134, no warnings.

**Consequence for the oracle:** Go wraps here, so an overflowing program
cannot have a Go twin — the twin would print `-9223372036854775808` and be
confidently wrong about us. Overflow cases live in `corpus/traps/` with an
expected trap message instead. This is a clean split rather than a
compromise, and overflow tests are rare.

### Control flow

| Op | Signature |
|---|---|
| `jump blockN(args...)` | terminator |
| `brif c, blockN(args...), blockM(args...)` | terminator, `c: i1` |
| `ret v?` | terminator |

### Calls

| Op | Signature |
|---|---|
| `call f(args...) -> v?` | direct call; result optional |

Builtins (`print`, `concat`, …) and the built-in methods are ordinary
`call`s to runtime functions. This
keeps them out of the instruction set entirely, which is where they belong.

### Memory and refcounting

| Op | Signature | Notes |
|---|---|---|
| `alloc <size>` | → `ref` | refcount starts at 1 |
| `load r, <offset>` | `(ref, imm) -> i64 \| ref` | |
| `store r, <offset>, v` | `(ref, imm, val)` | |
| `rc_inc r` | `(ref)` | |
| `rc_dec r` | `(ref)` | frees at zero |

## 4. Two forms of the IR

The IR is valid in two states, and the distinction matters:

- **pre-refcount** — what the frontend emits. No `rc_inc`/`rc_dec` anywhere.
- **post-refcount** — what a backend consumes. All refcount traffic explicit.

The refcount pass is the only thing that inserts these ops. **Backends never
reason about ownership** — they see explicit `rc_inc`/`rc_dec` and emit calls.
That is what keeps a second and third backend cheap, and it is why the
protocol below lives in one pass rather than being spread across emitters.

---

## 5. Ownership protocol

The part that is expensive to change. Everything else in this document is
cheap to revise.

### 5.1 Arguments are **borrowed**

The caller guarantees the argument stays alive for the duration of the call.
The callee does **not** decrement it, and does **not** need to increment it
just to use it. If the callee wants to keep the value past the call, it
performs its own `rc_inc`.

### 5.2 Returns are **owned (+1)**

The callee hands back a value the caller is responsible for eventually
`rc_dec`-ing.

### 5.3 Why this split

It is Swift's default (`guaranteed` arguments, `owned` returns) and it is the
one that minimises traffic on the common path: passing a value you already
hold, to a function that only reads it, costs **nothing**. The alternative
(+1 arguments) makes every call site retain and every callee release — correct,
symmetric, and slower on exactly the pattern that dominates real code.

The cost is that the caller must keep the value alive across the call, which
constrains where the refcount pass may place a `rc_dec`. That constraint is
easy to satisfy and easy to verify; the traffic is not easy to win back.

### 5.4 Literals are immortal

`sconst` yields an object with `rc == RC_IMMORTAL`. `rc_inc` and `rc_dec`
both early-return on it.

The alternative — teaching the refcount pass that literals are special — was
rejected. **`rc_dec` is emitted uniformly and the runtime decides.** One way to
do each thing: the pass never asks where a `ref` came from.

Immortal objects are never counted in `__rc_live`, because only `alloc`
increments the tracker. That keeps the invariant in `runtime/rc_debug.h`
meaningful: it counts heap objects that must be freed, and nothing else.

### 5.5 Non-atomic

v0 is single-threaded and `rc_inc`/`rc_dec` are plain increments. Making them
atomic later is a change to **two runtime functions**, not to the IR and not
to the emitter — *provided* no backend ever inlines the refcount operation by
hand. That is a rule, not a preference.

### 5.6 Moves across a thread boundary

§5.5 holds only while one thread can reach a value at a time. `send` and
`spawn` are the two places a reference can leave the thread that made it, so
they **move**: the sender's reference becomes the receiver's, with no
`rc_inc` and no `rc_dec` in between. Using the local afterwards is a compile
error.

A move may only take a value the current block **owns**:

  - an owned temporary (a call result, §5.2) — moved directly;
  - a local declared in *this* block — moved, and marked moved.

Everything else is refused at compile time. A parameter is borrowed (§5.1),
so the caller still holds it. A local from an enclosing block is refused too:
the move is a property of the *program point*, but the local outlives it, so
a loop body or one arm of an `if` would move the same reference twice. The
diagnostic points at `clone(x)` in both cases.

Retaining rather than moving is not the fix. Two threads sharing one
non-atomic counter is the defect; a `rc_inc` before the handoff only makes
the race begin at 2.

Aliasing the compiler cannot see is caught at run time by `rt_check_unique`,
and the check is **transitive**. Checking only the moved object is not
enough: a uniquely-owned wrapper can hold a reference that is shared, and
then several threads touch one non-atomic count through it.

So the check walks the graph reachable from the moved value and requires,
for every object in it, that the references into it from *within* the graph
account for its whole refcount — the mover's own reference to the root
counting as one. An object reached twice inside the graph is accepted: one
thread still owns all of it, and a conservative `rc == 1` test would refuse
it wrongly. Immortal objects — literals, and channels — are skipped and not
walked.

This needs a per-type `WalkFn` in `TypeInfo` alongside the `DropFn`: drop
releases and may free, walk only reports. It is a real check, not a debug
assertion, and costs time proportional to the graph, paid once per crossing
— against a lock and a condition variable that the crossing already pays.

Channels are exempt. A channel is *how* threads share, so it is aliased
rather than moved, and is immortal in v0 (`rt_chan_new`).

---

## 6. Calling convention

| | |
|---|---|
| Arguments | positional, borrowed (§5.1) |
| Return | single value or none, owned (§5.2) |
| `ref` | `Obj *` |
| `i64` | `int64_t` |
| `i1` | `bool` |

Aggregates passed or returned **by value do not exist in v0**. This is the
one place backend independence genuinely leaks — C, Cranelift and LLVM each
classify struct passing differently — so v0 sidesteps it entirely and the
decision gets made once, deliberately, when it is actually needed.

---

## 7. Lowering to C

### 7.1 Runtime — and why it is a separate translation unit

The runtime lives in `runtime/rt.c` + `runtime/rt.h`, compiled separately
from emitted code and linked. **This is load-bearing, not stylistic.**

Measured 2026-09-18, building the §7.3 example: with `rc_dec` as a
`static inline` in the same translation unit as the emitted code, `gcc -O2`
warns

```
'free' called on unallocated object 'str_hello' [-Wfree-nonheap-object]
```

because it inlines `rc_dec`, sees the argument is a static literal, and
cannot prove the `RC_IMMORTAL` guard makes the `free` unreachable. Marking
the slow path `noinline` does **not** help — gcc's interprocedural constprop
just clones a specialised `rc_release_slow.constprop` and warns there
instead. Moving the runtime into its own translation unit hides the static
objects from that analysis and is clean under `-O0` and `-O2` with
`-Wall -Wextra`.

`-flto` reintroduces the warning, so LTO is deliberately not used. If LTO
ever becomes worth having, the alternative is to **drop immortality and
heap-allocate literals once at startup**, so no static `Obj` exists at all —
it costs a little startup time and removes the special case in §5.4
entirely.

```c
/* rt.h — included by emitted code */
#define RC_IMMORTAL (-1)
typedef struct Obj { long rc; int64_t len; const char *data; } Obj;
void    rc_inc(Obj *o);
void    rc_dec(Obj *o);
int64_t rt_len(Obj *o);
void    rt_print(int64_t v);

/* rt.c — its own translation unit */
void rc_dec(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    RC_ASSERT(o->rc > 0, "decrement below zero");
    if (--o->rc == 0) { RC_TRACK_FREE(); free(o); }
}
```

`alloc` calls `RC_TRACK_ALLOC()`; `rc_dec` calls `RC_TRACK_FREE()` on the
transition to zero. Under `-DRC_DEBUG` the two must balance at exit, which is
oracle layer 4.

The general lesson, worth keeping: **the C emitter's output gets read by an
adversarial optimiser.** `run.sh` therefore builds with `-Wall -Wextra` and
treats any warning as a failure — the same instinct as Oro's
`clippy -D warnings` gate. A warning in generated code is a defect in the
emitter, and usually the early form of a UB bug.

### 7.2 Blocks

Basic blocks become labels; jumps become `goto`. Block parameters become
variables declared at function scope and assigned immediately before the jump:

```
jump block2(v5)        →     b2_p0 = v5; goto b2;
```

**Known trap:** when block parameters reference each other across a jump
(`jump block2(p1, p0)`), sequential assignment corrupts them. The general fix
is temporaries — the parallel-copy problem. v0 has no case that hits it, but
the emitter should assert rather than silently miscompile, because this is
exactly the kind of bug the `-O0` vs `-O2` differential will *not* catch.

### 7.3 Worked example

`corpus/core/002-refcount.src`:

```
int take(str s) {
    return s.size();
}

void main() {
    str s = "hello";
    print(take(s));
}
```

Syntax is C/Java-shaped: type-first declarations, semicolons, braces. That
is parseable here without C's lexer hack because there are no raw pointer
declarators and no `typedef` — `IDENT IDENT` is an unambiguous declaration
with two tokens of lookahead, the same way Java manages it.

Post-refcount IR:

```
func take(s: ref) -> i64 {
block0(s: ref):
    v0 = call rt_len(s)       ; borrowed — no refcount traffic
    ret v0
}

func main() -> i64 {
block0:
    v0 = sconst "hello"       ; immortal
    v1 = call take(v0)        ; borrowed — no inc, no dec
    call print(v1)
    rc_dec v0                 ; end of s's scope
    v2 = iconst 0
    ret v2
}
```

Note what is *absent*: no `rc_inc` before the call, no `rc_dec` inside
`take`. That is §5.1 paying for itself on a two-line program.

Emitted C:

```c
static Obj str_hello = { RC_IMMORTAL, 5, "hello" };

int64_t take(Obj *s) {
    int64_t v0 = rt_len(s);
    return v0;
}

int64_t lang_main(void) {
    Obj    *v0 = &str_hello;
    int64_t v1 = take(v0);
    rt_print(v1);
    rc_dec(v0);
    return 0;
}

int main(void) { return (int)lang_main(); }
```

Run under `gcc -O0`, `gcc -O2`, `clang -O0`, `clang -O2`: all four print `5`
then `__rc_live=0`. That is the walking skeleton green, and the point at which
the IR has been tested rather than assumed.

---

## 8. Deliberately absent

Not oversights. Each is deferred because it does not change the shape of v0,
and adding it later is additive:

closures · aggregates by value · dynamic dispatch and vtables · generics
(monomorphisation happens in the frontend, so the IR never sees them) ·
concurrency and atomics · cycle collection and weak refs · float ·
unsigned · arrays and indexing · modules · unwinding (errors are values)

## 9. Open questions

1. ~~**Integer overflow**~~ — **settled 2026-09-18: `int` is 64-bit and
   arithmetic traps.** See §2.1 and §3.
2. **Where the refcount pass places `rc_dec`** — last use, or end of scope?
   Last-use frees earlier and is what you eventually want; end-of-scope is
   simpler and is what v0 does. Note that last-use placement interacts with
   §5.1: the value must outlive the call that borrows it.
3. **`load` result typing** — the table says `-> i64 | ref`, which means the
   instruction is not self-describing. Either the offset carries a type or the
   instruction splits into `load_int`/`load_ref`. Splitting is probably right.
4. **String representation** — `Obj` with an out-of-line `data` pointer is
   convenient for static literals and bad for locality. Revisit when strings
   are real.
