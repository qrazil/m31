/* aarch64 context switch for green threads (Phase 4 -- portability;
 * docs/concurrency-decision.md, "Phases"). The aarch64 twin of
 * ctx_switch_x86_64.S: same two functions, same contract shape, same
 * "nothing clever" discipline -- read that file's header first, since this
 * one only calls out where AAPCS64 actually differs from System V x86-64
 * rather than re-explaining what does not.
 *
 * AAPCS64 (the ARM 64-bit procedure call standard) only. Caller-saved
 * registers (x0-x18, the other 64 bits of v0-v7/v16-v31) are NOT saved
 * here, on purpose, for the identical reason the x86-64 file gives: a
 * caller of rt_ctx_switch already knows the calling convention does not
 * promise those survive any call, and rt_ctx_switch IS a call. What has to
 * survive is AAPCS64's own callee-saved set:
 *
 *   - x19-x28                 (general-purpose callee-saved)
 *   - x29 (fp) and x30 (lr)   (frame pointer and link register)
 *   - sp                      (the one piece that actually makes this a
 *                              context switch instead of an ordinary call)
 *   - d8-d15 (the LOW 64 bits of v8-v15)
 *
 * That last line has no x86-64 analogue: System V x86-64 makes every xmm
 * register caller-saved, so ctx_switch_x86_64.S correctly saves none of
 * them -- any local the compiler wants to survive a `call` on x86-64 gets
 * spilled to the stack whether or not this file does anything about it.
 * AAPCS64 instead promises the CALLEE will preserve d8-d15 across a call,
 * which means a compiler is entitled to leave a live float/double local in
 * one of them across a call to rt_ctx_switch and never touch the stack at
 * all. Skipping this save/restore would not crash -- it would silently
 * hand the resumed context whatever d8-d15 happened to hold from
 * whichever OTHER context last ran on this carrier, corrupting floating-
 * point state no analysis of the integer registers or the stack would ever
 * catch. This project's own `float`/`double` are exactly the kind of code
 * that would trip over it. See runtime/greenthread_test.c's round-trip
 * test, which now carries float locals across the switch specifically to
 * catch a regression here.
 *
 * struct rt_ctx (runtime/greenthread.h, aarch64 branch), byte offsets:
 *     0   sp
 *     8   x19
 *     16  x20
 *     24  x21
 *     32  x22
 *     40  x23
 *     48  x24
 *     56  x25
 *     64  x26
 *     72  x27
 *     80  x28
 *     88  fp   (x29)
 *     96  lr   (x30)
 *     104 d8
 *     112 d9
 *     120 d10
 *     128 d11
 *     136 d12
 *     144 d13
 *     152 d14
 *     160 d15
 * This file and that struct definition are one contract; a change to
 * either without the other corrupts every stack silently rather than
 * crashing cleanly, so keep them in sync by hand with care -- exactly the
 * warning ctx_switch_x86_64.S gives for its own offsets.
 */

/* C symbol -> linker symbol naming is an OS (not CPU) convention: Linux/ELF
 * uses the C name unchanged, Darwin/Mach-O prepends an underscore to every
 * C-linkage symbol, always, regardless of architecture. This file crosses
 * that boundary three times (rt_ctx_switch, rt_ctx_trampoline defined here;
 * rt_ctx_entry_returned called here but defined in greenthread.c), so every
 * one of those three goes through C_SYM. Needs the .S (capital) extension
 * to actually reach the C preprocessor -- GCC/Clang's own file-suffix
 * convention, no special build-script flag required; renamed from .s for
 * exactly this, the same CI run that found the GNU-as-directive issue this
 * file's other comments describe also hit "Undefined symbols ... _rt_ctx_
 * switch" once those directives were removed, from building the file
 * without this. */
#if defined(__APPLE__)
#define C_SYM(x) _##x
#else
#define C_SYM(x) x
#endif

/* .type/.size/.note.GNU-stack are ELF-only concepts (Apple's assembler has
 * no symbol-table-type or stack-executability notion to accept Mach-O
 * equivalents for), restored here behind plain #if !defined(__APPLE__)
 * blocks at each site now that the .S extension makes conditionals
 * possible. The GNU-stack note is not just cosmetic on Linux: a modern
 * (Ubuntu 24.04 GitHub Actions runner's) `ld` emits "missing .note.GNU-
 * stack section implies executable stack" as a build-breaking warning
 * without it -- this project's own build scripts treat any non-empty
 * stderr as a failed build, so dropping it outright (an earlier version of
 * this fix did) is a real Linux regression, not a safe simplification.
 * .type/.size have no such consequence (informational symbol-table
 * metadata only) but are restored too, for completeness. */

    .text

/* void rt_ctx_switch(rt_ctx_t *from, rt_ctx_t *to); -- from in x0, to in x1.
 *
 * Saves the currently-running context into *from, loads *to, and resumes
 * there. On x86-64 the whole mechanism is the final `ret` popping a return
 * address off the NEW stack. aarch64's `ret` does not touch memory at all
 * -- it branches to whatever address sits in `lr` (x30), a register this
 * function just finished loading from *to a few instructions earlier. That
 * is actually a simpler mechanism than x86-64's, not a weaker one: there is
 * no "fake return address written onto the target stack" step for
 * rt_ctx_make to get right (see that function's aarch64 branch in
 * greenthread.h) -- `ret` here resumes exactly where `to` left off (or
 * starts it for the first time at rt_ctx_trampoline) purely from the
 * register file, the same way `b lr` would if that were legal assembler
 * syntax.
 */
    .globl C_SYM(rt_ctx_switch)
#if !defined(__APPLE__)
    .type C_SYM(rt_ctx_switch), %function
#endif
    .align 4
C_SYM(rt_ctx_switch):
    /* --- save the running context into *from (x0) ---
     * x9 is an ordinary caller-saved scratch register (AAPCS64's x9-x15),
     * not part of the saved set, used here only to get the live `sp` value
     * into a general register so it can be stored -- `str sp, [..]` is not
     * valid AArch64 assembly, `sp` may only be read via `mov` into another
     * register. */
    mov  x9, sp
    str  x9,  [x0, #0]
    str  x19, [x0, #8]
    str  x20, [x0, #16]
    str  x21, [x0, #24]
    str  x22, [x0, #32]
    str  x23, [x0, #40]
    str  x24, [x0, #48]
    str  x25, [x0, #56]
    str  x26, [x0, #64]
    str  x27, [x0, #72]
    str  x28, [x0, #80]
    str  x29, [x0, #88]
    str  x30, [x0, #96]
    str  d8,  [x0, #104]
    str  d9,  [x0, #112]
    str  d10, [x0, #120]
    str  d11, [x0, #128]
    str  d12, [x0, #136]
    str  d13, [x0, #144]
    str  d14, [x0, #152]
    str  d15, [x0, #160]

    /* --- load the target context from *to (x1) ---
     * Order does not matter for correctness except that `sp` must be
     * reloaded before the `ret` below actually transfers control -- not
     * because `ret` itself reads the stack (it does not), but because
     * whatever code `ret` branches to immediately starts using "the
     * current stack", which must already be `to`'s by then. Loading it
     * first, as below, is no more or less correct than loading it last;
     * none of these loads can fault or branch, so there is no partial-
     * switch state to worry about either way -- same reasoning as the
     * x86-64 file's matching comment, different register. x1 (the `to`
     * pointer itself) is never a destination of any load below, so it
     * stays valid as a base register across the whole sequence. */
    ldr  x9,  [x1, #0]
    mov  sp, x9
    ldr  x19, [x1, #8]
    ldr  x20, [x1, #16]
    ldr  x21, [x1, #24]
    ldr  x22, [x1, #32]
    ldr  x23, [x1, #40]
    ldr  x24, [x1, #48]
    ldr  x25, [x1, #56]
    ldr  x26, [x1, #64]
    ldr  x27, [x1, #72]
    ldr  x28, [x1, #80]
    ldr  x29, [x1, #88]
    ldr  x30, [x1, #96]
    ldr  d8,  [x1, #104]
    ldr  d9,  [x1, #112]
    ldr  d10, [x1, #120]
    ldr  d11, [x1, #128]
    ldr  d12, [x1, #136]
    ldr  d13, [x1, #144]
    ldr  d14, [x1, #152]
    ldr  d15, [x1, #160]

    ret
#if !defined(__APPLE__)
    .size C_SYM(rt_ctx_switch), . - C_SYM(rt_ctx_switch)
#endif

/* void rt_ctx_trampoline(void);
 *
 * The landing pad for a context that has never run before -- the aarch64
 * twin of ctx_switch_x86_64.S's trampoline, smuggling the entry function
 * and its argument through two callee-saved registers for the identical
 * reason: this was never actually CALLED (no `bl` executed, so `lr` was
 * never set by hardware for it; rt_ctx_make set it by hand), so there is no
 * ordinary way to pass it the real entry function and argument except
 * through registers rt_ctx_switch just finished loading from *to
 * immediately before the `ret` that lands here:
 *
 *     x19  the entry function, void (*)(void *)
 *     x20  its one argument
 *
 * rt_ctx_make also arranges `sp` here to be 16-byte aligned, which AAPCS64
 * requires at every public interface -- including the `blr` below, which
 * is as much a "public interface" as any ordinary call. Unlike
 * ctx_switch_x86_64.S's trampoline, nothing needs to be reserved on the
 * stack for a return address first: aarch64 return addresses live in `lr`,
 * a register, not on the stack, so `sp` here is simply the honest top of
 * the stack, no -8/-16 adjustment needed. */
    .globl C_SYM(rt_ctx_trampoline)
#if !defined(__APPLE__)
    .type C_SYM(rt_ctx_trampoline), %function
#endif
    .align 4
C_SYM(rt_ctx_trampoline):
    mov  x0, x20       /* argument -> first AAPCS64 argument register */
    blr  x19           /* entry(arg) */

    /* entry() is not supposed to return -- same as x86-64, see that file's
     * matching comment. rt_ctx_entry_returned (runtime/greenthread.c) is
     * _Noreturn, so the trap below is an unreachable safety net, not a
     * load-bearing instruction. */
    bl   C_SYM(rt_ctx_entry_returned)
    brk  #1
#if !defined(__APPLE__)
    .size C_SYM(rt_ctx_trampoline), . - C_SYM(rt_ctx_trampoline)
#endif

/* Marks the stack non-executable on Linux only -- see ctx_switch_x86_64.S's
 * matching comment for the full reasoning (including why dropping this
 * outright, an earlier version of this fix, was itself a regression: a
 * modern `ld` warns without it, and this project's build scripts treat any
 * warning as a failed build). This file writes no code onto any stack it
 * switches onto either, so nothing here ever needed an executable stack to
 * begin with. */
#if !defined(__APPLE__)
    .section .note.GNU-stack,"",%progbits
#endif
