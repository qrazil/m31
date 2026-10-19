# Heap watch: epoch close and age log

Status: **proposal, not decided** (2026-10-19). Nothing here is built.

## The position this starts from

A cycle that leaks is a bug in the program, not a gap in the language. A
collector is a pump bailing water out of a ship with a hole in it: it works as
long as the pump keeps up, and the hole is never fixed. m31 does not pump. It
already refuses a garbage collector, weak references, trial deletion and an
ownership discipline (`docs/closures-decision.md`, "Cycles"), and accepts
`Arena<T>` with integer handles for structures that are cyclic on purpose.

What is missing is a way for the developer to **see** the leak, while the
program runs, with enough detail to fix it. Like a queue's size, how long an
object should live is hard to know at compile time and easy to measure at run
time.

So the design has two rules, and both cost a few instructions per operation:

1. **No heap walks.** Nothing ever visits objects in order to find leaks, at
   any time and for any reason. A scan on a timer or on request is a garbage
   collector's mark phase with a different name, so it is ruled out.
2. **Nothing is freed.** The runtime reports and the developer fixes. A
   leaked object stays leaked.

There are two mechanisms. They complement each other: each one catches what
the other one misses.

---

## 1. Epoch close

### The idea

An **epoch** is a unit of work, such as a request, a task or one turn of a
loop. When it ends, it should have left nothing behind except what it chose to
keep. Each epoch keeps a count of the objects it made and did not keep. At the
end of the epoch that count must be zero. If it is not, every object still
counted is reachable only from other throwaway objects of the same epoch, which
is a leaked cycle, found without looking at a single object.

### Where epochs come from

- **Every spawned task is an epoch**, so a server that handles each request in
  its own task gets this with no annotation.
- **An explicit block**, for work that runs inside a longer-lived task:

  ```c
  while (true) {
      Request r = conn.read_request();
      epoch "request" {
          handle(r);
      }        // anything handle() made and did not keep is reported here
  }
  ```

  Epochs nest, and each thread has its own stack of open epochs. The keyword
  and its spelling are open questions (see the end of this document).

### The bookkeeping

Every object carries two small things in the bits of its count word that the
count never uses (see "Cost" below):

- **`depth`**: the depth in the stack of open epochs of the epoch that owns
  the object;
- **`kept`**: set once the object outlives every open epoch.

An object that is not kept always belongs to an epoch that is still open.
That is what keeps the tag small: it names a position in the stack, not one
epoch out of millions, so it never wraps around.

For each open epoch the runtime keeps a counter of its live objects that are
not kept, per type.

| Event | What it does |
|---|---|
| allocate | tag = current depth; `count[depth][type] += 1` |
| free, object not kept | `count[depth][type] -= 1` |
| a **store** puts value `v` into holder `h`: a field, an element, a map slot, a global | if `h` is kept, then **keep** `v`. Else, if `h.depth < v.depth`, then **adopt** `v` into `h.depth`. Otherwise do nothing, which is the common case: one compare. |
| assign to a local declared **outside** the epoch block | adopt into that local's depth (the compiler knows statically which locals these are, so it emits the operation only there) |
| the epoch block's result, or a value returned out of a spawned task | adopt into the enclosing depth |
| channel send / spawn capture | the value is moved, and on receive it is re-tagged into the receiver's current epoch. The send already walks the moved graph for `rt_check_unique`, so this rides along on a walk that happens anyway. |
| close the epoch | every nonzero `count[depth][type]` is reported, then the row is cleared |

**Keep** and **adopt** spread to what the value points at, but only to objects
that are deeper and not kept, and they stop at anything already shallower or
kept. An object's depth only ever decreases, and it can be kept only once, so
each object is moved a bounded number of times over its whole life. The cost
follows allocation, not the size of the heap.

### Why the report is exact

At close, the epoch's own locals are dead. Every way an object can outlive the
epoch legitimately (stored somewhere older, assigned to an outer local,
returned, sent) has moved it out of the epoch's count. Anything still counted
is therefore held only by other counted objects of the same epoch, so it is
unreachable, and it is a cycle or hangs off one.

That makes it exact **if the list of adoption paths is complete.** A path the
design forgets, such as a capture rule for closures, shows up as a false
report. That is a bug in the runtime, and tests find it, because every corpus
program must close its epochs clean.

### What a report looks like

```
heap-watch: epoch "request" (task 17, depth 2) closed with 3 objects never freed:
  2 Conn
  1 Session
```

Debug builds also record the allocation site:
`2 Conn @ net/server.m31:40`.

---

## 2. Age log

### The idea

Some objects are kept on purpose and live a long time, so epoch close stops
watching them once they are kept. They can still leak: two kept objects later
point at each other and the cache drops both, or a cache is simply never
cleared. No boundary marks that moment, because the release leaves a count of
1, and that looks exactly like an ordinary release.

What can be noticed is that the object is **too old**. A connection alive
after 10 minutes is leaked, whatever the reason.

### The bookkeeping, still without a scan

- Each **watched type** keeps its live objects in a list in birth order:
  append on allocation and unlink on free, both O(1). The oldest object is
  therefore always at the head.
- Each watched object stores a coarse birth time (a 32-bit tick in seconds).
- A timer, run by the scheduler about once a second, looks **only at the head
  of each list**. The cost grows with the number of watched types, never with
  the number of objects.

Budgets are configuration, not code, so they can be tuned without
rebuilding, exactly like a queue size:

```
M31_AGE=Conn:10m,Session:2h
```

```
heap-watch: Conn @ net/server.m31:40 alive 10m12s (budget 10m), 4 over budget
```

With no budget, a watched type still logs its oldest age and live count every
N seconds. That trend is how the developer picks the budget, like watching a
queue's depth before choosing its size.

### Which types are watched

The types named in `M31_AGE` are watched, plus every type in a debug build
run with `M31_AGE=*`. An unwatched type pays nothing for the age log.

---

## Does it cover everything?

No. Here is the full list.

| Leak | Epoch close | Age log | Today |
|---|---|---|---|
| Cycle among objects one unit of work made and did not keep | **exact**, at the end of that unit, with type and epoch | if watched and over budget | count only, at exit |
| Cycle formed later between kept objects | missed | **if the type is watched** | count only, at exit |
| Retained by mistake (a cache never cleared), not a cycle | missed (it is kept) | **if watched** | invisible: not a leak at exit if the cache is a global |
| Leak inside an epoch that never closes (the main loop, the top-level program) | missed until exit | **if watched** | count at exit |
| A leaked cycle that holds a resource (a file, a socket) | as above | as above (a `Conn` is the prime example) | count only |
| Kept cycle of an **unwatched** type in a release build | **missed** | **missed** | count only, at exit |

The last row is the honest gap: a leak among long-lived objects of a type
nobody chose to watch is found only by the existing `__rc_live` count at
exit. Two ways to narrow it:

- run test and staging builds with `M31_AGE=*`, which watches every type;
- treat any type with a destructor (one that owns a resource) as watched by
  default, since those are the leaks that hurt.

Neither mechanism **finds** a leak in code that never runs. Both report only
what happened, which is the point: it is runtime evidence, not proof.

---

## Pros and cons

**Pros**

- **No heap walk, ever.** Every cost is per operation and bounded, with no
  pauses and no dependence on heap size. It does not become a GC by the back
  door.
- **Exact where it applies.** Epoch close has no heuristics and, by
  construction, no false positives.
- **Points at the fix.** A report names the unit of work and the type, and in
  debug builds the allocation site. "Request handler leaks a Conn" is a bug
  report; "__rc_live=2" is not.
- **Fits what m31 already has.** Spawned tasks are natural epochs, the send
  already walks the moved graph, the store sites are all generated code, and
  the count word has spare bits.
- **Tunable at run time.** Age budgets are configuration, learned by watching
  the program, which is your queue-size argument.
- **Nothing is freed.** The developer keeps full responsibility, and behaviour
  is identical with heap watch on or off, except for what it prints.

**Cons**

- **Not complete.** Kept cycles of unwatched types slip through (see the
  table).
- **Adoption rules must be complete.** Every way a value can escape an epoch
  needs an operation. Missing one gives false reports. The list is short
  today (stores, outer locals, returns, sends, globals), but closures and any
  future feature must update it.
- **Store sites get a branch.** Every reference store pays a compare. It is
  cheap and predictable, but it is not free.
- **The spreading walk.** Keep and adopt walk newly made objects. Bounded and
  amortised, but a single store can occasionally move a large fresh graph,
  such as building a big list and then caching it.
- **The age log costs memory per watched object** (below).
- **A new surface word**, `epoch`, if explicit epochs are wanted. Spawned
  tasks alone need no syntax.
- **Delay.** A leak is reported when its epoch closes or its budget expires,
  not at the moment it happens.

---

## Cost (estimates; to be measured in qrazil/benchmark)

### Memory

| Item | Cost |
|---|---|
| epoch tag + `kept` bit | **0 bytes per object.** The count word has 60 bits of count (`runtime/rt.h`). Taking 8 bits of depth and 1 kept bit leaves 51 bits, far more references than can exist. |
| per-epoch counters | one row per open depth per thread: types × 8 bytes, allocated lazily. About 500 types × 8 B = 4 KB per open depth; a typical server task has 1–3 open, so about 12 KB per thread. |
| age log, per **watched** object | two list links (16 B) + birth tick (4 B), about **24 B per watched object** after alignment. 10 000 live `Conn` = 240 KB. Unwatched types: 0. |
| allocation site (debug builds only) | a 4 B site id; with alignment, 0–16 B per object depending on the malloc size class |
| `M31_AGE=*` (watch everything, test builds) | about 24 B per object. For a 16 B header plus a typical 32 B payload, that is +50 % memory. Acceptable for tests, not for production. |

### CPU

For scale: a `malloc`/`free` pair is roughly 20–50 ns.

| Operation | Added cost |
|---|---|
| allocate | one counter increment, about 1–2 ns |
| free (not kept) | one counter decrement, about 1–2 ns |
| reference store | one compare and a usually-not-taken branch, about 0.3–1 ns; plus the spreading walk when it adopts, amortised over the objects it moves |
| `rc_inc` / `rc_dec` | unchanged. They already mask `RC_FLAGS`; the mask grows to include the new bits. |
| epoch enter / close | push or pop plus reading one counter row, tens of ns. A request handler does far more work than that. |
| age log, watched types | about 3 ns on allocate and on free (link/unlink); a 1 s timer reads the head of each list |
| send / receive | re-tagging rides on the uniqueness walk that already happens |

**Guess at total cost:** a few percent (about 2–5 %) on code that allocates
heavily and stores many references, and close to 0 on computation-heavy code.
`M31_AGE=*` adds about 5 % more. Epoch close is cheap enough to consider
leaving **on in release builds**, with the age log off unless budgets are set.
The benchmark has to confirm this before anything is decided.

---

## Open questions

1. Should explicit epochs exist, or only spawned tasks at first? Starting
   with tasks only would need no new syntax.
2. The keyword: `epoch`, `scope`, `unit`, `work`? It also needs an optional
   name string for reports.
3. Should epoch close be on in release builds by default, or only behind a
   flag?
4. Should types with destructors be watched by default?
5. Where do reports go: stderr, a file (`M31_HEAPWATCH_LOG`), or a stdlib
   callback so a server can export them as metrics?
6. Should a closed epoch with leaks only log, or also trap in test builds, so
   that CI fails on any leaked cycle in the corpus?
