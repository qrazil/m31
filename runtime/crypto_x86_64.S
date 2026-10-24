/* x86-64 crypto kernels: AES-NI + PCLMULQDQ, SSE2 ChaCha20, mulq Poly1305,
 * SHA-NI SHA-256, and plain 64-bit SHA-512 and SHA-1.
 *
 * #included at the end of ctx_switch_x86_64.S (and so assembled as part of
 * it). Each kernel is declared by a `prim "c"` in the lib/crypto modules
 * (docs/ffi-decision.md), whose spans the compiler range-checks before the
 * call; lib/crypto/accel.m31 decides whether the CPU may run each one (cpuid,
 * through `rt_k_cpuid` below). System V x86-64 ABI, AT&T syntax, like
 * the context switch next to it. Every function is a leaf that touches only
 * the caller-saved registers plus whatever it pushes and pops itself, uses no
 * red zone and no stack beyond its own frame.
 *
 * Constant time by construction: AES-NI and PCLMULQDQ have no secret-indexed
 * table lookups; the ChaCha20 and Poly1305 kernels are straight-line
 * add/xor/rotate/multiply code; the loops are bounded by public block
 * counts. SHA-256, SHA-512 and SHA-1 only ever see public data in this
 * library (hashes of messages, certificates and keys-to-be-derived go
 * through HMAC and HKDF, which add nothing a table would leak), and
 * these implementations are table-free as well.
 *
 * Data pointers may be unaligned: every load and store is movdqu/movups or a
 * scalar access. A buffer is never read or written past `n` blocks (or `len`
 * octets for ChaCha20).
 *
 * Symbol naming: C_SYM, as in the context switch, for Mach-O's underscore.
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
#define CRYPTO_RODATA .section __TEXT,__const
#else
#define CRYPTO_FUNC(name) .globl C_SYM(name); .type C_SYM(name), @function; .p2align 4; C_SYM(name):
#define CRYPTO_END(name) .size C_SYM(name), . - C_SYM(name)
#define CRYPTO_RODATA .section .rodata
#endif

    .text

/* void rt_k_cpuid(uint32_t leaf, uint32_t sub, uint32_t *out)
 * out = { eax, ebx, ecx, edx } */
CRYPTO_FUNC(rt_k_cpuid)
    pushq %rbx
    movl %edi, %eax
    movl %esi, %ecx
    movq %rdx, %r8
    cpuid
    movl %eax, 0(%r8)
    movl %ebx, 4(%r8)
    movl %ecx, 8(%r8)
    movl %edx, 12(%r8)
    popq %rbx
    ret
CRYPTO_END(rt_k_cpuid)

/* ------------------------------------------------------------------------
 * AES-NI
 * ------------------------------------------------------------------------ */

/* void rt_k_aes_ecb(const uint8_t *rk, uint64_t rounds, const uint8_t *in,
 *                   uint8_t *out, uint64_t n)
 *   rdi rk, rsi rounds, rdx in, rcx out, r8 n */
CRYPTO_FUNC(rt_k_aes_ecb)
    testq %r8, %r8
    jz 9f
1:  cmpq $8, %r8
    jb 4f
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    movdqu 16*\r(%rdx), %xmm\r
    .endr
    movdqu (%rdi), %xmm8
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    pxor %xmm8, %xmm\r
    .endr
    leaq 16(%rdi), %r9
    leaq -1(%rsi), %r10
2:  movdqu (%r9), %xmm8
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    aesenc %xmm8, %xmm\r
    .endr
    addq $16, %r9
    decq %r10
    jnz 2b
    movdqu (%r9), %xmm8
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    aesenclast %xmm8, %xmm\r
    .endr
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    movdqu %xmm\r, 16*\r(%rcx)
    .endr
    addq $128, %rdx
    addq $128, %rcx
    subq $8, %r8
    jmp 1b
4:  testq %r8, %r8
    jz 9f
    movdqu (%rdx), %xmm0
    movdqu (%rdi), %xmm8
    pxor %xmm8, %xmm0
    leaq 16(%rdi), %r9
    leaq -1(%rsi), %r10
5:  movdqu (%r9), %xmm8
    aesenc %xmm8, %xmm0
    addq $16, %r9
    decq %r10
    jnz 5b
    movdqu (%r9), %xmm8
    aesenclast %xmm8, %xmm0
    movdqu %xmm0, (%rcx)
    addq $16, %rdx
    addq $16, %rcx
    decq %r8
    jmp 4b
9:  pxor %xmm0, %xmm0
    pxor %xmm8, %xmm8
    ret
CRYPTO_END(rt_k_aes_ecb)

/* void rt_k_aes_ctr(const uint8_t *rk, uint64_t rounds, uint8_t *ctr,
 *                   const uint8_t *in, uint8_t *out, uint64_t n)
 *   rdi rk, rsi rounds, rdx ctr (16 octets, big-endian, updated), rcx in,
 *   r8 out, r9 n.
 * The counter lives in r10 (high 64 bits) and r11 (low), as native integers;
 * each block's counter is rebuilt big-endian with bswap, and the increment is
 * add/adc across all 128 bits. */
CRYPTO_FUNC(rt_k_aes_ctr)
    testq %r9, %r9
    jz 9f
    pushq %rbx
    pushq %r12
    movq (%rdx), %r10
    bswapq %r10
    movq 8(%rdx), %r11
    bswapq %r11
.macro CTR_BLOCK x
    movq %r10, %rax
    bswapq %rax
    movq %r11, %rbx
    bswapq %rbx
    movq %rax, %xmm\x
    movq %rbx, %xmm9
    punpcklqdq %xmm9, %xmm\x
    addq $1, %r11
    adcq $0, %r10
.endm
1:  cmpq $8, %r9
    jb 4f
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    CTR_BLOCK \r
    .endr
    movdqu (%rdi), %xmm8
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    pxor %xmm8, %xmm\r
    .endr
    leaq 16(%rdi), %rax
    leaq -1(%rsi), %r12
2:  movdqu (%rax), %xmm8
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    aesenc %xmm8, %xmm\r
    .endr
    addq $16, %rax
    decq %r12
    jnz 2b
    movdqu (%rax), %xmm8
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    aesenclast %xmm8, %xmm\r
    .endr
    .irp r, 0, 1, 2, 3, 4, 5, 6, 7
    movdqu 16*\r(%rcx), %xmm9
    pxor %xmm9, %xmm\r
    movdqu %xmm\r, 16*\r(%r8)
    .endr
    addq $128, %rcx
    addq $128, %r8
    subq $8, %r9
    jmp 1b
4:  testq %r9, %r9
    jz 8f
    CTR_BLOCK 0
    movdqu (%rdi), %xmm8
    pxor %xmm8, %xmm0
    leaq 16(%rdi), %rax
    leaq -1(%rsi), %r12
5:  movdqu (%rax), %xmm8
    aesenc %xmm8, %xmm0
    addq $16, %rax
    decq %r12
    jnz 5b
    movdqu (%rax), %xmm8
    aesenclast %xmm8, %xmm0
    movdqu (%rcx), %xmm9
    pxor %xmm9, %xmm0
    movdqu %xmm0, (%r8)
    addq $16, %rcx
    addq $16, %r8
    decq %r9
    jmp 4b
8:  bswapq %r10
    movq %r10, (%rdx)
    bswapq %r11
    movq %r11, 8(%rdx)
    pxor %xmm0, %xmm0
    pxor %xmm8, %xmm8
    popq %r12
    popq %rbx
9:  ret
CRYPTO_END(rt_k_aes_ctr)

/* ------------------------------------------------------------------------
 * GHASH with PCLMULQDQ
 * ------------------------------------------------------------------------ */

    CRYPTO_RODATA
    .p2align 4
ghash_bswap:
    .byte 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0
    .text

/* Carry-less 128x128 multiply of xmm0 and xmm1 (both byte-reversed, i.e.
 * bit-reflected GCM operands), reduced modulo x^128 + x^7 + x^2 + x + 1:
 * Gueron and Kounavis, "Intel Carry-Less Multiplication Instruction and its
 * Usage for Computing the GCM Mode", figure 5. Result in xmm0. Clobbers
 * xmm2-xmm9. */
.macro GFMUL
    movdqa %xmm0, %xmm3
    pclmulqdq $0x00, %xmm1, %xmm3
    movdqa %xmm0, %xmm4
    pclmulqdq $0x10, %xmm1, %xmm4
    movdqa %xmm0, %xmm5
    pclmulqdq $0x01, %xmm1, %xmm5
    movdqa %xmm0, %xmm6
    pclmulqdq $0x11, %xmm1, %xmm6
    pxor %xmm5, %xmm4
    movdqa %xmm4, %xmm5
    pslldq $8, %xmm5
    psrldq $8, %xmm4
    pxor %xmm5, %xmm3
    pxor %xmm4, %xmm6
    movdqa %xmm3, %xmm7
    movdqa %xmm6, %xmm8
    pslld $1, %xmm3
    pslld $1, %xmm6
    psrld $31, %xmm7
    psrld $31, %xmm8
    movdqa %xmm7, %xmm9
    pslldq $4, %xmm8
    pslldq $4, %xmm7
    psrldq $12, %xmm9
    por %xmm7, %xmm3
    por %xmm8, %xmm6
    por %xmm9, %xmm6
    movdqa %xmm3, %xmm7
    movdqa %xmm3, %xmm8
    movdqa %xmm3, %xmm9
    pslld $31, %xmm7
    pslld $30, %xmm8
    pslld $25, %xmm9
    pxor %xmm8, %xmm7
    pxor %xmm9, %xmm7
    movdqa %xmm7, %xmm8
    psrldq $4, %xmm8
    pslldq $12, %xmm7
    pxor %xmm7, %xmm3
    movdqa %xmm3, %xmm2
    movdqa %xmm3, %xmm4
    movdqa %xmm3, %xmm5
    psrld $1, %xmm2
    psrld $2, %xmm4
    psrld $7, %xmm5
    pxor %xmm4, %xmm2
    pxor %xmm5, %xmm2
    pxor %xmm8, %xmm2
    pxor %xmm2, %xmm3
    pxor %xmm3, %xmm6
    movdqa %xmm6, %xmm0
.endm

/* void rt_k_ghash(const uint8_t *h, uint8_t *acc, const uint8_t *data,
 *                 uint64_t n)
 *   rdi h, rsi acc, rdx data, rcx n. acc = ((acc ^ block) * H) per block. */
CRYPTO_FUNC(rt_k_ghash)
    testq %rcx, %rcx
    jz 9f
    leaq ghash_bswap(%rip), %rax
    movdqu (%rax), %xmm10
    movdqu (%rdi), %xmm1
    pshufb %xmm10, %xmm1
    movdqu (%rsi), %xmm0
    pshufb %xmm10, %xmm0
1:  movdqu (%rdx), %xmm11
    pshufb %xmm10, %xmm11
    pxor %xmm11, %xmm0
    GFMUL
    addq $16, %rdx
    decq %rcx
    jnz 1b
    pshufb %xmm10, %xmm0
    movdqu %xmm0, (%rsi)
9:  ret
CRYPTO_END(rt_k_ghash)


/* ------------------------------------------------------------------------
 * ChaCha20, SSE2, four blocks at a time
 * ------------------------------------------------------------------------ */

    CRYPTO_RODATA
    .p2align 4
chacha_sigma:
    .ascii "expand 32-byte k"
chacha_lane:
    .long 0, 1, 2, 3
chacha_four:
    .long 4, 4, 4, 4
    .text

/* x = x <<< n (32-bit lanes), t a scratch register. */
.macro ROTL x, n, t
    movdqa \x, \t
    pslld $\n, \x
    psrld $(32-\n), \t
    por \t, \x
.endm

/* Two independent quarter rounds at once, on the vectors at stack slots
 * (a1 b1 c1 d1) and (a2 b2 c2 d2). One "word" of the working state is one
 * xmm holding that word for four blocks. */
.macro QR2 a1, b1, c1, d1, a2, b2, c2, d2
    movdqa 16*\a1(%rsp), %xmm0
    movdqa 16*\b1(%rsp), %xmm1
    movdqa 16*\c1(%rsp), %xmm2
    movdqa 16*\d1(%rsp), %xmm3
    movdqa 16*\a2(%rsp), %xmm4
    movdqa 16*\b2(%rsp), %xmm5
    movdqa 16*\c2(%rsp), %xmm6
    movdqa 16*\d2(%rsp), %xmm7
    paddd %xmm1, %xmm0
    paddd %xmm5, %xmm4
    pxor %xmm0, %xmm3
    pxor %xmm4, %xmm7
    ROTL %xmm3, 16, %xmm8
    ROTL %xmm7, 16, %xmm9
    paddd %xmm3, %xmm2
    paddd %xmm7, %xmm6
    pxor %xmm2, %xmm1
    pxor %xmm6, %xmm5
    ROTL %xmm1, 12, %xmm8
    ROTL %xmm5, 12, %xmm9
    paddd %xmm1, %xmm0
    paddd %xmm5, %xmm4
    pxor %xmm0, %xmm3
    pxor %xmm4, %xmm7
    ROTL %xmm3, 8, %xmm8
    ROTL %xmm7, 8, %xmm9
    paddd %xmm3, %xmm2
    paddd %xmm7, %xmm6
    pxor %xmm2, %xmm1
    pxor %xmm6, %xmm5
    ROTL %xmm1, 7, %xmm8
    ROTL %xmm5, 7, %xmm9
    movdqa %xmm0, 16*\a1(%rsp)
    movdqa %xmm1, 16*\b1(%rsp)
    movdqa %xmm2, 16*\c1(%rsp)
    movdqa %xmm3, 16*\d1(%rsp)
    movdqa %xmm4, 16*\a2(%rsp)
    movdqa %xmm5, 16*\b2(%rsp)
    movdqa %xmm6, 16*\c2(%rsp)
    movdqa %xmm7, 16*\d2(%rsp)
.endm

/* void rt_k_chacha20(const uint8_t *key, const uint8_t *iv, const uint8_t *in,
 *                    uint8_t *out, uint64_t len)
 *   rdi key (32), rsi iv (16: state words 12-15, counter first), rdx in,
 *   rcx out, r8 len. Stack frame (16-aligned): 0..255 working state,
 *   256..511 base state, 512..767 keystream for the current four blocks. */
CRYPTO_FUNC(rt_k_chacha20)
    testq %r8, %r8
    jz 9f
    pushq %rbp
    movq %rsp, %rbp
    subq $784, %rsp
    andq $-16, %rsp
    leaq chacha_sigma(%rip), %rax
    movdqu (%rax), %xmm0
    pshufd $0x00, %xmm0, %xmm1
    movdqa %xmm1, 256+16*0(%rsp)
    pshufd $0x55, %xmm0, %xmm1
    movdqa %xmm1, 256+16*1(%rsp)
    pshufd $0xAA, %xmm0, %xmm1
    movdqa %xmm1, 256+16*2(%rsp)
    pshufd $0xFF, %xmm0, %xmm1
    movdqa %xmm1, 256+16*3(%rsp)
    movdqu (%rdi), %xmm0
    pshufd $0x00, %xmm0, %xmm1
    movdqa %xmm1, 256+16*4(%rsp)
    pshufd $0x55, %xmm0, %xmm1
    movdqa %xmm1, 256+16*5(%rsp)
    pshufd $0xAA, %xmm0, %xmm1
    movdqa %xmm1, 256+16*6(%rsp)
    pshufd $0xFF, %xmm0, %xmm1
    movdqa %xmm1, 256+16*7(%rsp)
    movdqu 16(%rdi), %xmm0
    pshufd $0x00, %xmm0, %xmm1
    movdqa %xmm1, 256+16*8(%rsp)
    pshufd $0x55, %xmm0, %xmm1
    movdqa %xmm1, 256+16*9(%rsp)
    pshufd $0xAA, %xmm0, %xmm1
    movdqa %xmm1, 256+16*10(%rsp)
    pshufd $0xFF, %xmm0, %xmm1
    movdqa %xmm1, 256+16*11(%rsp)
    movdqu (%rsi), %xmm0
    pshufd $0x00, %xmm0, %xmm1
    leaq chacha_lane(%rip), %rax
    movdqu (%rax), %xmm2
    paddd %xmm2, %xmm1
    movdqa %xmm1, 256+16*12(%rsp)
    pshufd $0x55, %xmm0, %xmm1
    movdqa %xmm1, 256+16*13(%rsp)
    pshufd $0xAA, %xmm0, %xmm1
    movdqa %xmm1, 256+16*14(%rsp)
    pshufd $0xFF, %xmm0, %xmm1
    movdqa %xmm1, 256+16*15(%rsp)
1:  .irp i, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
    movdqa 256+16*\i(%rsp), %xmm0
    movdqa %xmm0, 16*\i(%rsp)
    .endr
    movl $10, %eax
2:  QR2 0, 4, 8, 12, 1, 5, 9, 13
    QR2 2, 6, 10, 14, 3, 7, 11, 15
    QR2 0, 5, 10, 15, 1, 6, 11, 12
    QR2 2, 7, 8, 13, 3, 4, 9, 14
    decl %eax
    jnz 2b
    .irp g, 0, 1, 2, 3
    movdqa 16*(4*\g+0)(%rsp), %xmm0
    movdqa 16*(4*\g+1)(%rsp), %xmm1
    movdqa 16*(4*\g+2)(%rsp), %xmm2
    movdqa 16*(4*\g+3)(%rsp), %xmm3
    paddd 256+16*(4*\g+0)(%rsp), %xmm0
    paddd 256+16*(4*\g+1)(%rsp), %xmm1
    paddd 256+16*(4*\g+2)(%rsp), %xmm2
    paddd 256+16*(4*\g+3)(%rsp), %xmm3
    movdqa %xmm0, %xmm4
    punpckldq %xmm1, %xmm4
    movdqa %xmm2, %xmm5
    punpckldq %xmm3, %xmm5
    punpckhdq %xmm1, %xmm0
    punpckhdq %xmm3, %xmm2
    movdqa %xmm4, %xmm1
    punpcklqdq %xmm5, %xmm4
    punpckhqdq %xmm5, %xmm1
    movdqa %xmm0, %xmm3
    punpcklqdq %xmm2, %xmm0
    punpckhqdq %xmm2, %xmm3
    movdqa %xmm4, 512+0*64+16*\g(%rsp)
    movdqa %xmm1, 512+1*64+16*\g(%rsp)
    movdqa %xmm0, 512+2*64+16*\g(%rsp)
    movdqa %xmm3, 512+3*64+16*\g(%rsp)
    .endr
    movdqa 256+16*12(%rsp), %xmm0
    leaq chacha_four(%rip), %rax
    movdqu (%rax), %xmm1
    paddd %xmm1, %xmm0
    movdqa %xmm0, 256+16*12(%rsp)
    cmpq $256, %r8
    jb 4f
    .irp i, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15
    movdqu 16*\i(%rdx), %xmm0
    pxor 512+16*\i(%rsp), %xmm0
    movdqu %xmm0, 16*\i(%rcx)
    .endr
    addq $256, %rdx
    addq $256, %rcx
    subq $256, %r8
    jnz 1b
    jmp 8f
4:  xorl %eax, %eax
5:  movzbl (%rdx,%rax), %r9d
    xorb 512(%rsp,%rax), %r9b
    movb %r9b, (%rcx,%rax)
    incq %rax
    cmpq %rax, %r8
    jne 5b
8:  pxor %xmm0, %xmm0
    .irp i, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47
    movdqa %xmm0, 16*\i(%rsp)
    .endr
    movq %rbp, %rsp
    popq %rbp
9:  ret
CRYPTO_END(rt_k_chacha20)

/* ------------------------------------------------------------------------
 * Poly1305, 64-bit limbs with mulq
 * ------------------------------------------------------------------------ */

/* void rt_k_poly1305_blocks(uint8_t *st, const uint8_t *data, uint64_t n,
 *                           uint64_t hibit)
 *   rdi st (h0 h1 h2 r0 r1, five little-endian u64), rsi data, rdx n,
 *   rcx hibit (0 or 1). h = (h + block + hibit * 2^128) * r mod 2^130 - 5,
 *   partially reduced: h2 stays below 8. The product is a 4-limb schoolbook
 *   multiply, then bits 130 and up are folded back as 5 times themselves. */
CRYPTO_FUNC(rt_k_poly1305_blocks)
    testq %rdx, %rdx
    jz 9f
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rdx, %rbp
    movq 0(%rdi), %r8
    movq 8(%rdi), %r9
    movq 16(%rdi), %r10
    movq 24(%rdi), %r11
    movq 32(%rdi), %r12
1:  addq 0(%rsi), %r8
    adcq 8(%rsi), %r9
    adcq %rcx, %r10
    addq $16, %rsi
    movq %r11, %rax
    mulq %r8
    movq %rax, %r13
    movq %rdx, %r14
    movq %r12, %rax
    mulq %r8
    addq %rax, %r14
    adcq $0, %rdx
    movq %rdx, %r15
    movq %r11, %rax
    mulq %r9
    addq %rax, %r14
    adcq %rdx, %r15
    movl $0, %ebx
    adcq $0, %rbx
    movq %r12, %rax
    mulq %r9
    addq %rax, %r15
    adcq %rdx, %rbx
    movq %r11, %rax
    mulq %r10
    addq %rax, %r15
    adcq %rdx, %rbx
    movq %r12, %rax
    imulq %r10, %rax
    addq %rax, %rbx
    movq %r15, %r10
    andq $3, %r10
    movq %r15, %rax
    shrdq $2, %rbx, %rax
    shrq $2, %rbx
    movq $5, %r15
    mulq %r15
    addq %rax, %r13
    adcq %rdx, %r14
    adcq $0, %r10
    imulq $5, %rbx, %rax
    addq %rax, %r14
    adcq $0, %r10
    movq %r13, %r8
    movq %r14, %r9
    decq %rbp
    jnz 1b
    movq %r8, 0(%rdi)
    movq %r9, 8(%rdi)
    movq %r10, 16(%rdi)
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
9:  ret
CRYPTO_END(rt_k_poly1305_blocks)

/* void rt_k_poly1305_finish(const uint8_t *st, const uint8_t *pad, uint8_t *out)
 *   rdi st, rsi pad (the one-time key's last 16 octets), rdx out.
 *   h mod 2^130 - 5 by a masked conditional subtraction (h + 5 reaching bit 130
 *   means h >= p), then (h + pad) mod 2^128. No branch on h. */
CRYPTO_FUNC(rt_k_poly1305_finish)
    movq 0(%rdi), %r8
    movq 8(%rdi), %r9
    movq 16(%rdi), %r10
    movq %r8, %rax
    movq %r9, %rcx
    movq %r10, %r11
    addq $5, %rax
    adcq $0, %rcx
    adcq $0, %r11
    shrq $2, %r11
    andq $1, %r11
    negq %r11
    xorq %r8, %rax
    andq %r11, %rax
    xorq %rax, %r8
    xorq %r9, %rcx
    andq %r11, %rcx
    xorq %rcx, %r9
    addq 0(%rsi), %r8
    adcq 8(%rsi), %r9
    movq %r8, 0(%rdx)
    movq %r9, 8(%rdx)
    ret
CRYPTO_END(rt_k_poly1305_finish)


/* ------------------------------------------------------------------------
 * SHA-256 with the SHA extensions
 * ------------------------------------------------------------------------ */

    CRYPTO_RODATA
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
sha256_bswap:
    .byte 3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8, 15, 14, 13, 12
    .text

/* void rt_k_sha256(uint8_t *state, const uint8_t *data, uint64_t n)
 *   rdi state (a..h, eight little-endian u32), rsi data, rdx n blocks.
 *   Registers: xmm1 = ABEF, xmm2 = CDGH, xmm0 = message + constants (the
 *   implicit operand of sha256rnds2), xmm3 scratch, xmm4-7 the four message
 *   vectors in rotation, xmm8/9 the saved state, xmm10 the byte-swap mask.
 *   Intel's reference sequence (Gulley et al., "Intel SHA Extensions"). */
CRYPTO_FUNC(rt_k_sha256)
    testq %rdx, %rdx
    jz 9f
    leaq sha256_k(%rip), %rax
    leaq sha256_bswap(%rip), %rcx
    movdqu (%rcx), %xmm10
    movdqu (%rdi), %xmm3
    movdqu 16(%rdi), %xmm2
    pshufd $0xB1, %xmm3, %xmm3
    pshufd $0x1B, %xmm2, %xmm2
    movdqa %xmm3, %xmm1
    palignr $8, %xmm2, %xmm1
    pblendw $0xF0, %xmm3, %xmm2
1:  movdqa %xmm1, %xmm8
    movdqa %xmm2, %xmm9
    /* rounds 0-3 */
    movdqu 0(%rsi), %xmm4
    pshufb %xmm10, %xmm4
    movdqa %xmm4, %xmm0
    paddd 0(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    /* rounds 4-7 */
    movdqu 16(%rsi), %xmm5
    pshufb %xmm10, %xmm5
    movdqa %xmm5, %xmm0
    paddd 16(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm5, %xmm4
    /* rounds 8-11 */
    movdqu 32(%rsi), %xmm6
    pshufb %xmm10, %xmm6
    movdqa %xmm6, %xmm0
    paddd 32(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm6, %xmm5
    /* rounds 12-15 */
    movdqu 48(%rsi), %xmm7
    pshufb %xmm10, %xmm7
    movdqa %xmm7, %xmm0
    paddd 48(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm7, %xmm3
    palignr $4, %xmm6, %xmm3
    paddd %xmm3, %xmm4
    sha256msg2 %xmm7, %xmm4
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm7, %xmm6
    /* rounds 16-19 */
    movdqa %xmm4, %xmm0
    paddd 64(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm4, %xmm3
    palignr $4, %xmm7, %xmm3
    paddd %xmm3, %xmm5
    sha256msg2 %xmm4, %xmm5
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm4, %xmm7
    /* rounds 20-23 */
    movdqa %xmm5, %xmm0
    paddd 80(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm5, %xmm3
    palignr $4, %xmm4, %xmm3
    paddd %xmm3, %xmm6
    sha256msg2 %xmm5, %xmm6
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm5, %xmm4
    /* rounds 24-27 */
    movdqa %xmm6, %xmm0
    paddd 96(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm6, %xmm3
    palignr $4, %xmm5, %xmm3
    paddd %xmm3, %xmm7
    sha256msg2 %xmm6, %xmm7
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm6, %xmm5
    /* rounds 28-31 */
    movdqa %xmm7, %xmm0
    paddd 112(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm7, %xmm3
    palignr $4, %xmm6, %xmm3
    paddd %xmm3, %xmm4
    sha256msg2 %xmm7, %xmm4
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm7, %xmm6
    /* rounds 32-35 */
    movdqa %xmm4, %xmm0
    paddd 128(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm4, %xmm3
    palignr $4, %xmm7, %xmm3
    paddd %xmm3, %xmm5
    sha256msg2 %xmm4, %xmm5
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm4, %xmm7
    /* rounds 36-39 */
    movdqa %xmm5, %xmm0
    paddd 144(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm5, %xmm3
    palignr $4, %xmm4, %xmm3
    paddd %xmm3, %xmm6
    sha256msg2 %xmm5, %xmm6
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm5, %xmm4
    /* rounds 40-43 */
    movdqa %xmm6, %xmm0
    paddd 160(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm6, %xmm3
    palignr $4, %xmm5, %xmm3
    paddd %xmm3, %xmm7
    sha256msg2 %xmm6, %xmm7
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm6, %xmm5
    /* rounds 44-47 */
    movdqa %xmm7, %xmm0
    paddd 176(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm7, %xmm3
    palignr $4, %xmm6, %xmm3
    paddd %xmm3, %xmm4
    sha256msg2 %xmm7, %xmm4
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm7, %xmm6
    /* rounds 48-51 */
    movdqa %xmm4, %xmm0
    paddd 192(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm4, %xmm3
    palignr $4, %xmm7, %xmm3
    paddd %xmm3, %xmm5
    sha256msg2 %xmm4, %xmm5
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    sha256msg1 %xmm4, %xmm7
    /* rounds 52-55 */
    movdqa %xmm5, %xmm0
    paddd 208(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm5, %xmm3
    palignr $4, %xmm4, %xmm3
    paddd %xmm3, %xmm6
    sha256msg2 %xmm5, %xmm6
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    /* rounds 56-59 */
    movdqa %xmm6, %xmm0
    paddd 224(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    movdqa %xmm6, %xmm3
    palignr $4, %xmm5, %xmm3
    paddd %xmm3, %xmm7
    sha256msg2 %xmm6, %xmm7
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    /* rounds 60-63 */
    movdqa %xmm7, %xmm0
    paddd 240(%rax), %xmm0
    sha256rnds2 %xmm0, %xmm1, %xmm2
    pshufd $0x0E, %xmm0, %xmm0
    sha256rnds2 %xmm0, %xmm2, %xmm1
    paddd %xmm8, %xmm1
    paddd %xmm9, %xmm2
    addq $64, %rsi
    decq %rdx
    jnz 1b
    pshufd $0x1B, %xmm1, %xmm3
    pshufd $0xB1, %xmm2, %xmm2
    movdqa %xmm3, %xmm1
    pblendw $0xF0, %xmm2, %xmm1
    palignr $8, %xmm3, %xmm2
    movdqu %xmm1, (%rdi)
    movdqu %xmm2, 16(%rdi)
9:  ret
CRYPTO_END(rt_k_sha256)

/* ------------------------------------------------------------------------
 * SHA-512, plain 64-bit registers
 * ------------------------------------------------------------------------ */

    CRYPTO_RODATA
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
    .text

/* One round; the eight state registers rotate roles between calls (pass
 * h a b c d e f g next). r14 is the byte offset t * 8 into both the constants
 * (r15) and the schedule on the stack. */
.macro S512RND a, b, c, d, e, f, g, h
    movq \e, %rax
    movq \e, %rcx
    movq \e, %rdx
    rorq $14, %rax
    rorq $18, %rcx
    rorq $41, %rdx
    xorq %rcx, %rax
    xorq %rdx, %rax
    movq \f, %rcx
    xorq \g, %rcx
    andq \e, %rcx
    xorq \g, %rcx
    addq %rcx, %rax
    addq \h, %rax
    addq (%r15,%r14), %rax
    addq (%rsp,%r14), %rax
    addq %rax, \d
    movq \a, %rcx
    movq \a, %rdx
    rorq $28, %rcx
    rorq $34, %rdx
    xorq %rdx, %rcx
    movq \a, %rdx
    rorq $39, %rdx
    xorq %rdx, %rcx
    addq %rcx, %rax
    movq \a, %rcx
    orq \b, %rcx
    andq \c, %rcx
    movq \a, %rdx
    andq \b, %rdx
    orq %rdx, %rcx
    addq %rcx, %rax
    movq %rax, \h
    addq $8, %r14
.endm

/* void rt_k_sha512(uint8_t *state, const uint8_t *data, uint64_t n)
 *   rdi state (a..h, eight little-endian u64), rsi data, rdx n blocks.
 *   Frame: 640 octets of message schedule, then the block count. */
CRYPTO_FUNC(rt_k_sha512)
    testq %rdx, %rdx
    jz 9f
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    subq $656, %rsp
    movq %rdx, 640(%rsp)
    leaq sha512_k(%rip), %r15
1:  xorl %r14d, %r14d
2:  movq (%rsi,%r14), %rax
    bswapq %rax
    movq %rax, (%rsp,%r14)
    addq $8, %r14
    cmpq $128, %r14
    jne 2b
3:  movq -120(%rsp,%r14), %rax
    movq %rax, %rcx
    movq %rax, %rdx
    rorq $1, %rax
    rorq $8, %rcx
    shrq $7, %rdx
    xorq %rcx, %rax
    xorq %rdx, %rax
    movq -16(%rsp,%r14), %r8
    movq %r8, %rcx
    movq %r8, %rdx
    rorq $19, %r8
    rorq $61, %rcx
    shrq $6, %rdx
    xorq %rcx, %r8
    xorq %rdx, %r8
    addq %r8, %rax
    addq -128(%rsp,%r14), %rax
    addq -56(%rsp,%r14), %rax
    movq %rax, (%rsp,%r14)
    addq $8, %r14
    cmpq $640, %r14
    jne 3b
    movq 0(%rdi), %r8
    movq 8(%rdi), %r9
    movq 16(%rdi), %r10
    movq 24(%rdi), %r11
    movq 32(%rdi), %r12
    movq 40(%rdi), %r13
    movq 48(%rdi), %rbx
    movq 56(%rdi), %rbp
    xorl %r14d, %r14d
4:  S512RND %r8, %r9, %r10, %r11, %r12, %r13, %rbx, %rbp
    S512RND %rbp, %r8, %r9, %r10, %r11, %r12, %r13, %rbx
    S512RND %rbx, %rbp, %r8, %r9, %r10, %r11, %r12, %r13
    S512RND %r13, %rbx, %rbp, %r8, %r9, %r10, %r11, %r12
    S512RND %r12, %r13, %rbx, %rbp, %r8, %r9, %r10, %r11
    S512RND %r11, %r12, %r13, %rbx, %rbp, %r8, %r9, %r10
    S512RND %r10, %r11, %r12, %r13, %rbx, %rbp, %r8, %r9
    S512RND %r9, %r10, %r11, %r12, %r13, %rbx, %rbp, %r8
    cmpq $640, %r14
    jne 4b
    addq %r8, 0(%rdi)
    addq %r9, 8(%rdi)
    addq %r10, 16(%rdi)
    addq %r11, 24(%rdi)
    addq %r12, 32(%rdi)
    addq %r13, 40(%rdi)
    addq %rbx, 48(%rdi)
    addq %rbp, 56(%rdi)
    addq $128, %rsi
    decq 640(%rsp)
    jnz 1b
    addq $656, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
9:  ret
CRYPTO_END(rt_k_sha512)


/* ------------------------------------------------------------------------
 * SHA-1 with the SHA extensions
 * ------------------------------------------------------------------------ */

/* void rt_k_sha1(uint8_t *state, const uint8_t *data, uint64_t n)
 *   rdi state (a..e, five little-endian u32), rsi data, rdx n blocks.
 *   xmm0 = ABCD (reversed), xmm1/xmm2 = E in alternation, xmm4-7 the message
 *   vectors in rotation, xmm8/9 the saved ABCD and E, xmm10 the full
 *   16-octet reversal mask. Intel's reference sequence. */
CRYPTO_FUNC(rt_k_sha1)
    testq %rdx, %rdx
    jz 9f
    leaq ghash_bswap(%rip), %rax
    movdqu (%rax), %xmm10
    movdqu (%rdi), %xmm0
    pshufd $0x1B, %xmm0, %xmm0
    movd 16(%rdi), %xmm1
    pslldq $12, %xmm1
1:  movdqa %xmm0, %xmm8
    movdqa %xmm1, %xmm9
    /* rounds 0-3 */
    movdqu 0(%rsi), %xmm4
    pshufb %xmm10, %xmm4
    paddd %xmm4, %xmm1
    movdqa %xmm0, %xmm2
    sha1rnds4 $0, %xmm1, %xmm0
    /* rounds 4-7 */
    movdqu 16(%rsi), %xmm5
    pshufb %xmm10, %xmm5
    sha1nexte %xmm5, %xmm2
    movdqa %xmm0, %xmm1
    sha1rnds4 $0, %xmm2, %xmm0
    sha1msg1 %xmm5, %xmm4
    /* rounds 8-11 */
    movdqu 32(%rsi), %xmm6
    pshufb %xmm10, %xmm6
    sha1nexte %xmm6, %xmm1
    movdqa %xmm0, %xmm2
    sha1rnds4 $0, %xmm1, %xmm0
    sha1msg1 %xmm6, %xmm5
    pxor %xmm6, %xmm4
    /* rounds 12-15 */
    movdqu 48(%rsi), %xmm7
    pshufb %xmm10, %xmm7
    sha1nexte %xmm7, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm7, %xmm4
    sha1rnds4 $0, %xmm2, %xmm0
    sha1msg1 %xmm7, %xmm6
    pxor %xmm7, %xmm5
    /* rounds 16-19 */
    sha1nexte %xmm4, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm4, %xmm5
    sha1rnds4 $0, %xmm1, %xmm0
    sha1msg1 %xmm4, %xmm7
    pxor %xmm4, %xmm6
    /* rounds 20-23 */
    sha1nexte %xmm5, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm5, %xmm6
    sha1rnds4 $1, %xmm2, %xmm0
    sha1msg1 %xmm5, %xmm4
    pxor %xmm5, %xmm7
    /* rounds 24-27 */
    sha1nexte %xmm6, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm6, %xmm7
    sha1rnds4 $1, %xmm1, %xmm0
    sha1msg1 %xmm6, %xmm5
    pxor %xmm6, %xmm4
    /* rounds 28-31 */
    sha1nexte %xmm7, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm7, %xmm4
    sha1rnds4 $1, %xmm2, %xmm0
    sha1msg1 %xmm7, %xmm6
    pxor %xmm7, %xmm5
    /* rounds 32-35 */
    sha1nexte %xmm4, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm4, %xmm5
    sha1rnds4 $1, %xmm1, %xmm0
    sha1msg1 %xmm4, %xmm7
    pxor %xmm4, %xmm6
    /* rounds 36-39 */
    sha1nexte %xmm5, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm5, %xmm6
    sha1rnds4 $1, %xmm2, %xmm0
    sha1msg1 %xmm5, %xmm4
    pxor %xmm5, %xmm7
    /* rounds 40-43 */
    sha1nexte %xmm6, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm6, %xmm7
    sha1rnds4 $2, %xmm1, %xmm0
    sha1msg1 %xmm6, %xmm5
    pxor %xmm6, %xmm4
    /* rounds 44-47 */
    sha1nexte %xmm7, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm7, %xmm4
    sha1rnds4 $2, %xmm2, %xmm0
    sha1msg1 %xmm7, %xmm6
    pxor %xmm7, %xmm5
    /* rounds 48-51 */
    sha1nexte %xmm4, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm4, %xmm5
    sha1rnds4 $2, %xmm1, %xmm0
    sha1msg1 %xmm4, %xmm7
    pxor %xmm4, %xmm6
    /* rounds 52-55 */
    sha1nexte %xmm5, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm5, %xmm6
    sha1rnds4 $2, %xmm2, %xmm0
    sha1msg1 %xmm5, %xmm4
    pxor %xmm5, %xmm7
    /* rounds 56-59 */
    sha1nexte %xmm6, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm6, %xmm7
    sha1rnds4 $2, %xmm1, %xmm0
    sha1msg1 %xmm6, %xmm5
    pxor %xmm6, %xmm4
    /* rounds 60-63 */
    sha1nexte %xmm7, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm7, %xmm4
    sha1rnds4 $3, %xmm2, %xmm0
    sha1msg1 %xmm7, %xmm6
    pxor %xmm7, %xmm5
    /* rounds 64-67 */
    sha1nexte %xmm4, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm4, %xmm5
    sha1rnds4 $3, %xmm1, %xmm0
    sha1msg1 %xmm4, %xmm7
    pxor %xmm4, %xmm6
    /* rounds 68-71 */
    sha1nexte %xmm5, %xmm2
    movdqa %xmm0, %xmm1
    sha1msg2 %xmm5, %xmm6
    sha1rnds4 $3, %xmm2, %xmm0
    pxor %xmm5, %xmm7
    /* rounds 72-75 */
    sha1nexte %xmm6, %xmm1
    movdqa %xmm0, %xmm2
    sha1msg2 %xmm6, %xmm7
    sha1rnds4 $3, %xmm1, %xmm0
    /* rounds 76-79 */
    sha1nexte %xmm7, %xmm2
    movdqa %xmm0, %xmm1
    sha1rnds4 $3, %xmm2, %xmm0
    sha1nexte %xmm9, %xmm1
    paddd %xmm8, %xmm0
    addq $64, %rsi
    decq %rdx
    jnz 1b
    pshufd $0x1B, %xmm0, %xmm0
    movdqu %xmm0, (%rdi)
    pextrd $3, %xmm1, 16(%rdi)
9:  ret
CRYPTO_END(rt_k_sha1)


/* ------------------------------------------------------------------------
 * GF(2^255 - 19) in ten signed limbs (radix 2^25.5): lib/crypto/field25519.m31's
 * multiply, add and subtract, instruction for instruction (ref10's fe_mul and
 * carry chain), so the kernel and the m31 fallback produce identical limbs.
 * Elements are `Array<int>` data: ten little-endian int64 limbs. imulq keeps
 * only the low 64 bits, which is exact because the bounds in field25519.m31
 * keep every column under 2^62. Straight-line, no branch, no table.
 * ------------------------------------------------------------------------ */

/* The carry chain over h0..h9 (r8-r15, rbx, rbp), then the ten stores to rdi. */
.macro FE_CARRY_STORE
    leaq 33554432(%r8), %rcx
    sarq $26, %rcx
    addq %rcx, %r9
    shlq $26, %rcx
    subq %rcx, %r8
    leaq 33554432(%r12), %rcx
    sarq $26, %rcx
    addq %rcx, %r13
    shlq $26, %rcx
    subq %rcx, %r12
    leaq 16777216(%r9), %rcx
    sarq $25, %rcx
    addq %rcx, %r10
    shlq $25, %rcx
    subq %rcx, %r9
    leaq 16777216(%r13), %rcx
    sarq $25, %rcx
    addq %rcx, %r14
    shlq $25, %rcx
    subq %rcx, %r13
    leaq 33554432(%r10), %rcx
    sarq $26, %rcx
    addq %rcx, %r11
    shlq $26, %rcx
    subq %rcx, %r10
    leaq 33554432(%r14), %rcx
    sarq $26, %rcx
    addq %rcx, %r15
    shlq $26, %rcx
    subq %rcx, %r14
    leaq 16777216(%r11), %rcx
    sarq $25, %rcx
    addq %rcx, %r12
    shlq $25, %rcx
    subq %rcx, %r11
    leaq 16777216(%r15), %rcx
    sarq $25, %rcx
    addq %rcx, %rbx
    shlq $25, %rcx
    subq %rcx, %r15
    leaq 33554432(%r12), %rcx
    sarq $26, %rcx
    addq %rcx, %r13
    shlq $26, %rcx
    subq %rcx, %r12
    leaq 33554432(%rbx), %rcx
    sarq $26, %rcx
    addq %rcx, %rbp
    shlq $26, %rcx
    subq %rcx, %rbx
    leaq 16777216(%rbp), %rcx
    sarq $25, %rcx
    leaq (%rcx,%rcx,8), %rax
    leaq (%rcx,%rax,2), %rax
    addq %rax, %r8
    shlq $25, %rcx
    subq %rcx, %rbp
    leaq 33554432(%r8), %rcx
    sarq $26, %rcx
    addq %rcx, %r9
    shlq $26, %rcx
    subq %rcx, %r8
    movq %r8, 0(%rdi)
    movq %r9, 8(%rdi)
    movq %r10, 16(%rdi)
    movq %r11, 24(%rdi)
    movq %r12, 32(%rdi)
    movq %r13, 40(%rdi)
    movq %r14, 48(%rdi)
    movq %r15, 56(%rdi)
    movq %rbx, 64(%rdi)
    movq %rbp, 72(%rdi)
.endm

/* void rt_k_fe_mul(int64_t *out, const int64_t *f, const int64_t *g)
 *   rdi out, rsi f, rdx g (out may alias either). */
CRYPTO_FUNC(rt_k_fe_mul)
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    subq $120, %rsp
    movq 8(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 0(%rsp)
    movq 16(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 8(%rsp)
    movq 24(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 16(%rsp)
    movq 32(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 24(%rsp)
    movq 40(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 32(%rsp)
    movq 48(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 40(%rsp)
    movq 56(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 48(%rsp)
    movq 64(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 56(%rsp)
    movq 72(%rdx), %rax
    leaq (%rax,%rax,8), %rcx
    leaq (%rax,%rcx,2), %rcx
    movq %rcx, 64(%rsp)
    movq 8(%rsi), %rax
    addq %rax, %rax
    movq %rax, 72(%rsp)
    movq 24(%rsi), %rax
    addq %rax, %rax
    movq %rax, 80(%rsp)
    movq 40(%rsi), %rax
    addq %rax, %rax
    movq %rax, 88(%rsp)
    movq 56(%rsi), %rax
    addq %rax, %rax
    movq %rax, 96(%rsp)
    movq 72(%rsi), %rax
    addq %rax, %rax
    movq %rax, 104(%rsp)
    movq 0(%rsi), %r8
    imulq 0(%rdx), %r8
    movq 72(%rsp), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r8
    movq 16(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r8
    movq 80(%rsp), %rax
    imulq 48(%rsp), %rax
    addq %rax, %r8
    movq 32(%rsi), %rax
    imulq 40(%rsp), %rax
    addq %rax, %r8
    movq 88(%rsp), %rax
    imulq 32(%rsp), %rax
    addq %rax, %r8
    movq 48(%rsi), %rax
    imulq 24(%rsp), %rax
    addq %rax, %r8
    movq 96(%rsp), %rax
    imulq 16(%rsp), %rax
    addq %rax, %r8
    movq 64(%rsi), %rax
    imulq 8(%rsp), %rax
    addq %rax, %r8
    movq 104(%rsp), %rax
    imulq 0(%rsp), %rax
    addq %rax, %r8
    movq 0(%rsi), %r9
    imulq 8(%rdx), %r9
    movq 8(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %r9
    movq 16(%rsi), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r9
    movq 24(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r9
    movq 32(%rsi), %rax
    imulq 48(%rsp), %rax
    addq %rax, %r9
    movq 40(%rsi), %rax
    imulq 40(%rsp), %rax
    addq %rax, %r9
    movq 48(%rsi), %rax
    imulq 32(%rsp), %rax
    addq %rax, %r9
    movq 56(%rsi), %rax
    imulq 24(%rsp), %rax
    addq %rax, %r9
    movq 64(%rsi), %rax
    imulq 16(%rsp), %rax
    addq %rax, %r9
    movq 72(%rsi), %rax
    imulq 8(%rsp), %rax
    addq %rax, %r9
    movq 0(%rsi), %r10
    imulq 16(%rdx), %r10
    movq 72(%rsp), %rax
    imulq 8(%rdx), %rax
    addq %rax, %r10
    movq 16(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %r10
    movq 80(%rsp), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r10
    movq 32(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r10
    movq 88(%rsp), %rax
    imulq 48(%rsp), %rax
    addq %rax, %r10
    movq 48(%rsi), %rax
    imulq 40(%rsp), %rax
    addq %rax, %r10
    movq 96(%rsp), %rax
    imulq 32(%rsp), %rax
    addq %rax, %r10
    movq 64(%rsi), %rax
    imulq 24(%rsp), %rax
    addq %rax, %r10
    movq 104(%rsp), %rax
    imulq 16(%rsp), %rax
    addq %rax, %r10
    movq 0(%rsi), %r11
    imulq 24(%rdx), %r11
    movq 8(%rsi), %rax
    imulq 16(%rdx), %rax
    addq %rax, %r11
    movq 16(%rsi), %rax
    imulq 8(%rdx), %rax
    addq %rax, %r11
    movq 24(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %r11
    movq 32(%rsi), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r11
    movq 40(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r11
    movq 48(%rsi), %rax
    imulq 48(%rsp), %rax
    addq %rax, %r11
    movq 56(%rsi), %rax
    imulq 40(%rsp), %rax
    addq %rax, %r11
    movq 64(%rsi), %rax
    imulq 32(%rsp), %rax
    addq %rax, %r11
    movq 72(%rsi), %rax
    imulq 24(%rsp), %rax
    addq %rax, %r11
    movq 0(%rsi), %r12
    imulq 32(%rdx), %r12
    movq 72(%rsp), %rax
    imulq 24(%rdx), %rax
    addq %rax, %r12
    movq 16(%rsi), %rax
    imulq 16(%rdx), %rax
    addq %rax, %r12
    movq 80(%rsp), %rax
    imulq 8(%rdx), %rax
    addq %rax, %r12
    movq 32(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %r12
    movq 88(%rsp), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r12
    movq 48(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r12
    movq 96(%rsp), %rax
    imulq 48(%rsp), %rax
    addq %rax, %r12
    movq 64(%rsi), %rax
    imulq 40(%rsp), %rax
    addq %rax, %r12
    movq 104(%rsp), %rax
    imulq 32(%rsp), %rax
    addq %rax, %r12
    movq 0(%rsi), %r13
    imulq 40(%rdx), %r13
    movq 8(%rsi), %rax
    imulq 32(%rdx), %rax
    addq %rax, %r13
    movq 16(%rsi), %rax
    imulq 24(%rdx), %rax
    addq %rax, %r13
    movq 24(%rsi), %rax
    imulq 16(%rdx), %rax
    addq %rax, %r13
    movq 32(%rsi), %rax
    imulq 8(%rdx), %rax
    addq %rax, %r13
    movq 40(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %r13
    movq 48(%rsi), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r13
    movq 56(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r13
    movq 64(%rsi), %rax
    imulq 48(%rsp), %rax
    addq %rax, %r13
    movq 72(%rsi), %rax
    imulq 40(%rsp), %rax
    addq %rax, %r13
    movq 0(%rsi), %r14
    imulq 48(%rdx), %r14
    movq 72(%rsp), %rax
    imulq 40(%rdx), %rax
    addq %rax, %r14
    movq 16(%rsi), %rax
    imulq 32(%rdx), %rax
    addq %rax, %r14
    movq 80(%rsp), %rax
    imulq 24(%rdx), %rax
    addq %rax, %r14
    movq 32(%rsi), %rax
    imulq 16(%rdx), %rax
    addq %rax, %r14
    movq 88(%rsp), %rax
    imulq 8(%rdx), %rax
    addq %rax, %r14
    movq 48(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %r14
    movq 96(%rsp), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r14
    movq 64(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r14
    movq 104(%rsp), %rax
    imulq 48(%rsp), %rax
    addq %rax, %r14
    movq 0(%rsi), %r15
    imulq 56(%rdx), %r15
    movq 8(%rsi), %rax
    imulq 48(%rdx), %rax
    addq %rax, %r15
    movq 16(%rsi), %rax
    imulq 40(%rdx), %rax
    addq %rax, %r15
    movq 24(%rsi), %rax
    imulq 32(%rdx), %rax
    addq %rax, %r15
    movq 32(%rsi), %rax
    imulq 24(%rdx), %rax
    addq %rax, %r15
    movq 40(%rsi), %rax
    imulq 16(%rdx), %rax
    addq %rax, %r15
    movq 48(%rsi), %rax
    imulq 8(%rdx), %rax
    addq %rax, %r15
    movq 56(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %r15
    movq 64(%rsi), %rax
    imulq 64(%rsp), %rax
    addq %rax, %r15
    movq 72(%rsi), %rax
    imulq 56(%rsp), %rax
    addq %rax, %r15
    movq 0(%rsi), %rbx
    imulq 64(%rdx), %rbx
    movq 72(%rsp), %rax
    imulq 56(%rdx), %rax
    addq %rax, %rbx
    movq 16(%rsi), %rax
    imulq 48(%rdx), %rax
    addq %rax, %rbx
    movq 80(%rsp), %rax
    imulq 40(%rdx), %rax
    addq %rax, %rbx
    movq 32(%rsi), %rax
    imulq 32(%rdx), %rax
    addq %rax, %rbx
    movq 88(%rsp), %rax
    imulq 24(%rdx), %rax
    addq %rax, %rbx
    movq 48(%rsi), %rax
    imulq 16(%rdx), %rax
    addq %rax, %rbx
    movq 96(%rsp), %rax
    imulq 8(%rdx), %rax
    addq %rax, %rbx
    movq 64(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %rbx
    movq 104(%rsp), %rax
    imulq 64(%rsp), %rax
    addq %rax, %rbx
    movq 0(%rsi), %rbp
    imulq 72(%rdx), %rbp
    movq 8(%rsi), %rax
    imulq 64(%rdx), %rax
    addq %rax, %rbp
    movq 16(%rsi), %rax
    imulq 56(%rdx), %rax
    addq %rax, %rbp
    movq 24(%rsi), %rax
    imulq 48(%rdx), %rax
    addq %rax, %rbp
    movq 32(%rsi), %rax
    imulq 40(%rdx), %rax
    addq %rax, %rbp
    movq 40(%rsi), %rax
    imulq 32(%rdx), %rax
    addq %rax, %rbp
    movq 48(%rsi), %rax
    imulq 24(%rdx), %rax
    addq %rax, %rbp
    movq 56(%rsi), %rax
    imulq 16(%rdx), %rax
    addq %rax, %rbp
    movq 64(%rsi), %rax
    imulq 8(%rdx), %rax
    addq %rax, %rbp
    movq 72(%rsi), %rax
    imulq 0(%rdx), %rax
    addq %rax, %rbp
    FE_CARRY_STORE
    addq $120, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    ret
CRYPTO_END(rt_k_fe_mul)

/* void rt_k_fe_add(int64_t *out, const int64_t *f, const int64_t *g)
 * void rt_k_fe_sub(int64_t *out, const int64_t *f, const int64_t *g) */
CRYPTO_FUNC(rt_k_fe_add)
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq 0(%rsi), %r8
    addq 0(%rdx), %r8
    movq 8(%rsi), %r9
    addq 8(%rdx), %r9
    movq 16(%rsi), %r10
    addq 16(%rdx), %r10
    movq 24(%rsi), %r11
    addq 24(%rdx), %r11
    movq 32(%rsi), %r12
    addq 32(%rdx), %r12
    movq 40(%rsi), %r13
    addq 40(%rdx), %r13
    movq 48(%rsi), %r14
    addq 48(%rdx), %r14
    movq 56(%rsi), %r15
    addq 56(%rdx), %r15
    movq 64(%rsi), %rbx
    addq 64(%rdx), %rbx
    movq 72(%rsi), %rbp
    addq 72(%rdx), %rbp
    FE_CARRY_STORE
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    ret
CRYPTO_END(rt_k_fe_add)

CRYPTO_FUNC(rt_k_fe_sub)
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq 0(%rsi), %r8
    subq 0(%rdx), %r8
    movq 8(%rsi), %r9
    subq 8(%rdx), %r9
    movq 16(%rsi), %r10
    subq 16(%rdx), %r10
    movq 24(%rsi), %r11
    subq 24(%rdx), %r11
    movq 32(%rsi), %r12
    subq 32(%rdx), %r12
    movq 40(%rsi), %r13
    subq 40(%rdx), %r13
    movq 48(%rsi), %r14
    subq 48(%rdx), %r14
    movq 56(%rsi), %r15
    subq 56(%rdx), %r15
    movq 64(%rsi), %rbx
    subq 64(%rdx), %rbx
    movq 72(%rsi), %rbp
    subq 72(%rdx), %rbp
    FE_CARRY_STORE
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    ret
CRYPTO_END(rt_k_fe_sub)

#if !defined(__APPLE__)
    .section .note.GNU-stack,"",@progbits
#endif

/* No executable stack. Also needed when this file is assembled on its own
 * (the kernel tests do), not only when ctx_switch_x86_64.S includes it. */
#if !defined(__APPLE__)
    .section .note.GNU-stack,"",@progbits
#endif
