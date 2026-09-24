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
> (see §5). `io` and `fs` cross at descriptors and names -- one sys-layer
> call per primitive (§7) -- with buffering, line splitting and errno
> policy in the language. The mechanism below is unchanged; what moved is
> where the line is drawn.

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
prim int  __open(str path, int flags, int mode);      // fd, or -errno
prim int  __read(int fd, bytes buf, int off, int n);  // bytes read, 0 at the end, or -errno
prim int  __fstat(int fd, List<int> out);             // pushes size, mode, mtime; 0 or -errno
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

**It has one lowering rule.** `__open` calls `rt_open`: strip the
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
  - **Amended 2026-09-21: a `prim` may write into a `bytes` it was handed**,
    and that is the only in-place mutation across the seam. It exists
    because `read(2)` fills a buffer the caller owns; the alternatives —
    returning a fresh `bytes` per read, or pushing octets one at a time —
    allocate or loop per call where a buffered reader wants neither. Three
    conditions make it containable, all enforced in the runtime wrapper and
    none trusted from the caller:
      - the write stays inside a range `[off, off + n)` that the wrapper
        checks against the buffer's **size** (not its capacity) before the
        system call, and a range outside it **traps** — it is a bug in the
        library, and the kernel would otherwise write past the allocation;
      - the buffer's size never changes: the prim neither grows nor
        shrinks it, so a `bytes` stays exactly what the language last made
        it, with every byte either what it was or what the kernel wrote;
      - the result says how many bytes were written, and bytes past that
        are unspecified but still in 0..255, so no invariant of `bytes`
        (reference §3.10) can be broken by a short read.
    `__read(fd, buf, off, n)` and `__listdir(path, buf)` are the two that
    use it (§7). A `str` is never written: it is immutable, and
    `__write_str` only reads one.

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

`from_errno` is ordinary language source in `lib/io.src`: a chain of
comparisons on the handful of numbers worth naming, everything else to
`Other`. The runtime returns the raw errno — in Linux numbering on every
host, `docs/sys-layer.md` §1 — and does not interpret it, so the one place
that knows what `2` means is readable, testable, and not in C.

It is public, because `fs` builds the same errors from the same
primitives: one vocabulary for everything that touches a file.

### Revised 2026-09-21: which variants are named

With `fs` using `io.Error` too, the set grew from three to seven, before
the freeze rather than after it:

| variant | errno | why a caller branches on it |
|---|---|---|
| `NotFound` | ENOENT 2 | the file may simply not be there yet |
| `PermissionDenied` | EACCES 13, **EPERM 1** | which of the two the kernel says depends on the file system (Go's `ErrPermission` and Rust's `PermissionDenied` both take both) |
| `IsDirectory` | EISDIR 21 | a path meant as a file |
| `NotADirectory` | ENOTDIR 20 | a path through a file; `listdir` on a file |
| `AlreadyExists` | EEXIST 17 | `mkdir` of something there; Go's `ErrExist` |
| `NotEmpty` | ENOTEMPTY 39 | `rmdir` of a directory still in use |
| `InvalidUtf8` | — | `io.read` of a file that is not text, a file name that cannot be a `str`: the one failure that is the library's own |
| `Other(int)` | anything else | printed, never promised |

Adding them broke every exhaustive `match` on `io.Error`, which is the
argument above made concrete: it is affordable exactly once, now.

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

*Done 2026-09-21 -- see §7. What follows is the reasoning as it stood.*

`io` currently crosses at whole-file primitives (`rt_file_read` and
friends), which puts buffering, line splitting and errno policy in C. Below
those, every system call already goes through one small layer with a libc
and a raw-Linux implementation -- `docs/sys-layer.md`, which also lists the
fd-based primitives `io` moves to. Moving the seam down to `open`/`read`/
`write`/`close` puts all of that in the language, and needs three things the
language does not have yet. Each is a core feature, not a library one, so
each is a freeze decision:

  - **A mutable byte buffer.** `str` is immutable, so a `read(fd, buf, n)`
    has nothing to read into. Every codec (base64, UTF-8, hashing) needs the
    same thing. *Done: `bytes`, reference §3.10 -- one byte per element, a
    capacity that `clear` keeps, and strict `utf8()` decoding back to text.
    `lib/io.src` is not rewired onto it yet.*
  - **Bitwise operators** `& | ^ ~ << >>` on `int`. Hashing, codecs, UTF-8
    decoding and float formatting are all bit manipulation; today they
    cannot be written at all. **Done**: reference §6.1, with the wrapping
    arithmetic and a float's bits (`to_bits`, `float.from_bits`) in §6.5a.
  - **A name for the receiver.** A method cannot name its receiver, so an
    enum method cannot `match` on itself and no method can pass itself to a
    function. `io.Error.to_str()` cannot be written because of it; so cannot
    any `describe()` on any enum. *Done: `this`, reference §4.3. Every
    stdlib error type now has a `to_str()`, so `print(e)` prints it, and the
    free `message(e)` functions are gone.*

Float formatting and parsing (`print` of a float, `parse_float`) were the
other C above the OS. Both are pure computation, and both are now language
source -- see §6. The runtime has no `strtod` and no float `snprintf` left.

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

---

## 6. The process primitives

`os`, `date`, `random` and `args` added one marked section to the runtime,
"process primitives", and it holds only facts the operating system owns:

| prim | answers |
|---|---|
| `__args(List<bytes> out)` | pushes every `argv[i]` as octets, `argv[0]` first (the emitted `main` hands argc/argv to `rt_args_init`); `os.args` decodes |
| `__env_map(List<bytes> out)` | pushes every variable as name, value, name, value ... octets, from `environ`; `os.env_map` decodes and skips what is not text |
| `__env(str name, List<bytes> out)` | 1 and pushes the value as octets, or 0 if unset; `os.env` decodes |
| `__exit(int code)` | flushes stdout, then `exit` -- the 0..255 check is in `lib/os.src` |
| `__clock(List<int> out)` | pushes seconds and nanoseconds from ONE `CLOCK_REALTIME` reading |
| `__entropy(int n, List<int> out)` | pushes n octets from `getentropy`; 0, or an errno. `rt_entropy` cuts the request into 256-octet calls, because that is all `getentropy` answers at once -- which is why `random`'s pool is 256 and not larger |

There was a sixth, `__panic(str msg)`, the one that was not an OS fact: library
source had no other way to say "this is a bug in the caller", and a library's
own preconditions (`random`'s `integer(3, 1)`, declaring `--help` on an
`args.Parser`, a timestamp outside `date`'s years) deserve a trap rather than
a `Result` that docs/errors-decision.md says a caller's bug must not get. It
is gone: that need was never the standard library's alone, and every program
now has it as the builtin statement `trap(msg)` (reference §6.6), which the
library uses like any other program. Its runtime half, `rt_panic`, stayed.

Everything above them is source: argv[0]'s inclusion, what unset means, the
calendar, rejection sampling, PCG on the wrapping methods and bitwise
operators, the pool that keeps `__entropy` to one call per 256 octets
(`random.System`, holding an `io.Buffer`), and the whole of `args`.

---

## 7. Float text is a module the compiler calls

`print` of a float, `f.to_str()`, `str(f)` and `s.parse_float()` are
language source: `lib/__floatfmt.src`, whose `format(float) -> str` and
`parse(str) -> Option<float>` the lowering calls by their qualified names
where it used to call `rt_print_float`, `rt_float_to_str` and
`rt_str_parse_float`. `print` formats and then prints the string.

**Formatting is Schubfach** -- the shortest decimal that reads back, from
three 128-bit products and no loop -- then laid out in the format the C
runtime always printed, byte for byte: C's `%.{p}g` at the smallest `p` that
reads back, with a whole number below 1e17 written out in full. That rule is
not quite "shortest": at 46 powers of two the closest shortest-length decimal
falls outside the narrow side of the lopsided rounding interval, and C then
prints one digit more. The module reproduces that exactly rather than change
what a program prints. **Parsing is Eisel-Lemire**, fast_float's
`compute_float` line for line, which settles every input of up to 19
significant digits; a longer one that it cannot settle is decided exactly by
comparing it with the halfway point between two floats in big integers.

It was checked differentially against the C it replaced: 1,433,824 floats
(uniform random bit patterns, subnormals, every exponent at its edges, powers
of ten and their neighbours, short decimals, whole numbers around 2^53 and
1e17) format to the same bytes and read back to the same bits, and 1,500,073
decimal strings (random digit counts up to 40 across the whole exponent
range, exact halfway points and a hair either side of them, 800-digit
inputs, and malformed text) parse to the same bits as glibc's `strtod` --
under gcc and clang at -O0 and -O2, with no mismatch. It is faster than the C at
-O2 (the old code called `snprintf` and `strtod` up to 17 times per float).

### Four decisions

  - **Loaded only when a program could need it.** The loader adds the module
    when any file contains a float literal, the keyword `float`, or the name
    `parse_float` -- every float a program holds was written as a literal,
    has its type spelt somewhere, or came out of `parse_float`, because
    there is no inference that could produce one silently. Always loading it
    was the alternative, and it costs every program: measured on a one-line
    program, loading it adds about 7,000 lines of C, 150 ms to a gcc -O0
    build and 350 ms to a gcc -O2 one, and 40 KB to an -O2 binary. If the
    check ever misjudges, the lowering reports a compiler bug by name rather
    than leaving an undefined function to the C compiler.
  - **Invisible.** The module is called `__floatfmt`. The parser refuses a
    `__` name in anything a program writes, `import` included, so no program
    can import it, name its functions, or collide with it, and the entry file
    cannot be called `__floatfmt.src` either. It is not a module users need:
    the conversions it provides are already spelt `print`, `to_str`, `str`
    and `parse_float`.
  - **It cannot recurse.** A `print` or `to_str` of a float inside the module
    would call itself. The lowering refuses both there, so the module is
    written without them -- it builds text from `int.to_str()`, which is C's
    and stays C's: it formats an integer, which is not the computation this
    moved.
  - **Its table is text.** Schubfach and Eisel-Lemire share one table of
    684 powers of ten to 128 bits. With no module-level constants in the
    language, it is string literals of hex digits behind a binary search;
    literals are immortal, so a lookup allocates nothing. The generator is a
    few lines of Python, quoted in a comment above the table. A search tree
    with a `return` per word was tried first and made the emitted C five
    times larger. A constant array would be the natural home for it, and is
    a language decision this does not take.

---

## 8. The file primitives: `io` and `fs` over descriptors

Decided and built 2026-09-21. `io` crossed at whole-file primitives
(`rt_file_read`, `rt_file_write`, `rt_file_append`, `rt_stdin_line`,
`rt_stderr_write`), which put the read loop, the stdin buffer, line
splitting, the `\r\n` rule and EINTR handling in C. They are gone. Each
primitive now is **one** sys-layer call (`runtime/sys.h`) with its result
passed through unchanged -- a value, or -errno -- and everything that
decides anything is in `lib/io.src` and `lib/fs.src`:

| prim | sys call | answers |
|---|---|---|
| `__open(str path, int flags, int mode)` | `sys_open` | fd |
| `__read(int fd, bytes buf, int off, int n)` | `sys_read` | bytes read into `buf[off..]`, 0 at the end (§2, amended) |
| `__write(int fd, bytes buf, int off, int n)` | `sys_write` | bytes written, possibly short |
| `__write_str(int fd, str s, int off, int n)` | `sys_write` | the same from a `str`, so text is not copied into `bytes` to be written |
| `__close(int fd)` | `sys_close` | 0 |
| `__seek(int fd, int off, int whence)` | `sys_lseek` | the new offset |
| `__fstat(int fd, List<int> out)` | `sys_fstat` | pushes size, mode, mtime_ns |
| `__stat(str path, bool follow, List<int> out)` | `sys_stat` | the same, by name; `follow` false is lstat |
| `__mkdir(str path, int mode)` | `sys_mkdir` | 0 |
| `__unlink(str path)` | `sys_unlink` | 0 |
| `__rmdir(str path)` | `sys_rmdir` | 0 |
| `__rename(str from, str to)` | `sys_rename` | 0 |
| `__symlink(str target, str path)` | `sys_symlink` | 0 |
| `__listdir(str path, bytes buf)` | `sys_listdir` | the bytes the whole listing needs; names NUL-separated in `buf` as far as it holds |
| `__out_flush()` | -- | writes out what `print` has buffered |

The C that is left does only what the language cannot: turn a `str` into
a C path (refusing an empty one or one holding a NUL, which the kernel
would truncate into a different name), and check a buffer range before
the kernel writes into it.

`__out_flush` is the one that is not a system call. `print` keeps its own
buffer in the runtime (`docs/sys-layer.md` §3), so a program writing to
descriptor 1 or 2 through `io` has to empty that buffer first or the two
paths to one stream reorder. `File.write` and `eprint` call it for fds 1
and 2; the policy -- when to flush -- is in the library.

### What moved into the language

  - **The read buffer.** A `File` reads 64 KiB at a time into a `bytes`
    it owns, allocated on the first read. Writes are unbuffered, so there
    is no `flush` to forget (Oro's rule).
  - **Whole-file reads.** `read_all` asks `__fstat` for a size hint and
    allocates once for a regular file, then confirms the end with one more
    read; a pipe takes the chunk loop.
  - **EINTR.** Every read and write loop retries on `-4`. `close` never
    does: on Linux the descriptor is gone by then.
  - **The push-back.** A read that fails part way — `read_all` and
    `read_until` both collect as they go — puts what it had collected back
    in front of the stream before it returns the error, so `Err` never means
    "and those bytes are gone". What goes back can be larger than the 64 KiB
    buffer it came out of, so it *becomes* the buffer and the next refill
    allocates a chunk again. There is no matching promise for a write and
    there cannot be: the kernel already has those bytes. `write`'s `Err`
    therefore means an unknown prefix was written and the stream must be
    closed, and `write_some` — one attempt, an exact count, `Err` only when
    nothing moved — is the escape hatch for a caller that has to resync
    instead. It is on `File`, `Buffer` and `net.Conn` and deliberately not
    in `io.Stream`, which stays the five methods `copy` and `read_line_of`
    are written against.
  - **No `read_line` without state.** A module cannot hold a buffer between
    calls, so `io.read_line()` could not read ahead at all: on a pipe or a
    terminal it read one byte per call to `read(2)`, as a shell's `read`
    builtin does; on a seekable descriptor (`prog < file`) it read a
    growing chunk and seeked back to just past the newline. That bought the
    guarantee that the rest of standard input was still there for
    `io.stdin()` or a child process, and nothing could make it cheap — the
    cost *was* the contract. It is **removed**: line reading is
    `io.read_line_of(stream, limit)`, written against `io.Stream`, and the
    program holds the stream (`io.stdin()`, called once). The buffer lives
    in the object, which is the answer everywhere else here too.
  - **Text vs octets.** `io.read` is `read_bytes` + strict `utf8()`,
    failing with `InvalidUtf8`; `io.read_bytes` is the file exactly. That
    is forced as much as chosen: `bytes.utf8()` is the language's only way
    from octets to a `str`, so a reader over `bytes` cannot return an
    unchecked `str` without a new primitive, and adding one would reopen
    reference §3.10's question in the wrong direction. *Since decided:* a
    `str` is always valid UTF-8 (docs/text-decision.md), and `__args` and
    `__env` now hand over `bytes` for the same reason.

### Why `__listdir` is one stateless call

The raw backend lists with `getdents64` on a descriptor; the C library
lists with a `DIR *` that owns one, through `fdopendir` and `readdir`. An
open/next/close triple would need the language to hold a handle whose
meaning differs per backend and that leaked if not closed (there were no
destructors then; docs/destructors-decision.md added them later, which
removes the leak but not the per-backend handle). One call that writes every name into a caller's buffer, and
says how big a buffer it needed if that one was too small, has no handle
at all and returns byte-identical results from both backends, which
`runtime/sys_test.c` checks.

---

## 9. The socket primitives: `lib/net.src` over descriptors

Decided in `docs/sys-layer.md` §9 and built 2026-09-23. `lib/net.src` is TCP
— connections, listeners, addresses — and it is language source down to
fifteen primitives, each **one** sys-layer call with its value or `-errno`
passed through:

| prim | sys call | answers |
|---|---|---|
| `__socket(int domain, int kind, int protocol)` | `sys_socket` | fd |
| `__bind(int fd, int family, int port, bytes addr)` | `sys_bind` | 0 |
| `__connect(int fd, int family, int port, bytes addr)` | `sys_connect` | 0 |
| `__bind_path(int fd, str path)`, `__connect_path` | the same, AF_UNIX | 0 |
| `__listen(int fd, int backlog)` | `sys_listen` | 0 |
| `__accept(int fd, List<int> out, bytes addr)` | `sys_accept` | fd; pushes the peer's family and port, writes its 16 address bytes |
| `__sockname(int fd, int peer, List<int> out, bytes addr)` | `sys_getsockname`/`getpeername` | 0, the same way |
| `__sockpath(int fd, int peer)` | the same | the AF_UNIX path, `""` if none |
| `__shutdown(int fd, int how)` | `sys_shutdown` | 0 |
| `__setsockopt(int fd, int opt, int value)` | `sys_setsockopt` | 0 |
| `__getsockopt(int fd, int opt)` | `sys_getsockopt` | the value |
| `__poll(List<int> fds, List<int> events, List<int> revents, int timeout_ms)` | `sys_poll` | how many are ready; clears `revents` and pushes one per descriptor |
| `__resolve(str host, int port, int family, List<int> out, bytes addrs)` | `sys_resolve` | how many addresses the name **has**; `-38` on the raw backend, always |
| `__ignore_sigpipe()` | `sys_ignore_sigpipe` | 0 |

`__read`, `__write` and `__close` are re-declared from §8 rather than added:
a socket is a descriptor, so `rt_read`, `rt_write` and `rt_close` already
work on one. A `prim` is a declaration like any other and `__` names belong
to the module that writes them, so both declarations lower to the same C
symbol by the same rule.

The C that is left in the marked section of `runtime/rt.c` does only what C
must: it takes a `SysAddr` apart into scalars plus sixteen bytes and puts it
back (a prim returns a scalar or a `str`, or writes into a collection it was
handed — §2), it builds the kernel's `struct pollfd` array, which the layer
itself may not allocate, and it checks that an address buffer really is
sixteen bytes before the kernel writes into it.

### What moved into the language

  - **The address parser and printer.** Dotted quad and the whole of IPv6
    text, `::` compression and an embedded IPv4 tail included, plus the
    RFC 5952 canonical printer that reads back. The sys layer deliberately
    has no `inet_pton`: turning `"127.0.0.1"` into four bytes is not a system
    call, it is the same work on both backends, and it is the kind of code
    that is wrong at the edges — so it lives where it can be read and where
    the corpus can check all ninety-four forms against Python's `ipaddress`.
  - **SO_REUSEADDR's default**, the backlog's default, and `Conn`'s 64 KiB
    read buffer.
  - **The short-write loop and the EINTR retries**, as in `io`.
  - **The timeout contract**, which is `io`'s and is cashed here. A read
    timeout is the ordinary way a read fails with half a message in hand, so
    `read_all` and `read_until` put that half back and the retry reads the
    message whole — and `wait` then answers `true` from the buffer without
    asking the kernel. A send timeout is the ordinary way a write fails with
    megabytes already delivered, and nothing can take those back, so
    `Conn.write`'s `Err` says the connection is finished and `Conn.write_some`
    is what a sender that must keep its offset uses instead.
  - **The errno vocabulary**, `net.from_errno`, in source beside `io`'s.

### Two error enums, and why that is not a mistake

`net.Conn`'s five stream methods answer with **`io.Error`** and everything
else in `net` answers with **`net.Error`**. That split is forced by
structural interfaces: an interface *is* its signatures (reference §3.4), so
a `read` returning `Result<bytes, net.Error>` would not satisfy `io.Stream`
and `io.copy` could not take a socket at all — which would throw away the
main thing a connection being a stream buys. Going the other way and
reusing `io.Error` for the whole module is worse: `connect` would report a
refused connection as `Other(111)`, and that is the outcome programs branch
on most.

It is liveable because the seam is **exact, not lossy**. Every errno a
socket produces — ECONNRESET 104, EPIPE 32, ETIMEDOUT 110, EAGAIN 11 — is
one `io.from_errno` does not name, so it arrives through a stream method as
`io.Error.Other(n)` with the number intact and `net.error_of(e)` converts it
back with nothing lost.

The third option, adding `Refused`, `TimedOut`, `Unreachable`, `Reset`,
`InUse` and `NameNotFound` to `io.Error`, is the one that would give a
program a single vocabulary. It is not `net`'s to take: §4's argument is
that an exhaustive `match` with no default makes a new variant a compile
error in every program that handles the error, and that this is affordable
exactly once — which was spent when `fs` grew the set from three to seven.
If it is ever paid again, `net.Error` folds into `io.Error` and `error_of`
goes away; nothing here is shaped to prevent that.

### `__resolve` is the one primitive with two answers

On the C library backend it is `getaddrinfo`. On the raw backend it is
`-ENOSYS`, permanently, because `getaddrinfo` is glibc's NSS machinery and a
static binary with no C library cannot `dlopen` the modules it needs
(`docs/sys-layer.md` §9). `net` treats that as an ordinary error on every
path that uses a name — which is the right shape anyway, since DNS fails on
real networks — and `net.Error.to_str()` gives errno 38 a message of its
own: *"this build cannot resolve host names; use a literal IP address."* A
program sees an `Err` that says what is wrong and what to do, not a crash,
and the corpus asks the question in a form whose answer is the same on both
backends.

## 10. `lib/http.src` needs no primitive at all

Added 2026-09-23. It is the first module with **no `prim` line in it**, and
that is the point rather than a happy accident: HTTP/1.1 is a wire format,
and a wire format baked into a runtime that promises to freeze is a set of
edge cases nobody can ever change. Oro says the same of its own
`std/http.oro` in its first paragraph — *"there is no Rust `http` module and
there should never be one"* — and the argument transfers exactly.

What it is written over is `net` and `io` and nothing else. The header block
is one `read_until(CRLFCRLF, MAX_HEAD)`; the lines are `bytes.split(CRLF)`;
the colon is `bytes.index_of`; a sized body is a loop over `read`; a chunked
body is a `read_until(CRLF, MAX_CHUNK_LINE)` for each size line and a `read`
for each chunk. Every one of those is a method §8 and §9 already put in the
language for reasons of their own, which is the test a primitive has to pass
(§2): `http` asked for nothing new, so nothing new was added.

Two consequences worth writing down.

**The parsers take an `io.Stream` and never a socket.** `read_request`,
`write_request`, `read_response` and `write_response` are declared against
the interface, so the whole grammar is driven from an `io.Buffer` in
`corpus/modules/stdlib-http*` with no listener, no port and no timing —
which is what makes it possible to check ninety-odd refusals in one
deterministic program, and to compare each one against Python's
`http.client` and `email.parser`. Only `serve`, `serve_conn`, `get` and
`fetch` mention `net`, because only they set a timeout or make a connection.

**There is no TLS and no plan to add one below the language.** `parse_url`
refuses `https://` before a socket exists. A TLS stack is not a system call
and does not belong at this seam; when there is one it will be a module up
here, over the same `net.Conn`, and `http` will take a `Stream` from it
without changing a line — which is the other thing the interface buys.
