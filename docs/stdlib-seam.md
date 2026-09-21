# The seam — where the language stops and C begins

`docs/stdlib-decision.md` promised a standard library **written in this
language**, and then listed `io` first. `io` is where that promise meets the
fact that reading a file is a syscall. This decides how a library written in
the language reaches C, without that becoming a general FFI.

It is four decisions: where stdlib source lives, how it calls out, who is
allowed to, and what `io`'s error type looks like so it survives the freeze.

> **Revised 2026-09-21: the standard library is standalone.** Everything in
> `lib/` is written in the language, and `prim` exists only where a language
> genuinely cannot reach: the operating system. `math` has no `prim` at all
> (see §5). `io` still crosses at the C library's file functions and is
> scheduled to move down to raw system calls once the language has the
> features §5 lists. The mechanism below is unchanged; what moved is where
> the line is drawn.

---

## 1. Stdlib source is embedded in the compiler

`lib/io.src` is an ordinary source file in this repository, compiled by this
compiler, with no privileges beyond §3 below. It reaches a program through
`include_str!` — the compiler binary carries the text.

`import io;` therefore resolves without a search path, without an install
step, and without an environment variable. That matters more than it sounds:
**every other answer has a way to be wrong on the user's machine.** Go has
`GOROOT` and a decade of issues about it being wrong; C has an include path
that differs per distribution; Python has `sys.path` and a startup sequence
long enough to have its own documentation page. The compiler here is one
binary with no dependencies, and the standard library not changing that is
worth an `include_str!`.

A file named `io.src` next to the program is then a **collision**, reported
by the same rule that already refuses two modules with one name. It is not
shadowing and not an override: module names are globally unique, and `io` is
taken.

The cost is honest and small: updating the standard library means rebuilding
the compiler. For a language that intends to freeze, that is the expected
case rather than the painful one.

### This is still a library, not compiler internals

Nothing in `lib/` is privileged AST or a hard-coded type. It is parsed,
typechecked, monomorphised, refcounted and emitted exactly like the program
that imports it — same diagnostics, same gates, same corpus. If a construct
is awkward to write there, the language is wrong, and that is precisely the
feedback a standard library is for.

---

## 2. Calling out is a `prim` declaration

A primitive is a function with a written signature and **no body**:

```c
prim int  __file_read(str path, List<str> out);   // 0, or an errno
prim int  __stdin_line(List<str> out);            // 1 read a line, 0 at the end
prim void __stderr_write(str s);
```

`prim` is a declaration, not an expression form and not an attribute. Two
properties fall out of that and both are the point:

**It goes through the whole front end.** A `prim` is parsed, its written
types are resolved and checked for visibility like anybody else's, and
monomorphisation sees it — so a `List<str>` parameter gets instantiated because
the declaration *mentions* it, not because the compiler was taught to build
that type from the inside. The alternative, registering a built-in signature
in the lowerer, cannot name a generic instantiation at all: generics are
erased before lowering (§`docs/ir-v0.md`), so by the time the lowerer runs
there is no machinery left to ask for `List<str>`. Declaring the
primitive in source puts it on the right side of that erasure.

**It has one lowering rule.** `__file_read` calls `rt_file_read`: strip the
leading underscores, prefix `rt_`. No table of special cases in the lowerer,
no per-primitive code, and a missing runtime function is a link error naming
the exact symbol. Arguments are borrowed and the return is owned, §5 of the
reference, with no exception carved out — the runtime function has the same
contract as `rt_concat`, which has had that contract since the first week.

### What a primitive may not be

A `prim` body is C's, so the checks the language relies on cannot see inside
it. Three restrictions keep that containable, and all three are checked:

  - **No generic `prim`.** `prim T __id<T>(T x)` would need a runtime
    function per instantiation. A primitive is one C symbol.
  - **No `prim` method, and no `prim` on a user type's field.** The seam is
    free functions only, so the set of C symbols a program can reach is
    exactly the set of `prim` declarations, listed in one place per module.
  - **Every `prim` deals only in what the runtime can build**: a scalar, a
    `str`, or an element pushed onto a collection the caller passed in. Not
    `Option` or `Result` — an earlier draft of this document said those were
    fine, and they are not: they are enums the compiler lays out and gives a
    `TypeInfo`, exactly like a user type. A failure therefore comes back as a
    raw errno, and the library builds its own `Result` in source.

---

## 3. Only stdlib source may use it

`prim` is refused in any module that did not come out of `lib/`, and so is a
call to any name beginning with `__`.

This is the whole reason the seam can exist without being a language
feature. An FFI is a promise about calling conventions, struct layout,
ownership across the boundary, and what happens when the other side
misbehaves — a promise this language is nowhere near ready to freeze. But a
standard library needs to make syscalls *today*. Restricting the mechanism to
source the compiler ships means the set of C the language can reach is
finite, reviewed, and covered by the corpus.

The public surface added by this decision is therefore **zero**. A user
program cannot tell `prim` exists except by reading `lib/`.

`__`-prefixed names are reserved outright — a program may not declare one
either. C reserves them for the same reason and it costs nobody anything.

### When a real FFI arrives

It replaces `prim` rather than joining it. Nothing here is designed to grow
into one: no calling-convention syntax, no layout control, no way to name a C
symbol other than by the transform above. That is deliberate — a mechanism
that is *almost* an FFI is how you end up freezing one by accident.

---

## 4. `io.Error` has an `Other(int)`, and that is not laziness

```c
pub enum Error {
    NotFound;
    PermissionDenied;
    IsDirectory;
    Other(int);
}
```

`docs/stdlib-decision.md` ruled out a single blessed error enum, and the
argument was sharp: **`match` is exhaustive and has no `default`, so adding a
variant is a compile error in every program that handles the error.** That
argument does not stop applying just because the enum belongs to one library
rather than to everybody. `io.Error` would be just as frozen.

`Other(int)` is the answer, and it is the *whole* answer: a failure mode
nobody enumerated arrives as `Other(errno)` rather than as a new variant, so
the enum never has to grow and every existing `match` keeps compiling. The
named variants are the ones a caller plausibly branches on; everything else
is a number the caller can print and the library never has to promise.

The same shape is what Go reached by a different road. `errors.Is` against a
handful of sentinels — `fs.ErrNotExist`, `fs.ErrPermission` — with every
other failure a `*PathError` carrying a raw `syscall.Errno`. Go got there
because its error interface admitted anything; this gets there because the
enum must not grow. Two languages, opposite mechanisms, same three names
worth having.

Rust's `io::ErrorKind` is the counter-example and it is an instructive one.
It is `#[non_exhaustive]` — an attribute that exists *specifically* so that a
`match` on it is forced to have a wildcard arm, because otherwise adding a
kind would break the world. They then added kinds anyway, repeatedly, and
spent years with a pile of unstable ones they could not ship. `Other(int)`
is `#[non_exhaustive]` without needing an attribute, because the wildcard arm
is a real variant with the errno in it, which is strictly more useful than a
`_` that discards it.

### The errno mapping lives in the library

`from_errno` is ordinary language source in `lib/io.src`: a `match` on the
handful of numbers worth naming, everything else to `Other`. The runtime
returns the raw platform errno and does not interpret it, so the one place
that knows what `2` means is readable, testable, and not in C.

---

## Order of work

1. `prim`: parse it, restrict it to `lib/`, reserve `__` names, lower it by
   the name transform.
2. Embedded modules: `lib/*.src` via `include_str!`, resolved by `import`,
   colliding with a same-named local file.
3. `lib/io.src` — `read`, `write`, `append`, `stdin_line`, `stderr`, and
   `Error` with `from_errno`.
4. ~~`lib/math.src` over `prim`~~ -- superseded: `math` is written
   entirely in the language (§5).

Steps 1 and 2 are independent of each other and both are small. Step 3 is
the first time this language is asked to be a library, and the interesting
output of it is the list of things that turn out to be awkward.

---

## 5. Standalone: the line is the operating system, and nothing above it

The first version of `math` crossed the seam for `sqrt`, `pow`, `floor`,
`ceil` and `round`. That was a shortcut, not a necessity: the whole of it is
now language source built from `+ - * /`, comparison and `int(x)`, correctly
rounded where libm is, within an ulp or three where libm is within one, and
the same bits on every target because nothing asks the platform. It links
without `-lm`.

That is the rule from here, and it is Go's. Go's standard library is Go down
to `syscall.Syscall`, which is a few lines of assembly per architecture;
`math.Sqrt` has an assembly fast path *and* a pure-Go fallback, and
`os.ReadFile` is Go all the way to the trap instruction. The only code that
is not Go is the code no language can be: entering the kernel, switching
stacks, atomic instructions. Our equivalent of Go's assembly is a small C
file, and `prim` is how the language names it.

### What `io` needs before it can move down

`io` currently crosses at `fopen`/`fread`, which puts buffering, line
splitting and errno policy in C. Moving the seam down to `open`/`read`/
`write`/`close` puts all of that in the language, and needs three things the
language does not have yet. Each is a core feature, not a library one, so
each is a freeze decision:

  - **A mutable byte buffer.** `str` is immutable, so a `read(fd, buf, n)`
    has nothing to read into. Every codec (base64, UTF-8, hashing) needs the
    same thing.
  - **Bitwise operators** `& | ^ ~ << >>` on `int`. Hashing, codecs, UTF-8
    decoding and float formatting are all bit manipulation; today they
    cannot be written at all.
  - **A name for the receiver.** A method cannot name its receiver, so an
    enum method cannot `match` on itself and no method can pass itself to a
    function. `io.Error.to_str()` cannot be written because of it; so cannot
    any `describe()` on any enum.

Float formatting and parsing (`print` of a float, `parse_float`) are the
other C that remains above the OS. Both are pure computation -- shortest
round-trip printing is the Ryu algorithm, correct parsing is Eisel-Lemire
with a big-integer fallback -- and both move into the language once bitwise
operators exist.

### Why this is what self-hosting needs anyway

A compiler written in the language needs exactly this library: read files,
build strings, hash maps, spawn the C compiler. Each missing feature above
blocks the compiler as surely as it blocks `io`. The path is Go's:

1. The standard library standalone above a syscall-sized C file.
2. The compiler rewritten in the language, compiled by the Rust compiler to
   C, then compiling itself; the build is right when stage 2 and stage 3
   emit identical C.
3. The Rust compiler kept only as the bootstrap, the way Go 1.5 required Go
   1.4 to build -- and eventually a native backend so the C compiler is no
   longer needed either, which is where Go's own toolchain ended up.
