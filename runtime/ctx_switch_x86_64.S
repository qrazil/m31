/* x86-64 context switch for green threads (Phase 1).
 *
 * docs/concurrency-decision.md, "Stacks: fixed, but not limited" and
 * "Phases". Two functions, and nothing else -- see runtime/greenthread.h
 * for the C-level contract (struct rt_ctx layout, rt_ctx_make, and
 * rt_fiber_switch, the seam that keeps this file ignorant of
 * rt_stack_limit). Kept deliberately small: this is exactly the kind of
 * code where "clever" is a liability, not a virtue.
 *
 * System V x86-64 ABI only. Caller-saved registers (rax, rcx, rdx, rsi,
 * rdi, r8-r11) are NOT saved here, on purpose: the calling convention
 * already tells a caller of rt_ctx_switch not to expect those to survive
 * across any call, and rt_ctx_switch is, from the caller's point of view,
 * just a call. What has to survive is the ABI's own callee-saved set --
 * rbx, rbp, r12, r13, r14, r15 -- plus rsp itself, which is the one piece
 * that actually makes this a context switch instead of an ordinary call.
 *
 * struct rt_ctx (runtime/greenthread.h), byte offsets:
 *     0  rsp
 *     8  rbx
 *     16 rbp
 *     24 r12
 *     32 r13
 *     40 r14
 *     48 r15
 * This file and that struct definition are one contract; a change to
 * either without the other corrupts every stack silently rather than
 * crashing cleanly, so keep them in sync by hand with care.
 */

/* C symbol -> linker symbol naming is an OS (not CPU) convention: Linux/ELF
 * uses the C name unchanged, Darwin/Mach-O prepends an underscore to every
 * C-linkage symbol, always, regardless of architecture. This file crosses
 * that boundary three times (rt_ctx_switch, rt_ctx_trampoline defined here;
 * rt_ctx_entry_returned called here but defined in greenthread.c), so every
 * one of those three goes through C_SYM. Needs the .S (capital) extension
 * to actually reach the C preprocessor -- GCC/Clang's own file-suffix
 * convention, no special build-script flag required; renamed from .s for
 * exactly this, found via ctx_switch_aarch64.S's matching comment (x86-64
 * macOS, macos-13, is not exercised by any CI job today, but this is the
 * identical bug waiting for the same file shape there). */
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

/* void rt_ctx_switch(rt_ctx_t *from, rt_ctx_t *to); -- from in rdi, to in rsi.
 *
 * Saves the currently-running context into *from, loads *to, and resumes
 * there. The whole mechanism is the final `ret`: it is not a jump to a
 * fixed label, it pops whatever return address sits on the NEW stack --
 * which is either a real one, left there by a previous call into
 * rt_ctx_switch from the context being resumed, or the trampoline address
 * rt_ctx_make wrote by hand for a context that has never run. Either way,
 * `ret` is what turns "load some registers" into "resume exactly where
 * that context left off (or start it for the first time)" without this
 * function needing to know or care which case it is.
 */
    .globl C_SYM(rt_ctx_switch)
#if !defined(__APPLE__)
    .type C_SYM(rt_ctx_switch), @function
#endif
    .align 16
C_SYM(rt_ctx_switch):
    /* --- save the running context into *from --- */
    movq %rsp, 0(%rdi)
    movq %rbx, 8(%rdi)
    movq %rbp, 16(%rdi)
    movq %r12, 24(%rdi)
    movq %r13, 32(%rdi)
    movq %r14, 40(%rdi)
    movq %r15, 48(%rdi)

    /* --- load the target context from *to ---
     * Order does not matter for correctness except that %rsp must be
     * reloaded before the `ret` below (it is: everything here happens
     * before that instruction). Loading it in the middle, as below, is no
     * more or less correct than loading it first or last -- none of these
     * loads can fault or branch, so there is no partial-switch state to
     * worry about either way. */
    movq 0(%rsi), %rsp
    movq 8(%rsi), %rbx
    movq 16(%rsi), %rbp
    movq 24(%rsi), %r12
    movq 32(%rsi), %r13
    movq 40(%rsi), %r14
    movq 48(%rsi), %r15

    ret
#if !defined(__APPLE__)
    .size C_SYM(rt_ctx_switch), . - C_SYM(rt_ctx_switch)
#endif

/* void rt_ctx_trampoline(void);
 *
 * The landing pad for a context that has never run before. rt_ctx_switch's
 * `ret`, above, lands here the first time a context rt_ctx_make built is
 * switched into. This was never actually CALLED -- there is no caller frame,
 * no return address pushed by hardware for it, nothing -- so there is no
 * ordinary way to pass it the real entry function and argument. Instead,
 * rt_ctx_make smuggles them through two callee-saved registers, because
 * those are exactly the registers rt_ctx_switch just finished loading from
 * *to immediately before the `ret` that lands here:
 *
 *     %r12  the entry function, void (*)(void *)
 *     %r13  its one argument
 *
 * rt_ctx_make also arranges %rsp here to be exactly what it would be
 * immediately BEFORE a `call` instruction (16-byte aligned) -- not after
 * one, since nothing called this. That is what makes the `call *%r12` below
 * land inside the entry function with %rsp at the ABI-correct post-call
 * alignment, the same as any other function call anywhere else in this
 * runtime.
 */
    .globl C_SYM(rt_ctx_trampoline)
#if !defined(__APPLE__)
    .type C_SYM(rt_ctx_trampoline), @function
#endif
    .align 16
C_SYM(rt_ctx_trampoline):
    movq %r13, %rdi    /* argument -> first System V argument register */
    call *%r12          /* entry(arg) */

    /* entry() is not supposed to return -- Phase 1 has no scheduler for
     * control to come back TO. rt_ctx_entry_returned (runtime/greenthread.c)
     * traps cleanly; it is _Noreturn, so the ud2 below is an unreachable
     * safety net, not a load-bearing instruction. */
    call C_SYM(rt_ctx_entry_returned)
    ud2
#if !defined(__APPLE__)
    .size C_SYM(rt_ctx_trampoline), . - C_SYM(rt_ctx_trampoline)
#endif

/* Marks the stack non-executable on Linux only -- identical reasoning to
 * the original pre-portability-fix version of this file; Mach-O has no
 * equivalent concept and Apple's assembler rejects this directive
 * outright, hence the #if. This file builds no code on the stack it
 * switches onto -- rt_ctx_make only ever writes one return address there
 * -- so there is nothing that needs an executable stack, here or anywhere
 * else in this runtime; omitting this on Linux just makes a modern `ld`
 * warn about it instead of granting it silently. */
#if !defined(__APPLE__)
    .section .note.GNU-stack,"",@progbits
#endif
