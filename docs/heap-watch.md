# Heap watch: a user guide

Heap watch finds leaked reference cycles in a program that is running, or
that has crashed. It is an **opt-in tool for debug and test builds**: you
ask for it when you build, and a default build has none of it -- not a
symbol in the runtime, not an instruction in your code. It is **not a
garbage collector**. It observes and reports; it never frees anything and
never changes what a program does. A cycle it finds is a bug for you to fix.

The design, with the reasons for each choice and the measurements, is in
`docs/heap-watch-design.md`. This page is what you need to use it.

## Build with it

```
./build.sh prog.m31 --heapwatch            # -> ./prog, a heap watch build
./build.sh prog.m31 --heapwatch -o prog-hw
```

Or by hand, with the flag on **both** the program's and the runtime's `cc`
line:

```
m31c -emit-c prog.m31 --out prog.c
cc -O2 -pthread -DRT_HEAPWATCH -I runtime -o prog prog.c \
   runtime/rt.c runtime/scheduler.c runtime/reactor_epoll.c runtime/ctx_switch_x86_64.S
```

A program built with the flag against a runtime built without it fails to
link; a program built without it against a runtime built with it runs as if
heap watch were off. Without the flag nothing is compiled in: the emitted C
names each hook as a macro that is nothing, and a gate in `gates.sh` checks
the machine code is the same as before heap watch existed.

A heap watch build costs a few percent of run time (see "Cost" below); do
not ship it as your release binary.

## Modes

`M31_HEAPWATCH` picks the mode when the program starts:

| Mode | What it does |
|---|---|
| `ledger` (default) | counts objects by allocation site, by sampling, and writes suspects to a file as they appear |
| `full` | the ledger, and **epoch close**: every spawned thread and every `heapwatch.epoch` block is a unit of work, and one that ends with a cycle it made left behind is reported at that moment |
| `0` | nothing counted, no file |

In a test build (the runtime compiled with `-DRC_DEBUG`, as the corpus's
`-O0` builds are) the default is `full`, every object is sampled, and a
unit of work that leaks a cycle **traps**. `M31_HEAPWATCH_TRAP=1` asks for
the trap in any build; `=0` turns it off.

## Environment variables

Everything is read once, at start; nothing needs a rebuild.

| Variable | Default | Effect |
|---|---|---|
| `M31_HEAPWATCH` | `ledger` (`full` in a `-DRC_DEBUG` build) | the mode |
| `M31_HEAPWATCH_SAMPLE` | 65536 (1 in `-DRC_DEBUG`) | mean bytes allocated between sampled objects; `1` samples every object, so counts are exact |
| `M31_HEAPWATCH_DIR` | `${TMPDIR:-/tmp}/m31-heapwatch-<uid>` | where the ledger, `.suspects` and `.trace` files go |
| `M31_HEAPWATCH_KEEP` | off | keep the files after a clean exit with nothing to report |
| `M31_HEAPWATCH_MIN` | 10 | a suspect's floor, in minutes |
| `M31_HEAPWATCH_FACTOR` | 10 | a suspect is older than this many times its site's p99 lifetime |
| `M31_AGE` | none | long-lived on purpose: `Type:forever`, `Type:10m`, `Type@file.m31:40:forever`, comma-separated |
| `M31_HEAPWATCH_CONF` | none | a file of `M31_AGE` entries, one per line, `#` for comments |
| `M31_HEAPWATCH_TRACE` | none | trace these types from the start: `Type`, `Type@file.m31:40`, comma-separated |
| `M31_HEAPWATCH_DEPTH` | 3 | how many holders up a trace path follows |
| `M31_HEAPWATCH_EDGES` | 1048576 | cap on trace's edge table |
| `M31_HEAPWATCH_TRAP` | on in `-DRC_DEBUG`, else off | an epoch that closes with leaks traps |
| `M31_HEAPWATCH_MINUTE_MS` | 60000 | how long a "minute" is; for tests |

## What a run leaves behind

A heap watch program keeps its counters in a file it maps:
`<program>.<pid>.ledger` in `M31_HEAPWATCH_DIR`. Counters are plain stores
into the mapping, so the file is current when the program traps, is
killed, or crashes -- including `kill -9`. Next to it, `.suspects` is a log
of what was found, and `.trace` holds paths when trace is on.

A clean exit with nothing to report removes the files (`M31_HEAPWATCH_KEEP=1`
keeps them). A program also removes the ledgers of dead processes of its
own name beyond the newest 8.

## Reading the report

```
m31c -heapwatch <ledger-file | pid>  [--all]
```

It works on a live process (by pid) or a dead one (by file). Take this
program, where one request in four leaves two nodes holding each other:

```
import heapwatch;

type Node { str name; List<Node> links; }

void handle(int round) {
    Node left = Node("left", []);
    Node right = Node("right", []);
    if (round % 4 == 0) {
        left.links.push(right);
        right.links.push(left);
    }
}

int round = 0;
while (round < 20) {
    heapwatch.epoch("request", () => handle(round));
    round = round + 1;
}
```

built with `--heapwatch` and run as
`M31_HEAPWATCH=full M31_HEAPWATCH_SAMPLE=1 M31_HEAPWATCH_TRACE=Node,List M31_HEAPWATCH_KEEP=1 ./leak`:

```
$ m31c -heapwatch leak.2405622.ledger
leak (pid 2405622), exited; up <1m, minute 0 of its clock
nothing to report
epoch "request" closed with leaks 5 times; last at minute 0: 1 List @ leak.m31:14, 1 Node @ leak.m31:14, ...
trace on for Node leak.m31:14, Node leak.m31:15, List, ...; 20 edges recorded
```

and the `.trace` file has the cycle itself:

```
Node  leak.m31:14  (epoch "request" left them)
  held by  List element             leak.m31:15
    held by  Node.links               leak.m31:15
      held by  List element             leak.m31:14
```

For a long-running server, the report is the other kind: a **SUSPECT** is
an allocation site whose oldest survivor is older than
max(`M31_HEAPWATCH_MIN` minutes, `M31_HEAPWATCH_FACTOR` x the site's own p99
lifetime). It is written to `.suspects` when it becomes one and again each
time its survivors double in age. A site that has never freed anything has
no baseline; it is listed as "never freed", not as a suspect.

With the default sampling (one object in 64 KiB) counts are estimates, and
the report says so. `M31_HEAPWATCH_SAMPLE=1` makes them exact, at a higher
cost.

## Marking what lives long on purpose

A game's textures, a cache filled at start, a connection pool: say so, and
they are not reported as old.

- in code: `heapwatch.forever(x)` returns `x` (in any build, including a
  default one) and marks that one object;
- without a rebuild: `M31_AGE=Texture:forever,Cache@main.m31:40:forever`, or
  a file of such lines named by `M31_HEAPWATCH_CONF`.

The object is still counted, still listed (as "forever"), still checked by
trace and epoch close, and still freed when its last reference goes.

## Units of work: `heapwatch.epoch`

```
import heapwatch;
heapwatch.epoch("request", () => handle(request));
T value = heapwatch.epoch_of("frame", ...);   // a block that returns a value
```

With `M31_HEAPWATCH=full` (or a test build), everything made while the body
runs is counted against the epoch. When it ends, what is **left, held only by
other objects of the same epoch, is a leaked cycle**: reported, and in a test
build a trap. An object leaves an epoch legitimately by being stored into
something made before it began, by being sent to another thread, or by being
what `epoch_of` returns. Every spawned thread is an epoch of its own. In a
default build `epoch` just runs its body.

## Trace: who holds it

```
m31c -heapwatch <pid> --trace Node        # turn on, in a running process
m31c -heapwatch <pid> --untrace Node
M31_HEAPWATCH_TRACE=Node ./prog           # from the start
```

Trace records which object holds which, for the types you name, and prints
the path from a suspect up to a root, a cycle, or `--depth` levels
(default 3). It costs nothing for a type nobody traces.

## Fixing a cycle leak

1. **See it.** `m31c -heapwatch` on the live or dead process: a suspect
   site, or an epoch that closed with leaks.
2. **Judge it.** Meant to live long? Mark it (`heapwatch.forever`,
   `M31_AGE`). Otherwise it is a bug.
3. **Find the holder.** Trace the type (`--trace Type` on the running
   process, or `M31_HEAPWATCH_TRACE=Type`), wait for the next occurrence,
   read the path.
4. **Fix it**, by what the path ends in:
   - a cycle among throwaway objects: break it before the unit of work
     ends (clear the back reference), or, if the structure is cyclic on
     purpose, put it in an `Arena<T>` and refer to nodes by handle;
   - something that should have left the epoch: return it, send it to the
     task that owns it, or clone what is kept;
   - a holder that never lets go: remove the entry when its owner ends.
5. **Prove it.** In a test build an epoch that leaks traps, so the fix gets
   a test that fails without it: put the code in a `heapwatch.epoch` and
   run the test with a heap watch build.

## Where it sits among m31's cycle tools

m31 has plain reference counting, no garbage collector and no weak
references, and will not get a cycle scan at run time. So cycles are
prevented by the programmer, and there are three tools, from earliest to
latest:

- **Static check (`m31c -cycles`).** The compile-time analysis of which
  stores can close a cycle, at zero run-time cost. It is specified in
  `docs/cycle-check-design.md` on the `design/cycle-check` branch and is
  not part of this release; when it lands it is the first thing to run. It
  can say a store *may* close a cycle; it cannot say a cycle *did* form.
- **`Arena<T>` with integer handles.** The answer for a graph that is cyclic
  on purpose: a handle is an integer, not a counted reference, so it cannot
  close a cycle. Use it where the static check or a heap watch report points
  at a structure that is meant to be cyclic.
- **Heap watch (this page).** The run-time check for what the other two did
  not prevent: a leak that happens only with real data, in a server that has
  run for a week. It names the allocation site and, with trace, the path.

Heap watch can never prove there is no leak; it can show that a unit of work
left none behind, in the runs you gave it.

## Cost

Against the same program built without heap watch (the table is in
`docs/heap-watch-design.md`, "Cost (measured)"; `bench/heapwatch/run.sh`
repeats it):

- a **default build** costs nothing: the machine code is identical, checked
  over the whole corpus (`bench/heapwatch/identity.sh`);
- a heap watch build in **`ledger`** mode costs 2-5 % in instructions on
  allocation- and store-heavy code, and is within the machine's noise in
  time on the five benchmarks (-3 % to +5 %);
- **`full`** costs 10-23 % in instructions and 5-23 % in time, the most on
  code that does nothing but allocate and free;
- `M31_HEAPWATCH=0` in a heap watch build costs what `ledger` does: the
  cost is the hooks' tests, not the counting.
