/* Phase 1 green-thread runtime primitives (docs/concurrency-decision.md,
 * "Phases" -- this is Phase 1, not Phase 2). There is no scheduler here and
 * none of this is reachable from emitted code: `spawn` still means an OS
 * thread (rt.h's "concurrency" section) until Phase 2 is built on top of
 * this. This header exists so that work has something solid to start from.
 *
 * Three pieces, matching the design doc exactly:
 *
 *   Part 1 -- the x86-64 context switch (rt_ctx_t, rt_ctx_switch,
 *             rt_ctx_make). The asm itself is runtime/ctx_switch_x86_64.s.
 *   Part 2 -- a slab stack allocator (rt_stack_t, rt_stack_alloc/free).
 *   Part 4 -- a byte-per-thread state table (rt_green_state_t,
 *             rt_gtstate_set/get).
 *
 * (Part 3, the compiler-emitted stack probe, is declared in rt.h instead --
 * rt_stack_limit and rt_stack_probe_slow -- because emitted code must see
 * it. Everything in this file is internal plumbing a scheduler will use
 * later, so it is #included only by rt.c and by this phase's own test
 * harness, the same way runtime/sys.h is.)
 *
 * NOT included by rt.h, and nothing in rt.c's own compiled text calls
 * rt_ctx_switch or takes the address of rt_ctx_trampoline -- see
 * rt_ctx_make and rt_fiber_switch below for exactly why that matters: it is
 * what keeps every existing build line in this repository (build.sh, run.sh,
 * every app and bench under apps/ and bench/) working unmodified, without
 * any of them needing to link runtime/ctx_switch_x86_64.s.
 */
#ifndef RT_GREENTHREAD_H
#define RT_GREENTHREAD_H

#include "rt.h" /* rt_stack_limit, RT_STACK_LIMIT_POISON, rt_trap */

#include <stddef.h>
#include <stdint.h>

/* A note on AddressSanitizer and this file, left here because it is exactly
 * the kind of thing the next person touching this needs to know before they
 * "fix" it:
 *
 * ASan tracks "the current stack" per OS thread by watching ordinary
 * call/return nesting. A hand-written context switch that moves %rsp to a
 * completely different, independently-allocated region without telling it
 * is invisible to that tracking. Confirmed directly while building this:
 * running the overflow test under ASan prints "ASan is ignoring requested
 * __asan_handle_no_return ... False positive error reports may follow" when
 * the probe's trap (rt_trap -> abort) fires on a fiber's stack -- but no
 * actual false-positive ERROR followed in any test run; the whole suite
 * (round trip, interleaving/isolation, multi-slab stress) is clean under
 * `-fsanitize=address,undefined` as a hard pass/fail gate.
 *
 * The standard real fix -- used by Boost.Context, Lua's coroutine library,
 * and the sanitizers project's own fiber examples -- is to bracket a switch
 * with __sanitizer_start_switch_fiber/__sanitizer_finish_switch_fiber. It
 * was tried here and reverted: bracketing just rt_ctx_switch inside
 * rt_fiber_switch calls "finish" only on the ORIGINATING side's eventual
 * resume, never on the TARGET side's entry (first-time, via
 * ctx_switch_x86_64.s's trampoline, or on a later resume) -- so the moment a
 * freshly-switched-into fiber initiates its OWN next switch, ASan sees a
 * second "start" before the first one's "finish" and reports "starting
 * fiber switch while in fiber switch", a real error this file would
 * otherwise ship with. Doing this correctly needs the target side's own
 * entry point to call "finish" BEFORE doing anything else -- including the
 * raw asm trampoline, for a fiber's very first entry -- which means
 * threading the saved fake-stack pointer through the hand-written asm
 * contract itself (an extra smuggled register, or a field in rt_ctx_t),
 * not just wrapping the C-level call. That is real, scoped work, not a
 * one-line fix, and attempting it half-correctly produced a worse result
 * (a spurious hard error) than leaving it alone. Flagged here as a known,
 * legitimate follow-up rather than attempted again speculatively. */

/* Every green-thread stack in Phase 1 is this one fixed size -- defined
 * here, ahead of Part 1, so it is available wherever it is needed; the full
 * rationale and the rest of the allocator built on it is Part 2, further
 * down.
 *
 * Raised from the original 64 KiB during the task that wired `spawn`/
 * `Chan`/`net` to this scheduler (docs/concurrency-decision.md, "Phase
 * 3.5"). IMPORTANT, CORRECTED NOTE from that task's own report: this value
 * was initially raised believing it fixed a `clang -O2`-only stack-overflow
 * trap in corpus/modules/stdlib-http-client. It does not reliably fix that
 * -- further investigation (same task) found the SAME program fails
 * intermittently (roughly 1 run in 2-3) at every stack size tried, 64 KiB
 * through 8 MiB, whenever more than one real carrier is active, and a TSan
 * run caught the actual cause directly: a genuine data race on
 * `rt_stack_limit` between two different carrier OS threads, one writing it
 * in `rt_fiber_switch` (this file), the other reading it in a green
 * thread's own compiler-emitted stack probe -- see that task's report for
 * the full TSan transcript and why it was not fixed there (pre-existing
 * Phase 1/2 mechanism, outside that task's stated scope to modify, and a
 * hand-rolled context-switch bug is exactly the kind of thing this project
 * has already learned not to patch half-confidently -- see the ASan note
 * above). A bigger stack is kept anyway, independent of that unresolved
 * bug, because it is still independently true that 64 KiB was never
 * exercised by a real compiled program before that task (only Phase 1's own
 * synthetic tests), and a real program's call depth under an aggressively-
 * inlining compiler plausibly does need more than that even in the
 * SINGLE-carrier case this race cannot reach -- not re-verified in isolation
 * given time spent on the race above, so treat this specific number as a
 * reasonable, cheap default rather than a precisely-justified one. Cheap
 * because a slab's bytes are a virtual-memory reservation, demand-paged,
 * not a commitment of real RAM per green thread that never uses it.
 * RT_SLAB_STACKS is unchanged, so this only grows each slab's byte size,
 * not its VMA count -- the "a million green threads is ~1000 VMAs"
 * argument in docs/concurrency-decision.md is unaffected. */
#define RT_STACK_SIZE  ((size_t)1024 * 1024)           /* 1 MiB per stack */
#define RT_SLAB_STACKS 1024                           /* stacks per slab */
#define RT_SLAB_BYTES  (RT_STACK_SIZE * RT_SLAB_STACKS) /* 1 GiB: 1 mmap */

/* ========================================================================
 * Part 1 -- context switch
 * ====================================================================== */

/* Callee-saved registers plus the stack pointer: the System V x86-64 ABI is
 * the entire contract (docs/concurrency-decision.md names nothing more
 * exotic). Caller-saved registers get no slot -- the C calling convention
 * already means rt_ctx_switch's caller does not expect them preserved
 * across a call, and rt_ctx_switch IS a call.
 *
 * Field order matches the byte offsets runtime/ctx_switch_x86_64.s hard-
 * codes (0, 8, 16, 24, 32, 40, 48). The two files are one contract; there is
 * no shared source of truth for the layout beyond this comment and that
 * file's matching one, so a change to either without the other is a bug.
 *
 * aarch64 (Phase 4) will need its own, differently-shaped struct -- nothing
 * about this layout is assumed anywhere outside this file and
 * ctx_switch_x86_64.s. */
typedef struct rt_ctx {
    uint64_t rsp;
    uint64_t rbx;
    uint64_t rbp;
    uint64_t r12;
    uint64_t r13;
    uint64_t r14;
    uint64_t r15;
} rt_ctx_t;

/* Save the running context into *from, load *to, and resume there --
 * defined in runtime/ctx_switch_x86_64.s. See that file for the mechanism:
 * it ends in one `ret`, not a jump, which is what turns "restore the
 * registers" into "resume exactly where that context left off" without
 * this function needing to know whether `to` ever ran before.
 *
 * Deliberately knows nothing about rt_stack_limit (rt.h) -- that is Part
 * 3's seam, kept out of this function on purpose so the asm stays minimal
 * and ignorant of anything above the raw ABI. Call rt_fiber_switch, below,
 * instead of this directly, once a stack has a real owner. */
void rt_ctx_switch(rt_ctx_t *from, rt_ctx_t *to);

/* The landing pad for a context that has never run. rt_ctx_switch's `ret`
 * jumps here the first time a freshly-made context is switched into -- see
 * rt_ctx_make below for the fake stack frame that arranges this, and
 * ctx_switch_x86_64.s for the two registers it reads to find the real entry
 * point and argument. Not meant to be called directly from C -- there is no
 * ordinary call that could reach it with the ABI it actually expects. */
void rt_ctx_trampoline(void);

/* Where ctx_switch_x86_64.s's trampoline goes if the entry function it
 * calls ever returns. Phase 1 has no scheduler for control to return TO, so
 * this traps, the same way every other "should not happen" in this runtime
 * does. Defined in greenthread.c; declared here because the .s file needs
 * to see this exact name. */
_Noreturn void rt_ctx_entry_returned(void);

/* Hand-build an initial context so that switching into it for the first
 * time starts `entry(arg)` running on `[stack_base, stack_base+stack_size)`
 * -- stack growing down, so the usable region starts at the high end --
 * rather than "resuming" something that never ran.
 *
 * `static inline`, and deliberately NOT moved into greenthread.c: it takes
 * the ADDRESS of rt_ctx_trampoline, which is defined only in
 * ctx_switch_x86_64.s. greenthread.c is #included into rt.c, and rt.c is
 * linked by every program in this repository whether or not it ever touches
 * green threads. If this function's body lived in rt.c's own compiled text,
 * that address-of would force rt_ctx_trampoline to be resolved at link time
 * for every one of those programs too, breaking every build line that links
 * runtime/rt.c without also linking ctx_switch_x86_64.o. As `static
 * inline`, the compiler only emits it into whichever translation unit
 * actually calls it -- this phase's test harness today, Phase 2's scheduler
 * later -- both of which link ctx_switch_x86_64.o on purpose. */
static inline void rt_ctx_make(rt_ctx_t *ctx, void *stack_base,
                                size_t stack_size, void (*entry)(void *),
                                void *arg) {
    /* The usable stack is [stack_base, stack_base + stack_size); align the
     * top down to 16 defensively (every real caller already hands in a
     * page-aligned mmap region, which is 16-aligned for free, but a test
     * harness building its own small scratch stack should not have to get
     * this right by hand too). */
    uintptr_t top = ((uintptr_t)stack_base + stack_size) & ~(uintptr_t)15;

    /* Leave room for exactly one fake return address, the way a real `call`
     * instruction would have. rt_ctx_switch's `ret` pops this value and
     * jumps to it; landing at rt_ctx_trampoline with %rsp sitting right
     * where a `call` would have left it (16-aligned minus the 8 bytes the
     * `call` itself would have pushed) is what lets the trampoline's own
     * `call *%r12` be ABI-correct -- see ctx_switch_x86_64.s. */
    uint64_t *sp = (uint64_t *)(top - 8);
    sp[0] = (uint64_t)(uintptr_t)rt_ctx_trampoline;

    ctx->rsp = (uint64_t)(uintptr_t)sp;
    /* Smuggled through to the trampoline in two callee-saved registers --
     * rt_ctx_switch is about to load these from *ctx right before the `ret`
     * above fires, which is the only channel into a context that was never
     * actually called with real arguments. */
    ctx->r12 = (uint64_t)(uintptr_t)entry; /* the entry function */
    ctx->r13 = (uint64_t)(uintptr_t)arg;   /* ... and its one argument */
    ctx->rbx = 0;
    ctx->rbp = 0;
    ctx->r14 = 0;
    ctx->r15 = 0;
}

/* ---- the Part 1 / Part 3 seam -------------------------------------------
 *
 * rt_ctx_switch above knows nothing about rt_stack_limit, on purpose. This
 * is the one place the two meet: switching INTO a green thread must update
 * rt_stack_limit to THAT thread's stack boundary before any of its code can
 * run its own probe correctly, and it has to happen on the carrier OS
 * thread doing the switching, which is exactly where this function runs,
 * and it has to happen BEFORE control actually reaches the target context
 * -- so the write comes first, then the switch.
 *
 * (It would be just as correct to set rt_stack_limit after rt_ctx_switch
 * returns here, for the symmetric reason: that is the point at which `from`
 * has resumed, on `from`'s own stack, and anything running there should see
 * `from`'s limit, not `to`'s. A real scheduler will need to set it on both
 * sides of this call, once it exists. Phase 1's tests set it on both sides
 * explicitly rather than teaching this helper two limits, since this phase
 * has no notion yet of what a currently-running fiber's own limit even is.)
 *
 * `static inline` for the same reason rt_ctx_make is: unreachable from
 * rt.c's own compiled text, so no program that does not use green threads
 * is forced to link ctx_switch_x86_64.o just because rt.c mentions this
 * function's name.
 *
 * No ASan fiber annotation here -- see the note above RT_STACK_SIZE for why
 * not, and what it would actually take. */
static inline void rt_fiber_switch(rt_ctx_t *from, rt_ctx_t *to,
                                    uintptr_t to_stack_limit) {
    rt_stack_limit = to_stack_limit;
    rt_ctx_switch(from, to);
}

/* ========================================================================
 * Part 2 -- slab stack allocator
 *
 * docs/concurrency-decision.md, "Stacks: fixed, but not limited": one mmap
 * per slab instead of one per stack, so a million green threads cost ~1000
 * VMAs instead of ~2,000,000 (two per thread, under the guard-page design
 * this replaces). No guard pages at all -- overflow detection is the
 * probe's job (Part 3), not the MMU's.
 * ====================================================================== */

typedef struct rt_slab rt_slab_t;

/* A single green-thread stack, handed out by rt_stack_alloc and returned by
 * rt_stack_free.
 *
 * `base` is the lowest valid address of the stack -- exactly the value a
 * fiber's rt_stack_limit (rt.h) must be set to when it is running, since
 * there is no guard page below it to catch anything the probe misses.
 * `top` is base + RT_STACK_SIZE, the initial stack pointer region to hand
 * rt_ctx_make.
 *
 * `slab` and `index` are what make rt_stack_free O(1): finding which slab
 * owns an arbitrary address would mean searching every slab's range, but
 * the handle already knows, because rt_stack_alloc is the only thing that
 * ever creates one. */
typedef struct rt_stack {
    void *base;
    void *top;
    rt_slab_t *slab;
    uint32_t index;
} rt_stack_t;

/* Hand out one free stack, allocating a brand new 64 MiB slab first if
 * every existing slab is full. Never fails silently: out of memory traps,
 * the same as every other allocation failure in this runtime (rt_trap). */
rt_stack_t rt_stack_alloc(void);

/* Return a stack to its slab's free list. `s` must have come from
 * rt_stack_alloc and must not be used again afterwards. */
void rt_stack_free(rt_stack_t *s);

/* ========================================================================
 * Part 4 -- byte-per-thread state table
 *
 * A flat array indexed by green-thread id, read and written in O(1), never
 * scanned to find work -- Phase 2's scheduler owns that policy; this is
 * just the storage. docs/concurrency-decision.md's enum, unchanged.
 * ====================================================================== */

typedef enum rt_green_state {
    RT_GT_RUNNABLE = 0,
    RT_GT_RUNNING,
    RT_GT_PARKED_IO,
    RT_GT_PARKED_CHAN,
    RT_GT_PARKED_TIMER,
    RT_GT_DEAD,
} rt_green_state_t;

/* Both grow the table (realloc, doubling) if `id` has not been seen before,
 * so a fresh id reads back RT_GT_RUNNABLE rather than undefined memory --
 * this is also implicitly its own capacity planner: a scheduler simply
 * never has to size this table up front, even at the "millions of green
 * threads" target scale. Growth is mutex-protected; a plain read or write
 * of an already-allocated slot is not, since Phase 1 has only ever one
 * writer per id (its own tests drive this directly, by hand) -- a real
 * concurrent-writer story is Phase 2's problem, once there is a scheduler
 * that could actually produce one. */
void rt_gtstate_set(uint32_t id, rt_green_state_t s);
rt_green_state_t rt_gtstate_get(uint32_t id);

#endif /* RT_GREENTHREAD_H */
