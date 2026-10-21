# Heap watch: one ledger for every object

Status: **approved; being built.** Revision 5, 2026-10-21. Phase 1, the
ledger, and phase 2, trace, are built; the ledger is on by default and trace
on request ("As built" under each part says what differs from the
proposal). Epoch close is next.

Revision 5: phase 2 built. Trace is turned on and off by
`m31c -heapwatch --trace` through the ledger's control page, records edges
on a store hook shared with epoch close, and prints paths to a root, a
cycle or the depth limit.

Revision 4: phase 1 built. The count word has a bit fewer than proposed
(`RC_HELD` arrived with the deep freeze in 0.10.0), the per-minute table
became a timing wheel, the tool is `m31c -heapwatch` in the CLI grammar, and
the open questions it had to answer are answered at the end.

Revision 3: the owner confirmed all three parts are wanted -- the age
ledger, epoch close and trace mode -- and asked for a way to mark objects
that live long on purpose (a game engine's assets and entities). Added:
"Three parts, one ledger", "Long-lived on purpose", "Trace: who holds it",
"Fixing a leak", "Walk-through: a game engine", "Feasibility" and "Phases".

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
   since when" -- and, on request, **who is holding it**.
4. **Long-lived on purpose is not a leak.** A developer can say, in code or
   in configuration, that an object or a type lives as long as it likes.
5. **Trace on the fly.** Who holds a leaked type can be asked of a running
   process, by an environment variable at start or a command at any time,
   with no rebuild and no restart.

---

## Three parts, one ledger

| Part | Question it answers | When it answers | Always on? |
|---|---|---|---|
| **Age ledger** | which site is leaking, how many, how old | once survivors outlive the site's normal lifetime | yes |
| **Epoch close** | this unit of work certainly leaked, and what | the moment the unit ends | yes |
| **Trace** | who holds the leaked object, up to a root | from the moment it is turned on | no, per type, on request |

They overlap only where it helps: a leaked cycle inside a request shows up
at once in epoch close, and later in the age ledger as well. Neither of the
first two can say who holds an object; trace cannot say what to look at
without them. All three write to the same ledger and are read by the same
tool, `m31c -heapwatch`.

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

The header stays two words (`runtime/rt.h`). Since 0.10.0's deep freeze added
`RC_HELD`, the count word has 59 bits below its flags, and no program needs
anywhere near that many. Heap watch splits them up:

```
 63    62      61..60   59    58..53    52..33         32..0
[sign][FROZEN][BUSY x2][HELD][EPOCH 6][BIRTH 20 bits][COUNT 33 bits]
```

- **COUNT**, 33 bits, allows 8 589 934 591 references to one object. A
  retain past that traps ("reference count overflow") instead of carrying
  into BIRTH; `rc_inc` tests for it with one compare.
- **BIRTH** is the minute the object was made, counted from program start.
  20 bits is 728 days. A process that lives longer stamps every later object
  with the last minute it can count, which reads as old -- the right side to
  err on for a leak report. All ones marks an object `heapwatch.forever`.
- **EPOCH** is for epoch close (below). The proposal had a 6-bit DEPTH and a
  KEPT bit; as built they are one 6-bit field, `62 - depth`, with 63 for an
  object counted against no epoch and 0 for one crossing between tasks.
  "Outlives more epochs" is then simply "larger", so a store compares two
  numbers, and the bit saved is what keeps COUNT at 33 bits beside `HELD`.

`rc_dec`, the uniqueness check, the snapshot and the emitted destructor
read the count through a mask, `RC_COUNT`; nothing reads or writes the count
word raw (`rt_rc_count`, `rt_rc_set_count`), and a runtime test checks that
a retain and a release leave every bit above the count alone.

The allocation site, meaning the file and line of the constructor or the
collection literal, comes from the type pointer. **Each allocation site gets
its own `TypeInfo`.** It is a copy of the type's record with the same drop,
walk and method table, plus a site id and a `canon` pointer to the real type.
That costs static data only, about 100 B per site and nothing per object.
Type identity checks in the runtime (`o->ty == &rt_lst_ref_type`, and a few
others) switch to `o->ty->canon`.

As built:

- A site is a **(type, source line)** pair: the line of the expression that
  makes the object (the statement's, unless the expression has a line of
  its own). The lowering puts an `Inst::Site` before each allocation in a
  block whose line differs from the last one; the emitter turns each user
  type's (type, line) pair into a static `ti_T<n>_s<slot>` and numbers the
  slots 1, 2, ... in emission order, so the C is the same on every run.
- The runtime's own objects -- a `str` from `concat`, a `List` from a
  literal, the `bytes` a primitive pushes -- are made inside the runtime,
  from a canonical record. Emitted code calls `rt_hw_at(line)` before each
  runtime call that may allocate (`ir::rt_call_allocates` lists the ones
  that cannot), and `rt_alloc` swaps the canonical record for the line's own,
  made the first time that line allocates that type and published once. A
  thread caches its last one, so a loop pays a compare.
- The file of a site is the entry file by its own name, a module of the
  program by its path from the entry file's directory (`ui/panel.m31`), and
  a module of the standard library as `net/http.m31` -- never a path on the
  compiling machine, so the emitted C is the same everywhere.
- TypeInfo grew `canon`, `site`, `loc`, `name` and a table of reference
  fields (name, offset, variant tag) for trace. The 18 identity checks in
  `runtime/` were converted; a gate (`no raw type identity checks`) refuses
  a comparison of `ty` itself with a type's address.

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

As built (`runtime/heapwatch.c`):

- **`live` is a timing wheel, not a sparse table.** Per site, 7 levels of 8
  cells: level 0 holds the last 8 minutes one by one, level L cells of 8^L
  minutes, and level 6 reaches past the 20-bit birth field. When a shard's
  clock moves on, a cell about to be reused hands its count up a level --
  O(sites) once a minute, never O(objects). It answers "how old are the
  survivors" to within a factor of 8 for old objects and exactly for recent
  ones, in 224 bytes per site and shard, with nothing to grow or reclaim.
- **`lifetime`** is 16 buckets of an object's age at death in minutes: 0 for
  under a minute, then `floor(log2 age) + 1`. A site's p99 is the upper
  bound of the bucket where 99 % of its deaths are reached.
- **Shards** are per OS thread (a carrier runs many green threads), each
  written only by its owner with plain loads and stores (relaxed atomics,
  which compile to exactly that). An object freed on another shard is
  subtracted there at whatever level that shard holds its birth minute; the
  reader merges each cell into the coarser block of any shard that contains
  it, so the +1 and the -1 always meet (`hw_sum_slot`, and the same in
  `src/tools/heapwatch.rs`). Shards are carriers + 8; a thread past the last
  shares it under a lock.
- **The tick** is the scheduler's monitor thread, which every compiled
  program already runs and which wakes every 5 ms: it advances the minute,
  stamps the ledger's wall clock once a second, and scans for suspects once a
  minute. `M31_HEAPWATCH_MINUTE_MS` shortens the minute for tests.

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
  `M31_AGE=Conn:10m,Config:forever`. See "Long-lived on purpose" for the
  full set of ways to say an object is meant to live.

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
  $ m31c -heapwatch server.81234.ledger
  server (pid 81234), not running -- it crashed or was killed; last seen 3m ago; up 2h
  SUSPECT  Conn     net/server.m31:40    37 alive > 40m (limit 10m, p99 lifetime <1m)
  SUSPECT  Session  app/session.m31:12    5 alive > 2h (limit 40m, p99 lifetime 4m)
  never    Cache    app/cache.m31:5      12 alive, never freed, oldest > 2h
  forever  Config   app/config.m31:8      1 alive
  epoch "request" closed with leaks 112 times; last at minute 118: 2 Conn @ net/server.m31:40
  ```

So after a redeploy, the old process's ledger is still on disk, and so is its
answer.

As built:

- The tool is a command of the compiler, `m31c -heapwatch <ledger|pid>
  [--all]`, in the CLI grammar every m31 program shares
  (docs/cli-conventions.md: `-word` is a command, so `m31 heapwatch` became
  `m31c -heapwatch`). Given a pid it looks in `$M31_HEAPWATCH_DIR` and in
  every `m31-heapwatch-*` under the temporary directory. It says whether
  the process is running, exited cleanly, or died (`kill -0`), and `--all`
  lists every site with objects, not only the ones worth a look. Its
  arithmetic is runtime/heapwatch.c's, on plain file reads.
- The directory is `$M31_HEAPWATCH_DIR`, else `${TMPDIR:-/tmp}/m31-heapwatch-
  <uid>` (mode 0700). A directory the program cannot write means a ledger
  in anonymous memory: everything is still counted, there is just nothing
  to read later.
- The file is laid out as a control page, a header, a slot table, the
  trace flags, a string area, an epoch table and the shards; the header
  ends with a magic number written last, and carries a version the tool
  checks. Its size is mostly sparse: ~20 MB apparent for a program with a
  few hundred sites on 12 cores, ~20-100 KB actually written.
- **A clean exit with nothing reported removes the files** (an atexit
  handler, which a trap, a signal or `kill -9` never reach -- exactly when
  they must stay). `M31_HEAPWATCH_KEEP=1` keeps them anyway. At start, a
  program removes the ledgers of dead processes of the same name beyond the
  newest 8 (open question 4).
- Everything above is mapped `MAP_SHARED` and written with plain stores; no
  exit hook is needed for the counts to be there after `kill -9`, which
  `runtime/tests/heapwatch_test.sh` checks.

### Long-lived on purpose

Some programs are mostly long-lived objects: a game engine's textures,
meshes and world, a server's connection pool. Age alone would flag them, or
leave them in the "never freed" list for a human to wave through every time.
There are three ways to say "this lives as long as it likes", from the
broadest to the narrowest, and all of them cost nothing per operation:

| Scope | How | Effect |
|---|---|---|
| a type | `M31_AGE=Texture:forever` (environment or the config file `M31_HEAPWATCH_CONF`) | every site of that type is exempt from age |
| a site | `M31_AGE=Texture@assets.m31:40:forever`, or a duration instead of `forever` | that site only; a duration replaces the learned threshold |
| one object | `heapwatch.forever(x)` in code, which returns `x` | that object only |

`heapwatch.forever(x)` moves the object from its birth-minute bucket to a
**forever** bucket by setting `BIRTH` to a reserved value (all ones), so it
needs no new bit. It is still counted, still listed (`forever  Texture
assets.m31:40  812 alive`), and still checked by epoch close and trace --
being marked long-lived exempts it from age and nothing else. A forever
object that is freed is simply uncounted; that is not an error.

As built: `lib/heapwatch.m31` declares `pub T forever<T>(T value)`, which
calls a builtin only the standard library can name, `__forever` -- a `prim`
cannot be generic (docs/stdlib-seam.md §2) -- and returns its argument; a
value that is not an object (an int, a value enum) passes through untouched.
`M31_AGE` entries are separated by commas; the file named by
`M31_HEAPWATCH_CONF` holds one per line, with `#` comments. A duration is a
number and `s`, `m`, `h` or `d`, rounded up to the minute; the most specific
entry wins (a site's over its type's). A malformed entry is reported once on
stderr and ignored. A type is named as the program spells it (`Conn`,
`net.Conn`), and an imported type by its bare name too; a site as
`Type@file.m31:line`, the file matching the ledger's or its last path
components.

A level, a scene or a match that *ends* is better described as an epoch than
as an age: `epoch "level" { play(level); }` reports everything the level
made and did not keep, the moment the level is unloaded (see the game engine
walk-through).

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
- **Say who holds the leaked object, by itself.** The ledger names the type,
  the allocation site, the age and the unit of work. The path of references
  that keeps it alive is trace's job, below, and trace has to be turned on
  for the type first.
- **Flag a young leak.** An age suspect needs time to age. Epoch close has no
  such delay, but only for objects that were never kept.
- **Tell apart objects that live long on purpose from those that leaked,
  when a site has no lifetime baseline yet.** Those show as "never freed" for
  the developer to judge.

Within those limits this is one tool, one file and one report for every
object in the heap.

---

## Trace: who holds it

The ledger says *what* leaked. Trace says *who is holding it*, without a heap
walk: it records reference edges **as they are made and broken**, for the
types it is asked to watch, and nothing else.

### Turning it on

- At start: `M31_HEAPWATCH_TRACE=Conn,Session@app/session.m31:12`.
- On a running process: `m31c -heapwatch <pid> --trace Conn` (and `--untrace`).
  The tool writes the request into a **control page** at the start of the
  process's ledger file. The scheduler tick reads that page once a second --
  one load -- and applies it. No signal, no restart, no rebuild.
- The trace set is a per-site byte array `traced[site]` in the ledger, not
  a flag in the read-only `TypeInfo`.

### What it records

Every reference store already knows the holder (the object or global being
written into) and the value. With trace on, the store path does one extra
load and compare -- `traced[v->ty->site]` -- next to the `rc_inc` that
already touches `v`'s header. When it is set, the slow path updates an
**edge table**:

| Event | Edge table |
|---|---|
| store `v` into slot `h.f` (field, element, map value, global) | add `v ← h.f` |
| overwrite or clear that slot | remove the old value's edge |
| holder `h` freed (its drop releases its children) | remove every edge from `h` |
| `v` freed | remove `v`'s row |

The field name is not stored at the call site: the slow path computes it
from the slot's offset in the holder and the holder's `TypeInfo` field table.
Collection internals (a List's element buffer, a Map's buckets) are recorded
against the collection object, which is the holder a reader cares about.

**Locals are not recorded, and do not need to be.** A leaked object outlives
every frame that made it, so whatever keeps it alive is a heap object or a
global -- exactly what trace sees. An object held only by a local is not a
leak.

### Expanding to a root

One edge says "Conn is held by `Session.conn`". That is often not enough: the
question is who keeps the Session. With `--depth N` (default 3), when a
traced object becomes a suspect or is left at an epoch close, trace turns
itself on for the **holder's site** too, and so on up to N levels. The
answer grows into a path that ends in one of:

- a **global or module-level holder**, which is a retained-by-mistake leak
  (a cache never cleared);
- a **cycle** -- the path comes back to an object already on it -- which is
  a leaked cycle, printed as the cycle;
- an **untraced holder** beyond the depth limit, printed as such.

### Its limit

Trace sees only edges made **after** it was turned on. An object leaked an
hour earlier, whose holder stored it an hour earlier, has no recorded edge.
In practice a leak repeats -- the site that leaked once leaks again -- so the
next occurrence is caught. `M31_HEAPWATCH_TRACE` at start avoids the wait
when the type is already known.

### Output

Appended to `<program>.<pid>.trace`, read by the same tool:

```
$ m31c -heapwatch server.81234 --paths
Conn  net/server.m31:40  (37 alive > 40m)
  held by  Session.conn      app/session.m31:12
  held by  Map value         app/cache.m31:5   SessionCache.by_id   (global)
  -> retained: SessionCache.by_id never removes closed sessions
Node  graph/build.m31:22  (epoch "request" left 2)
  held by  Node.next  graph/build.m31:22
  held by  Node.prev  graph/build.m31:22  <- cycle
```

### As built

- **Turning it on.** `M31_HEAPWATCH_TRACE=Conn,Session@app/session.m31:12` at
  start, or a `trace Conn` line in the `M31_HEAPWATCH_CONF` file; on a running
  process, `m31c -heapwatch <pid|ledger> --trace Conn Session` and `--untrace`,
  with `--depth N` (`M31_HEAPWATCH_DEPTH` at start, default 3). A type is
  traced at every site it has and every site it gets later.
- **The control page** is the ledger's first 4096 bytes: requests (trace,
  untrace, dump), then a sequence number the tool moves last; the tick
  compares it with the one it last applied, once a second, and the tool
  waits until the process has. The tool writes with plain file writes, not a
  mapping (the compiler has no `unsafe` and no dependencies), so two tools
  are kept apart by a lock file made with `create_new` beside the ledger; the
  page's lock word records which tool holds it. **Who may ask is whoever may
  write the ledger**: it is 0644 in a 0700 directory, so its owner (open
  question 9).
- **The store hook.** The emitter puts `rt_hw_store(h, v, offset)` before
  every store of a reference into a field -- construction included -- and
  `rt_hw_store_elem(o, i, v)` before every `o[i] = v` of one; the runtime's
  own stores (push, insert, a map's key and value, an array literal's
  elements) call the same slow path, and a clone, a slice or a copy for a
  `const` records what it holds when made. Its fast path is the test epoch
  close needs anyway -- `h`'s epoch field greater than `v`'s, or trace on for
  anything -- so with trace off a store costs two header loads (both
  already in a register or the cache: the frozen check read `h`, `rc_inc`
  wrote `v`), a load of a global and an untaken branch.
- **The edge table** is kept in memory, under one lock, and only ever holds
  what stores told it: a field's edge is keyed by (holder, offset) -- a
  field holds one value, so a store replaces the old value's edge without
  knowing it -- and a collection's by (holder, element) with a count, since a
  List may hold one object twice. Each edge is on its holder's list and its
  value's list, so the release of either removes it in time proportional to
  its own edges. `pop`, `remove_at`, `remove`, `clear` and an overwritten
  element or map value take theirs out. Past the cap (`M31_HEAPWATCH_EDGES`,
  default 1 048 576) new edges are dropped and counted, and the report says
  so (open question 8: the default depth 3 and cap 1 M are kept).
- **Field names** come from the holder's site record: a struct's reference
  fields by offset, an enum's payload slots by offset and variant tag; a
  collection's are `List element`, `Map key`, `Map value`.
- **Reports** go to `<program>.<pid>.trace`: when a traced site becomes a
  suspect or doubles its age again (its objects older than the limit), when
  an epoch closes leaving objects of a traced site (those objects), and when
  the tool asks (`--paths` on a running process: every traced object).
  Objects with the same path are printed once with their number. A path
  follows holders that are traced themselves, up to `--depth` levels; a
  holder that is not is traced from then on and named as such, and the next
  report goes further. It ends in a cycle, `<- cycle`; in "no recorded
  holder: a local, or stored before trace began"; or in an untraced holder
  past the depth.
- **There are no globals in m31** (docs/module-state-decision.md: no mutable
  module state), so the doc's "global or module-level holder" is, in m31, an
  object a local holds: the path ends there, at "no recorded holder".

```
$ m31c -heapwatch 81234 --trace Connection Session Map --depth 5
trace on for Connection in pid 81234
...
$ m31c -heapwatch 81234 --paths
2026-10-21T10:38:51Z Connection  t_trace.m31:29  (suspect: alive > 20m)
  18 objects:
  held by  Session.connection       t_trace.m31:29
    held by  Map value                t_trace.m31:26
      held by  Cache.by_number          t_trace.m31:26
        (no recorded holder: a local, or stored before trace began)
2026-10-21T10:38:51Z Node  t_trace.m31:33  (suspect: alive > 20m)
  19 objects:
  held by  List element             t_trace.m31:34
    held by  Node.links               t_trace.m31:34
      held by  List element             t_trace.m31:33
        held by  Node.links               t_trace.m31:33  <- cycle
```

---

## Fixing a leak

1. **See it.** `m31c -heapwatch <ledger>` on the live process, or on the dead
   one's ledger after a crash or a redeploy: a suspect site, or an epoch
   that closed with leaks.
2. **Judge it.** Long-lived on purpose? Mark it (`M31_AGE`,
   `heapwatch.forever`) and it stops being reported. Otherwise it is a bug.
3. **Find the holder.** `m31c -heapwatch <pid> --trace <Type>` on the running
   process, or `M31_HEAPWATCH_TRACE=<Type>` on the next start. Wait for the
   next occurrence; read the path.
4. **Fix it**, by what the path ends in:
   - a cycle among throwaway objects: break it, or put the structure in an
     `Arena<T>` if it is cyclic on purpose;
   - something that should have left the epoch: **return** it, **pass** it
     (send it to the task that owns it), or **clone** what is kept, so the
     unit of work holds nothing at its end;
   - a holder that never lets go: remove the entry when its owner ends.
5. **Prove it.** In tests, an epoch that closes with leaks traps, so the
   fix gets a test that fails without it.

---

## Walk-through: a game engine

A game loop is the hard case for age: most of what matters lives a long
time on purpose.

```c
Texture t = heapwatch.forever(load_texture("grass.png"));   // assets: forever

while running {
    epoch "level" {                      // everything a level made...
        Level lv = load_level(n);
        while !lv.done {
            epoch "frame" {              // ...and everything a frame made
                tick(lv, input());
                draw(lv);
            }
        }
    }                                    // ...is checked when it ends
}
```

- **Per-frame temporaries** that leak are reported at the end of the frame
  that leaked them. Sixty epoch closes a second cost a few microseconds in
  total.
- **Entities** that live for the level are adopted into the level epoch when
  they are stored in the level, and checked when the level unloads: a
  monster that is still held after its level is gone is reported then, with
  no age threshold to tune.
- **Assets** loaded once are marked forever and drop out of the age report;
  a texture cache keyed by name can be marked by type (`M31_AGE=Texture:forever`).
- **The world graph** -- parents and children that point at each other -- is
  cyclic on purpose and belongs in an `Arena<T>`; the arena is one object to
  the ledger, and its handles are integers that hold nothing.
- **A leak found in play testing** gets `M31_HEAPWATCH_TRACE=Monster` on the
  next run, or `m31c -heapwatch <pid> --trace Monster` while the game is open.

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
| trace edge table | only while tracing: ~32 B per edge to a traced object, capped (default 1 M edges ≈ 32 MB; past the cap new edges are dropped and the report says so) |
| per-thread shards | each OS thread keeps its own counters so updates stay non-atomic; the tool sums them. An object made on one thread and freed on another leaves +1 on one shard and −1 on the other, which sums correctly. |

### CPU

| Operation | Added cost |
|---|---|
| allocate | 3 counter increments (live, epoch, site lookup via `ty`): **~2–3 ns** |
| free | 2 decrements plus 1 histogram increment (one `clz` for log2): **~3–5 ns**. The sparse `live` lookup for an older minute is a small hash probe. |
| reference store | 1 compare, usually not taken: **~0.3–1 ns**, plus the spreading walk when it adopts (amortised) |
| `rc_inc` / `rc_dec` | unchanged. The count mask widens; overflow is checked by a compare `rc_inc` can fold in. |
| epoch enter / close | tens of ns |
| trace off (the default) | one load and compare on each reference store, next to the `rc_inc` that already touches `v`: **~0.3 ns** |
| trace on, store of a traced value | an edge-table insert (a hash probe): **~20–50 ns**, for that type only |
| trace on, overwrite or free of a traced value | an edge-table remove: **~20–50 ns** |
| `heapwatch.forever(x)` | a counter move: **~2 ns**, once per object |
| control page | one load per scheduler tick |
| scheduler tick (~1/s) | advance the minute; once a minute, check each site's oldest survivor against its threshold: **O(sites)**, never O(objects) |
| disk | none on the hot path (mmap); suspect log appends are rare |

**Overall guess:** about **3–7 %** on code that allocates heavily and stores
many references, and close to 0 on computation-heavy code. That is cheap
enough to propose it **on by default in every build**, release included,
because a server is exactly where it is needed. The benchmark must confirm
this before anything is decided.

---

## Feasibility

| Piece | Feasible? | Main risk | How it is caught |
|---|---|---|---|
| Count word split (60 → 33 bits + BIRTH, DEPTH, KEPT) | yes; `rc_inc`/`rc_dec` are out-of-line functions in `runtime/rt.h`, so the mask changes in one place | a path that reads the count word raw | sanitizers + an RC_DEBUG check that the flag bits survive every operation |
| Per-site `TypeInfo` with `canon` | yes; the compiler already emits one `TypeInfo` per type | the 18 `->ty ==` identity checks in `runtime/` must all go through `canon` | a gate that greps for a raw `->ty ==` |
| `live` / `lifetime` counters, mmap ledger, suspect log | yes; plain memory writes, one file per process | thread shards and file layout versioning | corpus run with the ledger read back by the tool |
| Forever marks | yes; a reserved `BIRTH` value, no new bit | none notable | corpus |
| Epoch close | yes, but the **hardest** part | an incomplete adoption list gives false reports (closures, sends, globals, future escape routes) | every corpus program must close clean; a false report fails the gate |
| Trace | yes; event-based, no heap walk | holders inside runtime collections must be attributed to the collection; edge table memory | corpus programs with a known holder path, checked against the trace output |
| On-the-fly control page | yes; the tool writes the page, the tick reads it | two tools writing at once | a lock word in the page |
| Green threads (in progress) | yes; shards become per worker thread, not per task | task migration between workers | same summing rule as OS threads |

Nothing here needs a heap walk, a pause, or a change to program behaviour.
The real work is in two places: the epoch adoption rules, and auditing the
runtime for raw type checks and raw count reads.

## Phases

1. **Ledger**: count split, per-site `TypeInfo`, `live`/`lifetime`, mmap
   ledger, suspect log, `m31c -heapwatch`, forever marks and `M31_AGE`. Useful
   on its own.
2. **Trace**: edge table, control page, `--depth`, `.trace` output. Needs
   the per-site ids from phase 1, not epochs.
3. **Epoch close**: spawned tasks first, then `epoch` blocks, then the
   test-build trap. Last because its adoption rules are the riskiest part.

Each phase is measured in qrazil/benchmark before the next starts.

## Configuration

Everything is read once, at start, from the environment; nothing needs a
rebuild.

| Variable | Default | Effect |
|---|---|---|
| `M31_HEAPWATCH` | on | `0` turns heap watch off: nothing counted, no file |
| `M31_HEAPWATCH_DIR` | `${TMPDIR:-/tmp}/m31-heapwatch-<uid>` | where the ledger, `.suspects` and `.trace` go |
| `M31_HEAPWATCH_KEEP` | off | keep the files after a clean exit with nothing to report |
| `M31_HEAPWATCH_MIN` | 10 | the suspect floor, in minutes |
| `M31_HEAPWATCH_FACTOR` | 10 | a suspect is older than FACTOR x the site's p99 lifetime |
| `M31_AGE` | none | `Type:forever`, `Type:10m`, `Type@file.m31:40:forever`, comma-separated |
| `M31_HEAPWATCH_CONF` | none | a file of `M31_AGE` entries, one per line, `#` comments |
| `M31_HEAPWATCH_MINUTE_MS` | 60000 | how long a minute is -- for tests, so "10 minutes" passes in a second |
| `M31_HEAPWATCH_TRACE` | none | trace these types from the start: `Type`, `Type@file.m31:40`, comma-separated |
| `M31_HEAPWATCH_DEPTH` | 3 | how many holders up a trace path follows (and turns trace on for) |
| `M31_HEAPWATCH_EDGES` | 1048576 | the trace edge table's cap; edges past it are dropped and counted |

## Open questions

Answered while building (each the simplest sound option; the owner may
overrule any of them):

1. **On by default.** Every compiled program counts, release builds
   included, and `M31_HEAPWATCH=0` turns it off. The cost section has the
   measurement this rests on.
3. **`F = 10` and a 10-minute floor**, both configurable
   (`M31_HEAPWATCH_FACTOR`, `M31_HEAPWATCH_MIN`). A site with no deaths has no
   baseline and is "never freed", not a suspect, unless `M31_AGE` gives it a
   limit.
4. **Retention: a clean exit with nothing reported leaves nothing**, and at
   start a program removes ledgers of dead processes of its own name beyond
   the newest 8. Days were rejected: a crash loop makes many ledgers in
   minutes, and a count bounds the disk where an age does not.
7. **`heapwatch.forever(x)`, a stdlib call**, generic, returning `x`. A
   keyword on the allocation would be grammar for something configuration
   already does by type and by site.
8. **Trace `--depth` 3 and an edge cap of 1 M**, as proposed, both
   configurable (`M31_HEAPWATCH_DEPTH`, `M31_HEAPWATCH_EDGES`, `--depth`).
9. **Whoever can write the ledger can turn trace on.** The file is 0644 in a
   0700 directory, so that is its owner; a separate check would be a second
   permission system for the one file that already has one.

Still open:

2. Explicit `epoch` blocks: the keyword and its spelling, or only spawned
   tasks at first?
5. Test builds: should an epoch that closes with leaks **trap**, so CI fails
   on any leaked cycle?
6. Should a stdlib API expose the ledger, so a server can serve it as
   metrics (such as `/debug/heap`)?
