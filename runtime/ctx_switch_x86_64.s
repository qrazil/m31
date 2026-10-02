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
    .globl rt_ctx_switch
    .align 16
rt_ctx_switch:
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
    .globl rt_ctx_trampoline
    .align 16
rt_ctx_trampoline:
    movq %r13, %rdi    /* argument -> first System V argument register */
    call *%r12          /* entry(arg) */

    /* entry() is not supposed to return -- Phase 1 has no scheduler for
     * control to come back TO. rt_ctx_entry_returned (runtime/greenthread.c)
     * traps cleanly; it is _Noreturn, so the ud2 below is an unreachable
     * safety net, not a load-bearing instruction. */
    call rt_ctx_entry_returned
    ud2

/* No .type/.size/.note.GNU-stack here (there used to be -- see git history
 * if you want the original Linux-only reasoning for each). This file is
 * assembled unpreprocessed (no -x assembler-with-cpp anywhere in build.sh/
 * run.sh/gates.sh) and runtime/arch.sh picks it for every x86-64 OS this
 * runtime targets, including macOS once macos-13 is re-enabled (see
 * .github/workflows/release.yml's build-macos job) -- and Apple's
 * integrated assembler (Mach-O, no ELF symbol-table/stack-executability
 * concept) rejects all three directives outright with "unknown directive".
 * ctx_switch_aarch64.s hit this for real on its first genuine macOS CI run;
 * fixed here too, preemptively, since it is the identical bug waiting for
 * the same file shape on the other architecture. All three were
 * informational/hardening, not load-bearing for correctness. */
