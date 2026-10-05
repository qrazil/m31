/* Tests for Phase 1 of docs/concurrency-decision.md: the context switch
 * (x86-64 and, since Phase 4, aarch64 -- this file is architecture-generic,
 * written entirely against rt_ctx_t/rt_ctx_make/rt_fiber_switch and never
 * against a register name directly), the slab stack allocator, the
 * compiler-emitted probe's runtime side, and the per-thread state table.
 * Run by runtime/greenthread_test.sh, which builds this under every
 * compiler/opt combination gates.sh already uses elsewhere, plus a
 * dedicated ASan+UBSan build.
 *
 * There is no RFC-style oracle for a context switch, so this is hand-
 * constructed, known-interleaving testing throughout: every expected log in
 * this file was traced BY HAND against the fixed, hard-coded switch order
 * right next to it, never produced by running the code first and copying
 * its output -- the same discipline the language corpus holds
 * hand-authored corpus/core fixtures to (README.md, "The oracle").
 *
 * Two argv-selected failure-mode sub-tests (`overflow`, `poison`) are meant
 * to abort the process -- SIGABRT, exit 134, exactly like a corpus/traps
 * program -- so runtime/greenthread_test.sh runs those as separate child
 * processes rather than in the main suite below, which must itself exit 0.
 *
 * One honest limitation, stated up front rather than glossed over: this
 * cannot test the probe end to end through a compiled .m31 program, because
 * nothing yet routes a compiled program's execution through
 * rt_fiber_switch -- `spawn` still means an OS thread (rt.h), and nothing
 * in this phase builds the scheduler that would make a real green thread
 * run on a slab stack. So the compiler side of the Part 3 contract is
 * checked separately, in runtime/greenthread_test.sh, by compiling a
 * trivial .m31 file with the actual m31c binary and grepping the emitted C
 * for a call to rt_stack_check() (the real, `noinline` out-of-line probe --
 * rt_stack_limit's own comment in rt.h explains why it must never be
 * inlined comparison text at the call site instead); the `overflow`/
 * `poison` sub-tests below reproduce the same comparison by hand against a
 * real stack and the real rt_stack_probe_slow, which is the strongest check
 * available on the runtime side before Phase 2 exists to close this gap
 * for real.
 */
#include "rt.h"
#include "greenthread.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int checks = 0;
static int failures = 0;

#define CHECK(cond, msg)                                                     \
    do {                                                                     \
        checks++;                                                            \
        if (!(cond)) {                                                       \
            failures++;                                                      \
            fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, msg);     \
        }                                                                     \
    } while (0)

/* ========================================================================
 * Test 1 -- round trip: switch main -> A -> main -> A -> main, and confirm
 * every one of A's locals survived both the trip away and the trip back.
 * ====================================================================== */

typedef struct {
    rt_ctx_t *self;
    rt_ctx_t *main_ctx;
} rt1_arg_t;

/* 0 = not finished yet, 1 = locals survived, -1 = corrupted */
static volatile int g_rt1_result = 0;

static void rt1_entry(void *argp) {
    rt1_arg_t *arg = (rt1_arg_t *)argp;

    /* Six values -- one more than the six callee-saved integer registers
     * (rbx, rbp, r12-r15 on x86-64; x19-x28/fp on aarch64, which has more
     * than enough, but the point is the same either way) rt_ctx_switch
     * actually saves, so there is necessarily at least one the compiler
     * cannot simply leave parked in a register across both calls below
     * without ever touching memory, whichever way it chooses to allocate
     * them. Either path (register, kept safe by the architecture's
     * ctx_switch_*.s; or stack, kept safe by the stack pointer being saved
     * and restored) is exactly what this test means to exercise. */
    long a = 0x1111111111111111L;
    long b = 0x2222222222222222L;
    long c = 0x3333333333333333L;
    long d = 0x4444444444444444L;
    long e = 0x5555555555555555L;
    long f = 0x6666666666666666L;

    /* Eight floating-point values -- exactly the aarch64 callee-saved
     * d8-d15 count (greenthread.h, ctx_switch_aarch64.S). On x86-64 this is
     * a no-op as far as ctx_switch_x86_64.S is concerned (System V makes
     * every xmm register caller-saved, so the compiler must already spill
     * any of these it keeps live across the rt_fiber_switch calls below to
     * the stack, which rsp save/restore already protects) -- but on aarch64
     * a compiler is entitled to leave these live in d8-d15 across the call,
     * trusting rt_ctx_switch to preserve them, which is exactly the
     * guarantee this is here to catch a regression in. */
    double fa = 1.0625, fb = -2.125, fc = 3.1875, fd = -4.375;
    double fe = 5.5625, ff = -6.75, fg = 7.8125, fh = -8.9375;

    /* Trip away: suspend back to main. */
    rt_fiber_switch(arg->self, arg->main_ctx, 0);

    /* Trip back: main resumed us. Every value above must be exactly what it
     * was before we left -- nothing on our own stack and nothing in any
     * register the compiler was trusting to survive the round trip should
     * have moved, because nothing else ever touches OUR stack or registers
     * between the two switches; main and anything else runs on its own. */
    bool ok = a == 0x1111111111111111L && b == 0x2222222222222222L &&
              c == 0x3333333333333333L && d == 0x4444444444444444L &&
              e == 0x5555555555555555L && f == 0x6666666666666666L &&
              fa == 1.0625 && fb == -2.125 && fc == 3.1875 && fd == -4.375 &&
              fe == 5.5625 && ff == -6.75 && fg == 7.8125 && fh == -8.9375;
    g_rt1_result = ok ? 1 : -1;

    rt_fiber_switch(arg->self, arg->main_ctx, 0);
    /* Not reached: main never resumes us a third time. */
    rt_ctx_entry_returned();
}

static void test_round_trip(void) {
    rt_ctx_t main_ctx;
    rt_stack_t stack = rt_stack_alloc();
    rt_ctx_t fctx;
    rt1_arg_t arg = { &fctx, &main_ctx };
    rt_ctx_make(&fctx, stack.base, rt_stack_size(), rt1_entry, &arg);

    g_rt1_result = 0;

    /* main -> A: A sets its locals, then switches straight back out. */
    rt_fiber_switch(&main_ctx, &fctx, (uintptr_t)stack.base);
    CHECK(g_rt1_result == 0, "fiber must not have finished its check yet");

    /* main -> A again: A checks its locals, then switches back for good. */
    rt_fiber_switch(&main_ctx, &fctx, (uintptr_t)stack.base);
    CHECK(g_rt1_result == 1,
          "fiber A's locals/register-resident state must survive a round trip");

    rt_stack_free(&stack);
}

/* ========================================================================
 * Test 2 -- a known interleaving across THREE manually-driven fibers, with
 * stack isolation folded in: each fiber also carries a distinctive canary
 * buffer it re-checks on every resume, so any bleed between stacks shows up
 * as a logged corruption marker instead of the expected step number.
 * ====================================================================== */

#define N_FIBERS 3
#define STEPS_PER_FIBER 4
#define N_SWITCHES (N_FIBERS * STEPS_PER_FIBER)
#define CANARY_BYTES 8192

typedef struct {
    rt_ctx_t ctx;
    rt_stack_t stack;
    rt_ctx_t *main_ctx;
    int id;
} fiber2_t;

static fiber2_t g_fibers[N_FIBERS];
static char g_log[N_SWITCHES][16];
static int g_log_n;

static void test2_log(int id, int step) {
    if (g_log_n < N_SWITCHES) {
        snprintf(g_log[g_log_n++], sizeof g_log[0], "F%d:%d", id, step);
    }
}

static void fiber2_entry(void *argp) {
    fiber2_t *me = (fiber2_t *)argp;
    unsigned char canary[CANARY_BYTES];
    for (int i = 0; i < CANARY_BYTES; i++) {
        canary[i] = (unsigned char)(me->id * 37 + i);
    }

    for (int step = 0; step < STEPS_PER_FIBER; step++) {
        /* On every resume after the first, some number of OTHER fibers may
         * have run a full step on their own, separate stacks since we last
         * ran -- our own canary must be exactly as we left it regardless.
         * Checked (and logged) BEFORE switching away again, so that each
         * time the driver resumes us produces exactly one log entry, in
         * lockstep with the driver's own hand-traced `order` below -- the
         * switch call must come LAST in this loop body, not first, or the
         * log for step N would not land until the resume for step N+1. */
        bool intact = true;
        for (int i = 0; i < CANARY_BYTES; i++) {
            if (canary[i] != (unsigned char)(me->id * 37 + i)) {
                intact = false;
                break;
            }
        }
        test2_log(me->id, intact ? step : 999 /* corruption marker */);

        rt_fiber_switch(&me->ctx, me->main_ctx, 0);
    }

    /* Stay parked if ever resumed again (the driver below never does). */
    for (;;) {
        rt_fiber_switch(&me->ctx, me->main_ctx, 0);
    }
}

static void test_interleaving_and_isolation(void) {
    rt_ctx_t main_ctx;
    g_log_n = 0;

    for (int i = 0; i < N_FIBERS; i++) {
        g_fibers[i].stack = rt_stack_alloc();
        g_fibers[i].main_ctx = &main_ctx;
        g_fibers[i].id = i;
        rt_ctx_make(&g_fibers[i].ctx, g_fibers[i].stack.base, rt_stack_size(),
                    fiber2_entry, &g_fibers[i]);
    }

    /* Hand-written switch order (3 fibers x 4 steps = 12 switches). Traced
     * by hand below -- NOT produced by running this test and pasting its
     * output; see the file header. */
    static const int order[N_SWITCHES] = { 0, 1, 2, 0, 1, 2, 1, 0, 2, 2, 0, 1 };

    /* Hand trace of `order`, one line per switch, tracking each fiber's own
     * step counter (which only it increments, on its own turn):
     *
     *   idx  id  that id's step counter afterwards   log entry
     *   0    0   0 -> step "0"                       F0:0
     *   1    1   0 -> step "0"                       F1:0
     *   2    2   0 -> step "0"                       F2:0
     *   3    0   1 -> step "1"                       F0:1
     *   4    1   1 -> step "1"                       F1:1
     *   5    2   1 -> step "1"                       F2:1
     *   6    1   2 -> step "2"                       F1:2
     *   7    0   2 -> step "2"                       F0:2
     *   8    2   2 -> step "2"                       F2:2
     *   9    2   3 -> step "3"                       F2:3
     *   10   0   3 -> step "3"                       F0:3
     *   11   1   3 -> step "3"                       F1:3
     *
     * which is exactly STEPS_PER_FIBER (4) entries per fiber, id 0..2. */
    static const char *const expected[N_SWITCHES] = {
        "F0:0", "F1:0", "F2:0", "F0:1", "F1:1", "F2:1",
        "F1:2", "F0:2", "F2:2", "F2:3", "F0:3", "F1:3",
    };

    for (int i = 0; i < N_SWITCHES; i++) {
        int id = order[i];
        rt_fiber_switch(&main_ctx, &g_fibers[id].ctx,
                         (uintptr_t)g_fibers[id].stack.base);
    }

    CHECK(g_log_n == N_SWITCHES, "every scripted switch must log exactly one entry");
    for (int i = 0; i < N_SWITCHES && i < g_log_n; i++) {
        if (strcmp(g_log[i], expected[i]) != 0) {
            fprintf(stderr, "FAIL interleaving[%d]: got %s, want %s\n", i,
                    g_log[i], expected[i]);
            failures++;
        }
        checks++;
    }

    for (int i = 0; i < N_FIBERS; i++) {
        rt_stack_free(&g_fibers[i].stack);
    }
}

/* ========================================================================
 * Test 3 -- multi-slab stress: force several slabs, check allocation and
 * free behave correctly across the slab boundary, and check the "1 slab = 1
 * VMA" claim directly against /proc/self/maps.
 * ====================================================================== */

static long count_vmas(void) {
    FILE *f = fopen("/proc/self/maps", "r");
    if (f == NULL) return -1;
    long n = 0;
    char line[1024];
    while (fgets(line, sizeof line, f) != NULL) {
        n++;
    }
    fclose(f);
    return n;
}

/* Enough stacks to force three slabs: two full (1024 each) and one
 * partially filled, so both "a slab fills up" and "a fresh slab is needed"
 * are actually exercised, not just the single-slab case. */
#define STRESS_COUNT (2 * RT_SLAB_STACKS + 450)

static int ptrcmp(const void *a, const void *b) {
    void *const *pa = (void *const *)a;
    void *const *pb = (void *const *)b;
    if (*pa < *pb) return -1;
    if (*pa > *pb) return 1;
    return 0;
}

static void test_multi_slab_stress(void) {
    rt_stack_t *stacks = malloc(STRESS_COUNT * sizeof *stacks);
    CHECK(stacks != NULL, "test harness allocation for the stress array");
    if (stacks == NULL) return;

    long vma_before = count_vmas();

    for (int i = 0; i < STRESS_COUNT; i++) {
        stacks[i] = rt_stack_alloc();
    }

    long vma_after_alloc = count_vmas();

    /* At most a handful of new VMAs for three new 64 MiB slabs -- nowhere
     * near STRESS_COUNT (2498) new ones, which is what one-mmap-per-stack
     * would cost. Measured on this machine: the kernel places consecutive
     * anonymous mmaps of identical protection back to back and merges them
     * into a SINGLE /proc/self/maps entry, so `grew` is often exactly 0 --
     * which is not a weaker result than "a few new VMAs", it is a stronger
     * one: not merely few VMAs per slab, but sometimes none at all beyond
     * what was already there. The lower bound is therefore 0, not 1; the
     * upper bound (10) is what actually guards against a regression back
     * toward one-VMA-per-stack, and absorbs anything else the process
     * might incidentally map (malloc growing the heap for bookkeeping, for
     * instance) without hiding that regression. */
    /* /proc/self/maps is Linux-only -- count_vmas() returns -1 when it
     * can't be opened (no procfs at all, e.g. macOS), and this is skipped
     * silently rather than failed, exactly like the second count_vmas()
     * use below: the structural "distinct slabs" check right after this is
     * the real, platform-independent test of the same claim; this one is
     * real, observed bonus evidence where it's available, not the only
     * evidence the claim has. */
    if (vma_before >= 0 && vma_after_alloc >= 0) {
        long grew = vma_after_alloc - vma_before;
        CHECK(grew >= 0 && grew <= 10,
              "allocating ~2500 stacks must add a small, slab-sized number "
              "of VMAs (possibly zero, if the kernel merges them), not one "
              "per stack");
    }

    /* The /proc/self/maps count above is real, observed evidence, but it is
     * not, by itself, a precise test of "1024 stacks share one slab": the
     * kernel merges ANY run of adjacent same-protection anonymous mappings
     * it placed back to back, so even a (hypothetically broken)
     * one-mmap-per-stack implementation would often show the same small
     * `grew` here, for the same reason. What actually distinguishes "a few
     * big slabs" from "one mapping per stack" is a structural count, not an
     * address-space one: how many DISTINCT slabs the allocator actually
     * used to satisfy these STRESS_COUNT requests. rt_stack_t.slab is
     * opaque (greenthread.h never defines struct rt_slab's contents to
     * callers) but it is still a comparable pointer, so this counts
     * distinct slabs without reaching into the allocator's internals. */
    {
        void **slab_ptrs = malloc(STRESS_COUNT * sizeof *slab_ptrs);
        CHECK(slab_ptrs != NULL, "test harness allocation for slab-pointer check");
        if (slab_ptrs != NULL) {
            for (int i = 0; i < STRESS_COUNT; i++) {
                slab_ptrs[i] = (void *)stacks[i].slab;
            }
            qsort(slab_ptrs, STRESS_COUNT, sizeof *slab_ptrs, ptrcmp);
            int distinct = STRESS_COUNT > 0 ? 1 : 0;
            for (int i = 1; i < STRESS_COUNT; i++) {
                if (slab_ptrs[i] != slab_ptrs[i - 1]) distinct++;
            }
            int expect = (STRESS_COUNT + RT_SLAB_STACKS - 1) / RT_SLAB_STACKS;
            CHECK(distinct == expect,
                  "STRESS_COUNT stacks must be packed into ceil(STRESS_COUNT / "
                  "RT_SLAB_STACKS) distinct slabs, not one slab per stack");
            free(slab_ptrs);
        }
    }

    /* Every stack's base must be unique and non-overlapping: sort the base
     * pointers and check consecutive gaps are each at least one stack. */
    void **bases = malloc(STRESS_COUNT * sizeof *bases);
    CHECK(bases != NULL, "test harness allocation for base-pointer check");
    if (bases != NULL) {
        for (int i = 0; i < STRESS_COUNT; i++) bases[i] = stacks[i].base;
        qsort(bases, STRESS_COUNT, sizeof *bases, ptrcmp);
        bool ok = true;
        for (int i = 1; i < STRESS_COUNT; i++) {
            if ((uintptr_t)bases[i] - (uintptr_t)bases[i - 1] < rt_stack_size()) {
                ok = false;
                break;
            }
        }
        CHECK(ok, "every allocated stack's address range must be distinct and non-overlapping");
        free(bases);
    }

    /* Each stack is genuinely its own, writable memory: stamp a per-index
     * pattern at both ends and read it back before freeing anything. */
    for (int i = 0; i < STRESS_COUNT; i++) {
        unsigned char *lo = (unsigned char *)stacks[i].base;
        unsigned char *hi = (unsigned char *)stacks[i].top - 1;
        *lo = (unsigned char)(i & 0xFF);
        *hi = (unsigned char)((i >> 8) & 0xFF);
    }
    bool pattern_ok = true;
    for (int i = 0; i < STRESS_COUNT; i++) {
        unsigned char *lo = (unsigned char *)stacks[i].base;
        unsigned char *hi = (unsigned char *)stacks[i].top - 1;
        if (*lo != (unsigned char)(i & 0xFF) || *hi != (unsigned char)((i >> 8) & 0xFF)) {
            pattern_ok = false;
            break;
        }
    }
    CHECK(pattern_ok, "each stack's memory must hold exactly what was written to it");

    for (int i = 0; i < STRESS_COUNT; i++) {
        rt_stack_free(&stacks[i]);
    }

    /* Re-allocating the same count again must reuse the freed stacks rather
     * than minting new slabs every time -- checked indirectly: if free()
     * did not work, this second pass would need three MORE slabs, which
     * would show up as another several-VMA jump on top of the first. */
    long vma_before_reuse = count_vmas();
    for (int i = 0; i < STRESS_COUNT; i++) {
        stacks[i] = rt_stack_alloc();
    }
    long vma_after_reuse = count_vmas();
    if (vma_before_reuse >= 0 && vma_after_reuse >= 0) {
        CHECK(vma_after_reuse - vma_before_reuse <= 2,
              "re-allocating the same count after freeing must reuse slabs, "
              "not map new ones");
    }
    for (int i = 0; i < STRESS_COUNT; i++) {
        rt_stack_free(&stacks[i]);
    }

    free(stacks);
}

/* ========================================================================
 * Test 4 -- the byte-per-thread state table (Part 4).
 * ====================================================================== */

static void test_state_table(void) {
    CHECK(rt_gtstate_get(0) == RT_GT_RUNNABLE, "a fresh id reads back Runnable");
    CHECK(rt_gtstate_get(777) == RT_GT_RUNNABLE,
          "an id past any previous growth still reads back Runnable");

    rt_gtstate_set(0, RT_GT_RUNNING);
    rt_gtstate_set(1, RT_GT_PARKED_IO);
    rt_gtstate_set(2, RT_GT_PARKED_CHAN);
    rt_gtstate_set(3, RT_GT_PARKED_TIMER);
    rt_gtstate_set(4, RT_GT_DEAD);
    /* Force table growth (initial capacity is 1024) well past it. */
    rt_gtstate_set(5000, RT_GT_RUNNING);

    CHECK(rt_gtstate_get(0) == RT_GT_RUNNING, "id 0 reads back what was set");
    CHECK(rt_gtstate_get(1) == RT_GT_PARKED_IO, "id 1 reads back what was set");
    CHECK(rt_gtstate_get(2) == RT_GT_PARKED_CHAN, "id 2 reads back what was set");
    CHECK(rt_gtstate_get(3) == RT_GT_PARKED_TIMER, "id 3 reads back what was set");
    CHECK(rt_gtstate_get(4) == RT_GT_DEAD, "id 4 reads back what was set");
    CHECK(rt_gtstate_get(5000) == RT_GT_RUNNING, "id past a growth boundary reads back what was set");
    /* Writing a far-away id must not disturb ids already set below it. */
    CHECK(rt_gtstate_get(0) == RT_GT_RUNNING, "growth must not disturb previously-set ids");
    CHECK(rt_gtstate_get(2500) == RT_GT_RUNNABLE,
          "an untouched id inside the grown range still reads back Runnable");
}

/* ========================================================================
 * Failure-mode sub-tests, run by the .sh harness as separate processes
 * because both are meant to abort (SIGABRT / exit 134).
 * ====================================================================== */

/* Exactly the agreed probe idiom (rt.h), reproduced by hand against a real
 * fiber stack. This is the strongest check available in Phase 1 of the
 * runtime side of the Part 3 contract -- see the file header for why a real
 * compiled .m31 program cannot exercise this yet. */
#define PROBE()                                                              \
    do {                                                                     \
        int __rt_probe_local;                                                \
        if ((uintptr_t)&__rt_probe_local < rt_stack_limit) {                 \
            rt_stack_probe_slow();                                           \
        }                                                                    \
    } while (0)

/* This recursion has no base case in the source -- the probe is what is
 * supposed to stop it, which is exactly the point of the test. gcc's
 * -Winfinite-recursion flags that honestly; silenced here rather than
 * worked around, since working around it would mean adding a fake base
 * case that could itself mask a real bug (the recursion never actually
 * reaching it). */
#if defined(__GNUC__)
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winfinite-recursion"
#endif
/* __attribute__((noinline)) alone is NOT enough at -O2, and this was found
 * the hard way: an earlier version of this function returned
 * `sum + overflow_recurse(depth + 1) + depth`, which is a provably
 * associative accumulation, and gcc -O2 -- without ever inlining anything,
 * `noinline` genuinely honoured -- rewrote the SELF-recursion into an
 * ordinary loop with an accumulator, because that transformation is valid
 * regardless of inlining. The result doesn't crash and doesn't trap: it
 * just never grows the real call stack at all, so the probe has nothing to
 * catch, and the test hangs forever instead of failing loudly (caught here
 * only because two such builds were still spinning at 99% CPU minutes
 * later). `noinline` stops a CALLER from absorbing this function; it says
 * nothing about the compiler rewriting the function's OWN control flow.
 *
 * The robust fix is to turn optimization off for this one function
 * entirely, regardless of the surrounding build's -O level, which is
 * exactly what greenthread_test.sh builds this file at (-O0 and -O2, both
 * compilers) to catch a regression like this again: `optimize("O0")` is
 * gcc's spelling, `optnone` is clang's. */
#if defined(__clang__)
__attribute__((noinline, optnone)) static long overflow_recurse(long depth) {
#elif defined(__GNUC__)
__attribute__((noinline, optimize("O0"))) static long overflow_recurse(long depth) {
#else
__attribute__((noinline)) static long overflow_recurse(long depth) {
#endif
    PROBE();
    /* Printed and flushed before anything else so the depth reached is on
     * stdout even though the process is about to be aborted -- abort() runs
     * no atexit handler and flushes nothing (same reason rt_trap flushes
     * output itself before calling it; see runtime/rt.c). */
    printf("depth %ld\n", depth);
    fflush(stdout);
    /* A real, sizable frame: large enough that the recursion overflows a
     * small test stack in a short, predictable number of calls, instead of
     * needing tens of thousands of frames to notice a bug either way. */
    volatile unsigned char padding[512];
    for (int i = 0; i < 512; i++) padding[i] = (unsigned char)(depth + i);
    long sum = padding[0];
    sum += overflow_recurse(depth + 1);
    return sum + depth;
}
#if defined(__GNUC__)
#pragma GCC diagnostic pop
#endif

typedef struct {
    rt_ctx_t *self;
    rt_ctx_t *main_ctx;
} overflow_arg_t;

static void overflow_entry(void *argp) {
    overflow_arg_t *arg = (overflow_arg_t *)argp;
    (void)arg;
    overflow_recurse(0);
    /* Not reached: overflow_recurse only returns by unwinding, and the
     * probe is expected to abort the process first. */
    rt_ctx_entry_returned();
}

/* `./greenthread_test overflow`: a fiber with a deliberately tiny usable
 * region (see the comment below on the limit) recurses until the probe
 * traps. Expected to abort -- run as its own process by the .sh harness. */
static int run_overflow_submode(void) {
    rt_ctx_t main_ctx;
    /* A real 64 KiB stack from the normal allocator, but rt_stack_limit is
     * set well ABOVE its true base -- reserving a real, mapped safety
     * margin below the limit. There is no guard page by design (Part 2), so
     * if the probe's comparison or placement were off by a little, the
     * recursion would run into mapped-but-"invalid" memory instead of
     * unmapped memory, and ASan (greenthread_test.sh's sanitized build)
     * would still catch the actual overflow as the stack-use-after-scope /
     * out-of-bounds write it would be -- this margin exists so a near-miss
     * is a visible ASan report instead of a lucky non-crash. */
    rt_stack_t stack = rt_stack_alloc();
    uintptr_t margin = 8192;
    uintptr_t limit = (uintptr_t)stack.base + margin;

    rt_ctx_t fctx;
    overflow_arg_t arg = { &fctx, &main_ctx };
    rt_ctx_make(&fctx, stack.base, rt_stack_size(), overflow_entry, &arg);

    rt_fiber_switch(&main_ctx, &fctx, limit);

    /* Only reached if the probe never fired at all -- a missed overflow,
     * the dangerous false negative this whole test exists to catch. */
    fprintf(stderr, "FAIL: recursion returned without the probe ever firing\n");
    return 1;
}

/* `./greenthread_test poison`: directly exercises the documented-but-
 * unimplemented poisoned branch of rt_stack_probe_slow (rt.h,
 * RT_STACK_LIMIT_POISON) without needing a scheduler to actually poison
 * anything -- confirms it traps cleanly rather than looping or corrupting
 * anything, which is all Phase 1 promises for that branch. */
static int run_poison_submode(void) {
    rt_stack_limit = RT_STACK_LIMIT_POISON;
    rt_stack_probe_slow();
    fprintf(stderr, "FAIL: rt_stack_probe_slow returned on a poisoned limit\n");
    return 1;
}

/* ========================================================================
 * Driver
 * ====================================================================== */

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "overflow") == 0) {
        return run_overflow_submode();
    }
    if (argc > 1 && strcmp(argv[1], "poison") == 0) {
        return run_poison_submode();
    }

    test_round_trip();
    test_interleaving_and_isolation();
    test_multi_slab_stress();
    test_state_table();

    printf("%d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
