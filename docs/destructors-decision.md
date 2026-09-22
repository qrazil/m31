# Destructors: the decision

Decided **2026-09-21**. Implemented in the same change: `src/lower.rs`
(`check_destructor_decls`, `refuse_destructor_call`), `src/emit_c.rs` (the
drop function), `lib/io.src` (`File.drop`), tests `corpus/*/77N-*` and
`corpus/modules/destructor-*`. The reference is §4.4.

---

## The problem

`io.File` dropped without `close()` kept its descriptor until the program
ended, and every `?` between an `open` and its `close` skipped the close.
lib/io.src said so in a doc comment -- "a `File` is not closed for you ...
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
*type* may have one: `void Box<T>.drop()` is instantiated with each
instantiation of `Box`, like its other methods.

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

## Recursion depth of release

Releasing an object releases its fields from inside its drop function, so
freeing a long chain recurses once per link. That is not new -- corpus
programs have always done it -- and a destructor does not deepen it: the
destructor returns before any field is released, so it is never on the stack
beneath the chain. Measured on this change, a singly linked list of `Node`s
through an `enum Link` (two objects per link), default 8 MiB stack:

| nodes | -O0 | -O2 |
|---|---|---|
| 50 000 | ok | ok |
| 100 000 | segfault | ok |
| 200 000 | segfault | segfault |

identical with and without a destructor on `Node`. lib/json.src met the same
limit building values (a 10 000-deep value needed MBs of stack at -O0, and
it went iterative). Recorded, not fixed: the fix -- an explicit work list in
`rc_dec` -- is a runtime change of its own, and it interacts with the order
guarantee above (a deferred release would run a field's destructor later
than its owner's return, still after it, which is what the rule says).

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
