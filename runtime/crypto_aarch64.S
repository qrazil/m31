/* aarch64 crypto kernels: ARMv8 AES + PMULL (GCM), NEON ChaCha20, umulh
 * Poly1305, SHA-1 and SHA-256 with the SHA extensions, and scalar SHA-512.
 *
 * #included at the end of ctx_switch_aarch64.S (and so assembled as part of
 * it); runtime/crypto_glue.c is the C side that checks arguments and decides
 * whether the CPU may run each kernel (getauxval(AT_HWCAP) on Linux,
 * sysctlbyname("hw.optional.arm.FEAT_*") on macOS). AAPCS64: arguments in
 * x0-x7, scratch in x9-x17 and v0-v7/v16-v31; callee-saved x19-x28 and d8-d15
 * are saved by the functions that use them. x18 (the platform register on
 * Apple) is never touched.
 *
 * Constant time by construction: AESE/AESMC and PMULL have no secret-indexed
 * lookups; ChaCha20 and Poly1305 are straight-line add/xor/rotate/multiply
 * code; loops are bounded by public block counts. The hashes only ever see
 * public data in this library, and are table-free too.
 *
 * Data pointers may be unaligned (ld1/st1 and ldp/stp tolerate it on normal
 * memory). A buffer is never read or written past `n` blocks (or `len` octets
 * for ChaCha20).
 */
#ifndef C_SYM
#if defined(__APPLE__)
#define C_SYM(x) _##x
#else
#define C_SYM(x) x
#endif
#endif

#if defined(__APPLE__)
#define CRYPTO_FUNC(name) .globl C_SYM(name); .p2align 4; C_SYM(name):
#define CRYPTO_END(name)
#else
#define CRYPTO_FUNC(name) .globl C_SYM(name); .type C_SYM(name), %function; .p2align 4; C_SYM(name):
#define CRYPTO_END(name) .size C_SYM(name), . - C_SYM(name)
/* The crypto and SHA-3 (SHA-512 instructions) extensions are default on Apple's
 * arm64 targets; elsewhere the assembler has to be told. */
    .arch armv8.2-a+crypto
#endif

    .text

/* ------------------------------------------------------------------------
 * AES
 * ------------------------------------------------------------------------ */

/* void rt_k_aes_ecb(const uint8_t *rk, uint64_t rounds, const uint8_t *in,
 *                   uint8_t *out, uint64_t n)
 *   x0 rk, x1 rounds, x2 in, x3 out, x4 n */
CRYPTO_FUNC(rt_k_aes_ecb)
    cbz x4, 9f
1:  cmp x4, #4
    b.lo 4f
    ld1 {v0.16b-v3.16b}, [x2], #64
    mov x9, x0
    sub x10, x1, #1
2:  ld1 {v16.16b}, [x9], #16
    aese v0.16b, v16.16b
    aesmc v0.16b, v0.16b
    aese v1.16b, v16.16b
    aesmc v1.16b, v1.16b
    aese v2.16b, v16.16b
    aesmc v2.16b, v2.16b
    aese v3.16b, v16.16b
    aesmc v3.16b, v3.16b
    subs x10, x10, #1
    b.ne 2b
    ld1 {v16.16b}, [x9], #16
    ld1 {v17.16b}, [x9]
    aese v0.16b, v16.16b
    aese v1.16b, v16.16b
    aese v2.16b, v16.16b
    aese v3.16b, v16.16b
    eor v0.16b, v0.16b, v17.16b
    eor v1.16b, v1.16b, v17.16b
    eor v2.16b, v2.16b, v17.16b
    eor v3.16b, v3.16b, v17.16b
    st1 {v0.16b-v3.16b}, [x3], #64
    sub x4, x4, #4
    b 1b
4:  cbz x4, 9f
    ld1 {v0.16b}, [x2], #16
    mov x9, x0
    sub x10, x1, #1
5:  ld1 {v16.16b}, [x9], #16
    aese v0.16b, v16.16b
    aesmc v0.16b, v0.16b
    subs x10, x10, #1
    b.ne 5b
    ld1 {v16.16b}, [x9], #16
    ld1 {v17.16b}, [x9]
    aese v0.16b, v16.16b
    eor v0.16b, v0.16b, v17.16b
    st1 {v0.16b}, [x3], #16
    sub x4, x4, #1
    b 4b
9:  movi v0.16b, #0
    ret
CRYPTO_END(rt_k_aes_ecb)

/* void rt_k_aes_ctr(const uint8_t *rk, uint64_t rounds, uint8_t *ctr,
 *                   const uint8_t *in, uint8_t *out, uint64_t n)
 *   x0 rk, x1 rounds, x2 ctr (16 octets, big-endian, updated), x3 in, x4 out,
 *   x5 n. The counter lives in x11 (high 64 bits) and x12 (low) as native
 *   integers, rebuilt big-endian per block with rev; the increment is
 *   adds/cinc across all 128 bits. */
.macro CTR_BLOCK v
    rev x13, x11
    rev x14, x12
    fmov d\v, x13
    mov v\v\().d[1], x14
    adds x12, x12, #1
    cinc x11, x11, cs
.endm

CRYPTO_FUNC(rt_k_aes_ctr)
    cbz x5, 9f
    ldp x11, x12, [x2]
    rev x11, x11
    rev x12, x12
1:  cmp x5, #4
    b.lo 4f
    CTR_BLOCK 0
    CTR_BLOCK 1
    CTR_BLOCK 2
    CTR_BLOCK 3
    mov x9, x0
    sub x10, x1, #1
2:  ld1 {v16.16b}, [x9], #16
    aese v0.16b, v16.16b
    aesmc v0.16b, v0.16b
    aese v1.16b, v16.16b
    aesmc v1.16b, v1.16b
    aese v2.16b, v16.16b
    aesmc v2.16b, v2.16b
    aese v3.16b, v16.16b
    aesmc v3.16b, v3.16b
    subs x10, x10, #1
    b.ne 2b
    ld1 {v16.16b}, [x9], #16
    ld1 {v17.16b}, [x9]
    aese v0.16b, v16.16b
    aese v1.16b, v16.16b
    aese v2.16b, v16.16b
    aese v3.16b, v16.16b
    eor v0.16b, v0.16b, v17.16b
    eor v1.16b, v1.16b, v17.16b
    eor v2.16b, v2.16b, v17.16b
    eor v3.16b, v3.16b, v17.16b
    ld1 {v4.16b-v7.16b}, [x3], #64
    eor v0.16b, v0.16b, v4.16b
    eor v1.16b, v1.16b, v5.16b
    eor v2.16b, v2.16b, v6.16b
    eor v3.16b, v3.16b, v7.16b
    st1 {v0.16b-v3.16b}, [x4], #64
    sub x5, x5, #4
    b 1b
4:  cbz x5, 8f
    CTR_BLOCK 0
    mov x9, x0
    sub x10, x1, #1
5:  ld1 {v16.16b}, [x9], #16
    aese v0.16b, v16.16b
    aesmc v0.16b, v0.16b
    subs x10, x10, #1
    b.ne 5b
    ld1 {v16.16b}, [x9], #16
    ld1 {v17.16b}, [x9]
    aese v0.16b, v16.16b
    eor v0.16b, v0.16b, v17.16b
    ld1 {v4.16b}, [x3], #16
    eor v0.16b, v0.16b, v4.16b
    st1 {v0.16b}, [x4], #16
    sub x5, x5, #1
    b 4b
8:  rev x11, x11
    rev x12, x12
    stp x11, x12, [x2]
    movi v0.16b, #0
9:  ret
CRYPTO_END(rt_k_aes_ctr)

/* ------------------------------------------------------------------------
 * GHASH with PMULL
 * ------------------------------------------------------------------------ */

/* void rt_k_ghash(const uint8_t *h, uint8_t *acc, const uint8_t *data,
 *                 uint64_t n)
 *   x0 h, x1 acc, x2 data, x3 n. Operands are held in "natural" bit order: the
 *   bits of every octet reversed (rbit), so a 128-bit block is the little-endian
 *   integer whose bit i is GCM's coefficient of x^i, and GCM's multiplication
 *   is the ordinary carry-less product reduced by x^128 = x^7 + x^2 + x + 1
 *   (constant 0x87). The 256-bit product is folded twice: the high half times
 *   0x87 (two PMULLs), whose 7 bits of overflow times 0x87 again. */
CRYPTO_FUNC(rt_k_ghash)
    cbz x3, 9f
    ld1 {v1.16b}, [x0]
    rbit v1.16b, v1.16b
    ld1 {v0.16b}, [x1]
    rbit v0.16b, v0.16b
    movi v31.16b, #0
    mov x9, #0x87
    dup v30.2d, x9
1:  ld1 {v2.16b}, [x2], #16
    rbit v2.16b, v2.16b
    eor v0.16b, v0.16b, v2.16b
    ext v4.16b, v0.16b, v0.16b, #8
    pmull v2.1q, v0.1d, v1.1d
    pmull2 v3.1q, v0.2d, v1.2d
    pmull v5.1q, v4.1d, v1.1d
    pmull2 v6.1q, v4.2d, v1.2d
    eor v5.16b, v5.16b, v6.16b
    ext v6.16b, v31.16b, v5.16b, #8
    eor v2.16b, v2.16b, v6.16b
    ext v6.16b, v5.16b, v31.16b, #8
    eor v3.16b, v3.16b, v6.16b
    pmull v4.1q, v3.1d, v30.1d
    pmull2 v5.1q, v3.2d, v30.2d
    ext v6.16b, v31.16b, v5.16b, #8
    eor v4.16b, v4.16b, v6.16b
    ext v6.16b, v5.16b, v31.16b, #8
    pmull v6.1q, v6.1d, v30.1d
    eor v2.16b, v2.16b, v4.16b
    eor v0.16b, v2.16b, v6.16b
    sub x3, x3, #1
    cbnz x3, 1b
    rbit v0.16b, v0.16b
    st1 {v0.16b}, [x1]
9:  ret
CRYPTO_END(rt_k_ghash)

/* ------------------------------------------------------------------------
 * ChaCha20, NEON, four blocks at a time
 * ------------------------------------------------------------------------ */

    .p2align 4
chacha_sigma:
    .long 0x61707865, 0x3320646e, 0x79622d32, 0x6b206574
chacha_lane:
    .long 0, 1, 2, 3

/* One quarter round on whole vectors (each holds one state word for four
 * blocks); t is scratch. */
.macro QR a, b, c, d, t
    add \a\().4s, \a\().4s, \b\().4s
    eor \d\().16b, \d\().16b, \a\().16b
    rev32 \d\().8h, \d\().8h
    add \c\().4s, \c\().4s, \d\().4s
    eor \t\().16b, \b\().16b, \c\().16b
    shl \b\().4s, \t\().4s, #12
    sri \b\().4s, \t\().4s, #20
    add \a\().4s, \a\().4s, \b\().4s
    eor \t\().16b, \d\().16b, \a\().16b
    shl \d\().4s, \t\().4s, #8
    sri \d\().4s, \t\().4s, #24
    add \c\().4s, \c\().4s, \d\().4s
    eor \t\().16b, \b\().16b, \c\().16b
    shl \b\().4s, \t\().4s, #7
    sri \b\().4s, \t\().4s, #25
.endm

/* void rt_k_chacha20(const uint8_t *key, const uint8_t *iv, const uint8_t *in,
 *                    uint8_t *out, uint64_t len)
 *   x0 key (32), x1 iv (16: state words 12-15, counter first), x2 in, x3 out,
 *   x4 len. Frame: 256 octets of base state (one vector per word, four blocks
 *   wide) and 256 of keystream, below the saved d8-d15. */
CRYPTO_FUNC(rt_k_chacha20)
    cbz x4, 9f
    stp x29, x30, [sp, #-96]!
    mov x29, sp
    stp d8, d9, [sp, #16]
    stp d10, d11, [sp, #32]
    stp d12, d13, [sp, #48]
    stp d14, d15, [sp, #64]
    sub sp, sp, #512
    adr x9, chacha_sigma
    ld1 {v16.4s}, [x9]
    ld1 {v17.4s, v18.4s}, [x0]
    ld1 {v19.4s}, [x1]
    adr x9, chacha_lane
    ld1 {v20.4s}, [x9]

    dup v21.4s, v16.s[0]
    str q21, [sp, #0]
    dup v21.4s, v16.s[1]
    str q21, [sp, #16]
    dup v21.4s, v16.s[2]
    str q21, [sp, #32]
    dup v21.4s, v16.s[3]
    str q21, [sp, #48]
    dup v21.4s, v17.s[0]
    str q21, [sp, #64]
    dup v21.4s, v17.s[1]
    str q21, [sp, #80]
    dup v21.4s, v17.s[2]
    str q21, [sp, #96]
    dup v21.4s, v17.s[3]
    str q21, [sp, #112]
    dup v21.4s, v18.s[0]
    str q21, [sp, #128]
    dup v21.4s, v18.s[1]
    str q21, [sp, #144]
    dup v21.4s, v18.s[2]
    str q21, [sp, #160]
    dup v21.4s, v18.s[3]
    str q21, [sp, #176]
    dup v21.4s, v19.s[0]
    add v21.4s, v21.4s, v20.4s
    str q21, [sp, #192]
    dup v21.4s, v19.s[1]
    str q21, [sp, #208]
    dup v21.4s, v19.s[2]
    str q21, [sp, #224]
    dup v21.4s, v19.s[3]
    str q21, [sp, #240]
1:
    ldr q0, [sp, #0]
    ldr q1, [sp, #16]
    ldr q2, [sp, #32]
    ldr q3, [sp, #48]
    ldr q4, [sp, #64]
    ldr q5, [sp, #80]
    ldr q6, [sp, #96]
    ldr q7, [sp, #112]
    ldr q8, [sp, #128]
    ldr q9, [sp, #144]
    ldr q10, [sp, #160]
    ldr q11, [sp, #176]
    ldr q12, [sp, #192]
    ldr q13, [sp, #208]
    ldr q14, [sp, #224]
    ldr q15, [sp, #240]
    mov w10, #10
2:  QR v0, v4, v8, v12, v16
    QR v1, v5, v9, v13, v17
    QR v2, v6, v10, v14, v18
    QR v3, v7, v11, v15, v19
    QR v0, v5, v10, v15, v16
    QR v1, v6, v11, v12, v17
    QR v2, v7, v8, v13, v18
    QR v3, v4, v9, v14, v19
    subs w10, w10, #1
    b.ne 2b
    ldr q16, [sp, #0]
    add v0.4s, v0.4s, v16.4s
    ldr q16, [sp, #16]
    add v1.4s, v1.4s, v16.4s
    ldr q16, [sp, #32]
    add v2.4s, v2.4s, v16.4s
    ldr q16, [sp, #48]
    add v3.4s, v3.4s, v16.4s
    ldr q16, [sp, #64]
    add v4.4s, v4.4s, v16.4s
    ldr q16, [sp, #80]
    add v5.4s, v5.4s, v16.4s
    ldr q16, [sp, #96]
    add v6.4s, v6.4s, v16.4s
    ldr q16, [sp, #112]
    add v7.4s, v7.4s, v16.4s
    ldr q16, [sp, #128]
    add v8.4s, v8.4s, v16.4s
    ldr q16, [sp, #144]
    add v9.4s, v9.4s, v16.4s
    ldr q16, [sp, #160]
    add v10.4s, v10.4s, v16.4s
    ldr q16, [sp, #176]
    add v11.4s, v11.4s, v16.4s
    ldr q16, [sp, #192]
    add v12.4s, v12.4s, v16.4s
    ldr q16, [sp, #208]
    add v13.4s, v13.4s, v16.4s
    ldr q16, [sp, #224]
    add v14.4s, v14.4s, v16.4s
    ldr q16, [sp, #240]
    add v15.4s, v15.4s, v16.4s
    trn1 v16.4s, v0.4s, v1.4s
    trn2 v17.4s, v0.4s, v1.4s
    trn1 v18.4s, v2.4s, v3.4s
    trn2 v19.4s, v2.4s, v3.4s
    trn1 v20.2d, v16.2d, v18.2d
    trn1 v21.2d, v17.2d, v19.2d
    trn2 v22.2d, v16.2d, v18.2d
    trn2 v23.2d, v17.2d, v19.2d
    str q20, [sp, #256]
    str q21, [sp, #320]
    str q22, [sp, #384]
    str q23, [sp, #448]
    trn1 v16.4s, v4.4s, v5.4s
    trn2 v17.4s, v4.4s, v5.4s
    trn1 v18.4s, v6.4s, v7.4s
    trn2 v19.4s, v6.4s, v7.4s
    trn1 v20.2d, v16.2d, v18.2d
    trn1 v21.2d, v17.2d, v19.2d
    trn2 v22.2d, v16.2d, v18.2d
    trn2 v23.2d, v17.2d, v19.2d
    str q20, [sp, #272]
    str q21, [sp, #336]
    str q22, [sp, #400]
    str q23, [sp, #464]
    trn1 v16.4s, v8.4s, v9.4s
    trn2 v17.4s, v8.4s, v9.4s
    trn1 v18.4s, v10.4s, v11.4s
    trn2 v19.4s, v10.4s, v11.4s
    trn1 v20.2d, v16.2d, v18.2d
    trn1 v21.2d, v17.2d, v19.2d
    trn2 v22.2d, v16.2d, v18.2d
    trn2 v23.2d, v17.2d, v19.2d
    str q20, [sp, #288]
    str q21, [sp, #352]
    str q22, [sp, #416]
    str q23, [sp, #480]
    trn1 v16.4s, v12.4s, v13.4s
    trn2 v17.4s, v12.4s, v13.4s
    trn1 v18.4s, v14.4s, v15.4s
    trn2 v19.4s, v14.4s, v15.4s
    trn1 v20.2d, v16.2d, v18.2d
    trn1 v21.2d, v17.2d, v19.2d
    trn2 v22.2d, v16.2d, v18.2d
    trn2 v23.2d, v17.2d, v19.2d
    str q20, [sp, #304]
    str q21, [sp, #368]
    str q22, [sp, #432]
    str q23, [sp, #496]
    ldr q0, [sp, #192]
    movi v1.4s, #4
    add v0.4s, v0.4s, v1.4s
    str q0, [sp, #192]
    cmp x4, #256
    b.lo 4f
    ld1 {v0.16b-v3.16b}, [x2], #64
    ld1 {v4.16b-v7.16b}, [x2], #64
    ld1 {v8.16b-v11.16b}, [x2], #64
    ld1 {v12.16b-v15.16b}, [x2], #64
    add x9, sp, #256
    ld1 {v16.16b-v19.16b}, [x9], #64
    ld1 {v20.16b-v23.16b}, [x9], #64
    ld1 {v24.16b-v27.16b}, [x9], #64
    ld1 {v28.16b-v31.16b}, [x9], #64
    eor v0.16b, v0.16b, v16.16b
    eor v1.16b, v1.16b, v17.16b
    eor v2.16b, v2.16b, v18.16b
    eor v3.16b, v3.16b, v19.16b
    eor v4.16b, v4.16b, v20.16b
    eor v5.16b, v5.16b, v21.16b
    eor v6.16b, v6.16b, v22.16b
    eor v7.16b, v7.16b, v23.16b
    eor v8.16b, v8.16b, v24.16b
    eor v9.16b, v9.16b, v25.16b
    eor v10.16b, v10.16b, v26.16b
    eor v11.16b, v11.16b, v27.16b
    eor v12.16b, v12.16b, v28.16b
    eor v13.16b, v13.16b, v29.16b
    eor v14.16b, v14.16b, v30.16b
    eor v15.16b, v15.16b, v31.16b
    st1 {v0.16b-v3.16b}, [x3], #64
    st1 {v4.16b-v7.16b}, [x3], #64
    st1 {v8.16b-v11.16b}, [x3], #64
    st1 {v12.16b-v15.16b}, [x3], #64
    subs x4, x4, #256
    b.ne 1b
    b 8f
4:  mov x9, #0
    add x10, sp, #256
5:  ldrb w11, [x2, x9]
    ldrb w12, [x10, x9]
    eor w11, w11, w12
    strb w11, [x3, x9]
    add x9, x9, #1
    cmp x9, x4
    b.ne 5b
8:  movi v0.16b, #0
    .irp i, 0, 16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240, 256, 272, 288, 304, 320, 336, 352, 368, 384, 400, 416, 432, 448, 464, 480, 496
    str q0, [sp, #\i]
    .endr
    add sp, sp, #512
    ldp d8, d9, [sp, #16]
    ldp d10, d11, [sp, #32]
    ldp d12, d13, [sp, #48]
    ldp d14, d15, [sp, #64]
    ldp x29, x30, [sp], #96
9:  ret
CRYPTO_END(rt_k_chacha20)

/* ------------------------------------------------------------------------
 * Poly1305, 64-bit limbs with mul/umulh
 * ------------------------------------------------------------------------ */

/* void rt_k_poly1305_blocks(uint8_t *st, const uint8_t *data, uint64_t n,
 *                           uint64_t hibit)
 *   x0 st (h0 h1 h2 r0 r1, five little-endian u64), x1 data, x2 n, x3 hibit
 *   (0 or 1). h = (h + block + hibit * 2^128) * r mod 2^130 - 5, partially
 *   reduced (h2 stays below 8): a 4-limb schoolbook product, then bits 130 and
 *   up folded back as 5 times themselves. Same algorithm as the x86-64 kernel. */
CRYPTO_FUNC(rt_k_poly1305_blocks)
    cbz x2, 9f
    ldp x4, x5, [x0]
    ldr x6, [x0, #16]
    ldp x7, x8, [x0, #24]
1:  ldp x9, x10, [x1], #16
    adds x4, x4, x9
    adcs x5, x5, x10
    adc x6, x6, x3
    mul x9, x4, x7
    umulh x10, x4, x7
    mul x11, x4, x8
    umulh x12, x4, x8
    mul x13, x5, x7
    umulh x14, x5, x7
    adds x10, x10, x11
    cinc x12, x12, cs
    adds x10, x10, x13
    adcs x12, x12, x14
    cset x15, cs
    mul x11, x5, x8
    umulh x13, x5, x8
    adds x12, x12, x11
    adc x15, x15, x13
    mul x11, x6, x7
    umulh x13, x6, x7
    adds x12, x12, x11
    adc x15, x15, x13
    mul x11, x6, x8
    add x15, x15, x11
    and x6, x12, #3
    extr x11, x15, x12, #2
    lsr x13, x15, #2
    mov x14, #5
    mul x4, x11, x14
    umulh x11, x11, x14
    madd x13, x13, x14, x11
    adds x4, x4, x9
    adcs x5, x10, x13
    adc x6, x6, xzr
    sub x2, x2, #1
    cbnz x2, 1b
    stp x4, x5, [x0]
    str x6, [x0, #16]
9:  ret
CRYPTO_END(rt_k_poly1305_blocks)

/* void rt_k_poly1305_finish(const uint8_t *st, const uint8_t *pad, uint8_t *out)
 *   x0 st, x1 pad (the one-time key's last 16 octets), x2 out. h mod 2^130 - 5
 *   by a masked conditional subtraction, then (h + pad) mod 2^128. No branch
 *   on h. */
CRYPTO_FUNC(rt_k_poly1305_finish)
    ldp x4, x5, [x0]
    ldr x6, [x0, #16]
    adds x7, x4, #5
    adcs x8, x5, xzr
    adc x9, x6, xzr
    lsr x9, x9, #2
    and x9, x9, #1
    neg x9, x9
    eor x7, x7, x4
    and x7, x7, x9
    eor x4, x4, x7
    eor x8, x8, x5
    and x8, x8, x9
    eor x5, x5, x8
    ldp x10, x11, [x1]
    adds x4, x4, x10
    adc x5, x5, x11
    stp x4, x5, [x2]
    ret
CRYPTO_END(rt_k_poly1305_finish)

/* ------------------------------------------------------------------------
 * SHA-256 with the SHA2 instructions
 * ------------------------------------------------------------------------ */

    .p2align 4
sha256_k:
    .long 0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5
    .long 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5
    .long 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3
    .long 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174
    .long 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc
    .long 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da
    .long 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7
    .long 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967
    .long 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13
    .long 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85
    .long 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3
    .long 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070
    .long 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5
    .long 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3
    .long 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208
    .long 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2

/* void rt_k_sha256(uint8_t *state, const uint8_t *data, uint64_t n)
 *   x0 state (a..h, eight little-endian u32), x1 data, x2 n blocks. v0 = abcd,
 *   v1 = efgh, v2/v3 the saved state, v16-v19 the message vectors in
 *   rotation, v4 message + constants, v5 a copy of abcd for SHA256H2, v6 the
 *   constants. ACLE's reference sequence. */
CRYPTO_FUNC(rt_k_sha256)
    cbz x2, 9f
    ld1 {v0.4s, v1.4s}, [x0]
1:  ld1 {v16.16b-v19.16b}, [x1], #64
    rev32 v16.16b, v16.16b
    rev32 v17.16b, v17.16b
    rev32 v18.16b, v18.16b
    rev32 v19.16b, v19.16b
    mov v2.16b, v0.16b
    mov v3.16b, v1.16b
    adr x9, sha256_k
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v16.4s, v6.4s
    sha256su0 v16.4s, v17.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v16.4s, v18.4s, v19.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v17.4s, v6.4s
    sha256su0 v17.4s, v18.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v17.4s, v19.4s, v16.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v18.4s, v6.4s
    sha256su0 v18.4s, v19.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v18.4s, v16.4s, v17.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v19.4s, v6.4s
    sha256su0 v19.4s, v16.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v19.4s, v17.4s, v18.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v16.4s, v6.4s
    sha256su0 v16.4s, v17.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v16.4s, v18.4s, v19.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v17.4s, v6.4s
    sha256su0 v17.4s, v18.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v17.4s, v19.4s, v16.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v18.4s, v6.4s
    sha256su0 v18.4s, v19.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v18.4s, v16.4s, v17.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v19.4s, v6.4s
    sha256su0 v19.4s, v16.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v19.4s, v17.4s, v18.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v16.4s, v6.4s
    sha256su0 v16.4s, v17.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v16.4s, v18.4s, v19.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v17.4s, v6.4s
    sha256su0 v17.4s, v18.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v17.4s, v19.4s, v16.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v18.4s, v6.4s
    sha256su0 v18.4s, v19.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v18.4s, v16.4s, v17.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v19.4s, v6.4s
    sha256su0 v19.4s, v16.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    sha256su1 v19.4s, v17.4s, v18.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v16.4s, v6.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v17.4s, v6.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v18.4s, v6.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    ld1 {v6.4s}, [x9], #16
    add v4.4s, v19.4s, v6.4s
    mov v5.16b, v0.16b
    sha256h q0, q1, v4.4s
    sha256h2 q1, q5, v4.4s
    add v0.4s, v0.4s, v2.4s
    add v1.4s, v1.4s, v3.4s
    sub x2, x2, #1
    cbnz x2, 1b
    st1 {v0.4s, v1.4s}, [x0]
9:  ret
CRYPTO_END(rt_k_sha256)

/* ------------------------------------------------------------------------
 * SHA-1 with the SHA1 instructions
 * ------------------------------------------------------------------------ */

/* void rt_k_sha1(uint8_t *state, const uint8_t *data, uint64_t n)
 *   x0 state (a..e, five little-endian u32), x1 data, x2 n blocks. v0 = abcd,
 *   s1/s2 = e in alternation, v16-v19 the message vectors in rotation,
 *   v20-v23 the four round constants, v3/v4 the saved state. */
CRYPTO_FUNC(rt_k_sha1)
    cbz x2, 9f
    ld1 {v0.4s}, [x0]
    ldr s1, [x0, #16]
    movz w9, #0x7999
    movk w9, #0x5A82, lsl #16
    dup v20.4s, w9
    movz w9, #0xEBA1
    movk w9, #0x6ED9, lsl #16
    dup v21.4s, w9
    movz w9, #0xBCDC
    movk w9, #0x8F1B, lsl #16
    dup v22.4s, w9
    movz w9, #0xC1D6
    movk w9, #0xCA62, lsl #16
    dup v23.4s, w9
1:  ld1 {v16.16b-v19.16b}, [x1], #64
    rev32 v16.16b, v16.16b
    rev32 v17.16b, v17.16b
    rev32 v18.16b, v18.16b
    rev32 v19.16b, v19.16b
    mov v3.16b, v0.16b
    mov v4.16b, v1.16b
    add v5.4s, v16.4s, v20.4s
    sha1h s2, s0
    sha1c q0, s1, v5.4s
    sha1su0 v16.4s, v17.4s, v18.4s
    sha1su1 v16.4s, v19.4s
    add v5.4s, v17.4s, v20.4s
    sha1h s1, s0
    sha1c q0, s2, v5.4s
    sha1su0 v17.4s, v18.4s, v19.4s
    sha1su1 v17.4s, v16.4s
    add v5.4s, v18.4s, v20.4s
    sha1h s2, s0
    sha1c q0, s1, v5.4s
    sha1su0 v18.4s, v19.4s, v16.4s
    sha1su1 v18.4s, v17.4s
    add v5.4s, v19.4s, v20.4s
    sha1h s1, s0
    sha1c q0, s2, v5.4s
    sha1su0 v19.4s, v16.4s, v17.4s
    sha1su1 v19.4s, v18.4s
    add v5.4s, v16.4s, v20.4s
    sha1h s2, s0
    sha1c q0, s1, v5.4s
    sha1su0 v16.4s, v17.4s, v18.4s
    sha1su1 v16.4s, v19.4s
    add v5.4s, v17.4s, v21.4s
    sha1h s1, s0
    sha1p q0, s2, v5.4s
    sha1su0 v17.4s, v18.4s, v19.4s
    sha1su1 v17.4s, v16.4s
    add v5.4s, v18.4s, v21.4s
    sha1h s2, s0
    sha1p q0, s1, v5.4s
    sha1su0 v18.4s, v19.4s, v16.4s
    sha1su1 v18.4s, v17.4s
    add v5.4s, v19.4s, v21.4s
    sha1h s1, s0
    sha1p q0, s2, v5.4s
    sha1su0 v19.4s, v16.4s, v17.4s
    sha1su1 v19.4s, v18.4s
    add v5.4s, v16.4s, v21.4s
    sha1h s2, s0
    sha1p q0, s1, v5.4s
    sha1su0 v16.4s, v17.4s, v18.4s
    sha1su1 v16.4s, v19.4s
    add v5.4s, v17.4s, v21.4s
    sha1h s1, s0
    sha1p q0, s2, v5.4s
    sha1su0 v17.4s, v18.4s, v19.4s
    sha1su1 v17.4s, v16.4s
    add v5.4s, v18.4s, v22.4s
    sha1h s2, s0
    sha1m q0, s1, v5.4s
    sha1su0 v18.4s, v19.4s, v16.4s
    sha1su1 v18.4s, v17.4s
    add v5.4s, v19.4s, v22.4s
    sha1h s1, s0
    sha1m q0, s2, v5.4s
    sha1su0 v19.4s, v16.4s, v17.4s
    sha1su1 v19.4s, v18.4s
    add v5.4s, v16.4s, v22.4s
    sha1h s2, s0
    sha1m q0, s1, v5.4s
    sha1su0 v16.4s, v17.4s, v18.4s
    sha1su1 v16.4s, v19.4s
    add v5.4s, v17.4s, v22.4s
    sha1h s1, s0
    sha1m q0, s2, v5.4s
    sha1su0 v17.4s, v18.4s, v19.4s
    sha1su1 v17.4s, v16.4s
    add v5.4s, v18.4s, v22.4s
    sha1h s2, s0
    sha1m q0, s1, v5.4s
    sha1su0 v18.4s, v19.4s, v16.4s
    sha1su1 v18.4s, v17.4s
    add v5.4s, v19.4s, v23.4s
    sha1h s1, s0
    sha1p q0, s2, v5.4s
    sha1su0 v19.4s, v16.4s, v17.4s
    sha1su1 v19.4s, v18.4s
    add v5.4s, v16.4s, v23.4s
    sha1h s2, s0
    sha1p q0, s1, v5.4s
    add v5.4s, v17.4s, v23.4s
    sha1h s1, s0
    sha1p q0, s2, v5.4s
    add v5.4s, v18.4s, v23.4s
    sha1h s2, s0
    sha1p q0, s1, v5.4s
    add v5.4s, v19.4s, v23.4s
    sha1h s1, s0
    sha1p q0, s2, v5.4s
    add v0.4s, v0.4s, v3.4s
    add v1.2s, v1.2s, v4.2s
    sub x2, x2, #1
    cbnz x2, 1b
    st1 {v0.4s}, [x0]
    str s1, [x0, #16]
9:  ret
CRYPTO_END(rt_k_sha1)

/* ------------------------------------------------------------------------
 * SHA-512, plain 64-bit registers
 * ------------------------------------------------------------------------ */

    .p2align 4
sha512_k:
    .quad 0x428a2f98d728ae22, 0x7137449123ef65cd
    .quad 0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc
    .quad 0x3956c25bf348b538, 0x59f111f1b605d019
    .quad 0x923f82a4af194f9b, 0xab1c5ed5da6d8118
    .quad 0xd807aa98a3030242, 0x12835b0145706fbe
    .quad 0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2
    .quad 0x72be5d74f27b896f, 0x80deb1fe3b1696b1
    .quad 0x9bdc06a725c71235, 0xc19bf174cf692694
    .quad 0xe49b69c19ef14ad2, 0xefbe4786384f25e3
    .quad 0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65
    .quad 0x2de92c6f592b0275, 0x4a7484aa6ea6e483
    .quad 0x5cb0a9dcbd41fbd4, 0x76f988da831153b5
    .quad 0x983e5152ee66dfab, 0xa831c66d2db43210
    .quad 0xb00327c898fb213f, 0xbf597fc7beef0ee4
    .quad 0xc6e00bf33da88fc2, 0xd5a79147930aa725
    .quad 0x06ca6351e003826f, 0x142929670a0e6e70
    .quad 0x27b70a8546d22ffc, 0x2e1b21385c26c926
    .quad 0x4d2c6dfc5ac42aed, 0x53380d139d95b3df
    .quad 0x650a73548baf63de, 0x766a0abb3c77b2a8
    .quad 0x81c2c92e47edaee6, 0x92722c851482353b
    .quad 0xa2bfe8a14cf10364, 0xa81a664bbc423001
    .quad 0xc24b8b70d0f89791, 0xc76c51a30654be30
    .quad 0xd192e819d6ef5218, 0xd69906245565a910
    .quad 0xf40e35855771202a, 0x106aa07032bbd1b8
    .quad 0x19a4c116b8d2d0c8, 0x1e376c085141ab53
    .quad 0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8
    .quad 0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb
    .quad 0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3
    .quad 0x748f82ee5defb2fc, 0x78a5636f43172f60
    .quad 0x84c87814a1f0ab72, 0x8cc702081a6439ec
    .quad 0x90befffa23631e28, 0xa4506cebde82bde9
    .quad 0xbef9a3f7b2c67915, 0xc67178f2e372532b
    .quad 0xca273eceea26619c, 0xd186b8c721c0c207
    .quad 0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178
    .quad 0x06f067aa72176fba, 0x0a637dc5a2c898a6
    .quad 0x113f9804bef90dae, 0x1b710b35131c471b
    .quad 0x28db77f523047d84, 0x32caab7b40c72493
    .quad 0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c
    .quad 0x4cc5d4becb3e42b6, 0x597f299cfc657e2a
    .quad 0x5fcb6fab3ad6faec, 0x6c44198c4a475817

/* One round; the eight state registers rotate roles between calls (pass
 * h a b c d e f g next). x13 is the byte offset t * 8 into both the constants
 * (x3) and the schedule on the stack. */
.macro S512RND a, b, c, d, e, f, g, h
    ror x12, \e, #14
    ror x14, \e, #18
    eor x12, x12, x14
    ror x14, \e, #41
    eor x12, x12, x14
    and x14, \e, \f
    bic x15, \g, \e
    eor x14, x14, x15
    ldr x15, [x3, x13]
    ldr x16, [sp, x13]
    add x12, x12, x14
    add x12, x12, x15
    add x12, x12, x16
    add x12, x12, \h
    add \d, \d, x12
    ror x14, \a, #28
    ror x15, \a, #34
    eor x14, x14, x15
    ror x15, \a, #39
    eor x14, x14, x15
    orr x16, \a, \b
    and x16, x16, \c
    and x15, \a, \b
    orr x15, x15, x16
    add x14, x14, x15
    add \h, x12, x14
    add x13, x13, #8
.endm

/* void rt_k_sha512(uint8_t *state, const uint8_t *data, uint64_t n)
 *   x0 state (a..h, eight little-endian u64), x1 data, x2 n blocks. Frame: 640
 *   octets of message schedule. a..h live in x4-x11. */
CRYPTO_FUNC(rt_k_sha512)
    cbz x2, 9f
    stp x29, x30, [sp, #-16]!
    sub sp, sp, #640
    adr x3, sha512_k
1:  mov x13, #0
2:  ldr x12, [x1, x13]
    rev x12, x12
    str x12, [sp, x13]
    add x13, x13, #8
    cmp x13, #128
    b.ne 2b
3:  sub x14, x13, #120
    ldr x12, [sp, x14]
    ror x14, x12, #1
    ror x15, x12, #8
    eor x14, x14, x15
    lsr x15, x12, #7
    eor x14, x14, x15
    sub x15, x13, #16
    ldr x12, [sp, x15]
    ror x15, x12, #19
    ror x16, x12, #61
    eor x15, x15, x16
    lsr x16, x12, #6
    eor x15, x15, x16
    add x14, x14, x15
    sub x15, x13, #128
    ldr x16, [sp, x15]
    add x14, x14, x16
    sub x15, x13, #56
    ldr x16, [sp, x15]
    add x14, x14, x16
    str x14, [sp, x13]
    add x13, x13, #8
    cmp x13, #640
    b.ne 3b
    ldp x4, x5, [x0]
    ldp x6, x7, [x0, #16]
    ldp x8, x9, [x0, #32]
    ldp x10, x11, [x0, #48]
    mov x13, #0
4:  S512RND x4, x5, x6, x7, x8, x9, x10, x11
    S512RND x11, x4, x5, x6, x7, x8, x9, x10
    S512RND x10, x11, x4, x5, x6, x7, x8, x9
    S512RND x9, x10, x11, x4, x5, x6, x7, x8
    S512RND x8, x9, x10, x11, x4, x5, x6, x7
    S512RND x7, x8, x9, x10, x11, x4, x5, x6
    S512RND x6, x7, x8, x9, x10, x11, x4, x5
    S512RND x5, x6, x7, x8, x9, x10, x11, x4
    cmp x13, #640
    b.ne 4b
    ldp x14, x15, [x0]
    add x4, x4, x14
    add x5, x5, x15
    stp x4, x5, [x0]
    ldp x14, x15, [x0, #16]
    add x6, x6, x14
    add x7, x7, x15
    stp x6, x7, [x0, #16]
    ldp x14, x15, [x0, #32]
    add x8, x8, x14
    add x9, x9, x15
    stp x8, x9, [x0, #32]
    ldp x14, x15, [x0, #48]
    add x10, x10, x14
    add x11, x11, x15
    stp x10, x11, [x0, #48]
    add x1, x1, #128
    sub x2, x2, #1
    cbnz x2, 1b
    add sp, sp, #640
    ldp x29, x30, [sp], #16
9:  ret
CRYPTO_END(rt_k_sha512)


/* ------------------------------------------------------------------------
 * GF(2^255 - 19) in ten signed limbs (radix 2^25.5): lib/crypto/field25519.m31's
 * multiply, add and subtract, instruction for instruction (ref10's fe_mul and
 * carry chain), so the kernel and the m31 fallback produce identical limbs.
 * Elements are `Array<int>` data: ten little-endian int64 limbs. `mul` keeps
 * only the low 64 bits, which is exact because the bounds in field25519.m31
 * keep every column under 2^62. Straight-line, no branch, no table.
 * ------------------------------------------------------------------------ */

/* The carry chain over h0..h9 (x3-x12; x13, x14 scratch, x15 = 2^25, x16 = 2^24,
 * x17 = 19), then
 * the ten stores to x0. */
.macro FE_CARRY_STORE
    mov x15, #0x2000000
    mov x16, #0x1000000
    add x13, x3, x15
    asr x13, x13, #26
    add x4, x4, x13
    lsl x14, x13, #26
    sub x3, x3, x14
    add x13, x7, x15
    asr x13, x13, #26
    add x8, x8, x13
    lsl x14, x13, #26
    sub x7, x7, x14
    add x13, x4, x16
    asr x13, x13, #25
    add x5, x5, x13
    lsl x14, x13, #25
    sub x4, x4, x14
    add x13, x8, x16
    asr x13, x13, #25
    add x9, x9, x13
    lsl x14, x13, #25
    sub x8, x8, x14
    add x13, x5, x15
    asr x13, x13, #26
    add x6, x6, x13
    lsl x14, x13, #26
    sub x5, x5, x14
    add x13, x9, x15
    asr x13, x13, #26
    add x10, x10, x13
    lsl x14, x13, #26
    sub x9, x9, x14
    add x13, x6, x16
    asr x13, x13, #25
    add x7, x7, x13
    lsl x14, x13, #25
    sub x6, x6, x14
    add x13, x10, x16
    asr x13, x13, #25
    add x11, x11, x13
    lsl x14, x13, #25
    sub x10, x10, x14
    add x13, x7, x15
    asr x13, x13, #26
    add x8, x8, x13
    lsl x14, x13, #26
    sub x7, x7, x14
    add x13, x11, x15
    asr x13, x13, #26
    add x12, x12, x13
    lsl x14, x13, #26
    sub x11, x11, x14
    add x13, x12, x16
    asr x13, x13, #25
    madd x3, x13, x17, x3
    lsl x14, x13, #25
    sub x12, x12, x14
    add x13, x3, x15
    asr x13, x13, #26
    add x4, x4, x13
    lsl x14, x13, #26
    sub x3, x3, x14
    str x3, [x0, #0]
    str x4, [x0, #8]
    str x5, [x0, #16]
    str x6, [x0, #24]
    str x7, [x0, #32]
    str x8, [x0, #40]
    str x9, [x0, #48]
    str x10, [x0, #56]
    str x11, [x0, #64]
    str x12, [x0, #72]
.endm

/* void rt_k_fe_mul(int64_t *out, const int64_t *f, const int64_t *g)
 *   x0 out, x1 f, x2 g (out may alias either). */
CRYPTO_FUNC(rt_k_fe_mul)
    sub sp, sp, #128
    mov x17, #19
    ldr x13, [x2, #8]
    mul x13, x13, x17
    str x13, [sp, #0]
    ldr x13, [x2, #16]
    mul x13, x13, x17
    str x13, [sp, #8]
    ldr x13, [x2, #24]
    mul x13, x13, x17
    str x13, [sp, #16]
    ldr x13, [x2, #32]
    mul x13, x13, x17
    str x13, [sp, #24]
    ldr x13, [x2, #40]
    mul x13, x13, x17
    str x13, [sp, #32]
    ldr x13, [x2, #48]
    mul x13, x13, x17
    str x13, [sp, #40]
    ldr x13, [x2, #56]
    mul x13, x13, x17
    str x13, [sp, #48]
    ldr x13, [x2, #64]
    mul x13, x13, x17
    str x13, [sp, #56]
    ldr x13, [x2, #72]
    mul x13, x13, x17
    str x13, [sp, #64]
    ldr x13, [x1, #8]
    lsl x13, x13, #1
    str x13, [sp, #72]
    ldr x13, [x1, #24]
    lsl x13, x13, #1
    str x13, [sp, #80]
    ldr x13, [x1, #40]
    lsl x13, x13, #1
    str x13, [sp, #88]
    ldr x13, [x1, #56]
    lsl x13, x13, #1
    str x13, [sp, #96]
    ldr x13, [x1, #72]
    lsl x13, x13, #1
    str x13, [sp, #104]
    ldr x13, [x1, #0]
    ldr x14, [x2, #0]
    mul x3, x13, x14
    ldr x13, [sp, #72]
    ldr x14, [sp, #64]
    madd x3, x13, x14, x3
    ldr x13, [x1, #16]
    ldr x14, [sp, #56]
    madd x3, x13, x14, x3
    ldr x13, [sp, #80]
    ldr x14, [sp, #48]
    madd x3, x13, x14, x3
    ldr x13, [x1, #32]
    ldr x14, [sp, #40]
    madd x3, x13, x14, x3
    ldr x13, [sp, #88]
    ldr x14, [sp, #32]
    madd x3, x13, x14, x3
    ldr x13, [x1, #48]
    ldr x14, [sp, #24]
    madd x3, x13, x14, x3
    ldr x13, [sp, #96]
    ldr x14, [sp, #16]
    madd x3, x13, x14, x3
    ldr x13, [x1, #64]
    ldr x14, [sp, #8]
    madd x3, x13, x14, x3
    ldr x13, [sp, #104]
    ldr x14, [sp, #0]
    madd x3, x13, x14, x3
    ldr x13, [x1, #0]
    ldr x14, [x2, #8]
    mul x4, x13, x14
    ldr x13, [x1, #8]
    ldr x14, [x2, #0]
    madd x4, x13, x14, x4
    ldr x13, [x1, #16]
    ldr x14, [sp, #64]
    madd x4, x13, x14, x4
    ldr x13, [x1, #24]
    ldr x14, [sp, #56]
    madd x4, x13, x14, x4
    ldr x13, [x1, #32]
    ldr x14, [sp, #48]
    madd x4, x13, x14, x4
    ldr x13, [x1, #40]
    ldr x14, [sp, #40]
    madd x4, x13, x14, x4
    ldr x13, [x1, #48]
    ldr x14, [sp, #32]
    madd x4, x13, x14, x4
    ldr x13, [x1, #56]
    ldr x14, [sp, #24]
    madd x4, x13, x14, x4
    ldr x13, [x1, #64]
    ldr x14, [sp, #16]
    madd x4, x13, x14, x4
    ldr x13, [x1, #72]
    ldr x14, [sp, #8]
    madd x4, x13, x14, x4
    ldr x13, [x1, #0]
    ldr x14, [x2, #16]
    mul x5, x13, x14
    ldr x13, [sp, #72]
    ldr x14, [x2, #8]
    madd x5, x13, x14, x5
    ldr x13, [x1, #16]
    ldr x14, [x2, #0]
    madd x5, x13, x14, x5
    ldr x13, [sp, #80]
    ldr x14, [sp, #64]
    madd x5, x13, x14, x5
    ldr x13, [x1, #32]
    ldr x14, [sp, #56]
    madd x5, x13, x14, x5
    ldr x13, [sp, #88]
    ldr x14, [sp, #48]
    madd x5, x13, x14, x5
    ldr x13, [x1, #48]
    ldr x14, [sp, #40]
    madd x5, x13, x14, x5
    ldr x13, [sp, #96]
    ldr x14, [sp, #32]
    madd x5, x13, x14, x5
    ldr x13, [x1, #64]
    ldr x14, [sp, #24]
    madd x5, x13, x14, x5
    ldr x13, [sp, #104]
    ldr x14, [sp, #16]
    madd x5, x13, x14, x5
    ldr x13, [x1, #0]
    ldr x14, [x2, #24]
    mul x6, x13, x14
    ldr x13, [x1, #8]
    ldr x14, [x2, #16]
    madd x6, x13, x14, x6
    ldr x13, [x1, #16]
    ldr x14, [x2, #8]
    madd x6, x13, x14, x6
    ldr x13, [x1, #24]
    ldr x14, [x2, #0]
    madd x6, x13, x14, x6
    ldr x13, [x1, #32]
    ldr x14, [sp, #64]
    madd x6, x13, x14, x6
    ldr x13, [x1, #40]
    ldr x14, [sp, #56]
    madd x6, x13, x14, x6
    ldr x13, [x1, #48]
    ldr x14, [sp, #48]
    madd x6, x13, x14, x6
    ldr x13, [x1, #56]
    ldr x14, [sp, #40]
    madd x6, x13, x14, x6
    ldr x13, [x1, #64]
    ldr x14, [sp, #32]
    madd x6, x13, x14, x6
    ldr x13, [x1, #72]
    ldr x14, [sp, #24]
    madd x6, x13, x14, x6
    ldr x13, [x1, #0]
    ldr x14, [x2, #32]
    mul x7, x13, x14
    ldr x13, [sp, #72]
    ldr x14, [x2, #24]
    madd x7, x13, x14, x7
    ldr x13, [x1, #16]
    ldr x14, [x2, #16]
    madd x7, x13, x14, x7
    ldr x13, [sp, #80]
    ldr x14, [x2, #8]
    madd x7, x13, x14, x7
    ldr x13, [x1, #32]
    ldr x14, [x2, #0]
    madd x7, x13, x14, x7
    ldr x13, [sp, #88]
    ldr x14, [sp, #64]
    madd x7, x13, x14, x7
    ldr x13, [x1, #48]
    ldr x14, [sp, #56]
    madd x7, x13, x14, x7
    ldr x13, [sp, #96]
    ldr x14, [sp, #48]
    madd x7, x13, x14, x7
    ldr x13, [x1, #64]
    ldr x14, [sp, #40]
    madd x7, x13, x14, x7
    ldr x13, [sp, #104]
    ldr x14, [sp, #32]
    madd x7, x13, x14, x7
    ldr x13, [x1, #0]
    ldr x14, [x2, #40]
    mul x8, x13, x14
    ldr x13, [x1, #8]
    ldr x14, [x2, #32]
    madd x8, x13, x14, x8
    ldr x13, [x1, #16]
    ldr x14, [x2, #24]
    madd x8, x13, x14, x8
    ldr x13, [x1, #24]
    ldr x14, [x2, #16]
    madd x8, x13, x14, x8
    ldr x13, [x1, #32]
    ldr x14, [x2, #8]
    madd x8, x13, x14, x8
    ldr x13, [x1, #40]
    ldr x14, [x2, #0]
    madd x8, x13, x14, x8
    ldr x13, [x1, #48]
    ldr x14, [sp, #64]
    madd x8, x13, x14, x8
    ldr x13, [x1, #56]
    ldr x14, [sp, #56]
    madd x8, x13, x14, x8
    ldr x13, [x1, #64]
    ldr x14, [sp, #48]
    madd x8, x13, x14, x8
    ldr x13, [x1, #72]
    ldr x14, [sp, #40]
    madd x8, x13, x14, x8
    ldr x13, [x1, #0]
    ldr x14, [x2, #48]
    mul x9, x13, x14
    ldr x13, [sp, #72]
    ldr x14, [x2, #40]
    madd x9, x13, x14, x9
    ldr x13, [x1, #16]
    ldr x14, [x2, #32]
    madd x9, x13, x14, x9
    ldr x13, [sp, #80]
    ldr x14, [x2, #24]
    madd x9, x13, x14, x9
    ldr x13, [x1, #32]
    ldr x14, [x2, #16]
    madd x9, x13, x14, x9
    ldr x13, [sp, #88]
    ldr x14, [x2, #8]
    madd x9, x13, x14, x9
    ldr x13, [x1, #48]
    ldr x14, [x2, #0]
    madd x9, x13, x14, x9
    ldr x13, [sp, #96]
    ldr x14, [sp, #64]
    madd x9, x13, x14, x9
    ldr x13, [x1, #64]
    ldr x14, [sp, #56]
    madd x9, x13, x14, x9
    ldr x13, [sp, #104]
    ldr x14, [sp, #48]
    madd x9, x13, x14, x9
    ldr x13, [x1, #0]
    ldr x14, [x2, #56]
    mul x10, x13, x14
    ldr x13, [x1, #8]
    ldr x14, [x2, #48]
    madd x10, x13, x14, x10
    ldr x13, [x1, #16]
    ldr x14, [x2, #40]
    madd x10, x13, x14, x10
    ldr x13, [x1, #24]
    ldr x14, [x2, #32]
    madd x10, x13, x14, x10
    ldr x13, [x1, #32]
    ldr x14, [x2, #24]
    madd x10, x13, x14, x10
    ldr x13, [x1, #40]
    ldr x14, [x2, #16]
    madd x10, x13, x14, x10
    ldr x13, [x1, #48]
    ldr x14, [x2, #8]
    madd x10, x13, x14, x10
    ldr x13, [x1, #56]
    ldr x14, [x2, #0]
    madd x10, x13, x14, x10
    ldr x13, [x1, #64]
    ldr x14, [sp, #64]
    madd x10, x13, x14, x10
    ldr x13, [x1, #72]
    ldr x14, [sp, #56]
    madd x10, x13, x14, x10
    ldr x13, [x1, #0]
    ldr x14, [x2, #64]
    mul x11, x13, x14
    ldr x13, [sp, #72]
    ldr x14, [x2, #56]
    madd x11, x13, x14, x11
    ldr x13, [x1, #16]
    ldr x14, [x2, #48]
    madd x11, x13, x14, x11
    ldr x13, [sp, #80]
    ldr x14, [x2, #40]
    madd x11, x13, x14, x11
    ldr x13, [x1, #32]
    ldr x14, [x2, #32]
    madd x11, x13, x14, x11
    ldr x13, [sp, #88]
    ldr x14, [x2, #24]
    madd x11, x13, x14, x11
    ldr x13, [x1, #48]
    ldr x14, [x2, #16]
    madd x11, x13, x14, x11
    ldr x13, [sp, #96]
    ldr x14, [x2, #8]
    madd x11, x13, x14, x11
    ldr x13, [x1, #64]
    ldr x14, [x2, #0]
    madd x11, x13, x14, x11
    ldr x13, [sp, #104]
    ldr x14, [sp, #64]
    madd x11, x13, x14, x11
    ldr x13, [x1, #0]
    ldr x14, [x2, #72]
    mul x12, x13, x14
    ldr x13, [x1, #8]
    ldr x14, [x2, #64]
    madd x12, x13, x14, x12
    ldr x13, [x1, #16]
    ldr x14, [x2, #56]
    madd x12, x13, x14, x12
    ldr x13, [x1, #24]
    ldr x14, [x2, #48]
    madd x12, x13, x14, x12
    ldr x13, [x1, #32]
    ldr x14, [x2, #40]
    madd x12, x13, x14, x12
    ldr x13, [x1, #40]
    ldr x14, [x2, #32]
    madd x12, x13, x14, x12
    ldr x13, [x1, #48]
    ldr x14, [x2, #24]
    madd x12, x13, x14, x12
    ldr x13, [x1, #56]
    ldr x14, [x2, #16]
    madd x12, x13, x14, x12
    ldr x13, [x1, #64]
    ldr x14, [x2, #8]
    madd x12, x13, x14, x12
    ldr x13, [x1, #72]
    ldr x14, [x2, #0]
    madd x12, x13, x14, x12
    FE_CARRY_STORE
    add sp, sp, #128
    ret
CRYPTO_END(rt_k_fe_mul)

/* void rt_k_fe_add(int64_t *out, const int64_t *f, const int64_t *g)
 * void rt_k_fe_sub(int64_t *out, const int64_t *f, const int64_t *g) */
CRYPTO_FUNC(rt_k_fe_add)
    mov x17, #19
    ldr x13, [x1, #0]
    ldr x14, [x2, #0]
    add x3, x13, x14
    ldr x13, [x1, #8]
    ldr x14, [x2, #8]
    add x4, x13, x14
    ldr x13, [x1, #16]
    ldr x14, [x2, #16]
    add x5, x13, x14
    ldr x13, [x1, #24]
    ldr x14, [x2, #24]
    add x6, x13, x14
    ldr x13, [x1, #32]
    ldr x14, [x2, #32]
    add x7, x13, x14
    ldr x13, [x1, #40]
    ldr x14, [x2, #40]
    add x8, x13, x14
    ldr x13, [x1, #48]
    ldr x14, [x2, #48]
    add x9, x13, x14
    ldr x13, [x1, #56]
    ldr x14, [x2, #56]
    add x10, x13, x14
    ldr x13, [x1, #64]
    ldr x14, [x2, #64]
    add x11, x13, x14
    ldr x13, [x1, #72]
    ldr x14, [x2, #72]
    add x12, x13, x14
    FE_CARRY_STORE
    ret
CRYPTO_END(rt_k_fe_add)

CRYPTO_FUNC(rt_k_fe_sub)
    mov x17, #19
    ldr x13, [x1, #0]
    ldr x14, [x2, #0]
    sub x3, x13, x14
    ldr x13, [x1, #8]
    ldr x14, [x2, #8]
    sub x4, x13, x14
    ldr x13, [x1, #16]
    ldr x14, [x2, #16]
    sub x5, x13, x14
    ldr x13, [x1, #24]
    ldr x14, [x2, #24]
    sub x6, x13, x14
    ldr x13, [x1, #32]
    ldr x14, [x2, #32]
    sub x7, x13, x14
    ldr x13, [x1, #40]
    ldr x14, [x2, #40]
    sub x8, x13, x14
    ldr x13, [x1, #48]
    ldr x14, [x2, #48]
    sub x9, x13, x14
    ldr x13, [x1, #56]
    ldr x14, [x2, #56]
    sub x10, x13, x14
    ldr x13, [x1, #64]
    ldr x14, [x2, #64]
    sub x11, x13, x14
    ldr x13, [x1, #72]
    ldr x14, [x2, #72]
    sub x12, x13, x14
    FE_CARRY_STORE
    ret
CRYPTO_END(rt_k_fe_sub)

#if !defined(__APPLE__)
    .section .note.GNU-stack,"",%progbits
#endif
