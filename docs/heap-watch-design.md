# Heap watch: one ledger for every object

Status: **proposal, not decided.** Revision 2, 2026-10-19. Nothing is built.

## Starting point

A leaked cycle is a bug in the program, not a gap in the language. A
collector works like a pump on a ship with a hole in it: it keeps you afloat
as long as it keeps up, and the hole never gets fixed. m31 already refuses a
garbage collector, weak references, trial deletion and an ownership
discipline (`docs/closures-decision.md`, "Cycles"). It accepts `Arena<T>` with
integer handles for structures that are cyclic on purpose.

What is missing is a way to **see** leaks while the program runs, in enough
detail to fix them. How long an object should live, like the right size for a
queue, is hard to know at compile time and easy to measure at run time.

### Requirements (from the owner, 2026-10-19)

1. **Every heap object** is covered, with no opt-in per type.
2. **Servers never exit cleanly.** They crash, or they are killed when a new
   build is deployed. So suspects must be **recorded while the program runs**
   and must survive a crash or `kill -9`. A report printed at exit is not
   enough.
3. **One stop.** A single mechanism answers "what is leaking, from where, and
   since when".

### Rules the design keeps

- **No heap walk, ever.** Nothing visits objects to find leaks: not on a
  timer, not on request, not at exit. A scan is a collector's mark phase under
  another name.
- **Nothing is freed.** The runtime reports and the developer fixes.
- **O(1) per operation.** Every cost is a counter update on an event that
  already happens: allocate, free, store.

---

## The ledger

The idea is to keep the books, not count the objects. The runtime never looks
at objects. It keeps **counters**, and every allocation and every free updates
them. The counters alone answer every question.

### What each object carries: nothing new

The header stays two words (`runtime/rt.h`). The count word has 60 bits below
its flags, and no program needs anywhere near that many. Heap watch splits
them up:

```
 63    62      61..60   59     58..53   52..33         32..0
[sign][FROZEN][BUSY x2][KEPT][DEPTH 6][BIRTH 20 bits][COUNT 33 bits]
```

- **COUNT**, 33 bits, allows 8.6 billion references to one object. An
  overflow traps instead of wrapping.
- **BIRTH** is the minute the object was made, counted from program start.
  20 bits is about 2 years. Nothing wraps for any process that lives less
  than 2 years; one that lives longer folds old minutes into one "ancient"
  bucket.
- **DEPTH** and **KEPT** are for epoch close (below).

The allocation site, meaning the file and line of the constructor or the
collection literal, comes from the type pointer. **Each allocation site gets
its own `TypeInfo`.** It is a copy of the type's record with the same drop,
walk and method table, plus a site id and a `canon` pointer to the real type.
That costs static data only, about 100 B per site and nothing per object.
Type identity checks in the runtime (`o->ty == &rt_lst_ref_type`, and a few
others) switch to `o->ty->canon`.

**Per-object memory cost: 0 bytes.**

### What the runtime counts

For every allocation site `s`:

| Counter | Updated on | Answers |
|---|---|---|
| `live[s][birth_minute]`, sparse | allocate `+1`; free `-1` (the object's birth minute is in its header) | how many objects from site `s` born at minute `m` are still alive, so **how old the survivors are** |
| `lifetime[s][k]`, 16 log2 buckets | free: the object's age goes in bucket `log2(age)` | how long objects from `s` **normally** live, learned from the program itself |
| `epoch[depth][s]` | see "Epoch close" | what the current unit of work made and did not keep |

The current minute is a global that the scheduler tick updates. Reading the
clock costs one load, never a system call.

### Suspects, with no configuration

A site is a **suspect** when its survivors are much older than its own
objects normally live:

> oldest survivor of site `s` > max(`M31_HEAPWATCH_MIN`, default 10 min,
> `F` × p99 of `lifetime[s]`, default `F` = 10)

- A `Conn` that normally lives 30 seconds and has survivors aged 40 minutes
  is a suspect, without anyone writing "10 minutes" anywhere.
- A site whose objects **never** die, such as configuration or a cache
  filled at startup, has no lifetime baseline. It is listed as "never
  freed, N alive", not flagged as a suspect, and the developer can see it
  and decide.
- Overrides are configuration, so changing them needs no rebuild:
  `M31_AGE=Conn:10m,Config:forever`.

This is the queue-size argument made automatic: the runtime measures each
site's normal lifetime and flags the objects that fall far outside it.

### Persistence: survives crash and `kill -9`

- The counter tables live in a **memory-mapped file**,
  `$M31_HEAPWATCH_DIR/<program>.<pid>.ledger`. Updating a counter is a plain
  memory write. The kernel writes the pages back to disk on its own, and
  **they survive the process dying in any way** (a crash, a trap, `kill -9`
  during a redeploy). Only a power loss or a kernel crash can lose the latest
  updates.
- A **suspect log**, `<program>.<pid>.suspects`, is a small append-only text
  file. The scheduler adds a line when a site **becomes** a suspect, when its
  oldest survivor gets older by another doubling, and when an epoch closes
  with leaks. These events are rare, so the log stays small.
- A tool reads both files from a dead or a live process:

  ```
  $ m31 heapwatch server.81234.ledger
  SUSPECT  Conn     net/server.m31:40    37 alive > 40m   (p99 lifetime 31s)
  SUSPECT  Session  app/session.m31:12    5 alive > 2h    (p99 lifetime 4m)
  forever  Config   app/config.m31:8      1 alive, never freed
  epoch "request" closed with leaks 112 times; last: 2 Conn @ net/server.m31:40
  ```

So after a redeploy, the old process's ledger is still on disk, and so is its
answer.

---

## Epoch close: exact cycles, at the moment they happen

Age tells you an object is **probably** leaked. Epoch close tells you it
**certainly** is, as soon as the unit of work that made it ends.

An **epoch** is a unit of work: every spawned task, plus an optional block
for work inside a long-lived task (`epoch "request" { handle(r); }`). It
should leave nothing behind except what it chose to keep.

| Event | Bookkeeping |
|---|---|
| allocate | DEPTH = the current epoch's depth; `epoch[depth][site] += 1` |
| free, object not KEPT | `epoch[depth][site] -= 1` |
| store `v` into holder `h` (field, element, map slot, global) | `h` is KEPT: **keep** `v`. `h.depth < v.depth`: **adopt** `v` into `h.depth`. Otherwise do nothing; this is the common case and costs one compare. |
| assign to a local declared outside the epoch block | adopt into that local's depth. The compiler knows statically which locals these are. |
| the block's result, or a task's return value | adopt into the enclosing depth |
| send / spawn capture | re-tag into the receiver's epoch, on the uniqueness walk the send already does |
| close | any nonzero `epoch[depth][site]` is a leak: it goes to the suspect log, then the row is cleared |

Keep and adopt spread to newer objects that are not kept, and stop at older
or kept ones. Depth only goes down and KEPT is set only once, so the cost
follows allocation, not heap size.

**Exact:** at close the epoch's locals are dead, and every legitimate way to
outlive the epoch has moved the object out of the count. Anything still
counted is held only by other dead objects of the same epoch. That is a
leaked cycle, with no false reports **provided the adoption list is
complete**. Every corpus program must close clean, which is how a missing
adoption rule gets caught.

---

## Does it cover everything?

| Leak | Caught by | When |
|---|---|---|
| Cycle among one unit of work's throwaway objects | epoch close, **exact** | the moment the unit ends |
| Cycle formed later between kept, long-lived objects | age vs the site's own lifetime | after it has aged past the threshold |
| Retained by mistake (a cache never cleared), not a cycle | age, or a "never freed" count that keeps growing | as it grows |
| Leak inside an epoch that never closes (the main loop) | age | after the threshold |
| Leaked cycle holding a resource (a socket, a file) | same rows; the site names the type | as above |
| A crash or `kill -9` before any of that | the ledger is already on disk | always |

**What it does not do:**

- **Prove that nothing leaks.** It reports what happened in the runs that
  happened. Code paths that never ran report nothing.
- **Say who holds the leaked object.** It names the type, the allocation
  site, the age and the unit of work, but not the path of references that
  keeps it alive. Finding that path is exactly the heap walk ruled out above.
  The site, plus which epoch leaked, is meant to be enough to find it in the
  code.
- **Flag a young leak.** An age suspect needs time to age. Epoch close has no
  such delay, but only for objects that were never kept.
- **Tell apart objects that live long on purpose from those that leaked,
  when a site has no lifetime baseline yet.** Those show as "never freed" for
  the developer to judge.

Within those limits this is one tool, one file and one report for every
object in the heap.

---

## Pros and cons

**Pros**

- **Every object, 0 bytes each.** All the state is in counters, plus a
  header that is already there.
- **No heap walk and no collection.** The cost per operation is bounded, with
  no pauses and no dependence on heap size.
- **Survives crashes and redeploys.** A memory-mapped ledger plus an
  append-only suspect log.
- **Learns its own thresholds.** Each site's lifetime histogram is the
  baseline, so no configuration is needed to start.
- **Exact where it can be.** Epoch close never reports a false leak.
- **Points at the fix.** Type, file and line, age, unit of work.
- **Same program behaviour either way.** Turning it off changes only what is
  written to the files.

**Cons**

- **Store sites gain a branch.** It is one predictable compare per reference
  store.
- **Adoption rules must stay complete.** Closures and any future feature that
  lets a value escape need an entry, or false epoch reports appear (the
  corpus catches them).
- **A per-site `TypeInfo`** means runtime type checks go through `canon`, and
  the emitted static data grows.
- **The count drops from 60 bits to 33.** It is still 8.6 billion, and an
  overflow traps rather than corrupting.
- **Age suspects need time**, and a site with no baseline needs a human to
  judge it.
- **Disk:** one ledger file per process. Ledgers from old pids need a
  retention rule (keep the last N).
- **The spreading walk** can move a large new graph in one store, for example
  building a big list and then caching it. The cost is amortised, but it
  shows up as a spike.

---

## Cost (estimates; measure in qrazil/benchmark before deciding)

For scale: a `malloc`/`free` pair is roughly 20–50 ns.

### Memory

| Item | Size |
|---|---|
| per object | **0 B** (spare bits in the count word) |
| per allocation site, static | ~100 B `TypeInfo` copy. 5 000 sites ≈ 500 KB of binary size. |
| per site, `lifetime` histogram | 16 × 4 B = 64 B. 5 000 sites ≈ 320 KB. |
| `live[site][minute]` | dense row for the current minute (sites × 4 B ≈ 20 KB); older minutes sparse, bounded by the number of distinct (site, minute) pairs that still have survivors. Typically tens of KB, and at most 1 entry per live object. |
| `epoch` rows | sites × 4 B per open depth per thread, allocated lazily; ≈ 20 KB × 1–3 open per task |
| ledger file on disk | the above, mapped: typically 1–5 MB per process |

### CPU

| Operation | Added cost |
|---|---|
| allocate | 3 counter increments (live, epoch, site lookup via `ty`): **~2–3 ns** |
| free | 2 decrements plus 1 histogram increment (one `clz` for log2): **~3–5 ns**. The sparse `live` lookup for an older minute is a small hash probe. |
| reference store | 1 compare, usually not taken: **~0.3–1 ns**, plus the spreading walk when it adopts (amortised) |
| `rc_inc` / `rc_dec` | unchanged. The count mask widens; overflow is checked by a compare `rc_inc` can fold in. |
| epoch enter / close | tens of ns |
| scheduler tick (~1/s) | advance the minute; once a minute, check each site's oldest survivor against its threshold: **O(sites)**, never O(objects) |
| disk | none on the hot path (mmap); suspect log appends are rare |

**Overall guess:** about **3–7 %** on code that allocates heavily and stores
many references, and close to 0 on computation-heavy code. That is cheap
enough to propose it **on by default in every build**, release included,
because a server is exactly where it is needed. The benchmark must confirm
this before anything is decided.

---

## Open questions

1. On by default in release builds, or behind `M31_HEAPWATCH=1`? The
   requirement argues for default on.
2. Explicit `epoch` blocks: the keyword and its spelling, or only spawned
   tasks at first?
3. Suspect thresholds: is `F = 10` × p99 with a 10-minute floor right?
4. Retention for ledgers of dead pids: keep the last N, or N days?
5. Test builds: should an epoch that closes with leaks **trap**, so CI fails
   on any leaked cycle?
6. Should a stdlib API expose the ledger, so a server can serve it as
   metrics (such as `/debug/heap`)?
