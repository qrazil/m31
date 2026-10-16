# Crypto acceleration: assembly kernels behind the stdlib seam

Decision from the owner (2026-10-16): no C for the crypto library, assembly is
acceptable where it buys real speed. This is what was built, what it measured,
the security notes, and what is left. Measurements and method are in
`docs/CRYPTO-PERF.md`.

## The mechanism: no compiler change

m31 has no FFI and does not need one here. `docs/stdlib-seam.md` already has the
tool: a `prim` declaration in a stdlib module lowers to a call of `rt_<name>`
(strip the underscores, prefix `rt_`), allowed only in modules that come out of
`lib/`, and a `prim` may write into a `bytes` it is handed. The kernels use exactly
that, so **no file under `src/` changed** (no owner sign-off needed on the compiler).

```
lib/crypto/sha256.m31 ... prim int __crypto_sha256(bytes state, bytes data, int off, int n);
                          if ((__crypto_features() & 2) != 0) { ... kernel ... } else { m31 code }
runtime/rt.h             int64_t rt_crypto_sha256(Obj *state, Obj *data, int64_t off, int64_t n);
runtime/crypto_glue.c    unpack the Bytes, range-check, feature-check, call rt_k_sha256
runtime/crypto_x86_64.S  rt_k_sha256: SHA-NI
runtime/crypto_aarch64.S rt_k_sha256: ARMv8 SHA2
```

* **The assembly is in separate `.S` files next to `ctx_switch_*.S`** and is `#include`d
  at the end of the matching `ctx_switch_*.S`, so every script that already links
  `$RT_CTX_ASM` (about twenty, including the release workflows and the apps) gets the
  kernels with no new file to name. Same conventions as the context switch: C_SYM for
  Mach-O's underscore, `.note.GNU-stack` on ELF only, `.type`/`.size` on ELF only,
  System V / AAPCS64, x18 untouched.
* **`runtime/crypto_glue.c` is the only C**, about 300 lines, `#include`d by `rt.c`. It does
  three things and implements no primitive: feature detection (cpuid via a four-line asm
  function on x86-64; `getauxval(AT_HWCAP)` on Linux aarch64; `sysctlbyname("hw.optional.arm.FEAT_*")`
  on macOS), argument unpacking with a range check against the buffer's **size** (the rule
  `rt_read` follows), and the `M31_CRYPTO_ACCEL` switch. The C was unavoidable for detection on
  aarch64 and for the `Bytes` layout; it is deliberately that small.
* **Fallback**: a kernel runs only when the CPU has the instructions
  (`__crypto_features()` bit set) and `M31_CRYPTO_ACCEL` has not switched it off; otherwise
  the m31 implementation runs, producing the same bytes. `M31_CRYPTO_ACCEL=off` disables
  all; `M31_CRYPTO_ACCEL=-aes,-sha256,-sha512,-sha1,-chacha,-poly,-fe` disables the named
  kernels. `tests/cryptodiff/test.sh` runs the whole differential suite with kernels on and
  off, so the two paths are held to the same oracles (hashlib, OpenSSL via `cryptography`).
* The limbs a field kernel returns are **bit-identical** to the m31 code's (it is ref10's
  multiply and carry chain, instruction for instruction), so a process can mix the paths.

## Kernels

| kernel | x86-64 | aarch64 | feature test |
|---|---|---|---|
| AES-128/256 ECB and CTR (8 blocks interleaved on x86, 4 on arm) | AES-NI | AESE/AESMC | AES, PCLMULQDQ, SSSE3, SSE4.1 / HWCAP_AES and PMULL |
| GHASH | PCLMULQDQ, Gueron's reduction | PMULL, two-fold reduction by 0x87 | same |
| ChaCha20 (4 blocks, any length) | SSE2 (always present) | NEON | none needed |
| Poly1305 (blocks, finish) | `mulq`, 64-bit limbs | `mul`/`umulh` | none needed |
| SHA-256 | SHA-NI | SHA2 instructions | CPUID.7.EBX[29] / HWCAP_SHA2 |
| SHA-1 | SHA-NI | SHA1 instructions | same |
| SHA-512 | scalar 64-bit | scalar 64-bit | none needed |
| GF(2^255-19) mul, add, sub (10 limbs) | `imulq` | `mul`/`madd` | none needed |

Not done: AVX2 ChaCha20 (SSE2 is within 1.5 to 2.5 times of OpenSSL's AVX2 here), a
stitched AES-CTR plus GHASH loop, the aarch64 SHA-512 extension instructions, ECDSA/RSA
big-number kernels, and a field squaring kernel.

## Measured gains (this host, min of five, kernels on vs kernels off; details in CRYPTO-PERF.md)

| operation | m31 only | with kernels | origin/master | OpenSSL |
|---|---:|---:|---:|---:|
| AES-256-GCM seal 1 MiB | 24.6 MiB/s | 914 MiB/s | 0.59 | 3122 |
| ChaCha20-Poly1305 seal 1 MiB | 113 | 556 | 9.6 | 1438 |
| SHA-512 1 MiB | 115 | 286 | 0.86 | 507 |
| SHA-256 1 MiB | 70 (no SHA-NI on this CPU) | 70 | 46.8 | 363 |
| X25519 | 2010 ops/s | 3084 | 177 | 23386 |
| Ed25519 verify | 407 ops/s | 617 | 43 | 6602 |

## Testing, and what was only built

`runtime/tests/crypto_kernels_test.py` calls every kernel directly with random inputs,
odd buffer offsets, canary bytes around outputs, and sizes 0/1/15/16/17/63/64/65/block
multiples, against `cryptography` (OpenSSL), hashlib, a straight Python GHASH and a Python
bigint model of the field.

* **x86-64 AES, GHASH, ChaCha20, Poly1305, SHA-512, field kernels**: run natively on this host.
* **x86-64 SHA-256 and SHA-1 (SHA-NI)**: this CPU lacks the extension. Run under
  `qemu-x86_64 -cpu max` (QEMU 8.2 emulates SHA-NI), for the raw kernels and, through
  `M31_RUN_PREFIX='qemu-x86_64-static -cpu max -L /' tests/cryptodiff/test.sh`, for the whole
  library path including dispatch. Not run on real SHA-NI silicon.
* **aarch64, every kernel**: assembled with clang (`--target=aarch64-linux-gnu`), linked with rust-lld
  into a freestanding program with the vectors embedded, run under `qemu-aarch64 -cpu max`
  (about 1800 checks). The harness was shown to fail when a kernel is corrupted. **Not run:**
  the m31 library on aarch64 (no cross libc here, so the glue's `getauxval` path, `rt.c`
  and the dispatch have only been compiled for x86-64), the macOS build (Mach-O syntax and
  the `sysctlbyname` names are written from the same pattern as `ctx_switch_aarch64.S`
  but never assembled by Apple's tools), and real ARM silicon. The aarch64 and macOS
  CI jobs are the first real test of those three; the first thing to check there is
  `runtime/tests/crypto_kernels_test.py aarch64` and `tests/cryptodiff/test.sh`.
* **CI runners**: a kernel never runs unless detection says the feature is present, so a
  runner without SHA extensions (many x86 GitHub runners) simply takes the m31 path for SHA-1
  and SHA-256; the other kernels need only SSE2 or are tied to AES-NI/PCLMUL/SSSE3/SSE4.1,
  present on every hosted runner.

## Security notes

* **Constant time by construction**: AES-NI, PCLMULQDQ, AESE and PMULL have no
  secret-dependent lookups; ChaCha20, Poly1305 and the field kernels are straight-line
  add/xor/rotate/multiply code (`imul`/`mul` are constant time on the CPUs involved, as
  they already were for the m31 limb arithmetic); every loop bound is a public length.
  Poly1305's final reduction is a masked select. SHA-1/256/512 see only public data in this
  library and are table-free anyway. Nothing was added that indexes memory by a secret.
* **What is not verified**: no timing measurement on hardware, no analysis of microarchitectural
  leaks beyond the above; the same caveat the m31 headers already carry.
* **Memory safety**: every pointer a kernel receives comes from a `Bytes` after a range check
  against its size, output buffers are checked mutable, and the kernels read and write exactly
  `n` blocks (or `len` octets); the tests put canary bytes either side. The ChaCha20 kernel
  zeroes its stack frame (key and keystream) before returning; the AES and field kernels
  leave only xmm/GPR residue in caller-saved registers.
* **Counter semantics**: the CTR kernel increments all 128 bits; GCM never wraps the low 32 bits
  (messages are capped at 16 MiB, counter starts at 2), so the two agree; ChaCha20 counts
  only word 12, so a 64-bit (OpenSSH layout) counter that would carry inside one call takes
  the m31 path.
* **Key schedules** are still computed by the m31 bitsliced code (constant time) and handed to
  the kernels as plain bytes.

## What is left, and the options

1. **Done and recommended to keep**: kernels behind `prim`, no compiler change. Cost: three
   runtime files (about 2000 lines of assembly, mostly generated-by-hand repetition of
   documented sequences) and one 300-line glue file; risk: assembly bugs, bounded by the
   tests above and the always-available m31 fallback.
2. **Language intrinsics** (`rotl32`, 64x64->128 multiply-high, `restrict`/no-alias, bounds-check
   elision, `bytes.load_le32/le64`). Gain: the portable path (CPUs without the extensions,
   and everything that is not AES/SHA/ChaCha/field arithmetic: ECDSA, RSA, bcrypt) gets
   roughly 1.5 to 2 times; evidence in `docs/CRYPTO-PERF.md`: native rotates are 1.9 times faster
   in C. Effort: a lowering per primitive in `src/back/emit_c`, tests, a freeze-sensitive
   language surface. Not needed for the primary goal now that kernels exist. **Owner
   decision**, not started.
3. **Big-number kernels for ECDSA, RSA, bcrypt**: these remain 40 to 400 times behind OpenSSL
   (P-256 sign 12 ms, RSA-2048 sign 53 ms) because `bignum.m31` is public-data Montgomery code
   with 24-bit limbs and `rsasign` is its own constant-time code. They matter for TLS client
   certificates, certificate chain verification and RSA host keys, not for Ed25519 SSH. A
   64-bit-limb multiply-accumulate kernel (`mulx`/`adx`, `mul`/`umulh`) would give an order
   of magnitude; effort about as large as everything done here for one more primitive family.
4. **Do nothing more**: the numbers above already meet the SSH and TLS bulk-throughput
   goals (AES-GCM and ChaCha20-Poly1305 within 1.5 to 3.5 times of OpenSSL; SSH transfers at
   OpenSSH's speed through a loopback sshd).

**Recommendation**: keep 1, run the aarch64 and macOS CI jobs and fix whatever they find before
release, do 3 only if RSA host keys or ECDSA client certificates matter to users, and leave 2 for
the owner's language-design discussion (it is worth having for its own sake, but it is no longer
on the critical path).
