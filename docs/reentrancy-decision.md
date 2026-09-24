# The runtime calls the program back: the decision

Decided **2026-09-23**, after an adversarial review found four
memory-unsafe programs. Implemented in `runtime/rt.h` (`RC_SORTING`,
`RC_PROBING`, `RC_FLAGS`, `rt_check_mutable`) and `runtime/rt.c`
(`rc_guard_take`, `rc_guard_drop`, `rt_sort_with`, the four `rt_map_*`
probes, `rt_map_clear`, `rc_dec`). Tests corpus/core/1000-1002,
corpus/traps/1010-1015. The reference is §3.9, §4.4a and §7.4.

Extended the same day with **the release paths** (below), the other way the
runtime reaches the program: a destructor, rather than a `cmp` or a `hash`.
`rt_list_clear` was unsafe; `rt_map_remove` and `rt_map_set` were tightened
to match the rule. Tests corpus/core/1150-1154.

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
through `drop` rather than through `cmp`. That is the next section.

---

# The release paths

`cmp`, `hash` and `eq` are three of the four ways the runtime runs the
program's own code in the middle of its own operation. The fourth is
**`drop`**: every path that releases a reference may end an object's life,
and a destructor can do anything, including reach back into the collection
the release came out of.

The mark is the wrong tool here, for a reason worth stating. `sort` and a
map probe have an *answer to finish computing*: the merge is halfway through
a permutation, the probe is holding a slot index, and a mutation makes the
rest of the work meaningless — so the honest reply is a trap. A release has
no answer to finish. `clear` is going to leave an empty collection whatever
happens; `remove` has already decided which entry goes. The work can simply
be done in an order that makes the mutation harmless, and then a destructor
that pushes onto the list it was just cleared out of is not a bug at all —
it is a pool handing its slot back, which §4.4 gives as the example of what
a destructor is *for*.

## The rule

**Detach first; touch nothing afterwards.**

Concretely, in two halves:

  1. Before the first release, put the structure into the state the
     operation will leave it in, and take what is being released out of the
     program's reach — into a local. `clear` detaches the whole buffer or
     table; `remove` marks the slot dead and decrements `len`; `set` puts
     the new value in the slot. A destructor then sees a finished
     collection, and whatever it does to it is an ordinary change to an
     ordinary collection.
  2. After the first release, read nothing out of the structure again — not
     a length, not a pointer, not a flag. Everything the rest of the
     operation needs is already in a local.

The second half is what keeps the receiver's own lifetime out of the
question. A destructor may drop the last reference to the collection it is
being released from, and then the collection is freed halfway through the
operation. The caller does retain the receiver across a call that can
release (`src/lower/hold.rs`, `releases`), so this cannot happen as things
stand — but that is a compiler invariant holding up a runtime file, and the
runtime is cheaper to write so that it does not need it. `rt_map_remove`
read `m->val_is_ref` *after* releasing the key, which is exactly the shape
the rule forbids; both flags are read up front now.

That retain stays where it is. `releases` is not only about the receiver: it
is what makes `may_run_code` true for a `clear`, a `remove` or a `set`, and
so what holds every *other* borrowed operand of the statement they appear in
(`src/lower/hold.rs`). Only the receiver half of it is now belt-and-braces.
Its comment still says the runtime goes on to the next element after a
release, which `rt_list_clear` no longer does.

**No new traps.** Nothing in §7.4 changes. A destructor that changes the
collection it is being released from is legal, and there is a defined answer
for what it sees.

## Every path, and what it does

| Path | Releases? | Mechanism |
|---|---|---|
| `rt_list_clear` | yes, every element | **detach** — the whole buffer, then release from the local |
| `rt_list_pop`, `rt_list_remove_at` | no | the reference is handed OUT; the statement that discards it releases afterwards, with the list already short |
| `rt_list_insert`, `rt_list_push`, growth/realloc | no | nothing is released, so no program code runs; `insert` re-reads `l->data` after the push that may have moved it |
| index store (`rt_index_set`), Array element store | no, in the runtime | the lowering reads the old value, retains the new, stores, and releases the old LAST (`src/lower.rs`) — the runtime holds nothing across it |
| a struct field store | no, in the runtime | same order, emitted by `src/lower.rs`: the field already holds the new value when the old one's destructor runs, so a destructor that writes the same field wins |
| `rt_map_set` replacing | yes, the old value | the slot holds the new value and the flag is in hand before the release; the probing mark comes off first |
| `rt_map_remove` | yes, key and value | the slot is DEAD and `len` decremented first; both flags read first |
| `rt_map_clear` | yes, every entry | **detach** — the whole table. Complete: key and value both come out of the detached table, and only a local is touched afterwards |
| `map_rehash` | no | runs under the caller's `RC_PROBING` mark |
| `rt_map_keys`, `rt_map_values`, `rt_map_clone`, `rt_seq_clone`, `rt_seq_slice` | no | they only retain; no program code runs, so the cached source pointer is safe |
| `rt_snapshot` / `copy_obj` / a type's `CopyFn` | the one `rc_dec` at the very end | a value that owns a resource cannot be in a snapshot at all (`refuse_resource` runs over the whole graph first), so no destructor exists to run |
| `rt_check_unique`'s walk | no | a `WalkFn` reports references, it never releases one; and it runs no program code |
| the channel paths | no | `send` and `recv` move a slot under a lock; ownership passes, nothing is released |
| `bytes` — every path | no | `bytes` holds no references at all, so `clear`, `truncate`, `drop_front`, `set` and the reserve path cannot run anything. Confirmed, not assumed |
| `lst_drop_refs`, `arr_drop_refs`, `map_drop`, emitted `drop_T` | yes, in place | see below |

## Why the drop functions may release in place

`lst_drop_refs` walks `l->data` releasing as it goes and then frees the
buffer; `map_drop` and the emitted `drop_T` do the same with a table and
with a struct's fields. That is the shape the rule forbids — and it is safe
here for a reason that holds only here.

A drop function runs when the count has reached **zero**. No reference to
the object exists, so no destructor it runs can name it: to reach the list
being dropped, an element's destructor would have to get there through some
reference, and any such reference would have kept the count above zero. The
one remaining way in is a field of an object that is *itself* dying, whose
fields are still set while its own drop runs — and that object's count is
zero too, so the same argument applies to it, one level up, all the way out.

Two escape hatches, both already closed: a destructor that stores `this`
somewhere that outlives the call is the **resurrection trap**
(`src/emit_c.rs`, §4.4), and a **cycle** is never released at all (§7.1), so
its members' destructors never run.

## What a destructor sees, and what stays legal

  - **Changing a different collection.** Nothing is marked on a release
    path, so every other collection in the program is untouched by this
    (corpus/core/1152).
  - **Reading the collection being released from.** It reads *finished*: a
    list being cleared is empty, a map is already one entry shorter. It is
    never seen half-cleared — which is the observable difference from the
    old code, where the first destructor under `clear` saw the full length
    (corpus/core/1150, 1152, 1154).
  - **Changing it.** Legal, and it sticks: a destructor that pushes onto the
    list being cleared leaves those elements in it when `clear` returns
    (corpus/core/1151). They are not released by the `clear` that is still
    running — it owns a detached buffer and does not know about them.
  - **Dropping the last reference to it.** Legal (corpus/core/1152). Unlike
    the same act under a `sort`, which traps, because there is no answer
    left to compute.
  - **Removing the very element being destroyed.** This was the bug. A
    `clear` that released in place left the element in the list while its
    destructor ran, so `remove_at(0)` handed the *same* object out a second
    time and the statement that discarded it released it again: `drop` ran
    twice, then the resurrection trap under `-DRC_DEBUG` and a
    heap-use-after-free under AddressSanitizer, on gcc and clang at -O0 and
    -O2 (corpus/core/1150). With the buffer detached there is nothing to
    remove.

## The cost

Detaching costs two stores. The one real price is that `clear` gives its
buffer back instead of keeping it, so a list cleared and refilled in a loop
allocates again each time round. Measured, gcc -O2, best of nine interleaved
runs, against the same runtime immediately before the change:

| | before | after | ratio |
|---|---|---|---|
| build and `clear` 1 000 000 `P`, ×8 | 0.42 s | 0.42 s | 1.00 |
| refill and `clear` 1 000 000 immortals, ×40 — the buffer-reuse worst case | 0.23 s | 0.22 s | 0.96 |
| 1 000 000 `pop`, ×8 (unchanged path) | 0.40 s | 0.40 s | 1.00 |
| 1 050 000 `remove_at(0)` on a 4096-element list (unchanged path) | 1.00 s | 1.02 s | 1.02 |
| 10 000 000 `set` that replace | 0.39 s | 0.39 s | 1.00 |

Nothing moved outside the noise of this machine, in either direction. The
second line is the one built to be unkind — a million *immortal* elements,
so every release is a single compare and the reallocation is nearly all that
is left — and it came out at or below the old time in all nine runs: giving
an 8 MB buffer back and taking a fresh one costs less than keeping a cold
one warm. A list of values does not pay at all: nothing is released there,
so no program code can run, so there is nothing to detach from and the
buffer is kept, the way `bytes.clear` keeps it.

`rt_map_remove` and `rt_map_set` each moved a load earlier in the same
straight line of code; nothing was added.
