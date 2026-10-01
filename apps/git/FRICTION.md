# Friction: writing a git client in this language

Stage 1 of `apps/git` is about 2 700 lines of the language (five library
modules and a CLI) plus 300 lines of test programs. Everything in it was
written against `docs/reference.md` and nothing in the compiler or `lib/` was
changed to make it easier. This is the record of what got in the way, what was
missing, and what the language was genuinely good at.

It is ordered by how much it cost, not by how interesting it is.

> **Three of these have since been fixed**, and this file has not been
> rewritten to hide that it asked for them — the source has been changed to
> use them, so the diff against this log is the evidence. A `case Tag:` that
> binds none of a variant's payload (§3 — *not* a `default`; exhaustiveness
> is untouched), a range `for` that makes §8's non-terminating `indented`
> impossible to write, and a formatter that keeps the parentheses the author
> wrote, so `sha1.m31`'s rounds are back in the shape FIPS 180-4 gives them
> (§10). A character literal, which this report did not ask for and
> `apps/markdown`'s did, is in too. Everything else below still stands.
> `docs/reference.md` §1.5, §5.5, §5.6 and §6.1 have the rules.

---

## 1. Enums are heap objects, so `Result` cannot enter a hot loop

This is the one that changed a design rather than a line.

The natural DEFLATE bit reader fails: `take(n)` can run out of input. So the
natural signature is

```c
Result<int, Error> Bits.take(int n)          // what I wanted
```

and every use is `b.take(3)?`, which is exactly what `?` is for. But
reference §3.7 says an enum is a reference type, so every `take` allocates a
`Result<int, Error>` on the heap, refcounts it and frees it. A Huffman symbol
costs one `take` per bit — up to fifteen — and a megabyte of output is
several million symbols. That is tens of millions of allocations to report a
condition that occurs at most once per stream.

So the reader cannot fail:

```c
type Bits {
    bytes src;  int at = 0;  int val = 0;  int cnt = 0;
    bool over = false;       // what I wrote
}

int Bits.take(int n) { ...; if (at >= src.size()) { over = true; } ... }
```

and every loop that consumes symbols checks `b.over` once per symbol. It
works, it is fast, and it is strictly worse code: the failure is now a
condition the caller must remember to test rather than one the type system
enforces, and a truncated stream decodes one junk symbol before anyone
notices. Nine places in `zlib.m31` check `over`, and a tenth that forgot to
would silently accept a truncated stream.

**What would fix it:** a `Result` (or any enum) whose payloads are all scalars
being passed flat rather than boxed. Nothing in the source language would have
to change. As it stands, "use `Result` for anything a caller should handle"
and "write a codec" are in tension, and the reference's advice loses.

The same reasoning is why `Huff.decode` returns `-1` for "no code matched"
instead of `Option<int>`. A sentinel, in a language whose whole point is that
`index_of` hides the sentinel from you.

**Update — the compiler now does exactly this, and it is not enough here.**
An enum whose payloads are all scalars is passed and returned by copy, with no
allocation and no refcount (`docs/value-enums.md`). Nothing in the source
language changed, which is what this item asked for.

The rewrite was done in full and measured: `take` and `byte` return
`Result<int, Error>`, `Huff.decode` takes the table it is decoding for and
answers `Err(BadCode(which))` rather than `-1`, every call site is `?`, and
the `over` flag and all nine of its checks are gone. Thirteen lines shorter,
29 `?` where there were 6, correct against all 29 `zlib` fixtures the first
time it ran. The patch is `bench/valenum/zlib-result.diff`.

It is **not checked in**, because it is 0.86x the throughput of the flag, on
both gcc and clang. The reason is a hard edge in the C ABI rather than
anything about enums: `Error` has six variants carrying two `int`s, so it is
24 bytes, so `Result<int, Error>` is 32 -- and System V returns anything over
16 bytes through a caller-allocated stack slot instead of two registers. A
Huffman symbol pays up to fifteen of those round trips.

What the optimisation did buy is the difference between "unaffordable" and
"a judgement call": the same rewrite before it was 0.53x. The numbers, and the
third arm that shows it, are in `bench/value-enums.md` §4.

---

## 2. `?` needs the error types to match exactly, so every layer boundary is a `match`

The diagnostic is excellent —

```
`?` needs the same error type on both sides: this fails with io.Error,
and the function returns object.Error
```

— and it is right, and §6.3 argues the case. But a git client is four layers
(`io` → `zlib` → `object` → `refs`) and every call across a layer is a
ten-line `match` that does nothing but rename the failure:

```c
// what I wanted
bytes stream = io.read_bytes(path)?;          // if io.Error -> Error.Io
bytes plain  = zlib.decompress(stream)?;      // if zlib.Error -> Error.Zlib

// what I wrote
bytes stream = [];
match (io.read_bytes(path)) {
    case Ok(bytes b): { stream = b; }
    case Err(io.Error e): {
        if (!fs.exists(path)) {
            return Result<Object, Error>.Err(Error.NotFound(id));
        }
        return Result<Object, Error>.Err(Error.Io(id, e));
    }
}
match (zlib.decompress(stream)) {
    case Ok(bytes plain): { return parse(id, plain); }
    case Err(zlib.Error e): {
        return Result<Object, Error>.Err(Error.Zlib(id, e));
    }
}
```

Note the second casualty: `stream` has to be declared with a dummy value
(`[]`) before the `match`, because §4.1 requires an initialiser and the real
value arrives inside an arm. That pattern — declare a throwaway, assign it in
one arm — appears seven times in the five modules and the CLI, and each one
is a variable that briefly holds a lie. It is also the only way to get a
value *out* of a `match`, since an arm is a block and not an expression.

Counted over the same six files: eleven `case Err(io.Error …)` arms, one for
`zlib.Error`, and eleven more for this program's own error types, none of
which does anything but rename a failure and return. `Result<…>.Err(` is
constructed 65 times.

**This is a deliberate decision** and I am not asking for implicit conversion.
But something has to give at four layers. The smallest thing that would help
is letting a `match` arm's block be an expression, so the rename is one line:
even `int x = match (e) { ... }` would collapse most of these.

---

## 3. There is no `default` in `match`, and no way to ask an enum which variant it is

`refs.rev_parse` wanted to say: try to resolve a short object name; if it is
*ambiguous* report that, and otherwise fall through to trying it as a ref.
That is one question about one error value. Here is what it took before I gave
up and redesigned:

```c
// what I wrote first -- and deleted
int ambiguity(object.Error e) {
    match (e) {
        case Ambiguous(str prefix, int n): { return n; }
        case NotFound(str a):              { return 0; }
        case Io(str a, io.Error b):        { return 0; }
        case Zlib(str a, zlib.Error b):    { return 0; }
        case NoHeader(str a):              { return 0; }
        case UnknownType(str a, str b):    { return 0; }
        case SizeMismatch(str a, int b, int c): { return 0; }
        case HashMismatch(str a, str b):   { return 0; }
        case BadTree(str a, str b):        { return 0; }
        case BadHeaders(str a, str b):     { return 0; }
        case BadName(str a):               { return 0; }
    }
}
```

Eleven arms, ten of which are the same, every payload spelled out with its
full type so it can be thrown away — and the whole thing breaks the day
`object.Error` grows a variant, which is exactly the property §5.6 wants and
exactly not what this caller needs.

The fix was to change the *other* module's API so the question is never asked:
`object.matching(gitdir, prefix)` returns a `List<str>`, and the caller counts
it. That is a better API, and I would not have found it under less pressure —
so half a point to the language. But `refs.head` needed the same trick a
second time (ask `exists()` before `resolve()`, rather than recognise
`NotFound` afterwards), and by then it was a workaround and not a discovery.

**A `default` arm is additive** (§5.6 says so itself). Twenty of the binding
names in this program are literally spelled `ignored`, and every one of them
also has to spell out a payload type — `case Tree(List<object.Entry> ignored)`
— to discard it. A `case Tree(_):`, or a bare `case Tree:` allowed for a
variant whose payload is unused, would delete all twenty.

---

## 4. `str` has no ordering

`List<str>.sort()` works. `List<Ref>.sort()` needs `int Ref.cmp(Ref)`. And
inside that `cmp` there is no way to compare the two `str` fields:

```c
pub int Ref.cmp(Ref other) {
    if (name < other.name) { return -1; }   // error: cannot compare values of type str
    ...
}
```

So:

```c
int before(str a, str b) {
    int n = a.size();
    if (b.size() < n) { n = b.size(); }
    int i = 0;
    while (i < n) {
        int x = a.byte_at(i);
        int y = b.byte_at(i);
        if (x != y) { return x - y; }
        i = i + 1;
    }
    return a.size() - b.size();
}
```

Twelve lines to re-derive an order the runtime already implements and uses
three lines away. `sort.by` from `lib/sort` does not help: it takes an
`Order<T>` whose `cmp` has the same problem inside it.

The diagnostic is the one place in this whole exercise where a message told me
what was wrong without telling me what to do. `cannot compare values of type
str` is true; it does not mention that `sort()` on a `List<str>` orders them
by their octets, which is what I wanted and what I then wrote by hand.

**What would fix it:** `str.cmp(other)` as a built-in method, returning what
the runtime's own comparison returns. `bytes` has no ordering either (§3.10
says so explicitly, "because `str` has none"), so this is one gap, not two.
Note that `==` on `str` and `bytes` *is* there and is by value, which is the
half that matters more.

---

## 5. Every element access is an out-of-line function call, and it is expensive

SHA-1 runs at **38–45 MB/s**. A plain portable C SHA-1 with no assembly is
around ten times that. The gap is not the algorithm — the code is FIPS 180-4
transcribed — it is the emitted C. Here is the message schedule's inner loop
as the compiler writes it:

```c
v37 = ((T6 *)v0)->f_w;
v40 = rt_index_get(v37, rt_isub(v33, INT64_C(3)));
v44 = rt_index_get(((T6 *)v0)->f_w, rt_isub(v33, INT64_C(8)));
v49 = rt_index_get(((T6 *)v0)->f_w, rt_isub(v33, INT64_C(14)));
v54 = rt_index_get(((T6 *)v0)->f_w, rt_isub(v33, INT64_C(16)));
v56 = ((T6 *)v0)->f_w;
rc_inc(v56);
...
rt_index_set(v56, v33, v63);
rc_dec(v56);
```

Three things, each defensible on its own:

  - `rt_index_get` and `rt_index_set` live in `runtime/rt.c`, which is a
    **separate translation unit** and which `gates.sh` forbids building with
    `-flto` (it reintroduces `-Wfree-nonheap-object`). So gcc cannot inline
    them, cannot hoist the bounds check out of a loop with a known trip count,
    and cannot keep the element buffer's address in a register across two
    accesses. A SHA-1 block is 64 octets and costs about 320 of these calls —
    five per octet hashed.
  - `rc_inc`/`rc_dec` are also forced out-of-line, by a gate of their own.
  - A value read out of a field is retained and released around what follows
    (§7.2), so an **index store through a field name** pays that pair.

The last one is fixable from inside the language, and it is worth 19%:

```c
// 31 MB/s                        // 37 MB/s
sched[i] = ...;                   Array<int> w = sched;   // once, at the top
                                  w[i] = ...;
```

(Best of nine alternating runs of the two binaries, so a load spike hits
both.) One line, one `rc_inc`/`rc_dec` pair per *block* instead of per
*store*, and nothing else about the program changes. I left the local in with
a comment, because the next person to read the file will wonder why it is
there.

What is not fixable from inside the language is the call per element. Whether
that matters depends on what the language is for; it is worth saying plainly
that **an array-indexing numeric kernel currently runs at about a tenth of C**,
and that this is a build-configuration decision (no LTO, forced out-of-line
refcounting) rather than anything about the source language. If `rt_index_get`
were `static inline` in `rt.h` for the non-debug build, most of this would go.

Measured throughput, `cc -O2`, x86-64. **The machine had several other agents'
builds running on it throughout**, so these are the best of several runs rather than a
mean, and the same binaries at a quieter moment gave 45 and 102. Treat them as
a floor, and the ratios between them as the datapoint rather than the
absolutes:

| | best of 9 | quieter moment |
|---|---|---|
| SHA-1, 8 MiB of random octets | 38 MB/s | 45 MB/s |
| inflate, 2.2 MB out, dynamic Huffman | 71 MB/s of output | 102 MB/s |
| loose objects end to end — read, inflate, SHA-1 verify, parse — 4 544 objects, 106 MB of content | 10 MB/s of content | — |

The inflate figure is the surprising one: it is *faster* than SHA-1 despite
decoding one bit at a time, because its inner loop is mostly `bytes` pushes
and local arithmetic while SHA-1's is 480 bounds-checked element accesses
per 64 octets hashed -- seven and a half per octet, each an out-of-line call. It is
also the number that most flatters the language, so it is worth saying that
zlib's own inflate is several times faster again, and that the `puff`-style
decoder here was chosen for clarity.

The end-to-end figure is the one that matters for the application, and it is
below what SHA-1 and inflate together predict (about 25 MB/s). The rest is
`Object.raw` being a `substr` copy of the plaintext, the per-object file open,
and the fact that `log` reads every commit twice (`git.m31` says so at the
place it happens). None of that is language friction; it is this program not
being tuned, and it is fast enough that tuning it would be the wrong work
before packfiles exist.

---

## 6. There is no module state, and top-level variables are invisible to functions

The diagnostic is the best one I met all day:

```
`p` is a local of the program body: the statements at the top level ARE the
body, so a function cannot see them -- pass it in as an argument, or declare
it `const`
```

It says exactly what happened and exactly what to do. But neither of its two
suggestions works for a CLI:

```c
// what I wanted
io.File out = io.stdout();
args.Parser p = args.Parser("ourgit", ...);
...
void write(bytes b) { out.write(b) ... }        // no: `out` is not in scope
```

  - `const` is out: §4.1 refuses a `const` that can hold a resource, and
    `io.File` has a destructor. `args.Parser` is not constructible at compile
    time either.
  - Passing it in works for the parser, and `dispatch` now takes
    `args.Parser parser` purely so it can print `usage()`.
  - For the output stream, threading a `File` through the seven functions that
    print was worse than the alternative, so `write()` calls
    `io.stdout()` afresh every time. That is safe (closing a standard
    stream's `File` leaves the descriptor alone) and it allocates a `File`
    per write.

This is the friction I expect every application to meet. A CLI has exactly
three process-wide things — the output stream, the parser, and the repository
being read — and the language's answer is that two of them become parameters
and one becomes a per-call allocation. `docs/module-state-decision.md` knows
this is open; this is a data point for it, and the shape of the answer it
needs is "a `const`-like binding that may hold a resource and is initialised
once", not general mutable globals.

---

## 7. Missing from the standard library

Each of these cost a hand-written helper. None is large; all of them are the
sort of thing every program that reads a binary format needs.

| wanted | what I wrote | where |
|---|---|---|
| `b.index_of(needle, from)` — a scan from an offset | `int find(bytes hay, int b, int from)`, a loop | `object.m31` |
| a single-**octet** search — `index_of` takes a `bytes` needle, so looking for a NUL means allocating `[0]` | the same `find` | `object.m31` |
| `b.rindex_of(needle)` | `int rfind(bytes hay, int b)`, a loop | `object.m31` |
| `str.cmp` / `bytes.cmp` | `int before(str a, str b)` | `refs.m31` (§4 above) |
| zero-padded integer formatting | `str two(int v)`, `str hex8(int v)`, `str one(int v)` | `git.m31`, `zlib.m31` |
| parse a decimal or octal run of **octets** | `int decimal(bytes)`, `int octal(bytes)` — `str.parse_int` exists but takes a `str`, so using it means building one first, and it accepts `+5` and surrounding space, which these formats do not | `object.m31` |
| an ordered `Map`, or a `Map` that iterates in insertion order | a parallel `List<str> order` beside the `Map` | `refs.m31` |
| a `bytes` literal | `bytes nl() { return "\n".to_bytes(); }` — and it still allocates on every call, because §3.10 says a literal of a mutable type must | `t_object.m31` |

The `bytes`-literal one deserves its own note, because the reasoning in §3.10
is completely convincing and the consequence still bites: `join` on a
`List<bytes>` takes a `bytes` separator, so joining on a newline allocates a
one-octet heap object per call. There is nowhere to hoist it to, because of §6.

---

## 8. Loop boilerplate

There is no three-clause `for` (§5.5) and no augmented assignment (§6.1). This
program has **52 `while` loops** and **44 `x = x + 1` statements**. A C or Rust
`for (i = 0; i < n; i++)` is four lines here:

```c
int i = 0;
while (i < n) {
    ...
    i = i + 1;
}
```

`for (int v in xs)` covers the cases where the index is not needed, and it is
used wherever it fits — all but one of the places that `continue` is a
`for … in` for exactly that reason. But a parser walks a buffer by offset and
a codec walks an array by index, so most of these loops genuinely need the
counter, and then the increment is a statement a `continue` skips past. The
one counted `while` here that also needed a `continue` shows what that costs
(`git.m31`, `indented`, skipping a leading blank line):

```c
while (at < n) {
    ...
    if (empty && first) {
        at = e + 1;         // the advance, repeated by hand
        continue;           // because `continue` jumps over the one below
    }
    ...
    at = e + 1;
}
```

Forgetting that first `at = e + 1` is not a wrong answer, it is a program that
never terminates, on the entirely ordinary input of a commit message
beginning with a blank line. A three-clause `for` cannot express the bug.

This is still the friction I would least want fixed if fixing it meant a
second way to spell a loop — but the trade is a real one and not only a
matter of keystrokes.

---

## 9. No second return value

Two types in this program exist only because a function returns one thing,
and a third type carries a field for the same reason:

```c
type Tables  { Huff lit; Huff dist; }        // zlib.m31: a dynamic block's two codes
type Headers { List<bytes> lines; bytes message; }   // object.m31: a commit's two halves
```

and `Huff` carries a field `int left` that is not part of a Huffman code at
all — it is how much of the code space the lengths left unused, which
`build()` needs to report alongside the code it built.

`Result` and an enum genuinely cover the cases §9 says they cover (a failure,
a sum). They do not cover "two things that are both fine", and a struct per
call site is the workaround. It is a small cost and it is honest about the
allocation; it is also two type declarations and one odd field that a reader
has to hold.

---

## 10. The formatter takes out parentheses I put in on purpose

```c
// what I wrote, checking it line by line against FIPS 180-4
f = d ^ (bb & (c ^ d));
f = (bb & c) | (d & (bb | c));

// what `m31c fmt` leaves behind
f = d ^ bb & (c ^ d);
f = bb & c | d & (bb | c);
```

Both are the same expression — §6.1 gives the bit operators Python's
precedence, and `&` really is tighter than `^` and `|`. And the argument for
removing redundant parentheses is the same argument as for one spelling per
call. But this is the one expression in the file whose whole job is to be
checkable against a published standard by eye, and the parentheses were the
checking. A reader now has to know the precedence table to verify a line that
was self-evident before.

I have left it formatted, because a gate is a gate. It is the only place where
following house style made the code worse.

---

## 11. Smaller things

  - **`fs.walk` returns paths, not names.** Turning a path back into a ref name
    is `text.strip_prefix(p, fs.join(gitdir, ""))` and then stripping a
    leading `/` — the `fs.join(x, "")` is doing something it was not designed
    for. A `walk` that reported paths relative to its root would be the
    natural shape for anything that walks a namespace.
  - **`text.strip_prefix`/`strip_suffix` return the string unchanged when it
    does not match**, so "did it end in `.z`?" is `strip_suffix(f, ".z") != f`.
    That works and reads badly. An `Option<str>` would read as `match`, which
    is worse. `ends_with` first, then strip, is what I should have written.
  - **`lib/args` has its own grammar and it is not git's.** A command is
    `-word`, an option is `--word`. So `git cat-file -t X` is
    `ourgit -cat-file --type X`. The module's header argues its case well and
    the case is good; it does mean a program that reimplements a famous CLI
    cannot reimplement its command line, and `-t` is not available at all
    because it would be a command called `t`.
  - **`Option<T>` has `or(v)` and `Result<T, E>` deliberately does not.** I
    wanted `.or()` on a `Result` four times and each time the thing I actually
    needed was to say what the failure was, so the rule was right each time.
  - **No `_` for an unused binding**, anywhere: not in a `match` arm, not in a
    `for`, not in a declaration. See §3.

---

## What the language was good at

This is not politeness; these are things that measurably did not go wrong.

  - **`bytes` is exactly the right type and it is nearly complete.** A binary
    format reader wants a mutable, indexable, growable octet buffer with
    `substr`, `extend`, `truncate`, `push`, `hex()` and a *checked* `utf8()`,
    and that is what is there. `drop_front` and `truncate` keeping the
    allocation is the detail that makes a read loop cheap. The one thing I
    reached for and did not find is a scan from an offset (§7).
  - **`str` is text and `bytes` is octets, and the compiler will not let you
    confuse them.** This is the single biggest correctness win in the whole
    program. A git tree entry's name is arbitrary octets; a commit author's
    name is arbitrary octets; a commit message is arbitrary octets. In Python
    I would have written `str` somewhere and found out on a repository with a
    Latin-1 filename in it. Here the types forced `bytes` all the way to the
    `write()` call, and `apps/git/test.sh` builds a fixture with a `\xe9` in a
    filename that passed the first time it ran. `print(b)` being **refused**,
    naming `hex()` and `utf8()`, is the rule that makes it stick.
  - **Overflow trapping never fired, and that is the point.** SHA-1 and
    Adler-32 both accumulate sums that must not overflow, and both are written
    with ordinary `+` because I could show the sum stays under 2^63. In C I
    would have written `uint32_t` and hoped; here the reasoning is checked at
    run time on every operation, at no cost I could measure. `wrapping_add`
    exists and this program never needed it, which is the right ratio.
  - **Python's bitwise precedence.** `x & 0xFF == 0` means what it looks like,
    and `(sum) & MASK` after a chain of `+` needs no parentheses because `+`
    binds tighter. I relied on this about forty times without checking, and
    checked afterwards, and was right every time. C's precedence here is a
    genuine trap and this is a genuine fix.
  - **Exhaustive `match` caught real bugs.** Adding `Tag` to the object enum
    broke four call sites, each of which needed a real decision. The verbosity
    complaint in §3 is about discarding payloads, not about exhaustiveness.
  - **The diagnostics.** Every error I made was reported at the right
    line:col, with the source echoed and a caret, and most of them told me
    what to do instead — `?` mismatch, discarded `Result`, missing `match`
    arm, `300 is not a byte; a byte is an int from 0 to 255`, wrong argument
    count. The top-level-local one (§6) explains a whole language decision in
    two lines at the place you need it. One message in this whole exercise
    (§4) fell short of that standard.
  - **Module constants are static data.** RFC 1951's five tables are
    `const Array<int>` and cost nothing: no initialisation order, no copy, no
    per-call setup, and `TABLE[i]` is an ordinary load. For a codec with
    published tables this is exactly right.
  - **Destructors meant no cleanup code.** `io.File` closes itself, so the CLI
    has no close path, no `defer`, and no way to leak a descriptor. I did not
    think about it once, which is the compliment.
  - **`Bits(src, at: 2)`** — mandatory positional, optional named — reads
    better than either C or Python at the call site, and I never had to look
    up an argument order.
  - **It is fast to build.** `m31c` turns the 2 700-line program into 43 000
    lines of C in 0.19 s, in a debug build of the compiler.
  - **It worked, mostly first time.** SHA-1 was correct on its first run.
    Inflate produced the right octets for all 29 fixtures on its first run —
    the only diff was that my program had sorted the cases by filename and the
    oracle by the name it printed, which is a bug in the test and not in the
    decoder. The object reader matched a from-scratch Python reader on 40 998
    lines of output from a real 4 544-object repository, first run. `log`
    needed three rounds against real `git`, and all three were git's
    presentation rules rather than anything about reading the format: a
    separator blank line that belongs *between* commits, C-style quoting of
    paths that are not printable ASCII, and tab expansion in commit messages.

    That is not a normal hit rate for this much bit-twiddling, and the reason
    is the two rules above: no silent overflow, and no confusion between text
    and octets.
