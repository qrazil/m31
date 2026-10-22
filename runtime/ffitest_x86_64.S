/* x86-64 test kernels for `prim "c"` (docs/ffi-decision.md, "Testing").
 *
 * #included at the end of ctx_switch_x86_64.S, like every kernel file. They
 * exist so the compiler's checked native call can be driven by corpus
 * programs on every build and every sanitizer, with arguments the crypto
 * kernels do not exercise: all six argument registers, an `Array<int>` span
 * read, written and summed, a scalar the declaration restricts, a `void` and
 * an `int` result. Declared in runtime/tests/ffitest.m31.
 *
 * System V AMD64, AT&T syntax. Every function is a leaf that touches only
 * caller-saved registers and reads and writes exactly the ranges named.
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
#define FFI_FUNC(name) .globl C_SYM(name); .type C_SYM(name), @function; .p2align 4; C_SYM(name):
#define FFI_END(name) .size C_SYM(name), . - C_SYM(name)
#endif
#endif

    .text

/* void rt_k_ffitest_xor(uint8_t *out, const uint8_t *a, const uint8_t *b, uint64_t n)
 *   out[i] = a[i] ^ b[i]; any of the three may alias another (rdi, rsi, rdx, rcx) */
FFI_FUNC(rt_k_ffitest_xor)
    testq %rcx, %rcx
    jz 2f
1:  movzbl (%rsi), %eax
    xorb (%rdx), %al
    movb %al, (%rdi)
    incq %rsi
    incq %rdx
    incq %rdi
    decq %rcx
    jnz 1b
2:  ret
FFI_END(rt_k_ffitest_xor)

/* int64_t rt_k_ffitest_sum(const int64_t *p, uint64_t n): the sum of n elements (rdi, rsi) */
FFI_FUNC(rt_k_ffitest_sum)
    xorl %eax, %eax
    testq %rsi, %rsi
    jz 2f
1:  addq (%rdi), %rax
    addq $8, %rdi
    decq %rsi
    jnz 1b
2:  ret
FFI_END(rt_k_ffitest_sum)

/* void rt_k_ffitest_fill(uint8_t *p, uint64_t n, uint64_t v): n octets set to v's low byte */
FFI_FUNC(rt_k_ffitest_fill)
    testq %rsi, %rsi
    jz 2f
1:  movb %dl, (%rdi)
    incq %rdi
    decq %rsi
    jnz 1b
2:  ret
FFI_END(rt_k_ffitest_fill)

/* int64_t rt_k_ffitest_mix(a, b, c, d, e, f): a + 2b + 3c + 4d + 5e + 6f,
 * every argument register (rdi rsi rdx rcx r8 r9) */
FFI_FUNC(rt_k_ffitest_mix)
    movq %rdi, %rax
    leaq (%rax,%rsi,2), %rax
    leaq (%rdx,%rdx,2), %r10
    addq %r10, %rax
    leaq (,%rcx,4), %r10
    addq %r10, %rax
    leaq (%r8,%r8,4), %r10
    addq %r10, %rax
    leaq (%r9,%r9,2), %r10
    addq %r10, %r10
    addq %r10, %rax
    ret
FFI_END(rt_k_ffitest_mix)

/* void rt_k_ffitest_double(int64_t *p, uint64_t n): each of n elements doubled in place */
FFI_FUNC(rt_k_ffitest_double)
    testq %rsi, %rsi
    jz 2f
1:  shlq $1, (%rdi)
    addq $8, %rdi
    decq %rsi
    jnz 1b
2:  ret
FFI_END(rt_k_ffitest_double)

/* int64_t rt_k_ffitest_scale(int64_t k, const uint8_t *p, uint64_t n): k times
 * the sum of the n octets (rdi, rsi, rdx) */
FFI_FUNC(rt_k_ffitest_scale)
    xorl %eax, %eax
    testq %rdx, %rdx
    jz 2f
1:  movzbl (%rsi), %ecx
    addq %rcx, %rax
    incq %rsi
    decq %rdx
    jnz 1b
2:  imulq %rdi, %rax
    ret
FFI_END(rt_k_ffitest_scale)
