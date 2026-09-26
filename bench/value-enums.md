# Value enums: what they cost and what they buy

The rule and the argument that nothing observable changes are in
`docs/value-enums.md`. This is the measurement.

Everything below is x86-64, System V, `-O2`, on a machine shared with several
other builds throughout. **Best of N, never the mean**, because the noise here
is other processes and noise only ever adds; where two things are compared
they are run alternately, so a load spike hits both.

---

## 1. The microbenchmarks — `bench/valenum/run.sh`

Each pair is the same program twice. The second copy adds **one line** — a
`List<T>` of the enum that the program never touches — which takes the type
off the value representation for the whole program (`docs/value-enums.md`
§2) and leaves the hot loop byte for byte identical. So the difference is
exactly the allocation, the free and the refcount traffic: what the program
cost before this change existed.

Fifty million iterations each; allocations counted by the runtime
(`runtime/rc_debug.h`, `-DRC_COUNT_ALLOCS`), not estimated.

| loop | value | boxed | | allocations, value | allocations, boxed |
|---|---|---|---|---|---|
| `Option<int>` (16 bytes) | **0.13 s** | 1.36 s | 10.5× | **0** | 50 000 001 |
| `Result<int, E>`, `E` payload-less (16 bytes) | **0.04 s** | 1.45 s | 36× | **0** | 50 048 829 |
| `Result<int, E>`, `E` two ints (32 bytes) | **0.45 s** | 1.49 s | 3.3× | **0** | 50 048 829 |

Fifty million allocations and fifty million frees, gone, in each case.

## 2. The thing the third row is telling you

**Sixteen bytes is a cliff.** System V returns a struct of 16 bytes or less in
`rax:rdx` and anything larger through a hidden pointer into the caller's
stack. `langc` emits no attributes and no packing, so this is the plain ABI,
and both gcc and clang agree; verified by reading the assembly:

| size | example | returned |
|---|---|---|
| 8 bytes | a payload-less enum | one register |
| 16 bytes | `Option<int>`, `Result<int, E>` for a payload-less `E` | two registers, no memory at all |
| 24 bytes and up | `Result<int, E>` where `E`'s widest variant is two ints | hidden pointer, caller's stack slot |

Rows two and three of §1 are the same loop with the same number of branches;
the only difference is which side of that line the return value falls on, and
it is worth **11×**.

A tag costs a machine word and `int` is 64-bit, so the arithmetic is easy and
unforgiving: `Result<T, E>` fits in registers only when `T` and `E` together
need one word. `Result<int, E>` is fine for a payload-less `E` and over the
line for an `E` that carries a single `int`.

Two things would move the line, neither of them in this change:

  - **narrower integer types** (`docs/ir-v0.md` §2.1 keeps `i32`/`i128` as
    additive future work) — a `Result<i32, E>` carrying `i32` payloads would
    fit a great deal more in 16 bytes;
  - **merging a nested enum's tag into its parent's** — `Result<int, E>`
    would become one tag space plus the widest payload, saving a word. It
    would take the 24-byte case to 16 and leave the 32-byte case at 24, so it
    is worth having and is not on its own enough.

## 3. The whole corpus

The corpus is 500-odd programs and its enums are mostly `Option<int>`,
`Result<T, io.Error>` and the standard library's own small enums, so most of
what it gained is allocations it no longer makes rather than time it no longer
takes; none of its programs is an allocation benchmark. What it is evidence
for is that **nothing changed**: every program's output, every trap message,
every diagnostic and `__rc_live=0` on every build are as they were, under gcc
and clang at `-O0` and `-O2`, on the C library and on raw system calls, and
under ASan and UBSan.

## 4. `apps/git`'s zlib — the case this was built for

`apps/git/FRICTION.md` §1 is the reason this work exists: the DEFLATE bit
reader wanted `Result<int, Error> take(int n)` and could not have it, so it
grew a `bool over` flag checked in nine places, "and a tenth that forgot would
silently accept a truncated stream".

The rewrite was done, in full: `take` and `byte` return
`Result<int, Error>`, `Huff.decode` takes the table it is decoding for and
returns `Err(BadCode(which))` instead of `-1`, every call site is `?`, the
`over` flag is gone and so are all nine checks. It is **13 lines shorter**, it
has 29 `?` where it had 6, and it decompressed all 29 Python-`zlib` fixtures
correctly the first time it ran. The patch is `bench/valenum/zlib-result.diff`.

It is **not checked in**, because of this:

| bit reader | gcc -O2 | clang -O2 |
|---|---|---|
| `over` flag (what is checked in) | 92 MB/s | 106 MB/s |
| `Result` per read, value enums | 79 MB/s (0.86×) | 91 MB/s (0.86×) |
| `Result` per read, boxed enums | 51 MB/s (0.55×) | 55 MB/s (0.52×) |

Best of 15 round-robin runs over a 2.2 MB dynamic-Huffman stream, all three
built from the same sources by the same compilers.

Read the third row first: **before this change, writing the bit reader the way
the reference tells you to cost 45% of its throughput.** That is why nobody
did. After it, the same code costs 14%.

Fourteen percent is a real number and not noise — both compilers agree to two
digits — so by the rule this work was done under ("ship the rewrite if it is
at least as fast") it is not shipped. The residual is entirely §2: `zlib.Error`
has six variants carrying two `int`s, so it is 24 bytes, so
`Result<int, Error>` is 32, so every `take` writes and reads a stack slot. A
Huffman symbol costs up to fifteen of them. Nothing about the enum can shrink
it while `int` is 64 bits.

Two things worth putting on the record beside that number:

  - The rewrite also makes `zlib.Error` better. Three of its variants carried
    a `str` naming which Huffman table was at fault; the patch makes that a
    `Table` enum, which is what let `Error` be a value at all and is a better
    spelling anyway — three names the compiler checks instead of three
    strings a typo could disagree about.
  - Inflate is not `apps/git`'s bottleneck. `apps/git/FRICTION.md` §5 measures
    the end-to-end loose-object read at 10 MB/s against inflate's 71–102, so
    14% of inflate is a low single-digit percentage of the program. Whether
    that buys a failure the type system enforces is the author's call, and the
    patch is there to apply.
