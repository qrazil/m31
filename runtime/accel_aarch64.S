/* aarch64: the same three functions as accel_x86_64.S, and a `rt_k_cpuid`
 * that reports an all-zero CPU, so that a library declaring the x86 probe
 * links on every architecture it supports (the library asks `rt_k_platform`
 * first and never reads the zeros).
 *
 *   int64_t rt_k_platform(void)   2 = aarch64 Linux, 3 = Apple arm64
 *   int64_t rt_k_cache_load(void), void rt_k_cache_store(int64_t)
 *   void    rt_k_cpuid(uint32_t leaf, uint32_t sub, uint32_t *out)
 *
 * AAPCS64; x18 is never touched. #included at the end of ctx_switch_aarch64.S.
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

/* int64_t rt_k_platform(void) */
FFI_FUNC(rt_k_platform)
#if defined(__APPLE__)
    mov x0, #3
#else
    mov x0, #2
#endif
    ret
FFI_END(rt_k_platform)

/* void rt_k_cpuid(uint32_t leaf, uint32_t sub, uint32_t *out): not an x86 CPU */
FFI_FUNC(rt_k_cpuid)
    stp xzr, xzr, [x2]
    ret
FFI_END(rt_k_cpuid)

/* int64_t rt_k_cache_load(void) */
FFI_FUNC(rt_k_cache_load)
#if defined(__APPLE__)
    adrp x1, C_SYM(rt_k_cache_word)@PAGE
    ldr x0, [x1, C_SYM(rt_k_cache_word)@PAGEOFF]
#else
    adrp x1, rt_k_cache_word
    ldr x0, [x1, :lo12:rt_k_cache_word]
#endif
    ret
FFI_END(rt_k_cache_load)

/* void rt_k_cache_store(int64_t value)   x0 */
FFI_FUNC(rt_k_cache_store)
#if defined(__APPLE__)
    adrp x1, C_SYM(rt_k_cache_word)@PAGE
    str x0, [x1, C_SYM(rt_k_cache_word)@PAGEOFF]
#else
    adrp x1, rt_k_cache_word
    str x0, [x1, :lo12:rt_k_cache_word]
#endif
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
