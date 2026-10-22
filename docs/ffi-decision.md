# Calling assembly and C-ABI functions from library code

`docs/stdlib-seam.md` gave the standard library one way out of the language:
a `prim` declaration that becomes a call of a hand-written runtime function
taking m31 objects. That is the right seam for the operating system. It is the
wrong one for **assembly**: a kernel written in assembly takes pointers and
integers, not `Obj *`, so every kernel needs a C function in front of it that
unpacks the object, checks the range, and calls it. The `crypto-perf` branch
(`docs/CRYPTO-ACCEL.md`) has 352 lines of exactly that, `runtime/crypto_glue.c`,
and the owner's rule is *no C for crypto; assembly accepted*.

Decided **2026-10-21** (owner: m31 will need assembly and FFI in the future, so
give it a safe way to call both from library code). This document is the
decision; `src/` implements it in the commit after it.

---

## Where it lives in the compiler

| stage | file |
|---|---|
| syntax and the declaration's checks | `src/front/parser/native.rs`, called from `parse_func` in `src/front/parser/decl.rs`; the AST types are `NativeDecl`, `NativeParam`, `SpanExpr` in `src/front/ast.rs` |
| the call site | none: a `prim "c"` is a `prim`, so `lower_call` emits the same `Inst::Call` of `rt_k_...`; `src/middle/lower/native.rs` builds `Module::natives` (`src/middle/ir.rs`, `NativeSig`) |
| **the checked call, the only place a pointer into an m31 object is made** | `src/back/emit_c/native.rs` |
| blocking wrap and spawn warning | `prim_targets` loses a `nonblocking` callee (`lower.rs`); the emitter and `src/back/emit_c/blocking.rs` are otherwise unchanged |
| formatting | `src/tools/fmt.rs` |

## The problem

1. **A kernel's contract is a pointer and a length, and C was the only thing
   that could produce either.** The glue does the same three things for every
   kernel: turn a `bytes` into `data + off`, check `off` and the count against
   the buffer's *size* (the rule `rt_read` already follows), and call. That is
   the compiler's job: it knows the declaration, the argument types and the
   call site.
2. **The checks live in C that is easy to get subtly wrong and costs a file
   per feature.** Twelve kernels were twelve hand-copied unpackers. A missed
   `rt_check_mutable` or a range checked against the capacity instead of the
   size is a memory-safety hole that nothing in the language would catch,
   because the language cannot see inside the glue.
3. **There is no way to say the kernel does not block.** Every `prim` call is
   wrapped in `rt_enter_blocking`/`rt_exit_blocking`
   (`docs/concurrency-decision.md`, "Blocking FFI") and every `prim` reachable
   from a spawned function gets a warning. For `read(2)` that is right. For
   an AES kernel it is a pair of calls and a false warning around pure compute.
4. **CPU feature detection needs two instructions and a file.** `cpuid` is an
   instruction, not a function; `getauxval` and `sysctlbyname` are C library
   calls. None may be reached from m31 today.

## What is decided

### One declaration form: `prim "c"`, with spans

```
prim "c" nonblocking void __k_aes_ctr(
    in bytes rk[0 .. 16 * (rounds + 1)],
    int rounds in 10 | 14,
    inout bytes ctr[0 .. 16],
    in bytes src[off .. off + 16 * n],
    out bytes dst[off .. off + 16 * n],
    int n,
    range int off);
```

(This is also exactly what `m31c fmt` prints: one parameter to a line.)

It is a `prim`: a bodiless declaration, only in modules that come out of `lib/`,
parsed and type-checked like any function, no generics, no methods, `__` name.
The string after `prim` names the **calling convention** of the callee:
`"c"` is the platform's C ABI (System V AMD64, AAPCS64), which is what an
assembly function follows too. It is a string and not a keyword so that no
identifier in any existing program changes meaning, and so a second convention
could be added later without a new word. The only accepted value today is `"c"`.

A parameter of a `prim "c"` is one of:

| written | the callee receives |
|---|---|
| `int name` | the integer, as a 64-bit value |
| `int name in A \| B \| ..` | the same, after a check that it is one of the listed literals; anything else traps |
| `range int name` | **nothing**: the value exists only to be named in a span's bounds |
| `in bytes name[LO .. HI]` | a pointer to octet `LO` of the buffer, read only |
| `out bytes name[LO .. HI]` | the same, may be written |
| `inout bytes name[LO .. HI]` | the same, read and written |
| `in Array<int> name[LO .. HI]` (and `out`, `inout`) | a pointer to element `LO` of the array; elements are 64-bit |

`LO` and `HI` are integer expressions over `+`, `*`, parentheses, integer
literals and the names of the `int` parameters of the same declaration. The span
is the half-open range `[LO, HI)`, in octets for `bytes` and in elements for an
`Array<int>`. That is the whole expression language: no subtraction (a span's
length cannot be made to wrap by a clever difference), no division, no calls.

The return type is `int` (the callee's 64-bit result register) or `void`.

**Why spans are written at the parameter and not as a convention
(`bytes` followed by an `int` length).** A convention would have to guess
whether `off` and `n` belong to the buffer before them or after, whether `n`
counts octets or blocks, and what the callee needs besides the pointer. The
kernels differ on all three: `aes_ctr` takes a count of 16-octet blocks, so its
span is `16 * n`; a key schedule's size depends on `rounds`; a state buffer is
a constant 40 octets. Writing the arithmetic where the parameter is declared
puts the contract the kernel relies on in one reviewable line next to the
symbol, and it is the *only* place that contract is written down.

**Why `range int`.** `off` is a bound for three spans but the kernel does not
want it (the pointers are already advanced). The portable register budget is
six integer arguments; spending one on a value the kernel never reads would
make `aes_ctr` impossible to declare. A `range int` costs no register and
cannot be mistaken for an argument.

**Why `int rounds in 10 | 14`.** A kernel's *scalars* can be as dangerous as
its pointers: a loop bound read from `rounds` walks the key schedule. The span
`rk[0 .. 16 * (rounds + 1)]` bounds the memory for any `rounds`, but a kernel
that special-cases 10 and 14 is only correct for those. Writing the legal
values in the declaration lets the *compiler* trap on 12, instead of every
caller (and the glue) remembering to.

### What is allowed where

`prim "c"` follows the `prim` rule unchanged: **library code only** (a module
that came out of `lib/`), refused anywhere else with the same message shape as
`prim`; no new keyword, so no program that compiles today stops compiling. A
`prim "c"` name must begin `__k_`; the callee symbol is `rt_k_` plus the rest
(`__k_aes_ctr` calls `rt_k_aes_ctr`). That is one more transform than `prim`'s
(`__open` is `rt_open`) and it is deliberate: the `k_` namespace holds only
functions written for this convention, so a `prim "c"` can never be pointed at
a runtime function that expects an `Obj *` (`rt_open`, `rt_concat`), which
would be a type confusion the C compiler would not see across the call.

A user program cannot write `prim "c"`, call a `__k_` name, or learn it exists
except by reading `lib/`. The public surface added is **zero**, as for `prim`.

What it does not add, on purpose: no raw pointer type, no `int`-to-pointer, no
struct layout, no callbacks into m31, no variadic callee, no callee-visible
`str`, no more than six arguments. A user-visible FFI is a separate decision
(`docs/stdlib-seam.md` §3 still stands: a real FFI replaces this, it does not
grow out of it). The string after `prim` is the single place such a decision
would attach.

### The safety argument

For every call of a `prim "c"`, the compiler emits, **in one function of
`src/back/emit_c`**, this sequence, and nothing else:

1. each span bound is evaluated with overflow-checked `+` and `*`; overflow
   traps;
2. each `int ... in A | B` is compared with its literals; a miss traps;
3. each span is checked **against the object's size**, not its capacity:
   `0 <= LO <= HI <= size`; a failure traps with the declaration's own name and
   the parameter's name;
4. an `out` or `inout` span's object is checked with `rt_check_mutable`
   (a frozen or immortal buffer refuses it, like any write);
5. the callee is called with the derived pointers and the scalars.

From which:

- **Every pointer a callee receives is derived by compiler-emitted code from an
  m31 value, after a range check against that value's size.** There is no
  other source of a pointer: not a parameter, not an `int`, not a global.
- **No raw pointer exists in m31 source.** The type of a span parameter is
  `bytes` or `Array<int>`, which the caller passes like any argument; the
  pointer exists only inside the emitted call, for its duration. There is no
  integer-to-pointer conversion and no way to write one, so an integer cannot
  become an address.
- **The pointer stays valid for the call.** The argument is borrowed, so the
  caller holds a reference through the call; a `bytes` can only change size
  through m31 code, and no m31 code runs while the callee does (there are no
  callbacks); an `Array`'s storage is inline and never moves. A value that
  crosses between threads is moved, never shared (`docs/concurrency-decision.md`,
  "Moved, not shared"), so no other thread can resize or free the buffer
  during the call; a constant (immortal) buffer is shared but can only be read,
  and is refused as an `out`/`inout`.
- **The callee cannot break a `bytes`.** Its size never changes, and every
  octet is a `uint8_t`, so the language's invariant that a `bytes` holds
  0..255 holds whatever the callee writes (`docs/stdlib-seam.md` §2, the
  amendment that let a `prim` write a `bytes`). An `Array<int>` holds any
  `int`.
- **The residual trust is exactly the callee.** A `prim "c"` is a promise by
  the kernel's author, checked by review and tests, that the callee reads only
  its `in` and `inout` spans, writes only its `out` and `inout` spans, within
  the declared range, for any scalars the declaration admits, and follows the
  ABI (preserves callee-saved registers, keeps the stack aligned, does not
  clobber the red zone it did not set up, does not touch `x18` on Apple
  platforms). The compiler cannot verify assembly. What the mechanism changes
  is that this promise is **per kernel and one line wide** instead of
  per kernel *plus* a hand-written unpacker.

Spans may alias. Two arguments may be the same buffer (in-place encryption,
`out` equal to an `in`), or overlapping ranges of it. Memory safety does not
depend on non-aliasing (every access is inside a checked range of a live
object), the invariants above hold regardless, and in-place use is wanted by
every cipher and by the field kernels. What a kernel computes on a partial
overlap is the kernel's documented business. Aliasing is therefore not a
diagnostic.

### Calling convention

C ABI, integer and pointer arguments only, **at most six arguments after
`range` parameters are removed** (the six argument registers of System V
AMD64; AAPCS64 has eight and RV64 eight, six is the portable subset). Every
scalar is 64 bits. No argument is passed on the stack, so a kernel never reads
the caller's frame. A `void` callee's result register is ignored; an `int`
callee returns in `rax`/`x0`.

A diagnostic refuses a seventh argument, a `bool`, `float`, `str` or other
parameter, a span of anything but `bytes` and `Array<int>`, a `range`/`in A|B`
parameter that is not an `int`, a bound that names an unknown parameter or
anything but `+ * ( )` and literals, a span parameter named in another span's
bound, and `prim "c"` in a method, generic or non-library function.

### Blocking, green threads and the scheduler

A call into a long assembly kernel does not park and does not yield: green
threads are cooperative (`docs/concurrency-decision.md`), and a kernel has no
yield point. Whether a call is *wrapped* is the declaration's choice:

- **`prim "c" nonblocking ...`** is a promise by the author that the callee
  makes no system call, waits for nothing and runs for a time bounded by its
  arguments. The call is **not** wrapped in `rt_enter_blocking`/
  `rt_exit_blocking`, and a spawned function reaching it gets **no**
  warning. Every crypto kernel is `nonblocking`.
- **Without the marker** the call is wrapped, exactly as a `prim` is today, and
  the spawn warning applies: it may block, so the scheduler must be able to
  hand the carrier's queue to a backup thread. This is the safe default and
  the one an annotation omitted by mistake gets.

`nonblocking` is the author's claim, not the compiler's proof; the review
checklist for a `.S` file is that it contains no `syscall`/`svc`/`ecall` and no
unbounded loop. The marker is a word after the string and not an attribute
because it changes the emitted code, like `static`.

A long `nonblocking` kernel does hold a carrier for its duration. The library
bounds that by calling it in slices (the crypto library already hands the
kernels at most one record at a time). The decision is recorded, not hidden:
a kernel that must run for tens of milliseconds is not `nonblocking`.

### Symbol resolution and linking

The assembly stays where it is: in `runtime/*.S` files that are `#include`d at
the end of the matching `runtime/ctx_switch_<arch>.S`, so that **no build script
names another file**. Every script that links `$RT_CTX_ASM` (`runtime/arch.sh`;
about twenty scripts, the release workflows and the apps) already gets every
kernel, in the release tarball too, because the tarball ships `runtime/` whole
and the included files sit next to the file that includes them. A new kernel
file is a new `#include` line in two files; nothing else.

Emitted C declares each callee `extern` with a signature built from the
declaration (`int64_t rt_k_x(const uint8_t *, uint64_t, ...)`), so a symbol
that nobody defined is a **link error naming the exact symbol**, as for
`prim`. A gate (`runtime/tests/ffi_symbols_test.sh`) checks that every
`prim "c"` in `lib/` has a definition in **each** architecture's assembly, so
the link error does not first appear on the architecture nobody built.

### Architectures: aarch64, x86-64, riscv64, and "absent"

A kernel exists per architecture, or does not exist. `runtime/rt.h` defines
`RT_HAVE_KERNELS` for the architectures whose `ctx_switch_*.S` includes
kernels (x86-64 and aarch64 today); the three-line `#if` sits next to the
description of `runtime/arch.sh` and is the one place that lists them. When it
is not defined (a future riscv64 build with a context switch and no kernels),
the emitted call is not a call:

- an `int` `prim "c"` **returns -1**: "absent";
- a `void` one **traps** (`native kernel absent on this target`), because
  nothing can tell its caller it did nothing.

The library therefore asks first, through one `int`-returning probe
(`__k_platform`, which says which architecture and operating system it was
assembled for), and falls back to m31 code when the answer is -1 or not a
platform with kernels. A kernel is never reached on a target that has none.

The freestanding raw-syscall backend (`-DRT_SYS_RAW`, `docs/sys-layer.md`) is
orthogonal: it replaces how `sys_*` reach the kernel, not how kernels are
linked, and `runtime/sys_linux.c` is not involved. The corpus runs under both
backends (the raw-syscall corpus gate), and the feature probing below uses
only the file API, so both backends take the same path.

### Mutability

`bytes` is mutable (`docs/types.md`, reference §3.10: a growable sequence of
octets that `b[i] = x` and `push` change), so an `out` span is an ordinary
write to a buffer the caller owns, and is refused on a frozen or immortal
buffer by `rt_check_mutable`. `in` is read only by contract and is **not**
refused on a frozen buffer, so a constant table or a frozen key schedule can be
an input. `Array<int>` has fixed length and mutable elements, with the same
rule. The size of a `bytes` never changes across a call.

### A bad range traps

A span outside its object, an overflowing bound, or a scalar outside its
`in A | B` list traps with a message that names the `prim`, the parameter and
the rule broken, through `rt_trap`, so the process exits with the same status
and the same stderr shape as every other runtime trap. It is a bug in the
library, not an error a caller can handle (`docs/stdlib-seam.md` §2, the same
reasoning as `rt_read`'s range check). A trap corpus program per case pins it.

### Testing

`runtime/ffitest_x86_64.S` and `runtime/ffitest_aarch64.S` hold a handful of
test kernels (an xor into a third buffer, a sum over an `Array<int>`, a fill, a
six-argument mixer, a scalar-constrained one), included the same way as the
real kernels. `runtime/tests/ffitest.m31` declares them. It is not in `lib/`
and not in the standard library a program sees: the compiler can import it as
`ffitest` only when the environment variable `M31C_FFITEST` is set, which
`scripts/run.sh` and `scripts/sanitize.sh` do, so the corpus programs drive the
kernels on every gcc/clang `-O0`/`-O2` build, under the sanitizer gate and under
the raw-syscall backend, while without the variable `import ffitest;` is the
ordinary "no such module" error (a corpus case pins that). Every diagnostic has
a `corpus/errors`/`corpus/modules` case and every trap a `corpus/traps` one. The aarch64 kernels are
assembled and run under `qemu-aarch64` in the same freestanding way the aarch64
crypto kernels are tested; the emitted-C half is architecture independent.

### CPU feature detection, in m31

Detection is library code over two tiny kernels, with no C:

| platform | how | needs |
|---|---|---|
| x86-64 | `__k_cpuid(leaf, subleaf, out[0..16])`, an assembly function that writes `eax, ebx, ecx, edx` | the `cpuid` instruction (all x86-64) |
| aarch64 Linux | read `/proc/self/auxv` through the file API and take `AT_HWCAP` | `/proc` mounted; else the m31 fallback runs |
| aarch64 macOS | **a documented static assumption**: every Apple arm64 CPU (M1 and later, A12 and later) has AES, PMULL, SHA1 and SHA2 | none; `M31_CRYPTO_ACCEL=off` is the escape |
| anything else | no kernels, m31 code | |

`mrs` of `ID_AA64ISAR0_EL1` was considered for Linux aarch64 and rejected: EL0
reads of ID registers trap and are emulated by the kernel only since 4.11, only
when `HWCAP_CPUID` is set, and QEMU's user mode emulates it only in recent
versions; reading `AT_HWCAP` from the aux vector is what `getauxval` does and
needs none of that. `sysctlbyname` on macOS was considered and rejected for
now: reaching it needs an assembly adapter that can be neither assembled nor run
here, and what it would report on any Apple silicon is the constant above. The
adapter is the first thing to add if a non-Apple arm64 macOS target ever
exists, and the library has one place to put it.

### The `M31_CRYPTO_ACCEL` switch

Read in m31 with `os.env`, once, at the first query, and cached in the module.
Same grammar as before: `off` or `0` disables every kernel; otherwise a comma
list of `-aes`, `-sha256`, `-sha512`, `-sha1`, `-chacha`, `-poly`, `-fe`
disables just those.

---

## Alternatives considered

**Keep a tiny C shim** (one function per kernel, or one generic trampoline
taking a pointer table). A generic trampoline is the interesting one: one C
function, `rt_call_k(sym, ptrs, n)`. It does not remove the problem, only
moves it: someone still has to turn an `Obj *` into a pointer after a range
check, and now the check is data in an array rather than code at a call site
the compiler can see. And "no C for crypto" is the owner's rule, which a shim
bends and this mechanism satisfies.

**Inline assembly in m31 source** (`asm("aesenc ...")`). It makes every
target a source-level concern, puts register allocation and clobbers into the
language, cannot be checked for the ABI, and would have to be *allowed* in
library code and *refused* elsewhere exactly like `prim`, with a vastly larger
surface to maintain. A kernel of 400 lines is a file, not an expression.

**`dlopen`/`dlsym` of a shared library.** Not static, not freestanding, not
available to the raw-syscall backend, not the same on macOS and Linux, and
turns "a missing symbol is a link error" into a run-time one. The kernels are
part of the runtime, built with it.

**A new keyword (`extern`)**. Every other part of the design (library-only,
declaration without a body, one symbol per declaration, the diagnostic) is
`prim`'s. `extern` would need a reserved word, break any program that uses the
word, and make two declarations to teach when there is one idea: the callee is
one of ours.

**Span bounds as separate length parameters** (`bytes buf, int off, int n`,
as `__read` has). It leaves the arithmetic (`16 * n`) to a C unpacker, which
is the glue again.

## Open questions

1. **User-visible FFI.** Out of scope; `prim "c"` is deliberately a library
   mechanism. If it comes, the string after `prim` and the span grammar are the
   starting point, and the owner should decide ownership, layout and callbacks
   before they are.
2. **A `str` span** (a read-only `in str` for a kernel that hashes text).
   Nothing needs it; adding it is one more case in one function.
3. **More than six arguments.** A kernel that wants more takes a state
   buffer by span instead, which is how the Poly1305 and SHA kernels already
   do it.
