# Destructors: the decision

Decided **2026-09-21**. Implemented in the same change: `src/lower.rs`
(`check_destructor_decls`, `refuse_destructor_call`), `src/emit_c.rs` (the
drop function), `lib/io.m31` (`File.drop`), tests `corpus/*/77N-*` and
`corpus/modules/destructor-*`. The reference is §4.4.

---

## The problem

`io.File` dropped without `close()` kept its descriptor until the program
ended, and every `?` between an `open` and its `close` skipped the close.
lib/io.m31 said so in a doc comment -- "a `File` is not closed for you ...
close on every path, including the error paths" -- which is a rule every
caller has to remember on every path, forever. `io.read_bytes` and
`io.write` follow it by hand; a program that opens a file and propagates an
error with `?` did not, and could not without giving up `?`.

## The decision, in one place

| | |
|---|---|
| **Mechanism** | A destructor: a method with the reserved name `drop`. Not `defer`. |
| **Spelling** | `void File.drop() { .. }` -- no parameters, `void`, not `static`, not `pub`, no type parameters of its own |
| **On** | Structs only (generic structs included). Not enums, distinct types or interfaces. |
| **When** | The moment the count reaches zero -- deterministic, on the thread that let go |
| **Order** | The destructor first, on a whole object; then the fields are released, so a field's destructor runs after its owner's |
| **Calls** | Never by the program. `x.drop()`, `this.drop()`, bare `drop()`, `T.drop()` are all refused |
| **Errors** | None: it returns `void`. A failure the program must see goes through an ordinary method (`close()`) |
| **Resurrection** | Checked at run time: the count must be back to exactly 1 when it returns, else a trap |
| **Trap inside** | Aborts, as every trap does |
| **Cycles** | Never freed, so never destroyed -- the language's existing cycle limitation |
| **Copies** | A value that owns a resource is never copied or frozen: `clone` of a type with a destructor is refused, and a `const` cannot hold one (see "A resource cannot be copied") |
| **Exit** | No guarantee for an object still alive when the program ends; in practice the top level's locals are released at the end of the program and do run |

## Why destructors and not `defer`

Refcounting already knows the exact moment each object dies. That is the
property Swift's `deinit` and Rust's `Drop` are built on, and that a garbage
collected language's finalizers lack -- a finalizer runs at some later time,
or never, which is why Java deprecated them and why Go and C# grew `defer`
and `using` instead. Those languages need a second mechanism because their
memory manager cannot say *when*. This one can.

  - **It cannot be forgotten at a use site.** The type declares it once; a
    caller that does nothing still gets it. `defer f.close()` has to be
    written after every `open`, which is the same rule-on-every-path problem
    moved one line up.
  - **It covers every exit for free.** Early `return`, `?`, `break`,
    reassignment, a list element removed, a `Result` holding a `File` going
    out of scope, an object moved to another thread and dropped there. The
    lowering already releases every owned local on every one of those paths
    -- that is what `__rc_live=0` asserts on every corpus program -- so the
    destructor rides on work that is already done and already checked.
  - **One mechanism, not two.** `defer` would be a second way to do the same
    job, with its own questions (evaluation time of the arguments, loops,
    interaction with `?`), and the language's habit is one spelling.

What a destructor does not do, `defer` would: run arbitrary code at the end
of a scope that is not tied to an object. Nothing in the stdlib or the corpus
needs that. If something does, a small guard type with a destructor is the
spelling.

## The rules, and why each

**The name is `drop`.** Rust's name, because "drop" is already what the
runtime calls the operation (`DropFn`, `drop_T3`), and a reserved *method
name* needs no new keyword. Swift's `deinit` would have been a keyword for
the same thing. The cost: no type may have an ordinary method called `drop`.

**No parameters, `void`.** Nobody calls it, so there is nobody to pass an
argument or to receive a result. In particular it **cannot return an error**,
as in Rust. `io.File.drop` closes silently and discards the close's result;
a writer that must know whether the close failed (NFS, a full disk) calls
`close()` itself and looks. That is also Rust's `File`: dropping it closes
and ignores errors; `sync_all` is how you find out.

**Not `static`, no type parameters of its own.** It acts on the dying
object, and there is no call site to infer a type argument from. A generic
*type* may have one: `void Wrap<T>.drop()` is instantiated with each
instantiation of `Wrap`, like its other methods.

**Not `pub`.** It is never called by name from anywhere, so exporting it
would mean nothing, and one spelling beats two that behave the same. It still
runs in whichever module drops the object (corpus/modules/destructor-foreign).

**Structs only.** A distinct type is erased to its base and has no object of
its own to die. An interface has no objects. An **enum** could technically
have one -- it is a heap object with a TypeInfo -- but nothing needs it, the
fields-by-bare-name reading of a destructor does not apply to a variant's
payload, and allowing it later breaks nothing where taking it away would.

**An interface may not require `drop`.** That would be a way to call it.

**Embedding does not promote it.** An embedded value is a field; it is
released with its owner, and its own destructor runs then, once. A forwarder
would run it a second time on a live object. The outer type may declare its
own destructor; it runs first, then the embedded one.

**It cannot be called.** Every spelling -- `f.drop()`, `this.drop()`, a bare
`drop()` inside a method, `File.drop()` -- is refused with the same message,
which says what to do instead: put the early work in an ordinary method and
call that from `drop` too. Refused as a destructor call even from another
module, rather than as a call to a private method, because privacy is not
the reason.

**`this` is borrowed, and the object is whole.** The destructor runs before
any field is released, so it can use every field -- close a descriptor held
in a field of a field -- and call sibling methods. `this` may be read, passed
to functions, and bound to a local for the length of the call.

## Resurrection

A destructor can store `this` somewhere reachable -- push it onto a list held
by a field, say. The object is dead: the runtime is about to release its
fields and free it, and the list would be left holding freed memory.

Two ways to stop that were considered:

  - **At compile time**, by allowing `this` only as the receiver of field
    reads and sibling calls. Too strict -- passing `this` to a function that
    merely reads it (`describe(this)`) is the natural way to log a dying
    object, and it would be refused -- and not even sufficient, because a
    sibling method can store its own `this`, so the check would have to be
    transitive through every method the destructor calls.
  - **At run time**, by the count. Chosen: cheap, and always right.

The mechanism: `rc_dec` calls the drop function when the count reaches 0.
The drop function sets the count to **1** -- the borrow the destructor is
given -- calls the destructor, and checks the count is **exactly 1** again
before setting it back to 0 and releasing the fields. More than 1 means a
reference to `this` outlived the call:

    trap: object resurrected in its destructor: `drop` stored `this` somewhere that outlives it

Running at 1 rather than 0 is not only for the check. The body may retain
and release `this` in pairs -- binding it to a local, passing it to a
function that stores it in a temporary. At 0, the release of such a pair
would reach zero a second time and free the object inside its own
destructor. corpus/core/773 binds `this` to a local in a destructor, which
emits exactly that `rc_inc`/`rc_dec` pair.

The cost is two stores and a compare per destroyed object of a type that
declares a destructor, and nothing for any other type.

## A resource cannot be copied

Decided **2026-09-22**, after an adversarial review found three bugs where
destructors met `const` (docs/const-decision.md) and `clone`. Implemented in
`src/lower/consts.rs` (`refuse_const_resource`, `resource_in`), the `clone`
path in `src/lower.rs`, `rt_snapshot` in `runtime/rt.c`, and a `resource`
name in every `TypeInfo`. Tests corpus/core/840-842, corpus/errors/840-848,
corpus/traps/840-843.

### What went wrong

  1. **Double release.** A `const` snapshot of a shared value is a deep copy.
     `void look(io.File f) { const io.File g = f; }` copied the File, the
     copy died at the end of `look`, and its destructor closed the caller's
     descriptor; the caller's next `open` got the same number back, and
     writes meant for the first file landed in the second. The same with a
     user destructor (`Res r = Res(8); const Res c = r;` printed "release 8"
     twice), with a File inside a fresh value whose other parts were shared
     (`const Log log = Log(tags, io.open(..)?);` -- the Log was copied, and
     releasing the original closed the copy's descriptor: EBADF), and with
     `clone(f)` on a File, a shallow copy of its fields that included the
     descriptor number.
  2. **Unbounded recursion.** `void log(R r) { const R seen = r; .. }` called
     from `R.drop` as `log(this)`: inside a destructor the count is 1 plus the
     argument's retain, so the const copied `this`; the copy died and ran its
     destructor, which called `log` on the copy, and so on until the stack
     overflowed. `describe(this)` is exactly what this record calls the
     natural way to log a dying object.
  3. **A frozen object's destructor.** The drop function sets the count to 1,
     which also clears the frozen bit, so a destructor on a frozen object
     could write its own fields -- but everything it reached was frozen with
     it, so `void Lease.drop() { p.free.push(slot); }` on a `const Lease`
     trapped at the end of the scope.

All three are the same mistake: an object that owns a resource treated as
plain data that can be duplicated or frozen.

### The rule

**A value that owns a resource can be neither copied nor frozen.** A type
*owns a resource* if it has a destructor, or can hold a value whose type has
one -- through a field (an embedded one included), a collection's element,
key or value, or an enum variant's payload. Rust's parallel: a `Drop` type is
never `Copy`, and is `Clone` only if it says how.

  - **`clone(x)` of a type with a destructor is a compile-time error.** It
    would duplicate the resource. The message says to share the reference
    instead (`=` aliases) or give the type a method that makes a real second
    resource (a `dup()` that opens the file again). Only the type's *own*
    destructor matters here, not one it can reach: `clone` is shallow, so a
    clone of a `Log` holding a `File` is a second `Log` sharing that one
    `File`, closed once, when the last reference goes (corpus/core/840). No
    run-time check backs this up, because none is needed: an interface
    cannot be cloned at all, and a generic is concrete by the time it is
    lowered.
  - **A `const` cannot hold a value that owns a resource** -- shared *or
    fresh*. At compile time, whenever the declared type (after
    monomorphisation, so `const T c = v;` in a generic is checked per
    instantiation) can hold one. At run time, as the backstop for what the
    type cannot show -- an interface, or a collection of them --
    `rt_snapshot` walks the whole unfrozen graph and traps, naming the type,
    before it freezes or copies anything:

        trap: a const cannot hold a value of type `Res`: it owns a resource (it has a destructor), which a constant can neither freeze nor copy; bind it without `const`

    The runtime knows a type owns a resource from a new last field of
    `TypeInfo`, `resource`: the type's name as the program spells it, or
    NULL. One field serves as both the flag and the message. Only the
    object's *own* type is checked; the walk reaches every object a
    transitive owner could hold, so it finds the one with the destructor
    wherever it is.

### Why a fresh value is refused too

The first draft of this rule refused a const at compile time only when the
value was not provably fresh, and would have frozen a fresh one in place --
the snapshot needs no copy then, so nothing is duplicated. Refused anyway,
for three reasons:

  - **A frozen resource is half usable, in a way that depends on how its
    type happens to be written.** A frozen `io.File` can be written -- as it
    happens, `File.write` stores no field -- but `read` traps (it moves the
    buffer position, a field), and so does `close()` (it sets `live`). A
    user of `const Log log = ..` would find `log.out.write(..)` working and
    `log.out.close()` trapping, and the line between them is an
    implementation detail of lib/io.m31.
  - **The destructor would be the one change a constant allows.** Releasing
    a resource changes the object (a File marks itself closed) and the world
    (the descriptor goes). Letting it run on a frozen object means either
    un-freezing the object for the call (what the drop function did,
    accidentally) or freezing its neighbours so that it traps (bug 3).
    Neither is "the value never changes".
  - **One rule is simpler than two.** Whether a value is fresh at a line is
    a run-time fact the programmer does not see -- `const` exists precisely
    so that they need not care (docs/const-decision.md). "A const of a
    `File` works if nobody else holds it" would be exactly the kind of rule
    that decision threw out.

What a `const` of a resource would have bought -- "this name is never
rebound" -- is not what `const` means in this language; it means the value
never changes, deeply. The spelling for a resource is a plain binding.

The alternative for bug 3 on its own -- let a frozen object's destructor
write its own fields, keep its neighbours frozen, and document "do not
`const` a Lease" -- was considered and is subsumed: under this rule a Lease
cannot be `const`, so the documentation would describe a case that cannot
arise.

### What a destructor may do, restated

A destructor never runs on a frozen object: nothing that has one can be
frozen (a module constant is immortal and never destroyed, and cannot be a
user type anyway). So it may change its own fields, as any method may, and
change any object it reaches that is not itself a constant. A `Lease` gives
its slot back to its pool (corpus/core/842); if the pool itself were a
`const`, the push would trap, correctly -- a constant pool cannot take a
slot back.

### Bug 2, without `const`

`void describe(R r) { R seen = r; print(..); }` called from `R.drop` as
`describe(this)` works and does not recurse: a plain binding is a second
reference to the same object, retained and released in a pair while the
drop function holds the count at 1 (corpus/core/841). Only `const` copied,
and `const` of an `R` is now refused (corpus/errors/844).

## Traps and threads

**A trap inside a destructor aborts**, like every trap: there is no unwinding
and so no running the other pending destructors on the way out. A trap is a
bug, and the process ends (reference §7.4). corpus/traps/771.

**Threads.** An object moved to another thread -- by `send` or as a `spawn`
argument -- is dropped by whichever thread lets go of it last, and its
destructor runs on that thread. The move rule guarantees there is only ever
one such thread, so the destructor needs no synchronisation of its own.
corpus/core/772.

**Immortal objects** (string literals, reference §7.3) are never destroyed,
and no user type can be immortal, so no destructor is affected.

## What is not guaranteed

**Cycles.** A cycle's counts never reach zero, so its members are never
destroyed and their destructors never run. That is the language's existing
cycle limitation (reference §7.1), not a new one -- but it now has a second
symptom besides memory: a `File` in a cycle keeps its descriptor.

**Objects alive at exit.** The language promises nothing about destructors of
objects still alive when the program ends, as Swift does not and Rust does
not for statics. In practice, every local of the top level is released when
the entry body falls off its end -- the same release that makes `__rc_live=0`
hold -- so their destructors do run, in reverse order of declaration, before
the program waits for its threads. What does not run: anything still held by
a thread that is running when the program exits, anything in a cycle, and
everything after `os.exit` or a trap, which leave without releasing.

## Recursion depth of release -- fixed 2026-09-23

Releasing an object released its fields from inside its drop function, so
freeing a long chain recursed once per link. A destructor did not deepen it:
the destructor returns before any field is released, so it was never on the
stack beneath the chain. Measured when destructors landed, on a singly
linked list of `Node`s through an `enum Link` (two objects per link),
default 8 MiB stack:

| nodes | -O0 | -O2 |
|---|---|---|
| 50 000 | ok | ok |
| 100 000 | segfault | ok |
| 200 000 | segfault | segfault |

identical with and without a destructor on `Node`. lib/encoding/json.m31 met the same
limit building values (a 10 000-deep value needed MBs of stack at -O0, and
it went iterative).

**Now fixed in `rc_dec` itself** (runtime/rt.c). The first decrement to reach
zero owns the release and runs a loop; every decrement that reaches zero
underneath it hands its object to a thread-local queue instead of calling
back into `rc_dec`. One C frame, whatever the shape of the graph. The queue
is a fixed 64-entry array that grows on the heap only for a graph wide
enough to need it, and hands that buffer back when it drains, so nothing is
held between releases and LeakSanitizer sees nothing at exit. Thread-local
because the counts are: two threads never reach one object.

Measured after, the same program:

| nodes | -O0 | -O2 |
|---|---|---|
| 1 000 000 | ok | ok |
| 10 000 000 | ok | ok |

with `__rc_live=0` in every case. `corpus/core/979` keeps 200 000 of them in
the gate -- both numbers that used to crash -- which is small enough to run
four times under two sanitizers without being noticed.

**What it cost.** The order across objects. The queue is FIFO, so an owner
is released, then everything it held, then everything those held:
breadth-first, where the recursion was depth-first. Within one object
nothing changed, and that is the part the order guarantee above names -- the
destructor runs before its own fields, and the fields go in declaration
order. `corpus/core/770` did not move, because everything in it is also held
by a local and dies one local at a time; `corpus/core/980` is the new case
that shows the difference and pins the answer. Getting depth-first back
would mean every drop function, generated and runtime alike, handing its
children over in reverse so a LIFO stack unwound them in order -- a change
to `src/emit_c.rs` and to every collection in the runtime, to restore an
order nothing promised.

**What it did not cost.** Two separate releases still run in their own
order, because the first drains the queue before it returns: the top-level
locals still die in reverse order of declaration. Out of memory while
growing the queue falls back to releasing that object in place, which
recurses -- the old behaviour, which is correct, and better than trapping
halfway through freeing a graph.

**What it did cost, and should not have**: everything a destructor's body
dropped went into the same queue. That is the next section.

## A destructor's body is ordinary code -- fixed 2026-09-23

Implemented in `runtime/rt.c` (`rt_drop_enter`, `rt_drop_leave`, and the
note "a destructor's body is top-level code"), `runtime/rt.h` (`RcDrain`)
and `src/emit_c.rs` (the two lines that bracket the call to the destructor).
Tests corpus/core/1003-1006. The reference is §4.4 and §7.1.

### What went wrong

`rc_dec` set one thread-local flag, `rc_releasing`, for the whole drain, so
**every** decrement that reached zero anywhere in code reached during that
drain was queued rather than run -- including the decrements a destructor's
own body made. The queue is drained by the outermost `rc_dec`, so a
destructor's temporaries were not freed until the release that called it had
finished walking the whole graph. Measured 2026-09-23:

| | at the top level | inside a destructor |
|---|---|---|
| `churn(3000)`, open and drop a file each time, `ulimit -n 1024` | 3 000 files, fine | **EMFILE at iteration 1021** |
| 3 000 000 short-lived four-field objects, peak RSS | 1.5 MB | **212 MB** |
| a value made and dropped inside a destructor body | its `drop` runs there | **runs after the enclosing destructor returns** |

The third is the one that is a language question rather than a resource
leak. Reference §4.4 says a destructor runs when the count reaches zero
"and never at any other time"; it ran at some other time. The first two are
that same fact costing descriptors and memory.

### The rule

**The queue belongs to the structural walk, and to nothing else.** An
object's *fields* are queued -- that is the 10 000 000-link chain the queue
exists for. The code a destructor's *body* runs is not part of the walk, and
behaves exactly as the same code does at the top level: the first decrement
in it to reach zero owns a drain of its own and runs it before the body's
next statement.

`rt_drop_enter` and `rt_drop_leave` bracket the call to the destructor, and
nothing else -- `src/emit_c.rs` emits them either side of it, which is the
one place the runtime's release reaches the program. `enter` sets the queue
state aside and hands the body a fresh, empty one; `leave` puts the outer
walk's back, untouched. Five words, saved in the drop function's own frame,
no allocation, nothing that can fail.

Measured after, the same three cases: 3 000 files fine inside a destructor
too; 1.5 MB inside a destructor against 1.5 MB at the top level; and `drop
Tmp` printed inside the body that made it. The 200 000-link chain of
corpus/core/979 still releases in one frame, and so does a 10 000 000-link
one, with a destructor on every link (corpus/core/1006) or without. The
bracket costs nothing measurable: 2 000 000 destructor calls in a chain
release, 289.8 ms with it against 291.0 ms with it stubbed out, gcc -O2,
best of seven interleaved runs.

### Why not drain the outer queue instead

The obvious alternative -- at the boundary, finish the pending queue before
running the body -- is wrong twice. It releases objects the walk has not
reached yet, so a destructor's body would see its siblings destroyed around
it, in an order that depends on where in the graph it sits. And it does that
draining *from inside the body*, one nested level per link, which is the
recursion the queue was built to remove: a chain of objects with destructors
would overflow the stack again.

### What is guaranteed, exactly

  - A destructor runs **when the count reaches zero**, on the thread that
    let go, before that release does anything else with the object. There is
    no other time, and nothing defers it.
  - Inside a destructor's body, **releases behave as they do at the top
    level**: a value made and let go there is destroyed there, before the
    next statement, and before the destructor returns.
  - **Across objects the walk is breadth-first** and costs one C frame
    whatever the shape of the graph (corpus/core/980, 979, 1006).
  - **Within one object**: the destructor first, on a whole object, then the
    fields in declaration order (corpus/core/770).
  - A destructor body that drops a value whose destructor drops another
    nests as ordinary calls nest; the depth is the program's, as any
    recursion's is.

## Found on the way

**A method called `close` cannot be called bare from a sibling.** `close` is
also the channel builtin, so a bare `close()` inside a method is refused as
ambiguous, and `this.close()` is refused because a sibling is called bare.
`File.drop` therefore calls `stream_close(s)` rather than `close()`. The
destructor did not cause this; it is where it first mattered. Either
spelling should be made to work -- probably `this.m()` allowed where the
bare name is ambiguous.

**A function returning a module-qualified type does not parse at the top
level**: `io.File must(str p) { .. }` fails with "expected `=`". A parameter
of that type is fine. corpus/core/774 works around it.
