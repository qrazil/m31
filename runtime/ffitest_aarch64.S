/* aarch64 test kernels for `prim "c"` (docs/ffi-decision.md, "Testing"): the
 * same functions as ffitest_x86_64.S, with the same meaning, under AAPCS64.
 *
 * #included at the end of ctx_switch_aarch64.S. Leaf functions using only
 * x0-x6 and w4/w5; x18 is never touched.
 */
#ifndef C_SYM
#if defined(__APPLE__)
#define C_SYM(x) _##x
#else
#define C_SYM(x) x
#endif
#endif

#ifndef FFI_FUNC
#if defined(__APPLE__)
#define FFI_FUNC(name) .globl C_SYM(name); .p2align 4; C_SYM(name):
#define FFI_END(name)
#else
#define FFI_FUNC(name) .globl C_SYM(name); .type C_SYM(name), %function; .p2align 4; C_SYM(name):
#define FFI_END(name) .size C_SYM(name), . - C_SYM(name)
#endif
#endif

    .text

/* void rt_k_ffitest_xor(uint8_t *out, const uint8_t *a, const uint8_t *b, uint64_t n)
 *   x0 out, x1 a, x2 b, x3 n */
FFI_FUNC(rt_k_ffitest_xor)
    cbz x3, 2f
1:  ldrb w4, [x1], #1
    ldrb w5, [x2], #1
    eor w4, w4, w5
    strb w4, [x0], #1
    subs x3, x3, #1
    b.ne 1b
2:  ret
FFI_END(rt_k_ffitest_xor)

/* int64_t rt_k_ffitest_sum(const int64_t *p, uint64_t n)   x0 p, x1 n */
FFI_FUNC(rt_k_ffitest_sum)
    mov x2, #0
    cbz x1, 2f
1:  ldr x3, [x0], #8
    add x2, x2, x3
    subs x1, x1, #1
    b.ne 1b
2:  mov x0, x2
    ret
FFI_END(rt_k_ffitest_sum)

/* void rt_k_ffitest_fill(uint8_t *p, uint64_t n, uint64_t v)   x0 p, x1 n, x2 v */
FFI_FUNC(rt_k_ffitest_fill)
    cbz x1, 2f
1:  strb w2, [x0], #1
    subs x1, x1, #1
    b.ne 1b
2:  ret
FFI_END(rt_k_ffitest_fill)

/* int64_t rt_k_ffitest_mix(a, b, c, d, e, f)   x0-x5: a + 2b + 3c + 4d + 5e + 6f */
FFI_FUNC(rt_k_ffitest_mix)
    add x0, x0, x1, lsl #1
    add x6, x2, x2, lsl #1
    add x0, x0, x6
    add x0, x0, x3, lsl #2
    add x6, x4, x4, lsl #2
    add x0, x0, x6
    add x6, x5, x5, lsl #1
    add x0, x0, x6, lsl #1
    ret
FFI_END(rt_k_ffitest_mix)

/* void rt_k_ffitest_double(int64_t *p, uint64_t n)   x0 p, x1 n */
FFI_FUNC(rt_k_ffitest_double)
    cbz x1, 2f
1:  ldr x2, [x0]
    lsl x2, x2, #1
    str x2, [x0], #8
    subs x1, x1, #1
    b.ne 1b
2:  ret
FFI_END(rt_k_ffitest_double)

/* int64_t rt_k_ffitest_scale(int64_t k, const uint8_t *p, uint64_t n)   x0 k, x1 p, x2 n */
FFI_FUNC(rt_k_ffitest_scale)
    mov x3, #0
    cbz x2, 2f
1:  ldrb w4, [x1], #1
    add x3, x3, x4
    subs x2, x2, #1
    b.ne 1b
2:  mul x0, x3, x0
    ret
FFI_END(rt_k_ffitest_scale)
