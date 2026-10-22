# Heap watch: one ledger for every object

Status: **approved and built.** Revision 7, 2026-10-21. All three parts
are built, in a **heap watch build** only: a program and runtime compiled
with `-DRT_HEAPWATCH` (`./build.sh prog.m31 --heapwatch`). The default
build has none of it. In a heap watch build the ledger is on, by sampling;
epoch close is on with `M31_HEAPWATCH=full`; trace is on request. "As built"
under each part says what differs from the proposal, and the cost section
has measurements in place of the estimates.

Revision 7: the cost. Revision 6 measured +16 % to +68 % on allocation-bound
benchmarks with heap watch on, and up to +20 % with it off, which could not
be merged. Now: heap watch is compiled in only on request ("Builds"), so
the default build is the code it was before heap watch; the ledger samples
objects by bytes allocated, as tcmalloc's and heapprofd's profilers do,
rather than counting every one; the per-object state left the count word's
BIRTH field and the per-site records of the runtime's own types, which took
`canon` and the 33-bit count with them; and epoch close counts the objects
left alive per epoch, naming the sites of sampled ones. "Cost" has the new
numbers.

Revision 6: phase 3 built. Every task is an epoch; blocks are the library
calls `heapwatch.epoch` and `heapwatch.epoch_of` rather than a keyword; a
test build traps on a leaky close, which makes every corpus program closing
clean a gate.

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

Since revision 7 these hold for a heap watch build, which a program has to
be built as ("Builds"): the owner ruled out the cost of having them in every
program (2026-10-21). Requirement 1 holds by sampling, exactly with
`M31_HEAPWATCH_SAMPLE=1`.

---

## Three parts, one ledger

| Part | Question it answers | When it answers | In a heap watch build |
|---|---|---|---|
| **Age ledger** | which site is leaking, how many, how old | once survivors outlive the site's normal lifetime | on (sampled) |
| **Epoch close** | this unit of work certainly leaked, and what | the moment the unit ends | `M31_HEAPWATCH=full`; on in a test build |
| **Trace** | who holds the leaked object, up to a root | from the moment it is turned on | per type, on request |

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
- **Nothing in a build that did not ask for it** (revision 7). See "Builds".

---

## Builds

Heap watch is in a program only when the program and its runtime are both
compiled with `-DRT_HEAPWATCH`: `./build.sh prog.m31 --heapwatch`, or the
flag on both `cc` lines. Without it -- the default -- the emitted C names
every hook as a macro that is nothing (`runtime/rt.h`, `RT_HEAPWATCH`), the
runtime's hooks are `#if`'d out, `heapwatch.c` is not compiled, and the count
word is the count and the flags exactly as before heap watch. A gate (`heap watch
only if asked`) checks that a default build of the
runtime has no heap watch symbol and that a program using
`lib/heapwatch.m31` has no heap watch call once preprocessed; with the flag,
both are there. `rc_inc`, `rc_dec`, `rt_alloc` and the runtime's stores
compile to the same machine code as before heap watch.

The two halves must agree. A program built with the flag and a runtime
built without it fail to link (`rt_hw_program`); a program built without
it and a runtime built with it run as if heap watch were off.

`lib/heapwatch.m31` works in both: `heapwatch.forever(x)` returns `x`, and
`heapwatch.epoch(name, body)` runs `body`, with or without the books.

A heap watch build runs in one of three modes, chosen at start by
`M31_HEAPWATCH`:

| Mode | What it does | Cost |
|---|---|---|
| `ledger` (default) | the sampled age ledger, the suspect log; trace on request | see "Cost": a few percent |
| `full` (default in a test build, `-DRC_DEBUG`) | the ledger, and epoch close for every task and block | see "Cost" |
| `0` | nothing counted, no file | the hooks' tests alone |

The corpus runs every program both ways (`scripts/run.sh`): its `-O0`
builds are heap watch builds, in a test build -- epoch close on, every
object sampled, a leaked cycle a trap -- and its `-O2` builds are default
builds, which must print the same. A test whose point is heap watch itself
has a `.heapwatch` file beside it and is a heap watch build every time.
`scripts/sanitize.sh` builds every other program with heap watch.

---

## The ledger

The idea is to keep the books, not count the objects. The runtime never looks
at objects. It keeps **counters**, and every allocation and every free updates
them. The counters alone answer every question.

### What each object carries: nothing new

The header stays two words (`runtime/rt.h`). Since 0.10.0's deep freeze added
`RC_HELD`, the count word has 59 bits below its flags, and no program needs
anywhere near that many. A heap watch build takes eight of them:

```
 63    62      61..60   59    58     57..52    51        50..0
[sign][FROZEN][BUSY x2][HELD][NOTE][EPOCH 6][SAMPLED][COUNT 51 bits]
```

- **COUNT**, 51 bits: 2^51 references would need 2^54 bytes of references,
  so, like the 59-bit count of the default build, it cannot overflow, and
  `rc_inc` does not check.
- **NOTE**: the object's release has something to tell heap watch -- it is
  sampled, counted by epoch close, or named by one of trace's edges. Every
  other release tests this bit and nothing more.
- **EPOCH** is for epoch close (below). The proposal had a 6-bit DEPTH and a
  KEPT bit; as built they are one 6-bit field, `62 - depth`, with 63 for an
  object counted against no epoch and 0 for one crossing between tasks.
  "Outlives more epochs" is then simply "larger", so a store compares two
  numbers. 0 everywhere when epoch close is off.
- **SAMPLED**: the ledger sampled this object (below); its site and birth
  minute are in the table of samples, keyed by its address.

`rc_dec`, the uniqueness check, the snapshot and the emitted destructor
read the count through a mask, `RC_COUNT`; nothing reads or writes the count
word raw (`rt_rc_count`, `rt_rc_set_count`), and a runtime test checks that
a retain and a release leave every bit above the count alone. In the
default build `RC_COUNT` is everything below the flags, as before.

The allocation site, meaning the file and line of the constructor or the
collection literal, comes from the type pointer for a user type. **Each
allocation site of a user type gets its own `TypeInfo`** -- in a heap watch
build; the default build's code names the type's own. It is a copy of the
type's record with the same drop, walk and method table, plus a site id and
a `canon` pointer to the real type. That costs static data only, about
100 B per site and nothing per object. Nothing compares a user type's
record by address, so nothing else changes. The runtime's own types --
the ones it does compare by address, `o->ty == &rt_lst_ref_type` -- are
never copied, and their objects' `ty` is always the type's own record.

As built:

- A site is a **(type, source line)** pair: the line of the expression that
  makes the object (the statement's, unless the expression has a line of
  its own). The lowering puts an `Inst::Site` before each allocation in a
  block whose line differs from the last one; the emitter turns each user
  type's (type, line) pair into a static `ti_T<n>_s<slot>` and numbers the
  slots 1, 2, ... in emission order, so the C is the same on every run. An
  allocation names its record as `RT_HW_TI(site, type)`, the site's in a
  heap watch build and the type's otherwise.
- The runtime's own objects -- a `str` from `concat`, a `List` from a
  literal, the `bytes` a primitive pushes -- are made inside the runtime.
  Emitted code says the line it is at before each runtime call that may
  allocate (`rt_hw_at(line)`; `ir::rt_call_allocates` lists the ones that
  cannot -- checks, conversions and system calls among them): on x86-64
  Linux one store to a thread-local through `%fs`, written as an `asm` so
  the compiler cannot cache the variable's address across a green thread's
  move to another carrier (the bug described at `rt_stack_check`); elsewhere
  a call. When such an object is SAMPLED, its slot is the line's own for its
  kind (`str`, `List`, ...), made the first time that line samples one. When
  it is not, epoch close and trace know it by its kind alone: one slot per
  kind, with no line.
- The file of a site is the entry file by its own name, a module of the
  program by its path from the entry file's directory (`ui/panel.m31`), and
  a module of the standard library as `net/http.m31` -- never a path on the
  compiling machine, so the emitted C is the same everywhere.
- TypeInfo grew `canon`, `site`, `loc`, `name` and a table of reference
  fields (name, offset, variant tag) for trace, in every build: static data,
  read only by heap watch. On a runtime type's record `loc` is its kind's
  column (1 for `str` ... 7 for `Map`).

Revision 6 put a BIRTH minute in the count word and swapped the runtime's
own objects' `ty` for per-line copies. That cost every build a check in
`rc_inc` (the count was 33 bits), a load in every type test (`o->ty->canon`)
and the hooks themselves; revision 7 removed all three.

**Per-object memory cost: 0 bytes**; a sampled object has a 24-byte entry
in the table of samples.

### What the runtime counts

For every allocation site `s`:

| Counter | Updated on | Answers |
|---|---|---|
| `live[s][birth_minute]`, sparse | a sampled object made: `+w`; freed: `-w` | how many objects from site `s` born at minute `m` are still alive, so **how old the survivors are** |
| `lifetime[s][k]`, 16 log2 buckets | a sampled object freed: its age goes in bucket `log2(age)`, weight `w` | how long objects from `s` **normally** live, learned from the program itself |
| `epoch[depth]` | see "Epoch close" | what the current unit of work made and did not keep |

The current minute is a global that the scheduler tick updates. Reading the
clock costs one load, never a system call.

**Sampling** (revision 7). The ledger does not see every object. Each
thread counts down the bytes it allocates, and the object that takes the
count below zero is **sampled**; the next countdown is drawn at random,
uniformly between 0 and twice `M31_HEAPWATCH_SAMPLE` (64 KiB by default), so
no loop can line up with it -- byte-based sampling, as in tcmalloc's and
heapprofd's heap profilers. A sampled object gets RC_HW_SAMPLED and an
entry in a table of samples -- its address, slot, birth minute and
**weight** `w`, how many objects of its size it stands for
(`M31_HEAPWATCH_SAMPLE / size`, at least 1) -- under one lock, touched once
per sample. The counters add weights, so every count the tool shows is an
estimate, and says so. A site that keeps objects alive keeps sampled ones
alive too: a leak of any size is found, a small one later than a large one.
`M31_HEAPWATCH_SAMPLE=1` samples every object and every count is exact; it
is a test build's default.

An object that is not sampled costs the countdown when it is made (a
subtract and a branch on a thread-local, before `malloc`) and the NOTE test
when it is freed. A thread's first allocation only starts its countdown.

As built (`runtime/heapwatch.c`):

- **`live` is a timing wheel, not a sparse table.** Per site, 7 levels of 8
  cells: level 0 holds the last 8 minutes one by one, level L cells of 8^L
  minutes, and level 6 reaches past the 20-bit birth minute. When a shard's
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
  it, so the +w and the -w always meet (`hw_sum_slot`, and the same in
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
**forever** bucket by setting its birth minute to a reserved value (all
ones). It is still counted, still listed (`forever  Texture
assets.m31:40  812 alive`), and still checked by epoch close and trace --
being marked long-lived exempts it from age and nothing else. A forever
object that is freed is simply uncounted; that is not an error. As built
(revision 7) the birth minute is the sampled object's, in the table of
samples: an object that is not sampled has nothing in the ledger to move,
and is marked only for trace, whose edges to it stop counting as old.

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
as an age: `heapwatch.epoch("level", () => play(level))` reports everything
the level made and did not keep, the moment the level is unloaded (see the
game engine walk-through).

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

### As built

- **Every task is an epoch**: green thread 0, which runs the program's
  statements, as `"main"`, and each spawned thread under its function's name
  (the emitted trampoline names it and calls `rt_hw_task_begin`). It closes
  when the thread's function returns (runtime/scheduler.c,
  `green_trampoline`). The books are per task, not per OS thread -- a
  `HwTask` hung off the green thread and reached through a slot the carrier
  sets around each dispatch -- because the move rule makes every counted
  object one task's at a time.
- **A block is `heapwatch.epoch(name, body)`**, or `heapwatch.epoch_of(name,
  body)` for one that hands back a value, with `body` a callback (`() =>
  handle(request)`; interfaces `heapwatch.Work` and `heapwatch.Make<T>`).
  Open question 2, decided: **a library call, not a keyword.** `epoch` as a
  keyword would break `date.epoch`, the standard library's own function, and
  every program that names anything `epoch`; a contextual `epoch "name" {
  .. }` statement needs the parser and a statement form in every pass that
  walks statements. A callback also closes two adoption rules by
  construction: it cannot assign the caller's locals (one capture mode, by
  value -- reference §6.7), and `break`, `continue` and `?` cannot leave
  it. A block form can come later as sugar for the same runtime pair.
- **The EPOCH field** (see "What each object carries") is `62 - depth` for
  an object counted against the open epoch at `depth` (0 is the task, so 61
  levels can nest; deeper blocks are counted as the deepest), 63 for one
  counted against none -- KEPT, and every immortal -- and 0 for one in
  transit between tasks.
- **What an epoch counts** (revision 7) is how many of its objects are
  alive: one more as each is made, one fewer as each is freed, a single
  number for the innermost epoch kept in the green thread itself (`HwHot`,
  three words in the scheduler's `rt_green`), so an allocation and a release
  add or take one from a counter one load away, and the outer epochs' in the
  task's rows. Which **sites** the survivors were made at is counted for the
  sampled ones, by site, in a small table per epoch. With
  `M31_HEAPWATCH_SAMPLE=1` -- a test build's default -- every object is
  sampled, and a close names every site exactly; otherwise it gives the
  exact number left and the sites of the sampled ones among them (`40
  objects; sampled: 1 Node @ server.m31:13`), or `none sampled`, and trace,
  if it is on, who holds them. Revision 6 counted every object by site,
  which was most of epoch close's cost.
- **The adoption list, as built:**

| Way out | Where | What happens |
|---|---|---|
| store into a field | `rt_hw_store` before every reference field store | `h`'s field > `v`'s: `v` and everything younger it reaches move to `h`'s epoch (`hw_adopt`); 63: kept |
| store into an element, a key or a value | `rt_hw_store_elem`; the runtime's push, insert, set and array-literal stores | the same |
| the value a block hands back | `heapwatch.epoch_of` | stored into a List made before the block opens, which is the first rule |
| assignment to an outer local | -- | cannot happen: the body is a callback |
| send, spawn argument | `rt_hw_send` after `rt_check_unique` | the value and what it reaches leave the sender's books (field 0) |
| a payload taken out of a `match` to be sent | `rt_hw_send` on the payload, after `EnumTake` | the payload leaves; the enum stays and is released here |
| receive | `rt_hw_receive` after `recv`, and at a spawned thread's start for each reference argument | what is in transit joins the receiver's current epoch |
| globals | -- | m31 has none |

- **At close** an epoch that still counts anything goes to the suspect log
  (`EPOCH "request" closed with leaks: 1 Node @ server.m31:13, ...`, sites in
  file and line order), to the ledger's table of epochs by name (how many
  times, when last, what), and -- for each leaked sampled site trace is on
  for -- to the trace file with the leaked objects' holders, which are then
  marked so the next close does not report them again. Then the row is
  cleared.
- **Test builds trap** (open question 5, decided): the corpus builds the
  runtime with `-DRC_DEBUG`, and so does any test that wants its refcount
  checks; in a heap watch build there epoch close is on by default and an
  epoch that closes with leaks traps, naming them. Any heap watch build
  traps with `M31_HEAPWATCH_TRAP=1`, and `M31_HEAPWATCH_TRAP=0` turns it off
  in a debug build. So **every corpus program closing clean** is a gate --
  in its heap watch builds, the `-O0` ones ("Builds"): each of its threads
  -- `main` and every spawned one -- is an epoch, and a missed adoption rule
  there is a trap and a failed test.
- **Proved by:** `corpus/core/1401` (every way out of a block, two deep,
  through captures, and a round trip through another thread), `1405`
  (threads, channels, a payload taken out of a match, epochs inside spawned
  threads), traps `1402` (a block), `1403` (a spawned thread) and `1404`
  (`main`) leaking a cycle, and `runtime/tests/heapwatch_test.sh` for a
  build that reports and carries on.

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

Within those limits this is one tool, one file and one report for the
whole heap -- in a heap watch build.

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
  every store of a reference into a field and `rt_hw_store_elem(o, i, v)`
  before every `o[i] = v` of one; the runtime's own stores (push, insert, a
  map's key and value, an array literal's elements) call the same slow
  path, and a clone, a slice or a copy for a `const` records what it holds
  when made. Its fast path loads one global, `rt_hw_watch` -- nonzero only
  with epoch close on or trace on for anything -- and with the ledger alone
  that is all a store costs. With epoch close on it then compares `h`'s and
  `v`'s EPOCH fields (two header loads, both already in a register or the
  cache). A store into an object made just before it in the same block,
  which nothing else has seen yet -- a constructor's -- is
  `rt_hw_store_fresh`, which tests for trace alone: the object is in the
  innermost epoch, so nothing it is given needs adopting. An object an edge
  names gets RC_HW_NOTE, so its release takes the edge out. A report reads
  no other thread's count word: an object's slot comes from its `ty`, or
  from the table of samples.
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
Texture grass = heapwatch.forever(load_texture("grass.png"));  // assets: forever

void play(int number) {
    Level level = load_level(number);
    while (!level.done) {
        // ...and everything a frame made
        heapwatch.epoch("frame", () => frame(level));
    }
}

while (running) {
    heapwatch.epoch("level", () => play(number));   // everything a level made...
}                                                   // ...is checked when it ends
```

(As built the block is a library call taking a callback; the proposal's
`epoch "level" { .. }` statement is not in the grammar -- see "Epoch close",
"As built".)

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

- **Nothing in a default build.** A program built without `-DRT_HEAPWATCH`
  runs the code it ran before heap watch.
- **0 bytes per object.** All the state is in counters, a header that is
  already there, and a table of the sampled objects.
- **No heap walk and no collection.** The cost per operation is bounded, with
  no pauses and no dependence on heap size.
- **Survives crashes and redeploys.** A memory-mapped ledger plus an
  append-only suspect log.
- **Learns its own thresholds.** Each site's lifetime histogram is the
  baseline, so no configuration is needed to start.
- **Exact where it can be.** Epoch close counts every object and never
  reports a false leak; with `M31_HEAPWATCH_SAMPLE=1` the ledger is exact
  too.
- **Points at the fix.** Type, file and line, age, unit of work.
- **Same program behaviour either way.** Heap watch changes only what is
  written to the files.

**Cons**

- **It has to be asked for.** A process that was not built with heap watch
  has no ledger to read after it crashed; finding a leak in production
  means deploying a heap watch build (a few percent, "Cost") and waiting
  for it to recur.
- **Sampled counts are estimates**, and a small leak is found later than a
  large one; `M31_HEAPWATCH_SAMPLE` trades cost for resolution.
- **Store sites gain a test** in a heap watch build: one load of a global
  per reference store, an EPOCH compare with epoch close on.
- **Adoption rules must stay complete.** Closures and any future feature that
  lets a value escape need an entry, or false epoch reports appear (the
  corpus catches them).
- **The emitted static data grows** in a heap watch build: a `TypeInfo` per
  user-type allocation site, and the line table.
- **Age suspects need time**, and a site with no baseline needs a human to
  judge it.
- **Disk:** one ledger file per process. Ledgers from old pids need a
  retention rule (keep the last N).
- **The spreading walk** can move a large new graph in one store, for example
  building a big list and then caching it. The cost is amortised, but it
  shows up as a spike.

---

## Cost (measured)

The proposal's estimates are kept at the end of this section for comparison;
the measurements replace them.

### CPU

Revision 7, measured 2026-10-22 on the five benchmarks revision 6 was
measured on: best of 15 runs, interleaved, gcc 14 `-O2`, x86-64 (an
i7-8750H, 12 threads, load 2-3 from other work), against 0.11.0
(`a0bd007`, before heap watch). Wall time is the column; the user-mode
cycles (fewest of the 15) and instructions of each run were counted too,
with `perf_event_open`. On this machine two builds of identical code differ
by up to 3 % in wall time and cycles from run to run; instruction counts do
not vary. `bench/heapwatch/run.sh` repeats the comparison.

| Benchmark | What it does | 0.11.0 | default build | heap watch build, `M31_HEAPWATCH=0` | heap watch build, ledger (its default) | heap watch build, `full` |
|---|---|---|---|---|---|---|
| `bench/trees` | binary trees: allocate and free 16 x 2^19 nodes | 0.561 s | 0.545 s (-2.8 %) | 0.555 s (-1.0 %) | 0.550 s (-1.9 %) | 0.592 s (+5.6 %) |
| `bench/heapwatch/allocs` | a struct, a str and a 2-element List per iteration, little else | 0.453 s | 0.448 s (-1.2 %) | 0.459 s (+1.4 %) | 0.463 s (+2.2 %) | 0.562 s (+24.0 %) |
| `bench/heapwatch/stores` | 40 M reference stores into fields and List slots, no allocation | 0.683 s | 0.688 s (+0.8 %) | 0.662 s (-3.0 %) | 0.663 s (-2.9 %) | 0.779 s (+14.1 %) |
| `bench/valenum/result-boxed` | a boxed `Result` per call, 50 M calls | 1.499 s | 1.483 s (-1.0 %) | 1.503 s (+0.3 %) | 1.494 s (-0.3 %) | 1.686 s (+12.5 %) |
| `bench/valenum/option-boxed` | a boxed `Option` per call | 0.784 s | 0.786 s (+0.2 %) | 0.779 s (-0.7 %) | 0.788 s (+0.5 %) | 0.877 s (+11.9 %) |

The same runs in cycles and instructions, against 0.11.0:

| Benchmark | default build: cycles / instructions | ledger: cycles / instructions | `full`: cycles / instructions |
|---|---|---|---|
| trees | +1.2 % / 0.0 % | +1.1 % / +2.1 % | +8.2 % / +9.6 % |
| allocs | -0.3 % / 0.0 % | +4.1 % / +4.0 % | +25.3 % / +22.0 % |
| stores | -0.1 % / 0.0 % | -4.4 % / +5.4 % | +13.1 % / +19.6 % |
| result-boxed | +0.1 % / 0.0 % | +0.8 % / +3.9 % | +13.8 % / +22.8 % |
| option-boxed | +0.8 % / 0.0 % | 0.0 % / +3.6 % | +12.3 % / +21.0 % |

- **The default build costs nothing, by construction.** Compiled without
  `-DRT_HEAPWATCH`, every function of `runtime/rt.c` and
  `runtime/scheduler.c` is the same machine code as 0.11.0's -- checked
  function by function, gcc and clang, at `-O0`, `-O1`, `-O2`, `-O3` and
  `-Os` -- and so is every function of the five benchmarks' emitted C at
  `-O0` and `-O2`; the instruction counts are equal to the last one. The
  only difference is static data: a TypeInfo has the fields heap watch
  reads (+0.7 KB of `.rodata` on `bench/trees`). The time differences in
  the "default build" column are the machine's noise.
- **A heap watch build with the ledger** costs 2-5 % in instructions: the
  sampling countdown in `rt_alloc` (a subtract and a branch on a
  thread-local), the NOTE test in `rc_release`, one load and a branch per
  reference store, and the line before a runtime call that may allocate.
  The sampling itself -- one object in 64 KiB on average -- is lost in it.
  In time it is within the machine's noise: -2.9 % to +2.2 % above, and
  -4.1 % to +4.0 % in a second measurement of the same builds (21 runs).
- **`full`** adds epoch close: one more count up as each object is made and
  one down as it is freed, in the green thread's own HwHot, and an EPOCH
  compare on every reference store. 6-25 % on these benchmarks (10-24 % in
  the second measurement), the most on
  `allocs`, which does nothing but allocate, free and store; meant for
  tests -- every corpus program's `-O0` builds run in it -- and for hunting a
  leak in a heap watch build.
- **Where revision 6's cost went.** Revision 6 counted every object in a
  per-site timing-wheel cell and lifetime bucket (a load, an add and a store
  each, on a shard looked up per operation), stamped a birth minute and an
  epoch in every count word (so `rc_inc` had to check a 33-bit count), made
  the runtime's own objects' `ty` a per-line copy (so every type test loaded
  `o->ty->canon`, and `rt_alloc` looked the copy up by line), counted every
  object by site in its epoch's hash table, and had all of that compiled
  into every program. Measured then: +16 % to +68 % with it on, up to +20 %
  with it off. A word in front of each object for the site and birth was
  tried on the way: it cost `trees` 14 % on its own, by moving 40-byte
  objects into malloc's next size class, and was dropped for sampling.

### Memory

| Item | Measured |
|---|---|
| per object | **0 B** in the default build and in a heap watch build: spare bits in the count word |
| per sampled object | a 24 B entry in the table of samples, which stays at most half full |
| per allocation site, static | in a heap watch build, one 104 B TypeInfo copy per user-type site, and a site's line is 16 B in the line table: about 50 KB for an HTTP server program (341 user-type sites, 735 lines). In every build, a TypeInfo's name and field table: +0.7 KB on `bench/trees` |
| runtime code | a heap watch build: +41 KB of text (`examples/tour` 93 KB -> 135 KB) and +100 KB of zeroed data (scratch tables), untouched until used; the default build: none |
| per site and shard, in the ledger | 320 B (the wheel, the lifetime histogram, the forever count); only shards a thread used and sites that sampled are ever written |
| ledger file | apparent size about 1.3 MB per shard (sites + 4 096 for the runtime's own) times shards (carriers + 8): 27 MB on a 12-core machine, sparse; **written: 20-100 KB** for the corpus's and the tests' programs |
| resident memory | +300-500 KB in a heap watch build (19.0 MB -> 19.5 MB on `trees`, 5.8 -> 6.1 MB on `allocs`) |
| epoch books | per task, three words in its green thread, and a small table per open depth for its sampled objects, 16 cells to start; freed when the task ends |
| trace edge table | only while tracing: 56 B per edge plus its hash entries, capped (1 M edges by default) |

### The proposal's estimates, for comparison

Allocate ~2-3 ns, free ~3-5 ns, a reference store ~0.3-1 ns, "about 3-7 %
on code that allocates heavily". Counting every object, as proposed and as
revision 6 did, was well over that on allocation-bound code (+16 % to
+68 %); sampling, as revision 7 does, is under it.

---

## Feasibility

| Piece | Feasible? | Main risk | How it is caught |
|---|---|---|---|
| Count word fields (59 → 51 bits + NOTE, EPOCH, SAMPLED), heap watch build only | yes; `rc_inc`/`rc_dec` are out-of-line functions in `runtime/rt.c`, so the mask changes in one place | a path that reads the count word raw | sanitizers + a runtime test that a retain and a release leave the fields alone |
| Per-site `TypeInfo` for user types | yes; the compiler already emits one `TypeInfo` per type | a runtime type check on a user type's record | none exists; the runtime's own types are never copied (revision 7) |
| Nothing in a default build | yes; `#if RT_HEAPWATCH` in the runtime, macros for the emitted hooks | a hook left outside them | a gate: no heap watch symbol in a default runtime, no heap watch call in a default program |
| `live` / `lifetime` counters, mmap ledger, suspect log | yes; plain memory writes, one file per process | thread shards and file layout versioning | corpus run with the ledger read back by the tool |
| Forever marks | yes; a reserved birth minute, no new bit | none notable | corpus |
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

All three are built (2026-10-21), one commit each, and measured together in
"Cost" against the commit before the first.

## Configuration

Everything is read once, at start, from the environment; nothing needs a
rebuild -- once the program is a heap watch build (`-DRT_HEAPWATCH`,
`./build.sh --heapwatch`), which is the one thing that does.

| Variable | Default | Effect |
|---|---|---|
| `M31_HEAPWATCH` | `ledger`; `full` in a `-DRC_DEBUG` build | `ledger`: the sampled ledger, trace on request; `full`: and epoch close; `0`: nothing counted, no file |
| `M31_HEAPWATCH_SAMPLE` | 65536; 1 in a `-DRC_DEBUG` build | the mean bytes allocated between sampled objects; `1` samples every object, and every count is exact |
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
| `M31_HEAPWATCH_TRAP` | on in a `-DRC_DEBUG` build, else off | `1`: an epoch that closes with leaks traps; `0`: it is only reported |

## Open questions

Answered while building (each the simplest sound option; the owner may
overrule any of them):

2. **Spawned tasks, and blocks as a library call: `heapwatch.epoch(name,
   () => body)`.** A keyword `epoch` would take the name from
   `date.epoch`; a contextual statement needs the parser and every pass that
   walks statements; a callback needs neither, and its rules (no assigning
   the caller's locals, no `break` or `?` out of it) close two adoption
   rules by construction. See "Epoch close", "As built".
5. **Yes: a test build traps.** "Test build" is the runtime compiled with
   `-DRC_DEBUG`, which is what the corpus and the runtime's own tests use;
   `M31_HEAPWATCH_TRAP=1` asks for it in any build, `=0` turns it off.
1. **Off by default -- revision 7.** Revision 6 had it on in every
   program, at +16 % to +68 % on allocation-bound benchmarks and up to
   +20 % with it switched off, which could not be merged. Now a default build
   has none of it, and a heap watch build (`-DRT_HEAPWATCH`) has the sampled
   ledger on and epoch close on request. The cost section has the
   measurements this rests on. Requirements 1 and 2 hold in a heap watch
   build -- every object is covered, by sampling (exactly with
   `M31_HEAPWATCH_SAMPLE=1`), and the ledger survives a crash -- and not in
   a default one.
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

6. Should a stdlib API expose the ledger, so a server can serve it as
   metrics (such as `/debug/heap`)? Not built: nothing asked for it yet, and
   the tool already reads a live process's ledger.
10. **Epoch accounting reads the task through a thread-local set by the
    scheduler.** A program that calls the runtime from an OS thread that is
    not a carrier (a hand-written C harness) gets no epochs -- its objects
    are counted against none -- rather than a task of its own.
11. **A false report marks objects.** After an epoch's leaked objects of a
    traced site are reported, they are marked as counted against no epoch so
    the next close does not report them again. If an adoption rule were
    missing and such an object were in fact still reachable, stores into it
    would from then on keep, not adopt -- fewer reports, never a false one.
    The corpus gate is what keeps the list complete.
12. **A cheaper mode: built, revision 7.** `M31_HEAPWATCH=ledger` is a
    heap watch build's default, and epoch close is `full`.
13. **Should a default build sample too?** The sampled ledger costs a few
    percent ("Cost"), most of it the hooks' tests rather than the sampling.
    Shipping it in every build would put those tests back in every program;
    it is the owner's call, and a one-line change to `build.sh` and the
    corpus runner.
