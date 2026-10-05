/* Phase 1 green-thread runtime primitives (docs/concurrency-decision.md,
 * "Phases" -- this is Phase 1, not Phase 2). There is no scheduler here and
 * none of this is reachable from emitted code: `spawn` still means an OS
 * thread (rt.h's "concurrency" section) until Phase 2 is built on top of
 * this. This header exists so that work has something solid to start from.
 *
 * Three pieces, matching the design doc exactly:
 *
 *   Part 1 -- the context switch (rt_ctx_t, rt_ctx_switch, rt_ctx_make).
 *             The asm itself is runtime/ctx_switch_x86_64.S on x86-64,
 *             runtime/ctx_switch_aarch64.S on aarch64 (Phase 4 --
 *             portability; runtime/arch.sh picks the right one at build
 *             time). rt_ctx_t's layout and rt_ctx_make's body are each
 *             `#if defined(__x86_64__) / #elif defined(__aarch64__)`
 *             below, one shape per architecture's ABI.
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
 * any of them needing to link the architecture's ctx_switch_*.s file.
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
 * ctx_switch_x86_64.S's trampoline, or on a later resume) -- so the moment a
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

/* A note on ThreadSanitizer and this file -- the gap the ASan note above
 * describes has a TSan equivalent, found and now fixed (docs/
 * concurrency-decision.md, "Known gap" / "Phase 3.5"): TSan's own internal
 * stack-trace bookkeeping (StackDepotBase::Put and friends) intermittently
 * segfaulted -- never a real race report, never inside any function this
 * project defines -- when a green thread was suspended by one carrier OS
 * thread and resumed by a different one, purely because nothing told TSan
 * that the OS thread doing the resuming was now running a completely
 * different logical thread of execution on a completely different stack.
 * Unlike the ASan situation above, TSan's own fiber API is NOT asymmetric
 * (no separate "start"/"finish" halves, so none of the trampoline-threading
 * problem described above applies here): `__tsan_switch_to_fiber(target,
 * 0)` is one call, made by whichever side is ABOUT to jump, naming the
 * fiber identity execution is jumping TO, immediately before the actual
 * stack switch (rt_ctx_switch) that performs it. This file's callers
 * (runtime/scheduler.c's carrier_dispatch/rt_sched_yield/rt_sched_park/
 * green_trampoline) make exactly that call around each of their
 * rt_fiber_switch invocations -- not added inside rt_fiber_switch itself,
 * since not every caller of rt_fiber_switch has (or needs) a notion of a
 * fiber identity to switch to: runtime/greenthread_test.c, Phase 1's own
 * standalone harness, exercises raw fiber round-trips on a single OS
 * thread, never the cross-carrier resume pattern that triggers this gap,
 * and is never built under TSan (runtime/greenthread_test.sh has no TSan
 * variant) -- so it is deliberately left uninstrumented rather than given
 * fiber identities it has no use for. `flags` is always 0 (the default,
 * synchronizing switch): the handoff this runtime performs across a
 * context switch is a real happens-before edge (one real memory write,
 * ordered before the switch; one real read after it, from whichever OS
 * thread resumes), and 0 is what tells TSan to model it as one, same as an
 * ordinary mutex unlock/lock pair would -- the non-synchronizing flag
 * exists for fibers that already have their own separate synchronization
 * and would otherwise be double-counted, which does not describe this
 * runtime. Declared here, under the same portable thread_sanitizer feature
 * test runtime/phase3_test.c already uses (gcc defines
 * __SANITIZE_THREAD__ directly; clang exposes __has_feature(
 * thread_sanitizer)), rather than via <sanitizer/tsan_interface.h>, so
 * nothing in this build depends on that optional header being installed;
 * these four signatures are stable, documented compiler-rt entry points. */
#if defined(__SANITIZE_THREAD__)
#define RT_TSAN_BUILD 1
#elif defined(__has_feature)
#if __has_feature(thread_sanitizer)
#define RT_TSAN_BUILD 1
#endif
#endif
#ifndef RT_TSAN_BUILD
#define RT_TSAN_BUILD 0
#endif

#if RT_TSAN_BUILD
void *__tsan_create_fiber(unsigned flags);
void  __tsan_destroy_fiber(void *fiber);
void  __tsan_switch_to_fiber(void *fiber, unsigned flags);
void *__tsan_get_current_fiber(void);
#endif

/* Every green-thread stack in Phase 1 is this one fixed size -- defined
 * here, ahead of Part 1, so it is available wherever it is needed; the full
 * rationale and the rest of the allocator built on it is Part 2, further
 * down. Fixed-size, permanently: docs/concurrency.md's "Fixed-size stacks,
 * forever" -- growable/copying stacks are incompatible with this runtime's
 * C interop (raw pointers into a stack that a copy would invalidate), so
 * the only lever here is this constant, not a growth mechanism.
 *
 * Raised once already, from the original 64 KiB during the task that wired
 * `spawn`/`Chan`/`net` to this scheduler (docs/concurrency-decision.md,
 * "Phase 3.5"), to 1 MiB. IMPORTANT, CORRECTED NOTE from that task's own
 * report, still true and worth re-reading before ever raising this value
 * again: that first raise was initially believed to fix a `clang
 * -O2`-only stack-overflow trap in corpus/modules/stdlib-http-client, but
 * does not reliably fix that -- a TSan run caught the real cause directly: a
 * genuine data race on `rt_stack_limit` between two different carrier OS
 * threads, one writing it in `rt_fiber_switch` (this file), the other
 * reading it in a green thread's own compiler-emitted stack probe,
 * reproducing intermittently (roughly 1 run in 2-3) ONLY when more than one
 * real carrier is active, at every stack size tried -- a scheduling bug, not
 * a space one, and still unfixed (pre-existing Phase 1/2 mechanism, outside
 * that task's own scope).
 *
 * Raised a second time here, from 1 MiB to 8 MiB, for a DIFFERENT and
 * properly distinguished reason: apps/markdown's own pathological-input
 * test (test.sh, "deep-quote" -- 20,000 nested blockquotes) traps here with
 * a genuine, 100%-reproducible overflow, confirmed to persist unchanged
 * under `LANG_NUM_CARRIERS=1` (10/10 runs, both with and without that env
 * var) -- ruling out the race above by the same test that would have to
 * trigger it (a single carrier cannot race itself), and confirming this one
 * really is a single green thread's own recursive call depth
 * (apps/markdown/blocks.m31's own header: "nesting costs a recursive call
 * and nothing else") genuinely exceeding its stack, not a second instance
 * of the unresolved bug above. 8 MiB (empirically: the pathological case
 * passes cleanly with this, where 1 MiB started failing around ~4,500
 * levels) matches a typical OS thread's own default stack size, so this is
 * not an unusually generous number.
 *
 * Still cheap, same reasoning the first raise gave: a slab's bytes are a
 * virtual-memory reservation, demand-paged, not a commitment of real RAM
 * per green thread that never uses it. RT_SLAB_STACKS is unchanged, so this
 * only grows each slab's byte size (1 GiB to 8 GiB), not its VMA count --
 * the "a million green threads is ~1000 VMAs" argument in
 * docs/concurrency-decision.md is unaffected.
 *
 * `LANG_STACK_SIZE` (an environment variable, parsed once, lazily, by
 * `rt_stack_size()` in greenthread.c): this constant is still the fixed
 * size every stack in a given process gets -- "fixed-size stacks, forever"
 * (above) is about there being no growth/copy mechanism DURING a green
 * thread's life, not about the number being unchangeable before any
 * thread exists. A workload with its own unusually deep recursion (the
 * exact shape that forced the raise above) can ask for more without a
 * custom build, the same way `LANG_NUM_CARRIERS`/`LANG_FUEL_SIZE`/
 * `LANG_GLOBAL_QUEUE_CAP` (scheduler.c) are already env-var-tunable
 * scheduler constants rather than build-time-only ones. RT_STACK_SIZE
 * below is that env var's fallback, used verbatim if it is unset or
 * invalid -- never a compile-time-only value itself now. */
#define RT_STACK_SIZE  ((size_t)8 * 1024 * 1024)       /* 8 MiB per stack, if LANG_STACK_SIZE is unset */
#define RT_SLAB_STACKS 1024                           /* stacks per slab */

/* The resolved stack size for this process: `LANG_STACK_SIZE` if it names a
 * valid positive integer of bytes, RT_STACK_SIZE otherwise -- read and
 * cached ONCE, via `pthread_once` (the same idiom rt.c's own
 * `g_sched_once`/`g_reactor_once` already use for exactly this "lazy,
 * race-free, one-time init" need), so every caller after the first gets the
 * same answer regardless of which OS thread asks or how many ask at once.
 * Declared here rather than left as a macro because RT_SLAB_BYTES (below,
 * now computed, not `#define`d) and every other former use of RT_STACK_SIZE
 * need a single runtime value, not a build-time constant, once it can come
 * from the environment. */
size_t rt_stack_size(void);

/* ========================================================================
 * Part 1 -- context switch
 * ====================================================================== */

/* Callee-saved registers plus the stack pointer: the platform ABI is the
 * entire contract (docs/concurrency-decision.md names nothing more
 * exotic). Caller-saved registers get no slot -- the C calling convention
 * already means rt_ctx_switch's caller does not expect them preserved
 * across a call, and rt_ctx_switch IS a call.
 *
 * Two shapes, one per architecture this runtime has a hand-written context
 * switch for (docs/concurrency-decision.md, "Phase 4" -- portability).
 * Exactly one of these is compiled, selected the same way the matching .s
 * file is selected at build time (runtime/arch.sh): by the compiler's own
 * predefined architecture macro, not a build flag this header would have
 * to be told separately. Field order in each branch matches the byte
 * offsets that branch's .s file hard-codes; the two are one contract, with
 * no shared source of truth for the layout beyond this comment and that
 * file's matching one, so a change to either without the other is a bug
 * that corrupts every stack silently rather than crashing cleanly. */
#if defined(__x86_64__)

/* System V x86-64 ABI. Byte offsets runtime/ctx_switch_x86_64.S hard-codes:
 * 0, 8, 16, 24, 32, 40, 48. */
typedef struct rt_ctx {
    uint64_t rsp;
    uint64_t rbx;
    uint64_t rbp;
    uint64_t r12;
    uint64_t r13;
    uint64_t r14;
    uint64_t r15;
} rt_ctx_t;

#elif defined(__aarch64__)

/* AAPCS64. Byte offsets runtime/ctx_switch_aarch64.S hard-codes: 0, 8, 16,
 * 24, 32, 40, 48, 56, 64, 72, 80, 88, 96, 104, 112, 120, 128, 136, 144,
 * 152, 160. The d8-d15 tail has no x86-64 analogue -- System V x86-64 makes
 * every xmm register caller-saved, so ctx_switch_x86_64.S's struct has
 * nothing corresponding to it, but AAPCS64 requires the callee (this
 * function) to preserve d8-d15 across a call, so a value this project's
 * `float`/`double` keeps live in one of them across a context switch must
 * be saved here or it silently corrupts -- see ctx_switch_aarch64.S's
 * header comment for the full reasoning. */
typedef struct rt_ctx {
    uint64_t sp;
    uint64_t x19;
    uint64_t x20;
    uint64_t x21;
    uint64_t x22;
    uint64_t x23;
    uint64_t x24;
    uint64_t x25;
    uint64_t x26;
    uint64_t x27;
    uint64_t x28;
    uint64_t fp;  /* x29 */
    uint64_t lr;  /* x30 */
    uint64_t d8;
    uint64_t d9;
    uint64_t d10;
    uint64_t d11;
    uint64_t d12;
    uint64_t d13;
    uint64_t d14;
    uint64_t d15;
} rt_ctx_t;

#else
#error "runtime/greenthread.h: no struct rt_ctx layout for this architecture " \
       "-- this runtime has a hand-written context switch only for x86-64 " \
       "and aarch64 (docs/concurrency-decision.md, Phase 4); see " \
       "ctx_switch_x86_64.S and ctx_switch_aarch64.S for what a new port " \
       "needs to provide."
#endif

/* Save the running context into *from, load *to, and resume there --
 * defined in runtime/ctx_switch_x86_64.S on x86-64, runtime/ctx_switch_
 * aarch64.S on aarch64 (selected by runtime/arch.sh at build time; see
 * either file's own header for the mechanism). Both end in one `ret`, not
 * a jump, which is what turns "restore the registers" into "resume exactly
 * where that context left off" without this function needing to know
 * whether `to` ever ran before.
 *
 * Deliberately knows nothing about rt_stack_limit (rt.h) -- that is Part
 * 3's seam, kept out of this function on purpose so the asm stays minimal
 * and ignorant of anything above the raw ABI. Call rt_fiber_switch, below,
 * instead of this directly, once a stack has a real owner. */
void rt_ctx_switch(rt_ctx_t *from, rt_ctx_t *to);

/* The landing pad for a context that has never run. rt_ctx_switch's `ret`
 * jumps here the first time a freshly-made context is switched into -- see
 * rt_ctx_make below for how each architecture arranges this, and the
 * matching ctx_switch_*.s file for the two registers it reads to find the
 * real entry point and argument. Not meant to be called directly from C --
 * there is no ordinary call that could reach it with the ABI it actually
 * expects. */
void rt_ctx_trampoline(void);

/* Where the trampoline goes if the entry function it calls ever returns.
 * Phase 1 has no scheduler for control to return TO, so this traps, the
 * same way every other "should not happen" in this runtime does. Defined
 * in greenthread.c; declared here because both .s files need to see this
 * exact name. */
_Noreturn void rt_ctx_entry_returned(void);

/* Hand-build an initial context so that switching into it for the first
 * time starts `entry(arg)` running on `[stack_base, stack_base+stack_size)`
 * -- stack growing down, so the usable region starts at the high end --
 * rather than "resuming" something that never ran.
 *
 * `static inline`, and deliberately NOT moved into greenthread.c: it takes
 * the ADDRESS of rt_ctx_trampoline, which is defined only in the
 * architecture's own ctx_switch_*.s file. greenthread.c is #included into
 * rt.c, and rt.c is linked by every program in this repository whether or
 * not it ever touches green threads. If this function's body lived in
 * rt.c's own compiled text, that address-of would force rt_ctx_trampoline
 * to be resolved at link time for every one of those programs too, breaking
 * every build line that links runtime/rt.c without also linking the
 * ctx_switch_*.o this architecture needs. As `static inline`, the compiler
 * only emits it into whichever translation unit actually calls it -- this
 * phase's test harness today, Phase 2's scheduler later -- both of which
 * link the right ctx_switch_*.o on purpose (runtime/arch.sh picks it).
 *
 * One function per architecture below, rather than one function with
 * scattered #ifdefs in its body: the two stack-frame shapes are different
 * enough (x86-64 writes a fake return address onto the new stack; aarch64
 * needs no such write, see ctx_switch_aarch64.S) that interleaving them
 * would be harder to audit than two short, self-contained versions. */
#if defined(__x86_64__)

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
     * `call *%r12` be ABI-correct -- see ctx_switch_x86_64.S. */
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

#elif defined(__aarch64__)

static inline void rt_ctx_make(rt_ctx_t *ctx, void *stack_base,
                                size_t stack_size, void (*entry)(void *),
                                void *arg) {
    /* Same usable-region/alignment reasoning as the x86-64 branch above. */
    uintptr_t top = ((uintptr_t)stack_base + stack_size) & ~(uintptr_t)15;

    /* No fake return address to write, unlike x86-64: aarch64's `ret`
     * branches to whatever is in `lr` (x30), a register rt_ctx_switch loads
     * from *ctx directly, not a value popped off the stack. So `sp` here is
     * simply the honest 16-aligned top of the stack -- see
     * ctx_switch_aarch64.S's header for why that is a simplification, not a
     * shortcut. */
    ctx->sp = (uint64_t)(uintptr_t)top;
    ctx->lr = (uint64_t)(uintptr_t)rt_ctx_trampoline;

    /* Smuggled through to the trampoline in two callee-saved registers --
     * rt_ctx_switch is about to load these from *ctx right before the `ret`
     * above fires, which is the only channel into a context that was never
     * actually called with real arguments. */
    ctx->x19 = (uint64_t)(uintptr_t)entry; /* the entry function */
    ctx->x20 = (uint64_t)(uintptr_t)arg;   /* ... and its one argument */
    ctx->x21 = 0;
    ctx->x22 = 0;
    ctx->x23 = 0;
    ctx->x24 = 0;
    ctx->x25 = 0;
    ctx->x26 = 0;
    ctx->x27 = 0;
    ctx->x28 = 0;
    ctx->fp = 0;
    ctx->d8 = 0;
    ctx->d9 = 0;
    ctx->d10 = 0;
    ctx->d11 = 0;
    ctx->d12 = 0;
    ctx->d13 = 0;
    ctx->d14 = 0;
    ctx->d15 = 0;
}

#endif /* rt_ctx_make per architecture -- see the #if/#elif on struct rt_ctx
        * above for the #else/#error case; it already fired by the time
        * either rt_ctx_make here would be reached, so none is needed again. */

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

#if RT_TSAN_BUILD
    /* This stack lease's own TSan fiber identity -- see this file's own
     * "A note on ThreadSanitizer" comment, above, for the full account.
     * Created in rt_stack_alloc, destroyed in rt_stack_free (runtime/
     * greenthread.c): one TSan fiber per logical lease of a stack, not per
     * physical slab slot, so a slot's later reuse by a completely
     * different green thread gets a fresh fiber identity rather than
     * inheriting a stale one's happens-before history. Field only exists
     * in a TSan build -- rt_stack_t's layout is pure C with no asm-side
     * contract (unlike rt_ctx_t), so varying its size by build mode is
     * safe. */
    void *tsan_fiber;
#endif
} rt_stack_t;

/* Hand out one free stack, allocating a brand new 64 MiB slab first if
 * every existing slab is full. Never fails silently: out of memory traps,
 * the same as every other allocation failure in this runtime (rt_trap).
 * In a TSan build, also creates this lease's own tsan_fiber (above). */
rt_stack_t rt_stack_alloc(void);

/* Return a stack to its slab's free list. `s` must have come from
 * rt_stack_alloc and must not be used again afterwards. In a TSan build,
 * also destroys this lease's tsan_fiber (above) -- safe here because, by
 * the time a green thread's stack is freed, the caller (runtime/
 * scheduler.c's carrier_dispatch, in its `finished` branch) has always
 * already switched this OS thread's TSan fiber identity back to the
 * carrier's own native fiber (green_trampoline's switch-away does this
 * before this stack can ever be freed), so tsan_fiber is never the
 * current fiber of any thread at the point it is destroyed. */
void rt_stack_free(rt_stack_t *s);

/* DIAGNOSTIC ONLY, not for master -- see
 * .claude/worktrees/diagnostics-fuel-dip-investigation's own commits. How
 * many times rt_stack_alloc has had to create a fresh slab (each one a
 * mmap(1 GiB) call made while holding g_slab_lock, the single global mutex
 * every concurrent rt_stack_alloc/rt_stack_free call across every carrier
 * contends for), and the total nanoseconds spent inside those mmap calls. */
uint64_t rt_diag_slab_allocs(void);
uint64_t rt_diag_slab_alloc_ns(void);

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
    RT_GT_PARKED_QUEUE, /* scheduler.c's squeue_push backpressure -- a green
                          * thread's `spawn` waiting for the shared queue to
                          * have room, never a carrier blocking on it. */
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
