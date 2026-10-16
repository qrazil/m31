# Crypto performance: measurements and the m31-only pass

What `lib/crypto/*.m31` costs, measured against OpenSSL and the other libraries on
the same host, what was changed in m31 itself to make it cheaper, and what the
hardware kernels add on top (their design is in `docs/CRYPTO-ACCEL.md`).

## Method

* **Harness**: `bench/crypto/bench.m31` is one micro-benchmark per run (`OP SIZE ITERS`);
  `bench/crypto/run.py` builds it with `./build.sh` into a fresh `mktemp` directory,
  calibrates iterations to about 0.3 s, runs each (op, size) five times pinned to one CPU
  (`taskset -c 3`, `nice`) and reports the **minimum** and the median. The host is shared and
  was loaded (load average 3 to 15) during development, so only minima are comparable: the
  best run is what the CPU can do, the median is what the neighbours allowed.
  `bench/crypto/table.py` joins two result files; `bench/crypto/e2e.py` is the SSH benchmark.
* **References**, same host and minutes: `openssl speed -evp` per size (best of three),
  `openssl speed rsa2048 ecdsap256 ed25519 ecdhx25519`; Python `cryptography` for raw
  ChaCha20 and Poly1305 (meaningful only from 16 KiB up, Python's call overhead dwarfs small
  sizes); PyNaCl for Ed25519 key generation. This OpenSSL is the system build with its
  assembly (AVX2 ChaCha20-Poly1305, AES-NI + PCLMUL GCM; this CPU has no SHA extensions,
  so its SHA-256 is the SSSE3/AVX2 assembly). Masking CPU features with `OPENSSL_ia32cap`
  was tried for a "plain code" baseline, but OpenSSL's integer assembly is still assembly,
  so no number here claims to be plain C.
* **Host**: Intel(R) Core(TM) i7-8750H CPU @ 2.20GHz (turbo above), Fedora 40, Linux 6.14,
  gcc 14.2, `cc -O2` for the emitted C (what `build.sh` does). AES-NI, PCLMULQDQ, AVX2; no SHA-NI.
* **Before** is `origin/master` at 0f4c4da; **after** is this branch; **kernels off** is
  the same build run with `M31_CRYPTO_ACCEL=off`: the pure-m31 result of the restructuring
  pass alone.
* **Correctness gates** for every change: `tests/crypto25519/test.sh`, `tests/ssh/test.sh`,
  `tests/tls/test_aesgcm.sh`, and the new `tests/cryptodiff/test.sh` (about a thousand random
  and edge-size cases: sizes 0, 1, 15 to 17, 31 to 33, 55 to 57, 63 to 65, 111 to 129,
  block multiples; hashes fed in odd-sized pieces so the block routine starts at unaligned
  offsets; every AEAD case also round-trips and rejects a flipped tag), each run with the
  kernels on, with `M31_CRYPTO_ACCEL=off`, and (cryptodiff) under `qemu-x86_64 -cpu max` for
  the SHA extensions this host lacks. Oracles are hashlib, `cryptography` (OpenSSL) and
  PyNaCl, never m31 itself.

## Results

Host: Intel(R) Core(TM) i7-8750H CPU @ 2.20GHz; OpenSSL 3.2.4 11 Feb 2025 (Library: OpenSSL 3.2.4 11 Feb 2025); load average at the end 3.3; min of 5 runs each.

| primitive | size | before | after | after, kernels off | OpenSSL | after vs before | OpenSSL vs after |
|---|---:|---:|---:|---:|---:|---:|---:|
| sha1 | 1 KiB | 50.1 MiB/s | 69.0 MiB/s | 71.9 | 583 | 1.4x | 8.4x |
| sha1 | 16 KiB | 57.1 MiB/s | 72.5 MiB/s | 79.8 | 687 | 1.3x | 9.5x |
| sha1 | 1 MiB | 54.3 MiB/s | 80.2 MiB/s | 76.0 | 729 | 1.5x | 9.1x |
| sha256 | 1 KiB | 40.9 MiB/s | 62.2 MiB/s | 62.0 | 303 | 1.5x | 4.9x |
| sha256 | 16 KiB | 44.7 MiB/s | 69.5 MiB/s | 71.2 | 368 | 1.6x | 5.3x |
| sha256 | 1 MiB | 46.8 MiB/s | 70.6 MiB/s | 70.3 | 363 | 1.5x | 5.1x |
| sha512 | 1 KiB | 0.76 MiB/s | 157 MiB/s | 88.7 | 388 | 206.7x | 2.5x |
| sha512 | 16 KiB | 0.85 MiB/s | 273 MiB/s | 113 | 552 | 319.9x | 2.0x |
| sha512 | 1 MiB | 0.86 MiB/s | 286 MiB/s | 115 | 507 | 332.2x | 1.8x |
| hmac-sha256 | 1 KiB | 31.3 MiB/s | 45.1 MiB/s | 46.1 | 282 | 1.4x | 6.3x |
| hmac-sha256 | 16 KiB | 43.2 MiB/s | 67.8 MiB/s | 68.6 | 341 | 1.6x | 5.0x |
| hmac-sha256 | 1 MiB | 46.4 MiB/s | 70.0 MiB/s | 73.7 | 351 | 1.5x | 5.0x |
| chacha20 | 1 KiB | 10.4 MiB/s | 550 MiB/s | 142 |  | 53.1x |  |
| chacha20 | 16 KiB | 10.7 MiB/s | 782 MiB/s | 149 | 761 | 73.3x | 1.0x faster |
| chacha20 | 1 MiB | 9.91 MiB/s | 779 MiB/s | 149 | 528 | 78.6x | 1.5x faster |
| poly1305 | 1 KiB | 139 MiB/s | 956 MiB/s | 399 |  | 6.9x |  |
| poly1305 | 16 KiB | 113 MiB/s | 1760 MiB/s | 466 | 1698 | 15.6x | 1.0x faster |
| poly1305 | 1 MiB | 150 MiB/s | 1865 MiB/s | 474 | 4535 | 12.5x | 2.4x |
| chacha20-poly1305-seal | 1 KiB | 8.52 MiB/s | 285 MiB/s | 99.8 | 1377 | 33.5x | 4.8x |
| chacha20-poly1305-seal | 16 KiB | 9.07 MiB/s | 529 MiB/s | 113 | 1560 | 58.3x | 2.9x |
| chacha20-poly1305-seal | 1 MiB | 9.58 MiB/s | 556 MiB/s | 113 | 1438 | 58.1x | 2.6x |
| chacha20-poly1305-open | 1 KiB | 7.17 MiB/s | 281 MiB/s | 99.3 | 1533 | 39.3x | 5.4x |
| chacha20-poly1305-open | 16 KiB | 9.71 MiB/s | 522 MiB/s | 114 | 1618 | 53.7x | 3.1x |
| chacha20-poly1305-open | 1 MiB | 9.78 MiB/s | 552 MiB/s | 114 | 1490 | 56.4x | 2.7x |
| aes-gcm-seal | 1 KiB | 0.53 MiB/s | 112 MiB/s | 19.5 | 1420 | 211.8x | 12.7x |
| aes-gcm-seal | 16 KiB | 0.51 MiB/s | 648 MiB/s | 24.2 | 2721 | 1278.0x | 4.2x |
| aes-gcm-seal | 1 MiB | 0.59 MiB/s | 914 MiB/s | 24.6 | 3122 | 1543.8x | 3.4x |
| aes-gcm-open | 1 KiB | 0.50 MiB/s | 113 MiB/s | 19.4 | 1481 | 227.4x | 13.1x |
| aes-gcm-open | 16 KiB | 0.53 MiB/s | 639 MiB/s | 24.1 | 2720 | 1204.4x | 4.3x |
| aes-gcm-open | 1 MiB | 0.58 MiB/s | 908 MiB/s | 24.6 | 3000 | 1564.9x | 3.3x |
| hkdf | 32 B | 1.08 MiB/s | 1.41 MiB/s | 1.53 |  | 1.3x |  |
| aes-ecb-block | 16 B | 0.67 MiB/s | 132 MiB/s | 9.95 |  | 196.0x |  |
| x25519 |  | 177 ops/s | 3084 ops/s | 2010 | 23386 | 17.4x | 7.6x |
| ed25519-keygen |  | 91.9 ops/s | 1393 ops/s | 926 |  | 15.2x |  |
| ed25519-sign |  | 42.4 ops/s | 504 ops/s | 372 | 18237 | 11.9x | 36.2x |
| ed25519-verify |  | 43.2 ops/s | 617 ops/s | 407 | 6602 | 14.3x | 10.7x |
| ecdsa-p256-sign |  | 61.2 ops/s | 83.0 ops/s | 81.2 | 35530 | 1.4x | 427.9x |
| ecdsa-p256-verify |  | 90.3 ops/s | 176 ops/s | 169 | 12154 | 1.9x | 69.1x |
| rsa2048-sign |  | 14.3 ops/s | 18.8 ops/s | 19.5 | 1387 | 1.3x | 73.6x |
| rsa2048-verify |  | 784 ops/s | 1048 ops/s | 1104 | 46150 | 1.3x | 44.0x |
| bcrypt-pbkdf | 16 B | 3.00 ops/s | 4.51 ops/s | 4.73 |  | 1.5x |  |

Ratios in the last column are OpenSSL divided by this library; "faster" means at or past
OpenSSL's number within noise. "x25519" is one scalar multiplication
(`x25519.base_point`). `openssl speed` has no Ed25519 key-generation row. SHA-1 and SHA-256
stay at the m31 numbers on this host: it has no SHA extensions, so the SHA-NI kernels
(tested under emulation) are not selected. ECDSA, RSA and bcrypt were not touched
beyond what they inherit from SHA-512 (see "Left" in `docs/CRYPTO-ACCEL.md`).

## End to end: m31ssh and m31scp against a local sshd

`bench/crypto/e2e.py`: a disposable sshd on an ephemeral 127.0.0.1 port restricted to one
cipher, temp keys and HOME, every process killed by pid. Download is `ssh host cat file >
/dev/null` (server to client); upload is `scp file host:dest` (`m31scp` over SFTP against the
real `scp`); handshake is `ssh host true` (connect, key exchange, auth, exec, exit).
16 MiB payload, best of three; handshakes are the best of ten.

Before (origin/master):

| cipher | client | download MiB/s | upload MiB/s | handshake ms (min / median) |
|---|---|---:|---:|---:|
| chacha20-poly1305@openssh.com | openssh | 39.8 | 92.8 | 361 / 387 |
| chacha20-poly1305@openssh.com | m31 | 9.4 | 10.6 | 422 / 426 |
| aes256-gcm@openssh.com | openssh | 38.2 | 48.2 | 394 / 400 |
| aes256-gcm@openssh.com | m31 | 0.7 | 0.8 | 394 / 396 |

After:

| cipher | client | download MiB/s | upload MiB/s | handshake ms (min / median) |
|---|---|---:|---:|---:|
| chacha20-poly1305@openssh.com | openssh | 39.5 | 90.8 | 362 / 367 |
| chacha20-poly1305@openssh.com | m31 | 35.8 | 59.9 | 356 / 367 |
| aes256-gcm@openssh.com | openssh | 40.4 | 102.9 | 370 / 375 |
| aes256-gcm@openssh.com | m31 | 38.8 | 60.9 | 363 / 374 |

The real `ssh` and `scp` here are bounded by the one local sshd and the pipe, not by AES-NI
or ChaCha20, so the OpenSSH column barely depends on the cipher. `m31ssh` went from 9.4 and
0.7 MiB/s (ChaCha20-Poly1305 and AES-GCM downloads) to within about 10 percent of OpenSSH;
`m31scp` upload went from 10.6 and 0.8 to 60 MiB/s, which is the SFTP framing and copies in
the library now, not the cipher. The roughly 360 ms handshake is identical for both clients and
for both builds: it is sshd's own connection setup on this host, not crypto (the
m31-side key exchange plus Ed25519 verification went from about 34 ms of arithmetic to about
2 ms), so handshake latency is no longer something the crypto layer can improve.

## What changed in m31 (the "kernels off" column)

The emitted C pays for a noinline `rt_stack_check` per call, a bounds check per element access
and an overflow check per `+ - *`. Every change below is shaped around those and uses
nothing but m31:

* **ChaCha20-Poly1305**: the permutation is one function holding sixteen locals instead of a
  call per quarter round; words are re-masked to 32 bits at the top of each double round, so
  the C compiler can fold the sign fill out of every `>>` and drop the add overflow checks;
  the keystream is XORed into a pre-sized buffer a block at a time instead of `push` per
  octet; Poly1305 runs in place over the caller's buffers (no padded copy of
  AAD, ciphertext and lengths) with `wrapping_mul` limb products (the bound argument is in
  the file header). Seal 9.6 to 113 MiB/s.
* **SHA-512**: the heap-allocated two-halves `Word64` became plain ints with `wrapping_add`
  and masked shifts: 0.86 to 115 MiB/s. SHA-256: masks that let the compiler fold sign
  fills: 47 to 70.
* **AES and GHASH**: the x^254 SWAR S-box (about 14 field multiplications per eight bytes)
  became the Boyar-Peralta 113-gate circuit on bitsliced state, four blocks at a time, still
  no table and no secret-dependent branch or index; GHASH became BearSSL's carry-less
  multiply built from integer multiplications with "holes", with Karatsuba. 0.59 to
  24.6 MiB/s, constant time at the source level, recorded in both file headers.
* **Field arithmetic**: 16 limbs of 16 bits (256 products per multiplication, three allocating
  carry passes) became ref10's 10 limbs of radix 2^25.5 (100 products, one carry chain)
  with ref10's addition chains for inversion; the X25519 ladder and the Ed25519 scalar
  multiplication write into scratch elements allocated once. x25519 5.65 to 0.50 ms with
  the kernels off.

No secret-indexed table lookup was introduced anywhere.

## Evidence for a rotate primitive (a proposal, not implemented)

A hand-written C prototype outside the repository (scalar ChaCha20, one block at a time,
`gcc -O2`, this machine): with `uint32_t` and `(x << n) | (x >> (32 - n))` it runs at
429 MiB/s; with `uint64_t` words masked to 32 bits after every operation, which is what m31
has to emit, 222 MiB/s: **1.9 times slower**, before m31's own bounds and overflow checks
(the m31 version of the same loop measures 98 MiB/s before the register-level
restructuring and 149 after). A `rotl32(x, n)` intrinsic lowered to a native rotate would
recover most of that gap for ChaCha20, SHA-1 and SHA-256 in the portable path. With the
kernels in place it matters only on CPUs without them.
