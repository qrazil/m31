/* x86-64: what the m31 library needs to decide at run time whether a kernel
 * may run (lib/crypto/accel.m31), as assembly so that no C is involved.
 *
 * #included at the end of ctx_switch_x86_64.S. `rt_k_cpuid` itself is in
 * crypto_x86_64.S; this file has the three functions around it:
 *
 *   int64_t rt_k_platform(void)        which platform the kernels were
 *                                      assembled for: 1 = x86-64
 *   int64_t rt_k_cache_load(void)      one process-wide word, 0 until stored
 *   void    rt_k_cache_store(int64_t)
 *
 * The cache word is the one piece of mutable state the library keeps between
 * calls, and an m31 module may have none (docs/module-state-decision.md): it
 * is a single aligned 64-bit word that every writer writes with the same
 * value (the detected feature set), so a race between two first callers is
 * benign. docs/ffi-decision.md, "CPU feature detection, in m31".
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

/* int64_t rt_k_platform(void) */
FFI_FUNC(rt_k_platform)
    movl $1, %eax
    ret
FFI_END(rt_k_platform)

/* int64_t rt_k_cache_load(void) */
FFI_FUNC(rt_k_cache_load)
    movq C_SYM(rt_k_cache_word)(%rip), %rax
    ret
FFI_END(rt_k_cache_load)

/* void rt_k_cache_store(int64_t value)   rdi */
FFI_FUNC(rt_k_cache_store)
    movq %rdi, C_SYM(rt_k_cache_word)(%rip)
    ret
FFI_END(rt_k_cache_store)

#if defined(__APPLE__)
    .section __DATA,__data
#else
    .data
#endif
    .p2align 3
C_SYM(rt_k_cache_word):
    .quad 0
    .text

/* No executable stack. Also needed when this file is assembled on its own
 * (the kernel tests do), not only when ctx_switch_x86_64.S includes it. */
#if !defined(__APPLE__)
    .section .note.GNU-stack,"",@progbits
#endif
