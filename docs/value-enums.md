# Value enums

**An enum whose payloads are all scalars is passed and returned by copy, with
no allocation and no refcount.**

This is an **optimisation**, not a language change. There is no new keyword,
no rule about which types are values and which are references for a person to
learn, and nothing in `docs/reference.md` changes except a note about
performance. §4 is the argument that a program cannot tell.

It exists because of `apps/git/FRICTION.md` §1 and `apps/markdown/FRICTION.md`
§1.4, which are the same complaint twice: `Result` and `Option` are the right
types for a hot loop and cost an allocation per use, so both programs wrote a
sentinel instead — a `bool over` checked in nine places in `zlib.src`, a
`bool ok` field in `marker_of`. The reference's advice ("use `Result` for
anything a caller should handle") lost to the cost of following it.

---

## 1. The rule

An enum is a **value enum** when

  - every payload of every variant is `int`, `float` or `bool` — or a
    `distinct` type over one of those, which is erased to it — or another
    value enum;

and none of the exclusions in §2 applies. Everything else is a heap object
with a header, exactly as before.

A payload-less variant carries nothing and never disqualifies anything, so an
enum with no payloads at all (`enum Kind { Text; Raw; Delim; }`) is a value
enum: a tag, eight bytes, no allocation.

**Worked answers to the questions the rule has to settle:**

| payload | value enum? | why |
|---|---|---|
| `int`, `float`, `bool` | yes | a scalar; a copy is the whole value |
| `distinct int Price` | yes | erased to `int` before the IR |
| `str`, `bytes` | **no** | a reference: a copy would have to retain, which is the refcount traffic this removes |
| `Array<int>`, `List<T>`, `Map<K,V>`, `Chan<T>` | **no** | the same, and they are mutable, so a copy would be a second name for one object |
| a struct | **no** | the same |
| another value enum | **yes** | it nests as a union member, at its own size |
| a boxed enum | **no** | it is a reference |
| a value enum inside a **boxed** enum | fine, and the outer one stays boxed | a boxed enum has the same tag-and-union layout, so the value one sits inline (§3) |
| an enum that reaches itself | **no** | it would have no finite size — see below |
| `Option<int>`, `Result<int, int>` | yes | generic, instantiated at a scalar |
| `Option<str>`, `Result<bytes, E>` | no | instantiated at a reference |
| `Option<void>` | yes | monomorphisation drops a `void` payload, so `Some` carries nothing |

**Recursion.** `enum Tree { Leaf; Node(int, Tree); }` is not a value enum: a
struct that contains itself has no size. The qualifying set is computed as a
**least** fixpoint — it starts empty and only adds an enum once every payload
it carries is *already* known to qualify — so a recursive enum, and any
mutually recursive group, is simply never added. There is no special case for
it and no cycle check to get wrong.

**It is a whole-program property, decided once.** `Option<int>` is a value
enum everywhere in a program or nowhere in it. Two spellings of one type
cannot disagree about its shape, and monomorphisation has already made
`Option<int>` a single declaration (`Option$int`), so there is one answer to
give.

## 2. What is excluded, and why

Three things need a value to be an object. Each of them takes the enum back to
the boxed representation for the whole program — the type is still an enum
and the program still means what it meant, it is just slower again.

  1. **A collection's element, a map's key or value, a channel's element.**
     The runtime stores one `int64_t` per slot (`runtime/rt.h`: `Arr`, `Lst`,
     `MapSlot`, the channel buffer), and a tag plus a union is not one
     machine word. `List<Option<int>>` therefore boxes `Option<int>`.
     Detected from the type table: monomorphisation leaves each container's
     type arguments as its `$t0`/`$t1` fields.
  2. **Satisfying an interface.** Dispatch reads a vtable out of the object
     header (`docs/reference.md` §3.4), and a value has no header. Checked by
     method name and arity, which over-approximates: an enum that merely
     looks like it satisfies an interface stays boxed and is only slower,
     never wrong. This is the same reason §3.4 already gives for a `distinct`
     type not satisfying one.
  3. **Crossing a thread boundary** by `spawn`. `rt_check_unique` walks an
     object graph (`docs/ir-v0.md` §5.6) and there is no object to walk.
     Boxing keeps `spawn f(opt)` behaving exactly as it did — including the
     compile error on using the local afterwards — rather than requiring a
     second argument about what a move means for a copy. A channel's element
     is already excluded by (1).

**What is deliberately NOT on that list** is a boxed enum carrying a value
enum. It was, in the first version of this, and it was a trap: a boxed enum's
payload used to be a machine-word slot, so one `Result<bytes, Error>` — a heap
object because of the `bytes` — dragged `Error` onto the heap, and with
`Error` every `Result<int, Error>` in the program. In `apps/git` that is
`zlib.decompress`'s return type, so the optimisation switched itself off in
exactly the program it was built for. §3 says what was done instead.

(3) is the only one the type table cannot answer on its own. Rather than
predict it, `lower::lower_program` lowers the program, and if the lowering
meets a `spawn` of a value enum it boxes that type and lowers the whole
program again. The boxed set only grows and is bounded by the number of types,
so it terminates; in practice it never runs twice.

**The backstop.** `ir::Module::verify` walks the finished IR and panics if an
`IrTy::Val` ever reached a refcount operation, a runtime call, an interface
receiver or a reference-typed field — and, separately, if a call to one of the
program's own functions produces a value of the wrong shape. A case missed
above would be a struct handed to something that reads it as a pointer, which
the C compiler would not always catch; this turns it into a loud compiler bug
instead. It runs on every compile.

The second check earned its place immediately. `str.parse_float` is
implemented in the language (`lib/__floatfmt.src`) and its result was being
given the `Obj *` shape at the call site, because until now every user type
had it. That was a C type error in eleven corpus programs, reported by gcc
from inside forty thousand lines of generated code; the verifier names the
instruction and the function instead.

## 3. The representation

**One layout for every enum.** A boxed enum used to be a tag followed by as
many `int64_t` slots as the widest variant needed, shared between variants. It
is now a tag and a union of per-variant structs — exactly what a value enum
is. The only difference between the two is the `Obj` header a boxed one
carries and the heap it lives on:

```c
/* zlib#Error -- a value: a tag and a union, by copy */
typedef struct {
    int64_t tag;
    union {
        struct { int64_t p0; } v0;                /* Truncated(int)        */
        struct { int64_t p0; int64_t p1; } v2;    /* StoredLength(int,int) */
        struct { T5v p0; } v3;                    /* OverSubscribed(Table) */
    } u;
} T9v;

/* Result$bytes$zlib#Error -- boxed, because of the bytes */
typedef struct {
    Obj hdr;
    int64_t tag;
    union {
        struct { Obj *p0; } v0;   /* Ok(bytes)                             */
        struct { T9v p0; } v1;    /* Err(Error) -- INLINE, at its own size */
    } u;
} T11;
```

  - The **tag** is the variant's declaration index, the number `match`
    compares against, unchanged.
  - The **payload members are typed** and per variant, rather than
    machine-word slots shared between variants. Three things follow, and the
    second is the one that matters:
      - a payload may be another value enum, at its own size;
      - a **boxed** enum may hold a value enum inline, so a `Result` that is
        on the heap for one of its variants does not force its error type onto
        the heap too — the trap described in §2;
      - a `float` payload is a `double` member rather than a bit pattern in a
        slot, so it skips the `rt_f2i` / `rt_i2f` round trip it used to pay.
  - A variant with no payload contributes no union member; an enum where no
    variant has one gets no union at all, because a union with no members is
    not C.
  - The drop, walk and copy functions already switched on the tag to find the
    reference-carrying slots, so they name the union arm now and are otherwise
    unchanged. `EnumTake` clears a member instead of a slot.
  - Value enums are emitted **before** every other type and in dependency
    order, because one may be a field of an ordinary type or a payload of
    another. The order exists because the rule admits no cycle.

The IR gains one type, `IrTy::Val(tid)`, and `Ref` stays **the only managed
shape** — which is the property `docs/ir-v0.md` §2 says keeps the refcount
pass mechanical. `docs/ir-v0.md` §6 said aggregates by value did not exist in
v0 and that the decision should "get made once, deliberately, when it is
actually needed"; this is that decision, and it is confined to the C backend's
`c_name`.

### What gcc and clang actually do — measured, not assumed

x86-64, System V ABI, both compilers at `-O2` (`langc` emits no
`__attribute__` and no packing, so this is the plain ABI):

| size | example | returned |
|---|---|---|
| 8 bytes | a payload-less enum | one register (`rax`) |
| 16 bytes | `Option<int>`, `Result<int, E>` for a payload-less `E` | **two registers** (`rax:rdx`), no memory at all |
| 24 bytes and up | `Result<int, Error>` where `Error`'s widest variant is two words | hidden pointer (`sret`), caller-allocated stack slot |

Verified by reading the assembly from both compilers: a 16-byte
tag-and-payload struct is built in registers and returned in registers, with
no store and no load. At 24 bytes the caller passes a pointer and the callee
writes through it — three or four stores and three or four loads, on the
stack, in cache.

**Sixteen bytes is a cliff, and it is worth 11×** on a loop that does nothing
but return one (`bench/value-enums.md` §2). Both sides of it are far below a
`rt_alloc` plus a `free` plus two out-of-line refcount calls, which is what
the same value used to cost — but a `Result` whose error type carries two
`int`s is 32 bytes and goes through memory, and that is the whole of why the
`apps/git` rewrite in `bench/value-enums.md` §4 came out 14% slower than the
sentinel it replaces rather than faster.

## 4. Nothing observable changes

The claim the whole thing rests on: **a payload cannot be mutated — you build
another enum instead — so a value enum has no identity a program can detect,
and copying it is invisible.** Here is every way a program could try.

**Identity.** There is no identity operator. `==` on a user type is that
type's `eq` method and never a pointer comparison (`docs/reference.md` §6.2);
there is no `is`, no address-of, no pointer-to-integer conversion, and
`print` of a user type is its `to_str` method, not an address (§6.5a). So two
enums with the same tag and the same payloads are already indistinguishable,
and a copy is exactly that.

The reference had already worked this out for `str`, in §3.2: "`str` is
immutable, so its aliasing is not observable except through identity, which
matters only at a thread boundary." A value enum is immutable for the same
reason a `str` is, and a thread boundary is exactly the case §2 (3) excludes —
so the sentence carries over word for word, and the exclusion is what makes it
carry over.

**`==`.** Goes through the type's own `bool E.eq(E)`, which is now a direct
call taking two structs. `eq` can only read the tag and the payloads, which
are identical in the copy. An `eq` that somehow distinguished them would have
to see identity, and it cannot.

**Mutation.** An enum's payload cannot be assigned: the only way to reach one
is a `match` binding (§5.6), and the only way to change one is to construct
another enum. There is no shared mutable state to alias, so "one object or
two" is not a question a program can ask.

**A generic function.** Monomorphisation runs first, so `f<Option<int>>` is an
ordinary function taking the struct by value. The IR never sees a generic.

**A collection.** Excluded — §2 (1). The type is boxed and behaves as before.

**A channel send, `spawn`, `rt_check_unique`.** Excluded — §2 (1) and (3).
Boxed, so the move rules, the uniqueness check and every diagnostic about them
are unchanged. This is deliberate: the alternative was to argue that a copy
needs no uniqueness check (true) and that the *compile error* on using a moved
local should therefore go away (a language change), and the second does not
follow from the first.

**`const` and freezing.** A `const` local of a reference type takes a snapshot
(`rt_snapshot`) so that nothing else can change it underneath. A value enum
skips it, and that is not a weakening: the binding is already a copy nothing
else can reach, its payloads are scalars so nothing under it is mutable
either, and a payload cannot be assigned in any case. `rt_snapshot` would have
nothing to freeze. It also traps on a value that owns a resource, and a value
enum cannot own one — see the next paragraph. A **module** constant of a user
type is refused today (§4.5, "a user type is not allowed yet"), so that case
does not arise.

**A destructor.** An enum cannot declare one: `docs/reference.md` §4.4 says
"only a struct may have one — not an enum", and
`lower::check_destructor_decls` enforces it. Nor can a value enum *reach* one:
every payload is a scalar or another value enum, so nothing under it owns a
resource and nothing under it has a destructor to run. Destructor timing
elsewhere in the program is therefore untouched — there was never a release to
schedule.

**Interfaces.** Excluded — §2 (2).

**`clone`.** `clone` of a value enum is the assignment that binds the result.
It still means what §6.5a says — "the copy holds the same references, each
retained once more", vacuously, since it holds none, and writing one cannot be
seen through the other, since neither can be written.

*This turned up a real bug on the way.* `clone` of a **boxed** enum was
copying the type's `fields`, and an enum's `fields` is empty — so
`clone(Shape.Circle(7))` allocated a fresh object, copied neither the tag nor
the payload, and handed back `Shape.Empty`, silently, with no diagnostic. It
is fixed here (`ir::Inst::EnumClone`), because otherwise this change would
have made `clone` correct for value enums and left it wrong for the others.
`corpus/core/1295` and `1296` are the regression tests.

**The refcount invariant.** `__rc_live` counts `rt_alloc`s that must be freed.
A value enum performs none, so it still balances at zero — which the corpus
checks on every program on every build.

**Traps, evaluation order, arithmetic.** Untouched: the same instructions in
the same order, with the allocation and the refcount pairs removed.

**Where the claim would have broken**, and what was done about it: an enum
satisfying an interface, and an enum crossing a thread boundary. Both are in
§2 as exclusions rather than as arguments, because in both cases the honest
answer needed a change to what a program means, and the rule is narrower
instead.

## 5. What it costs and what it buys

`bench/value-enums.md` has the numbers and the programs that produced them.
In one line: fifty million `Option<int>`s returned from a loop went from
1.36 s and fifty million allocations to 0.13 s and none.

## 6. What this does not do

  - It does not make a `Result` carrying a `str` free. A payload that is
    genuinely text still costs an object — though a boxed enum's inline
    payload (§3) means only that one enum pays, not everything that mentions
    it.
  - It does not make a big `Result` free. Over 16 bytes the value goes through
    the stack, and `apps/git`'s zlib rewrite came out 14% slower than its
    sentinel because of it (`bench/value-enums.md` §4). The two things that
    would move that line — narrower integer types, and merging a nested
    enum's tag into its parent's — are both additive and neither is here.
  - It does not remove the allocation from a `List<Option<int>>`. §2 (1).
  - It does not change what a person writes. That is the point.
