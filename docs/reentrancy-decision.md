# The runtime calls the program back: the decision

Decided **2026-09-23**, after an adversarial review found four
memory-unsafe programs. Implemented in `runtime/rt.h` (`RC_SORTING`,
`RC_PROBING`, `RC_FLAGS`, `rt_check_mutable`) and `runtime/rt.c`
(`rc_guard_take`, `rc_guard_drop`, `rt_sort_with`, the four `rt_map_*`
probes, `rt_map_clear`, `rc_dec`). Tests corpus/core/1000-1002,
corpus/traps/1010-1015. The reference is §3.9, §4.4a and §7.4.

---

## The problem

Until `cmp`, `eq` and `hash` went into `TypeInfo`, a built-in method ran no
program code: `sort` compared machine words, a map hashed an int or a str,
and the runtime was alone for the length of every operation.
corpus/core/823 opens by saying so.

That is no longer true. `sort()` on a `List<P>` calls `P.cmp` between every
pair, and a `Map<K, V>` keyed on a user type calls `K.hash` and `K.eq` on
every probe. The program can do anything in those methods, including reach
back into the collection the runtime is in the middle of — and the runtime
was holding, across the call:

  - **an element buffer address.** `rt_sort_with` reads `slots(o)` once and
    merges through it. A `cmp` that pushes onto the list being sorted
    reallocates that buffer: segfault, gcc and clang, -O0 and -O2.
  - **an element that may be the last of its kind.** A `cmp` that only
    *replaces* an element frees the one that was there; the merge is still
    holding it. AddressSanitizer saw a heap-use-after-free; every build
    without it printed a plausible answer.
  - **a slot index.** `map_probe` returns an index into `m->slots`, and
    `set`, `get`, `has` and `remove` read or write that slot after the
    callback has returned. An `eq` that inserts enough entries to rehash
    makes the index meaningless: a NULL dereference in `rc_dec` from
    `remove`, and from `set` either a write silently discarded or a live
    entry clobbered without being released — 410 objects leaked, and neither
    the size nor the value the program asked for.

`map_rehash` probes too, so a `set` that grows the table runs `hash` while
the old table is still in a local.

## The rule

**A collection may not be changed while the runtime is using it.**

For the length of one operation that calls back into the program — a
`sort`, or one map probe and the slot use that follows it — the receiver is
*in use*. Every path that changes a collection refuses one that is: `push`,
an index store, `insert`, `remove_at`, `pop`, `clear`, `truncate`,
`reverse`, a second `sort`, `set` and `remove` — the same set `const`
already refuses, checked by the same line. The message names the operation:

    trap: the list changed while it was being sorted: `cmp` may read the
    list being sorted, but not change it

    trap: the map changed while it was being searched: `hash` and `eq` may
    read the map being searched, but not change it

**Letting go of it counts too.** A `cmp` that overwrites the field the list
was read from drops the last reference and frees the buffer under the merge,
without changing the collection at all. `rc_dec` checks the mark on the path
where a count reaches zero, and traps with its own message.

A trap and not a defined behaviour, because there is no defined behaviour to
offer: "the element you added may or may not be sorted" is not a rule a
program can be written against. Mutating a collection while it is being
sorted or probed is a bug (docs/errors-decision.md, "no existing trap
becomes an error"). Java throws `ConcurrentModificationException` for the
same situation and calls it fail-fast; this is the same answer without an
exception to catch.

### What stays legal

  - **Reading the receiver.** Through the merge the list is always a
    permutation of the elements it started with, and the map's table is
    whole until the probe returns, so `size()`, an index, `contains`,
    `get`, `keys()` and `values()` all answer something true
    (corpus/core/1000, 1002). A `for ... in` over either is a read.
  - **A nested read of the same receiver.** `m.get(k)` from inside that
    map's own `eq` probes the same map again. The mark is taken by whoever
    finds it clear and dropped only by them, so the inner probe does not
    unmark the outer one (corpus/core/1002).
  - **Changing a different collection.** Only the receiver is marked, so a
    `cmp` may keep a tally, memoise into a map, or sort a scratch list
    (corpus/core/1001). That has to stay legal: a comparison that counts how
    often it ran is ordinary code.
  - **Changing the elements' own fields.** The slots do not move.

## Why a mark on the receiver, and not the alternatives

**Re-read `slots(o)` and re-probe after every callback.** Sound for the
buffer address, and it makes `sort` cost a load per comparison; but it
cannot save the freed *element* case at all — the merge is holding a
reference the program released — and re-probing per `eq` turns a linear
probe into a quadratic one. It also leaves the program with a defined-but-
useless behaviour where a trap is the honest answer.

**A flag field in `Lst`, `Arr` and `Map`.** A whole word, or a layout change
in three structs that `src/lower/consts.rs` emits as static data for module
constants. The header word was already being loaded and tested.

**Refuse it at compile time.** A `cmp` cannot be shown not to reach its own
list: it gets there through a field, through any function it calls, through
an interface. The check would have to be transitive through the whole
program, and would refuse the legal reads with it.

## The cost

The mark is **two bits of the refcount word**, directly below `RC_FROZEN`,
and `rt_check_mutable` tests all three at once — the same single test that
was already there for frozen, against a different constant. So a mutation
that is not refused costs nothing at all, and an operation that takes the
mark costs two stores for the whole operation, not per element.

Measured, gcc -O2, best of nine interleaved runs, against the same runtime
with `rc_guard_take` stubbed out:

| | with the guard | without | ratio |
|---|---|---|---|
| sort 1 000 000 ints (no program code at all) | 168.4 ms | 171.1 ms | 0.98 |
| sort 1 000 000 of a user type (~20M `cmp` calls) | 424.4 ms | 432.5 ms | 0.98 |
| 300 000 `set` + 300 000 `get` on a user-typed key | 176.6 ms | 178.1 ms | 0.99 |

All three came out marginally *faster* with the guard, which is to say the
cost is below the noise floor of this machine — the two builds differ by two
stores per `sort()` call and two per map operation, against 20 000 000
comparisons and 600 000 probes. The count keeps 60 bits, which is 10^18
references to one object.

An immortal object is never marked: `RC_IMMORTAL` is every bit set, so the
bits could not be cleared again. It does not need them — it reads as frozen,
so sorting it already traps, and a module-constant map's keys can only be
ints or strs (`src/lower/consts.rs` refuses a user-typed key, because the
compiler would have to run the program's `hash` to lay the table out), which
run no program code.

## Found on the way

**A list's `contains` and `index_of` need no mark at all.** They were on the
list of things to check, and the answer is that they cannot get there:
`src/lower.rs` *refuses* `contains` on a list whose element type is a
reference other than `str` — "`contains` compares `int`, `float`, `bool` and `str`; `P` would
need its own comparison" — so `rt_seq_contains` compares a str by value, a
float as a float, and anything else by word, and runs no program code
whatever. Nothing can change the list under them, so the cached `slots(o)`
in both is safe as it stands. If `contains` is ever extended to a type with
an `eq`, it needs `RC_PROBING` (or `RC_SORTING`) around its loop, for
exactly the reason `sort` does. A MAP's `contains` is `rt_map_has`, which
does probe and is marked with the rest.

**`Map.clear` released its entries while walking its own table.** A value's
destructor is program code and may reach that map. The table is now detached
before anything is released, so the destructor finds an empty map it can
fill again rather than the one being walked. `rt_map_remove` does the same:
the slot is marked dead and `len` decremented before either release runs.
`rt_list_clear` still releases in place, and a destructor that removed from
the list being cleared could still be surprised — the same family of bug,
through `drop` rather than through `cmp`, and not fixed here.
