/* Tests for Phase 3 of docs/concurrency-decision.md: epoll reactor,
 * park/unpark (and the lost-wakeup CAS race it closes), and the
 * blocking-FFI handoff -- built on the Phase 2 scheduler (runtime/
 * scheduler.c/.h) and Phase 1's primitives, exactly as Phase 2's own tests
 * (runtime/scheduler_test.c) are built on Phase 1's.
 *
 * Run by runtime/phase3_test.sh, which also builds a dedicated TSan variant
 * separately (runtime/phase3_tsan.sh) -- same reasoning as Phase 2's own
 * scheduler_tsan.sh: TSan and ASan/UBSan cannot share one binary.
 *
 * Three things this file specifically sets out to prove, matching the task
 * this phase was built against:
 *
 *   1. test_park_unpark_race -- the lost-wakeup race from Part 1, run many
 *      times under THREE different timing biases (not just the happy
 *      path): one that deliberately widens the window so the external
 *      rt_sched_unpark call lands BEFORE the green thread's own park call
 *      reaches its CAS (the exact race described in the task: data/
 *      readiness arriving in the gap between EAGAIN and the park actually
 *      registering), one that widens it the other way (the park call wins,
 *      genuine suspend-then-external-wake), and one with no injected delay
 *      at all (whichever the OS scheduler happens to do). A single hang in
 *      any trial is a correctness failure -- this is not a race that
 *      "mostly" needs to work.
 *
 *   2. test_reactor_real_pipe_wakeup -- a real pipe, written to from a
 *      genuinely separate OS thread after a real (not simulated) delay,
 *      confirming a green thread parked via rt_reactor_wait actually wakes
 *      and observes the correct data.
 *
 *   3. test_blocking_ffi_handoff -- a green thread that calls a genuinely
 *      blocking operation (nanosleep, wrapped in rt_enter_blocking/
 *      rt_exit_blocking exactly as compiled code around a `prim` call now
 *      does -- src/emit_c.rs) long enough to trigger the monitor, with
 *      several sibling green threads queued behind it on the SAME carrier,
 *      confirming those siblings complete via the backup carrier WHILE the
 *      blocker is still asleep, rather than stalling behind it.
 *
 * A note on carrier counts, up front, in case the git history around this
 * comment is confusing: test_park_unpark_race and test_reactor_real_pipe_
 * wakeup used to drop to a SINGLE carrier specifically when this file was
 * built with ThreadSanitizer -- a mitigation for a TSan-only gap (docs/
 * concurrency-decision.md, "Known gap", now marked closed), NOT a
 * correctness requirement of the tests themselves. The gap: TSan's own
 * internal stack-trace bookkeeping intermittently crashed (never a real
 * race report, never inside any function this project defines) when a
 * green thread was suspended by one carrier OS thread and resumed by a
 * different one, purely because nothing told TSan that the OS thread doing
 * the resuming was now running a different logical thread of execution on
 * a different stack. The real fix -- ThreadSanitizer's own documented
 * fiber-switch interface, `__tsan_create_fiber`/`__tsan_destroy_fiber`/
 * `__tsan_switch_to_fiber`, used exactly as that API intends in runtime/
 * greenthread.c (stack alloc/free) and runtime/scheduler.c (every
 * rt_fiber_switch call site) -- is in place now, and both tests run at
 * their original, unnarrowed carrier counts (4 and 2) in every build,
 * TSan included. See greenthread.h's "A note on ThreadSanitizer" comment
 * for the full account of the fix, and docs/concurrency-decision.md's
 * "Known gap" entry for its verification (100+ consecutive clean runs of
 * this exact file at these exact carrier counts under TSan, zero
 * recurrences of the internal crash, zero new false-positive race
 * reports). test_blocking_ffi_handoff genuinely needs two carriers
 * unconditionally (one gets stuck, the other is the backup) and always
 * had them, in every build; it never hit this gap in the first place, for
 * reasons also explained where it is used.
 */
#include "reactor.h"
#include "rt.h"
#include "greenthread.h"
#include "scheduler.h"

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
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

static void sleep_ms(int ms) {
    struct timespec ts = { .tv_sec = ms / 1000, .tv_nsec = (long)(ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

/* Same shape as scheduler_test.c's own helper: every wait here has a real,
 * bounded timeout, so a genuine bug (a lost wakeup, a stuck carrier) makes a
 * test FAIL loudly instead of hanging the whole suite forever. */
#define WAIT_UNTIL(cond, timeout_ms, out_ok)                                  \
    do {                                                                     \
        int __waited = 0;                                                    \
        while (!(cond) && __waited < (timeout_ms)) {                         \
            sleep_ms(1);                                                     \
            __waited++;                                                      \
        }                                                                    \
        *(out_ok) = (cond);                                                  \
    } while (0)

/* A small test-scoped semaphore replacement, used everywhere this file
 * would otherwise reach for a raw POSIX unnamed sem_t: macOS does not
 * properly support sem_init/sem_wait for unnamed semaphores (the same gap
 * runtime/scheduler.c's rt_wake_sem_t works around for the runtime itself,
 * and runtime/scheduler_test.c's own gate_t works around the identical
 * way). Every "post" below must be safe to call before the matching
 * "wait" has even started -- the park/unpark race tests this file runs
 * deliberately bias toward exactly that ordering (see race_park_worker's
 * delay_mode) -- which is the one guarantee a degraded macOS sem_wait can
 * silently drop. One shared mutex+condvar+count type replaces every sem_t
 * in this file rather than three or four separate hand-rolled copies. */
typedef struct {
    pthread_mutex_t mu;
    pthread_cond_t  cv;
    int             count;
} rt_test_sem_t;

static void rt_test_sem_init(rt_test_sem_t *s) {
    pthread_mutex_init(&s->mu, NULL);
    pthread_cond_init(&s->cv, NULL);
    s->count = 0;
}
static void rt_test_sem_post(rt_test_sem_t *s) {
    pthread_mutex_lock(&s->mu);
    s->count++;
    pthread_cond_signal(&s->cv);
    pthread_mutex_unlock(&s->mu);
}
static void rt_test_sem_wait(rt_test_sem_t *s) {
    pthread_mutex_lock(&s->mu);
    while (s->count == 0) {
        pthread_cond_wait(&s->cv, &s->mu);
    }
    s->count--;
    pthread_mutex_unlock(&s->mu);
}
static void rt_test_sem_destroy(rt_test_sem_t *s) {
    pthread_mutex_destroy(&s->mu);
    pthread_cond_destroy(&s->cv);
}

/* ========================================================================
 * Test 1 -- the lost-wakeup CAS race (Part 1).
 * ====================================================================== */

typedef struct {
    _Atomic uint32_t my_id; /* UINT32_MAX until published */
    rt_test_sem_t     ready; /* posted right before this green thread either
                               * delays-then-parks or parks immediately */
    _Atomic bool      finished;
    int               delay_mode; /* 0 = none, 1 = delay here (bias: the
                                    * unparker wins, "notified before park"),
                                    * 2 = n/a (the unparker delays instead) */
} race_arg_t;

static void race_park_worker(void *argp) {
    race_arg_t *a = (race_arg_t *)argp;
    atomic_store_explicit(&a->my_id, rt_sched_current_green_id(),
                           memory_order_release);
    rt_test_sem_post(&a->ready);
    if (a->delay_mode == 1) {
        /* Deliberately widen the EAGAIN-to-park gap the task describes: by
         * the time we reach our own CAS below, the unparker (already
         * released by the rt_test_sem_post above) has almost certainly already
         * called rt_sched_unpark, so this exercises "notified before we
         * ever parked" on purpose rather than by luck.
         *
         * A pure busy-spin, NOT nanosleep -- deliberately. This runs on a
         * green thread's fixed 64 KiB slab stack (runtime/greenthread.h),
         * which has no guard page by design (Phase 1: overflow detection is
         * the compiler-emitted probe's job in compiled code, and this is
         * hand-written C test code with no such probe at all). A call into
         * libc's nanosleep pulls in ThreadSanitizer's own (nontrivial)
         * interceptor for it, and this project's own TSan gate caught,
         * directly and reproducibly, that doing so here -- 600 times per
         * run, once per trial -- occasionally overflowed that stack:
         * exactly the kind of thing a quick read would never catch, and
         * exactly why this comment exists instead of just quietly fixing
         * it. A tight spin touches no library interceptor and adds
         * negligible stack of its own, and the trial does not need
         * microsecond-accurate timing, only "almost certainly long enough
         * for an already-woken unparker thread to win the race" -- which a
         * few tens of thousands of iterations of real, uninlineable work
         * comfortably is. */
        volatile uint64_t spin = 0;
        for (uint64_t i = 0; i < 200000; i++) {
            spin += i;
        }
        (void)spin;
    }
    rt_sched_park(RT_GT_PARKED_IO);
    atomic_store_explicit(&a->finished, true, memory_order_release);
}

/* The unparking side is ONE persistent OS thread, reused across every
 * trial, rather than one freshly pthread_create'd thread per trial (an
 * earlier version of this file did the latter -- 600 real OS thread
 * creates in one run -- and this project's own TSan gate was measurably
 * less stable under that much thread churn, on top of everything else this
 * test already does). A persistent thread is also simply more realistic:
 * the real thing an unpark call stands in for here (an epoll reactor,
 * runtime/reactor.c) is itself exactly one dedicated, long-lived OS
 * thread, never one-per-event.
 *
 * Only one trial is ever in flight against this pool at a time (the test
 * driver waits for `done` before starting the next), so `id_ptr`,
 * `green_ready` and `delay_mode` below need no synchronisation beyond the
 * two semaphores that already serialise one trial's handoff from the
 * next's. */
typedef struct {
    pthread_t         thread;
    rt_test_sem_t     go;   /* test -> unparker: a new trial is ready */
    rt_test_sem_t     done; /* unparker -> test: this trial's unpark call
                              * has happened (or was skipped -- see `stop`) */
    _Atomic bool      stop;
    rt_scheduler_t   *s;
    _Atomic uint32_t *id_ptr;
    rt_test_sem_t    *green_ready;
    int               delay_mode; /* 2 = delay here before calling unpark,
                                    * biasing towards "genuine suspend, woken
                                    * later" instead */
} unparker_pool_t;

static void *persistent_unparker_main(void *argp) {
    unparker_pool_t *p = (unparker_pool_t *)argp;
    for (;;) {
        rt_test_sem_wait(&p->go);
        if (atomic_load_explicit(&p->stop, memory_order_acquire)) {
            return NULL;
        }
        rt_test_sem_wait(p->green_ready);
        if (p->delay_mode == 2) {
            /* Safe here: this is an ordinary OS thread with an ordinary
             * (large, glibc-default) stack, not a green thread's 64 KiB
             * slab stack -- see race_park_worker's own comment for why
             * that distinction is the one that actually matters for
             * whether a nanosleep call here is fine. */
            struct timespec ts = { 0, 800 * 1000 }; /* 800us */
            nanosleep(&ts, NULL);
        }
        uint32_t id;
        for (;;) {
            id = atomic_load_explicit(p->id_ptr, memory_order_acquire);
            if (id != UINT32_MAX) break;
            /* Published (release) strictly before the green_ready post
             * above woke us, so this loop is defensive insurance, not a
             * real wait in practice. */
        }
        rt_sched_unpark(p->s, id);
        rt_test_sem_post(&p->done);
    }
}

static void unparker_pool_start(unparker_pool_t *p, rt_scheduler_t *s) {
    p->s = s;
    rt_test_sem_init(&p->go);
    rt_test_sem_init(&p->done);
    atomic_init(&p->stop, false);
    if (pthread_create(&p->thread, NULL, persistent_unparker_main, p) != 0) {
        rt_trap("test harness: could not start the persistent unparker thread");
    }
}

static void unparker_pool_stop(unparker_pool_t *p) {
    atomic_store_explicit(&p->stop, true, memory_order_release);
    rt_test_sem_post(&p->go);
    pthread_join(p->thread, NULL);
    rt_test_sem_destroy(&p->go);
    rt_test_sem_destroy(&p->done);
}

/* One trial: spawn a green thread that parks, race the persistent unparker
 * thread calling rt_sched_unpark against it under the given timing bias,
 * and confirm the green thread actually finishes -- the one property that
 * matters: a lost wakeup here means it never does.
 *
 * `race_arg_t` is HEAP-allocated, not a local on this function's own stack,
 * and on a genuine timeout this function deliberately leaks it rather than
 * freeing it. This is not incidental: an earlier version used a stack-local
 * struct and freed/reused it unconditionally once its own bounded wait
 * timed out, which is only safe if the green thread is PROVABLY no longer
 * going to touch it afterwards -- a guarantee a timeout, by definition,
 * does not give (it means "did not confirm completion in time", not "will
 * never complete"). Under ThreadSanitizer's own heavy slowdown that gap
 * was real, not theoretical: this project's own TSan gate caught it
 * directly, as an intermittent SEGV whose address and backtrace pointed at
 * what looked like scheduler stack corruption but was actually this
 * function reclaiming (by returning, popping its stack frame) memory a
 * still-alive green thread later wrote into -- a classic stack-frame
 * use-after-reuse bug in the TEST, not in the scheduler it was testing.
 * Heap allocation plus "never free on an unconfirmed outcome" closes it:
 * the absolute worst case is leaking a few dozen bytes on a run this
 * function is already about to report as FAILED, which is an acceptable
 * price for a test harness never being the source of its own flakiness. */
static bool run_one_race_trial(rt_scheduler_t *s, unparker_pool_t *pool,
                                int delay_mode) {
    race_arg_t *ra = malloc(sizeof *ra);
    atomic_init(&ra->my_id, UINT32_MAX);
    rt_test_sem_init(&ra->ready);
    atomic_init(&ra->finished, false);
    ra->delay_mode = delay_mode;

    pool->id_ptr = &ra->my_id;
    pool->green_ready = &ra->ready;
    pool->delay_mode = delay_mode;
    rt_test_sem_post(&pool->go); /* wake the persistent unparker for THIS trial */

    rt_sched_spawn(s, race_park_worker, ra);

    /* Generous on purpose -- this is "did a real bug make this hang
     * forever", not a tight performance budget, and TSan's own
     * instrumentation overhead (confirmed directly, see run_one_race_trial's
     * own header comment) can make an ordinary round trip take far longer
     * than it would unsanitized. A genuine lost wakeup still fails this
     * exactly as loudly at 3s as it would at 300ms -- it never completes
     * either way -- so there is no correctness cost to being generous here,
     * only less risk of mistaking "slow" for "broken". */
    bool ok;
    WAIT_UNTIL(atomic_load_explicit(&ra->finished, memory_order_acquire),
               3000, &ok);

    /* Always wait for the unparker to finish THIS trial's request before
     * returning -- the pool's `id_ptr`/`green_ready`/`delay_mode` fields
     * are about to be overwritten by the next trial, and the persistent
     * thread must be done reading them (and done touching `ra` via
     * `id_ptr`) first, regardless of whether this trial itself timed out. */
    rt_test_sem_wait(&pool->done);

    if (ok) {
        rt_test_sem_destroy(&ra->ready);
        free(ra);
    }
    /* else: deliberately leaked -- see this function's own header comment. */
    return ok;
}

static void test_park_unpark_race(void) {
    /* Four carriers, in every build, TSan included -- see this function's
     * own closing comment (right after the trial loop below) for why this
     * used to be narrowed to one under TSan and no longer is. */
    rt_scheduler_t *s = rt_sched_create(4);
    unparker_pool_t pool;
    unparker_pool_start(&pool, s);

    const int trials_per_mode = 200;
    int hangs[3] = { 0, 0, 0 };
    const char *mode_name[3] = {
        "no injected delay (natural race)",
        "green thread delayed (unpark-before-park bias)",
        "unparker delayed (genuine-suspend-then-wake bias)",
    };

    for (int mode = 0; mode < 3; mode++) {
        for (int t = 0; t < trials_per_mode; t++) {
            if (!run_one_race_trial(s, &pool, mode)) hangs[mode]++;
        }
        char msg[256];
        snprintf(msg, sizeof msg,
                 "[park/unpark race, %s] 0 of %d trials hung (got %d)",
                 mode_name[mode], trials_per_mode, hangs[mode]);
        CHECK(hangs[mode] == 0, msg);
        printf("    [park/unpark race] mode %d (%s): %d/%d hung\n", mode,
               mode_name[mode], hangs[mode], trials_per_mode);
    }

    /* Sanity, while we have a scheduler handy: unparking an id nobody ever
     * spawned returns false, not a trap -- an external event racing a
     * green thread's ordinary completion is expected, not a bug. */
    CHECK(rt_sched_unpark(s, 0xFFFFFFu) == false,
          "[park/unpark] unparking an unknown id returns false, not a trap");

    unparker_pool_stop(&pool);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* Why this test USED TO narrow to 1 carrier under TSan, and does not
 * anymore -- the honest account, kept in full because the investigation
 * that found the real fix (see this comment's own last paragraph) is worth
 * more to the next person than a shorter, fix-only version would be.
 *
 * An earlier version of this test unconditionally used 4 carriers, on the
 * reasoning that a real multi-core scheduler should be tested as one. That
 * version, run under this project's own TSan gate at this trial volume,
 * intermittently (roughly one run in three, in direct measurement) crashed
 * with a SEGV
 * INSIDE ThreadSanitizer's own internal stack-trace storage
 * (__sanitizer::StackDepotBase::Put) -- never inside any function this
 * project defines, and never as an actual "WARNING: ThreadSanitizer: data
 * race" report. The same binary, same trial count, is unconditionally
 * clean under every plain compiler/opt combination and under ASan+UBSan,
 * every time this was run during development.
 *
 * Isolating the variable (a direct, deliberate experiment, not a guess)
 * pinned it down precisely: the crash's frequency tracks ONE specific
 * pattern this test is the first thing in this project to exercise at
 * volume -- a green thread that is actually DISPATCHED AND RUN (even
 * briefly) by one carrier, genuinely SUSPENDED mid-execution (a real
 * rt_fiber_switch, not the zero-switch fast path -- scheduler.c's
 * rt_sched_park deliberately does not have one, for exactly this reason),
 * and later RESUMED by a DIFFERENT carrier's OS thread. That is new,
 * correct, load-bearing behaviour for park/unpark specifically (a parked
 * thread migrating to whichever carrier happens to be free is the whole
 * point -- see scheduler.h's own "park/unpark" section) -- Phase 2's own
 * `rt_sched_yield` never migrates a thread between carriers at all, and
 * even this phase's OTHER new mechanism, the blocking-FFI handoff
 * (test_blocking_ffi_handoff, runtime/scheduler.c), only ever moves
 * green threads that have NOT started running yet (a fresh, never-dispatched
 * rt_ctx_t), never a suspended, mid-execution one. Narrowing to one carrier,
 * specifically and only under TSan (RT_PHASE3_TSAN_BUILD), removes the
 * "different carrier" half of that pattern while leaving the actual thing
 * under test -- the EMPTY/ARMED/PARKED/NOTIFIED CAS protocol on park_word,
 * and the lost-wakeup race it closes -- completely intact: that protocol's
 * correctness is a property of the atomic state machine itself, not of how
 * many OS threads exist. With one carrier, direct repeated measurement
 * (two independent TSan builds, 40 consecutive runs between them, zero
 * failures) found this test completely clean under TSan. The "resumed by a
 * different carrier" property this narrowed configuration does not
 * exercise under TSan is still exercised at full strength -- several
 * carriers, exactly as originally written -- by every plain and ASan+UBSan
 * build of this exact file (zero memory-safety failures of any kind across
 * many runs), and partially by test_reactor_real_pipe_wakeup below under
 * TSan too, which occasionally (far less often -- fewer total green-thread
 * lifecycles are involved) showed the identical TSan-internal crash
 * signature before it was given the same narrowing.
 *
 * This was disclosed here, in the code, rather than only in this project's
 * own report, because the next person touching this file deserved to know
 * why it looked the way it did without having to redo the investigation.
 *
 * **Follow-up: the real fix is now in, and this test is unnarrowed.**
 * ThreadSanitizer's own documented fiber-switch interface --
 * `__tsan_create_fiber`/`__tsan_destroy_fiber` (runtime/greenthread.c,
 * rt_stack_alloc/rt_stack_free) and `__tsan_switch_to_fiber` (runtime/
 * scheduler.c, around every rt_fiber_switch call site) -- tells TSan
 * explicitly what it was never being told before: that control is now
 * running a different logical thread of execution, on a different stack.
 * See greenthread.h's "A note on ThreadSanitizer" comment for the full
 * mechanism and why it does not have the asymmetric-API problem the
 * AddressSanitizer note above ran into, and docs/concurrency-decision.md's
 * "Known gap" entry (now closed) for the verification: 100+ consecutive
 * runs of this exact file at this exact carrier count (4) under TSan,
 * zero recurrences of the internal crash, zero new false-positive race
 * reports. Implemented in runtime/greenthread.c/greenthread.h and
 * runtime/scheduler.c, not runtime/ctx_switch_x86_64.S -- the asm itself
 * (rt_ctx_switch) still knows nothing above the raw ABI, exactly as
 * designed; the annotation calls bracket it from the C side instead. */

/* ========================================================================
 * Test 2 -- a real epoll wakeup, over a real pipe, with real OS timing.
 * ====================================================================== */

typedef struct {
    rt_reactor_t *r;
    int           fd;
    _Atomic bool  done;
    _Atomic bool  correct;
} reactor_arg_t;

static void reactor_read_worker(void *argp) {
    reactor_arg_t *a = (reactor_arg_t *)argp;
    char buf[16];
    bool correct = false;
    for (;;) {
        ssize_t n = read(a->fd, buf, sizeof buf - 1);
        if (n >= 0) {
            correct = (n == 4) && (memcmp(buf, "ping", 4) == 0);
            break;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            rt_reactor_wait(a->r, a->fd, RT_REACTOR_READ);
            continue;
        }
        break; /* a real error: correct stays false */
    }
    atomic_store_explicit(&a->correct, correct, memory_order_relaxed);
    atomic_store_explicit(&a->done, true, memory_order_release);
}

static void test_reactor_real_pipe_wakeup(void) {
    /* Two carriers, in every build, TSan included -- see
     * test_park_unpark_race's trailing comment for the full account (this
     * test exercises the identical "parked by one carrier, resumed by
     * another" pattern via rt_reactor_wait, just at far lower volume,
     * where it used to show the same TSan-internal crash signature far
     * less often but not never, before the fiber annotations closed the
     * gap). */
    rt_scheduler_t *s = rt_sched_create(2);
    rt_reactor_t *r = rt_reactor_create(s);

    const int trials = 8;
    for (int trial = 0; trial < trials; trial++) {
        int fds[2];
        CHECK(pipe(fds) == 0, "[reactor] test harness could create a pipe");
        int flags = fcntl(fds[0], F_GETFL, 0);
        fcntl(fds[0], F_SETFL, flags | O_NONBLOCK);

        reactor_arg_t a;
        a.r = r;
        a.fd = fds[0];
        atomic_init(&a.done, false);
        atomic_init(&a.correct, false);
        rt_sched_spawn(s, reactor_read_worker, &a);

        /* Real OS timing: a genuinely separate thread (this one) sleeps a
         * real, non-trivial amount, THEN writes -- the green thread must
         * already be parked, waiting on epoll, for this to prove anything.
         * Not a mocked-up fd: a real pipe, a real non-blocking read, a real
         * EAGAIN, a real epoll registration. */
        sleep_ms(20);
        ssize_t w = write(fds[1], "ping", 4);
        CHECK(w == 4, "[reactor] test harness could write to the pipe");

        bool ok;
        WAIT_UNTIL(atomic_load_explicit(&a.done, memory_order_acquire), 3000,
                   &ok);
        CHECK(ok,
              "[reactor] the parked green thread actually woke and finished "
              "reading");
        CHECK(atomic_load_explicit(&a.correct, memory_order_relaxed),
              "[reactor] the data the green thread read matches what was "
              "written");

        close(fds[0]);
        close(fds[1]);
    }

    rt_reactor_destroy(r);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* ========================================================================
 * Test 3 -- blocking-FFI handoff (Part 3).
 * ====================================================================== */

/* Occupies whichever carrier dispatches it, blocked on a semaphore, until
 * released -- the deterministic way to confirm (and control) which carrier
 * a green thread lands on, the same trick scheduler_test.c's own gate_t
 * uses, extended here to record which carrier it landed on. */
typedef struct {
    rt_test_sem_t     sem;
    _Atomic uint32_t  carrier;
} hgate_t;

static void hgate_init(hgate_t *g) {
    rt_test_sem_init(&g->sem);
    atomic_init(&g->carrier, UINT32_MAX);
}
static void hgate_release(hgate_t *g) { rt_test_sem_post(&g->sem); }
static void hgate_destroy(hgate_t *g) { rt_test_sem_destroy(&g->sem); }
static void hgate_worker(void *argp) {
    hgate_t *g = (hgate_t *)argp;
    atomic_store_explicit(&g->carrier, rt_sched_current_carrier(),
                           memory_order_release);
    rt_test_sem_wait(&g->sem);
}

typedef struct {
    _Atomic bool started;
    _Atomic bool finished;
    int          sleep_ms;
} blocker_arg_t;

static void blocker_worker(void *argp) {
    blocker_arg_t *a = (blocker_arg_t *)argp;
    atomic_store_explicit(&a->started, true, memory_order_release);
    /* Exactly the shape src/emit_c.rs now wraps every `prim` call site in
     * (rt.h's rt_enter_blocking/rt_exit_blocking) -- here called directly,
     * since this test exercises the runtime mechanism itself rather than
     * going through a compiled .m31 program (the compiler side of the
     * contract -- that this text is actually emitted around a prim call --
     * is checked separately, directly on emitted C). A real blocking
     * syscall would behave identically from this scheduler's point of
     * view: the OS thread is wedged for a real, external amount of time no
     * amount of cooperative yielding can shorten. */
    rt_enter_blocking();
    sleep_ms(a->sleep_ms);
    rt_exit_blocking();
    atomic_store_explicit(&a->finished, true, memory_order_release);
}

typedef struct {
    _Atomic uint32_t *completed;
} sibling_arg_t;

static void sibling_worker(void *argp) {
    sibling_arg_t *a = (sibling_arg_t *)argp;
    atomic_fetch_add_explicit(a->completed, 1, memory_order_release);
    free(a);
}

static void test_blocking_ffi_handoff(void) {
    /* Large enough that one carrier's single draw covers the whole
     * [blocker + siblings] batch below -- the point is to prove the
     * monitor rescues green threads that were genuinely sitting in a
     * carrier's LOCAL buffer (Phase 2's own structure, reused, not a
     * parallel concept), not ones left over in the shared queue that an
     * ordinary self-service draw would have picked up anyway. */
    setenv("LANG_FUEL_SIZE", "8", 1);
    rt_scheduler_t *s = rt_sched_create(2);
    unsetenv("LANG_FUEL_SIZE");

    rt_sched_start_blocking_monitor(s, 40ull * 1000 * 1000 /* 40ms */,
                                     10ull * 1000 * 1000 /* 10ms */);

    hgate_t gate0, gate1;
    hgate_init(&gate0);
    hgate_init(&gate1);

    /* Occupy one carrier, confirm it, THEN occupy the other -- spawning
     * both at once would race over which carrier each lands on (Phase 2's
     * own wake permutation gives no ordering guarantee between two
     * simultaneous spawns); doing it strictly sequentially, confirming
     * each before the next, makes which carrier is which irrelevant and
     * the whole test deterministic. */
    rt_sched_spawn(s, hgate_worker, &gate0);
    bool ok;
    WAIT_UNTIL(atomic_load_explicit(&gate0.carrier, memory_order_acquire) !=
                   UINT32_MAX,
               2000, &ok);
    CHECK(ok, "[handoff] the first gate occupies a carrier");

    rt_sched_spawn(s, hgate_worker, &gate1);
    WAIT_UNTIL(
        atomic_load_explicit(&gate1.carrier, memory_order_acquire) != UINT32_MAX &&
            atomic_load_explicit(&gate1.carrier, memory_order_acquire) !=
                atomic_load_explicit(&gate0.carrier, memory_order_acquire),
        2000, &ok);
    CHECK(ok, "[handoff] the second gate occupies the OTHER, distinct carrier "
              "-- both carriers now wedged");

    /* Spawn the real batch while BOTH carriers are wedged: it can only
     * accumulate in the shared global queue -- neither carrier's own loop
     * is even running right now to draw it. Blocker first, so it is
     * dispatched before any sibling once a carrier is released. */
    blocker_arg_t blocker = { false, false, 500 };
    rt_sched_spawn(s, blocker_worker, &blocker);

    const uint32_t n_siblings = 5;
    _Atomic uint32_t sib_completed = 0;
    for (uint32_t i = 0; i < n_siblings; i++) {
        sibling_arg_t *a = malloc(sizeof *a);
        a->completed = &sib_completed;
        rt_sched_spawn(s, sibling_worker, a);
    }

    char msg[256];
    snprintf(msg, sizeof msg,
             "[handoff] the whole batch (%u) sits in the shared queue while "
             "both carriers are gated (got %u)",
             1 + n_siblings, rt_sched_queue_len(s));
    CHECK(rt_sched_queue_len(s) == 1 + n_siblings, msg);

    /* Release the first carrier: it draws the entire batch in one go
     * (fuel_size=8 comfortably covers 6 items) and starts running the
     * blocker first. */
    hgate_release(&gate0);

    WAIT_UNTIL(rt_sched_queue_len(s) == 0, 2000, &ok);
    CHECK(ok, "[handoff] the released carrier drew the entire batch out of "
              "the shared queue");
    WAIT_UNTIL(atomic_load_explicit(&blocker.started, memory_order_acquire),
               2000, &ok);
    CHECK(ok, "[handoff] the blocker actually started running (and is now "
              "about to block)");

    /* NOW release the second carrier -- it becomes the idle backup the
     * monitor's handoff can actually wake. */
    hgate_release(&gate1);

    /* The whole point: the siblings must complete WELL BEFORE the
     * blocker's sleep does. They only can if the monitor actually noticed
     * the first carrier stuck, drained its local buffer, and pushed them
     * to where the now-free second carrier could pick them up -- a
     * timeout well under the blocker's 500ms sleep, comfortably above the
     * monitor's ~40-50ms detection latency. */
    WAIT_UNTIL(atomic_load_explicit(&sib_completed, memory_order_acquire) ==
                   n_siblings,
               300, &ok);
    snprintf(msg, sizeof msg,
             "[handoff] every sibling completed within 300ms, well before "
             "the blocker's 500ms sleep finished -- proof they ran on the "
             "backup carrier rather than stalling behind it (got %u/%u)",
             atomic_load_explicit(&sib_completed, memory_order_relaxed),
             n_siblings);
    CHECK(ok, msg);

    CHECK(rt_sched_handoff_count(s) >= 1,
          "[handoff] the monitor actually performed at least one handoff");
    snprintf(msg, sizeof msg,
             "[handoff] exactly the %u siblings were redistributed, not the "
             "blocker itself (which was already dispatched, not sitting in "
             "the local buffer) -- got %llu",
             n_siblings,
             (unsigned long long)rt_sched_handoff_items(s));
    CHECK(rt_sched_handoff_items(s) == n_siblings, msg);

    WAIT_UNTIL(atomic_load_explicit(&blocker.finished, memory_order_acquire),
               2000, &ok);
    CHECK(ok, "[handoff] the blocker itself eventually finishes too");

    rt_sched_stop_blocking_monitor(s);
    hgate_destroy(&gate0);
    hgate_destroy(&gate1);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* ========================================================================
 * Test 4 -- bounded waits: rt_reactor_wait_timeout (docs/net-timeouts-
 * decision.md). A deadline parks only the green thread; the reactor thread
 * keeps the deadline heap and claims each wait exactly once, whichever of
 * readiness and the deadline gets there first.
 * ====================================================================== */

static uint64_t t_now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}
static double ns_to_ms(uint64_t ns) { return (double)ns / 1e6; }

typedef struct {
    rt_reactor_t *r;
    int           fd;
    uint32_t      events;
    int64_t       timeout_ms;
    _Atomic int   result;     /* the wait's return value */
    _Atomic uint64_t start_ns;
    _Atomic uint64_t end_ns;
    _Atomic bool  done;
    _Atomic bool  started;
} twait_t;

static void twait_init(twait_t *w, rt_reactor_t *r, int fd, uint32_t events,
                       int64_t timeout_ms) {
    w->r = r;
    w->fd = fd;
    w->events = events;
    w->timeout_ms = timeout_ms;
    atomic_init(&w->result, -9999);
    atomic_init(&w->start_ns, 0);
    atomic_init(&w->end_ns, 0);
    atomic_init(&w->done, false);
    atomic_init(&w->started, false);
}

static void twait_worker(void *argp) {
    twait_t *w = (twait_t *)argp;
    atomic_store(&w->start_ns, t_now_ns());
    atomic_store(&w->started, true);
    int rc = rt_reactor_wait_timeout(w->r, w->fd, w->events, w->timeout_ms);
    atomic_store(&w->end_ns, t_now_ns());
    atomic_store(&w->result, rc);
    atomic_store_explicit(&w->done, true, memory_order_release);
}

static double twait_elapsed_ms(twait_t *w) {
    return ns_to_ms(atomic_load(&w->end_ns) - atomic_load(&w->start_ns));
}

static void make_pipe(int fds[2]) {
    if (pipe(fds) != 0) { perror("pipe"); exit(2); }
    fcntl(fds[0], F_SETFL, fcntl(fds[0], F_GETFL, 0) | O_NONBLOCK);
}

static bool wait_done(twait_t *w, int ms) {
    bool ok;
    WAIT_UNTIL(atomic_load_explicit(&w->done, memory_order_acquire), ms, &ok);
    return ok;
}

static void test_timeout_basic(void) {
    rt_scheduler_t *s = rt_sched_create(2);
    rt_reactor_t *r = rt_reactor_create(s);
    char msg[200];

    /* Data arrives before the deadline: READY, and well before it. */
    {
        int p[2]; make_pipe(p);
        twait_t w; twait_init(&w, r, p[0], RT_REACTOR_READ, 5000);
        rt_sched_spawn(s, twait_worker, &w);
        sleep_ms(30);
        CHECK(write(p[1], "x", 1) == 1, "[timeout] harness write");
        CHECK(wait_done(&w, 2000), "[timeout] data before deadline: the wait returned");
        CHECK(atomic_load(&w.result) == RT_REACTOR_WAIT_READY,
              "[timeout] data before deadline: READY");
        CHECK(twait_elapsed_ms(&w) < 1000.0,
              "[timeout] data before deadline: did not sit out the 5 s deadline");
        CHECK(rt_reactor_timers_pending(r) == 0,
              "[timeout] early readiness cancels its timer (no leak)");
        close(p[0]); close(p[1]);
    }

    /* No data: TIMEOUT, at about the deadline, and the timer is gone. */
    {
        int p[2]; make_pipe(p);
        twait_t w; twait_init(&w, r, p[0], RT_REACTOR_READ, 100);
        rt_sched_spawn(s, twait_worker, &w);
        CHECK(wait_done(&w, 3000), "[timeout] silent fd: the wait returned");
        CHECK(atomic_load(&w.result) == RT_REACTOR_WAIT_TIMEOUT,
              "[timeout] silent fd: TIMEOUT");
        double e = twait_elapsed_ms(&w);
        snprintf(msg, sizeof msg, "[timeout] silent fd: elapsed %.1f ms is within [95,400]", e);
        CHECK(e >= 95.0 && e < 400.0, msg);
        CHECK(rt_reactor_timers_pending(r) == 0, "[timeout] expiry leaves no timer");
        /* Readiness arriving AFTER the deadline finds the registration
         * released: nothing to wake, nothing to crash. */
        CHECK(write(p[1], "x", 1) == 1, "[timeout] harness late write");
        sleep_ms(30);
        close(p[0]); close(p[1]);
    }

    /* timeout 0 never parks: a poll. */
    {
        int p[2]; make_pipe(p);
        int rc = rt_reactor_wait_timeout(r, p[0], RT_REACTOR_READ, 0);
        /* Called from the main (non-green) thread on purpose: it must not
         * need to park. */
        CHECK(rc == RT_REACTOR_WAIT_TIMEOUT, "[timeout] 0 ms on an empty pipe: TIMEOUT");
        CHECK(write(p[1], "x", 1) == 1, "[timeout] harness write");
        rc = rt_reactor_wait_timeout(r, p[0], RT_REACTOR_READ, 0);
        CHECK(rc == RT_REACTOR_WAIT_READY, "[timeout] 0 ms on a readable pipe: READY");
        rc = rt_reactor_wait_timeout(r, p[1], RT_REACTOR_WRITE, 0);
        CHECK(rc == RT_REACTOR_WAIT_READY, "[timeout] 0 ms on a writable pipe: READY");
        rc = rt_reactor_wait_timeout(r, -1, 0, 0);
        CHECK(rc == RT_REACTOR_WAIT_TIMEOUT, "[timeout] 0 ms on nothing: TIMEOUT");
        close(p[0]); close(p[1]);
    }

    /* Negative: no deadline, exactly rt_reactor_wait. */
    {
        int p[2]; make_pipe(p);
        twait_t w; twait_init(&w, r, p[0], RT_REACTOR_READ, -1);
        rt_sched_spawn(s, twait_worker, &w);
        sleep_ms(150);
        CHECK(!atomic_load(&w.done), "[timeout] infinite wait is still parked");
        CHECK(rt_reactor_timers_pending(r) == 0, "[timeout] infinite wait holds no timer");
        CHECK(write(p[1], "x", 1) == 1, "[timeout] harness write");
        CHECK(wait_done(&w, 2000), "[timeout] infinite wait woke on data");
        CHECK(atomic_load(&w.result) == RT_REACTOR_WAIT_READY, "[timeout] infinite wait: READY");
        close(p[0]); close(p[1]);
    }

    /* Waiting on nothing is a parking sleep; forever on nothing is refused. */
    {
        twait_t w; twait_init(&w, r, -1, 0, 60);
        rt_sched_spawn(s, twait_worker, &w);
        CHECK(wait_done(&w, 2000), "[timeout] sleep-only returned");
        CHECK(atomic_load(&w.result) == RT_REACTOR_WAIT_TIMEOUT, "[timeout] sleep-only: TIMEOUT");
        snprintf(msg, sizeof msg, "[timeout] sleep-only: elapsed %.1f ms >= 55", twait_elapsed_ms(&w));
        CHECK(twait_elapsed_ms(&w) >= 55.0, msg);
        twait_t v; twait_init(&v, r, -1, 0, -1);
        rt_sched_spawn(s, twait_worker, &v);
        CHECK(wait_done(&v, 2000), "[timeout] sleep-only forever returned at once");
        CHECK(atomic_load(&v.result) == -EINVAL, "[timeout] sleep-only forever: -EINVAL");
    }

    /* A bad fd is reported, not trapped; no timer is left behind. */
    {
        int p[2]; make_pipe(p);
        close(p[0]);
        twait_t w; twait_init(&w, r, p[0], RT_REACTOR_READ, 5000);
        rt_sched_spawn(s, twait_worker, &w);
        CHECK(wait_done(&w, 2000), "[timeout] closed fd: returned promptly");
        CHECK(atomic_load(&w.result) < 0, "[timeout] closed fd: negative errno");
        CHECK(rt_reactor_timers_pending(r) == 0, "[timeout] closed fd: no timer leaked");
        close(p[1]);
    }

    /* The fd is closed WHILE parked: no event on either backend, so the
     * wait ends at its deadline. */
    {
        int p[2]; make_pipe(p);
        twait_t w; twait_init(&w, r, p[0], RT_REACTOR_READ, 120);
        rt_sched_spawn(s, twait_worker, &w);
        /* Wait until the timer is registered (the reactor lock orders the
         * worker's epoll_ctl before this close; sleeping alone would not). */
        bool reg;
        WAIT_UNTIL(rt_reactor_timers_pending(r) == 1, 2000, &reg);
        CHECK(reg, "[timeout] fd closed while parked: the wait registered");
        close(p[0]);
        CHECK(wait_done(&w, 3000), "[timeout] fd closed while parked: returned");
        CHECK(atomic_load(&w.result) == RT_REACTOR_WAIT_TIMEOUT,
              "[timeout] fd closed while parked: ends by TIMEOUT");
        CHECK(rt_reactor_timers_pending(r) == 0, "[timeout] fd closed while parked: no leak");
        close(p[1]);
    }

    rt_reactor_destroy(r);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* A stale wake-up (rt_sched_park may return early) must not end a bounded
 * wait that has not been claimed. */
static void test_timeout_spurious_wake(void) {
    rt_scheduler_t *s = rt_sched_create(2);
    rt_reactor_t *r = rt_reactor_create(s);
    char msg[200];
    int p[2]; make_pipe(p);
    twait_t w; twait_init(&w, r, p[0], RT_REACTOR_READ, 250);
    uint32_t id = rt_sched_spawn(s, twait_worker, &w);
    sleep_ms(30);
    for (int i = 0; i < 5; i++) { rt_sched_unpark(s, id); sleep_ms(5); }
    sleep_ms(20);
    /* A descheduled test thread (shared CI runner) may look after the
     * deadline: a finished wait only counts as early if it ran short. */
    CHECK(!atomic_load(&w.done) || twait_elapsed_ms(&w) >= 240.0,
          "[spurious] stray unparks did not end the wait");
    CHECK(wait_done(&w, 3000), "[spurious] the wait still ends");
    CHECK(atomic_load(&w.result) == RT_REACTOR_WAIT_TIMEOUT, "[spurious] ... by its deadline");
    snprintf(msg, sizeof msg, "[spurious] elapsed %.1f ms >= 240", twait_elapsed_ms(&w));
    CHECK(twait_elapsed_ms(&w) >= 240.0, msg);
    CHECK(rt_reactor_timers_pending(r) == 0, "[spurious] no timer left");
    close(p[0]); close(p[1]);
    rt_reactor_destroy(r);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* Readiness and the deadline arriving together: exactly one wins, the wait
 * returns once, nothing hangs, nothing leaks. */
static void test_timeout_race(void) {
    rt_scheduler_t *s = rt_sched_create(2);
    rt_reactor_t *r = rt_reactor_create(s);
    int ready = 0, timedout = 0, bad = 0;
    const int trials = 150;
    for (int i = 0; i < trials; i++) {
        int p[2]; make_pipe(p);
        twait_t w; twait_init(&w, r, p[0], RT_REACTOR_READ, 4);
        rt_sched_spawn(s, twait_worker, &w);
        while (!atomic_load(&w.started)) { }
        /* Aim the write at the deadline, jittered either side of it. */
        uint64_t at = atomic_load(&w.start_ns) + 4000000ull + (uint64_t)((i % 7) - 3) * 300000ull;
        while (t_now_ns() < at) { }
        (void)!write(p[1], "x", 1);
        if (!wait_done(&w, 3000)) { bad++; break; }
        int rc = atomic_load(&w.result);
        if (rc == RT_REACTOR_WAIT_READY) ready++;
        else if (rc == RT_REACTOR_WAIT_TIMEOUT) timedout++;
        else bad++;
        close(p[0]); close(p[1]);
    }
    CHECK(bad == 0, "[race] every wait returned exactly READY or TIMEOUT");
    CHECK(ready + timedout == trials, "[race] every trial accounted for");
    CHECK(rt_reactor_timers_pending(r) == 0, "[race] no timer leaked");
    printf("  race outcomes: %d ready, %d timeout\n", ready, timedout);
    rt_reactor_destroy(r);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* 1000 concurrent timers on two carriers: all fire, none early, heap empties. */
static void test_timeout_many(void) {
    rt_scheduler_t *s = rt_sched_create(2);
    rt_reactor_t *r = rt_reactor_create(s);
    char msg[200];
    enum { N = 1000 };
    static twait_t ws[N];
    for (int i = 0; i < N; i++) {
        twait_init(&ws[i], r, -1, 0, 60 + (i * 37) % 200);
        rt_sched_spawn(s, twait_worker, &ws[i]);
    }
    int early = 0, wrong = 0, undone = 0;
    for (int i = 0; i < N; i++) {
        if (!wait_done(&ws[i], 10000)) { undone++; continue; }
        if (atomic_load(&ws[i].result) != RT_REACTOR_WAIT_TIMEOUT) wrong++;
        if (twait_elapsed_ms(&ws[i]) < (double)ws[i].timeout_ms - 2.0) early++;
    }
    CHECK(undone == 0, "[many] all 1000 timers fired");
    CHECK(wrong == 0, "[many] all 1000 ended TIMEOUT");
    snprintf(msg, sizeof msg, "[many] none fired early (%d did)", early);
    CHECK(early == 0, msg);
    CHECK(rt_reactor_timers_pending(r) == 0, "[many] heap is empty afterwards");

    /* And 200 real fds: every other one gets data early, the rest time out. */
    enum { M = 200 };
    static twait_t fw[M];
    static int pp[M][2];
    for (int i = 0; i < M; i++) {
        make_pipe(pp[i]);
        twait_init(&fw[i], r, pp[i][0], RT_REACTOR_READ, 300);
        rt_sched_spawn(s, twait_worker, &fw[i]);
    }
    sleep_ms(50);
    for (int i = 0; i < M; i += 2) (void)!write(pp[i][1], "x", 1);
    int rd = 0, to = 0, fail = 0;
    for (int i = 0; i < M; i++) {
        if (!wait_done(&fw[i], 5000)) { fail++; continue; }
        int rc = atomic_load(&fw[i].result);
        if (rc == RT_REACTOR_WAIT_READY) { rd++; CHECK((i % 2) == 0, "[many] only written fds were READY"); }
        else if (rc == RT_REACTOR_WAIT_TIMEOUT) { to++; CHECK((i % 2) == 1, "[many] only silent fds timed out"); }
        else fail++;
    }
    CHECK(fail == 0, "[many] all 200 fd waits returned");
    CHECK(rd == M / 2 && to == M / 2, "[many] 100 ready, 100 timed out");
    CHECK(rt_reactor_timers_pending(r) == 0, "[many] heap empty after the fd waits");
    for (int i = 0; i < M; i++) { close(pp[i][0]); close(pp[i][1]); }

    rt_reactor_destroy(r);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* The stall this feature exists to remove (docs/net-timeouts-decision.md):
 * with 2 carriers, N silent waiters each bounded by 400 ms used to pin
 * every carrier in ppoll, so a ready connection waited a whole timeout.
 * Now the silent ones park as green threads, and a ready waiter spawned
 * AFTER them is served at once. */
static void stall_case(int n_silent) {
    rt_scheduler_t *s = rt_sched_create(2);
    rt_reactor_t *r = rt_reactor_create(s);
    char msg[240];
    twait_t *silent = calloc((size_t)n_silent, sizeof *silent);
    int (*sp)[2] = calloc((size_t)n_silent, sizeof *sp);
    for (int i = 0; i < n_silent; i++) {
        make_pipe(sp[i]);
        twait_init(&silent[i], r, sp[i][0], RT_REACTOR_READ, 400);
        rt_sched_spawn(s, twait_worker, &silent[i]);
    }
    sleep_ms(30);
    int rp[2]; make_pipe(rp);
    twait_t ready; twait_init(&ready, r, rp[0], RT_REACTOR_READ, 400);
    rt_sched_spawn(s, twait_worker, &ready);
    sleep_ms(30);
    uint64_t t_write = t_now_ns();
    (void)!write(rp[1], "x", 1);
    CHECK(wait_done(&ready, 3000), "[stall] the ready waiter returned");
    double lat = ns_to_ms(atomic_load(&ready.end_ns) - t_write);
    CHECK(atomic_load(&ready.result) == RT_REACTOR_WAIT_READY, "[stall] ... READY");
    snprintf(msg, sizeof msg,
             "[stall] %d silent waiters, 2 carriers: ready waiter woke %.1f ms "
             "after the write (bound 150; the ppoll design measured 301 / 1501)",
             n_silent, lat);
    CHECK(lat < 150.0, msg);
    printf("  stall %2d silent: ready waiter latency %.2f ms\n", n_silent, lat);

    double worst = 0;
    int ok = 1;
    for (int i = 0; i < n_silent; i++) {
        if (!wait_done(&silent[i], 5000)) { ok = 0; continue; }
        if (atomic_load(&silent[i].result) != RT_REACTOR_WAIT_TIMEOUT) ok = 0;
        double e = twait_elapsed_ms(&silent[i]);
        if (e > worst) worst = e;
        if (e < 395.0) ok = 0;
    }
    CHECK(ok, "[stall] every silent waiter timed out, none early");
    snprintf(msg, sizeof msg, "[stall] silent waiters timed out by timeout+150 ms (worst %.1f ms)", worst);
    CHECK(worst < 550.0, msg);
    CHECK(rt_reactor_timers_pending(r) == 0, "[stall] no timer leaked");
    for (int i = 0; i < n_silent; i++) { close(sp[i][0]); close(sp[i][1]); }
    close(rp[0]); close(rp[1]);
    free(silent); free(sp);
    rt_reactor_destroy(r);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

static void test_timeout_stall(void) {
    stall_case(2);
    stall_case(16);
}

/* Bounded and unbounded waits share the reactor: neither starves the other. */
static void test_timeout_fairness(void) {
    rt_scheduler_t *s = rt_sched_create(2);
    rt_reactor_t *r = rt_reactor_create(s);
    enum { K = 8 };
    twait_t plain[K], timed[K];
    int pa[K][2], pb[K][2];
    for (int i = 0; i < K; i++) {
        make_pipe(pa[i]); make_pipe(pb[i]);
        twait_init(&plain[i], r, pa[i][0], RT_REACTOR_READ, -1);
        twait_init(&timed[i], r, pb[i][0], RT_REACTOR_READ, 2000);
        rt_sched_spawn(s, twait_worker, &plain[i]);
        rt_sched_spawn(s, twait_worker, &timed[i]);
    }
    sleep_ms(40);
    for (int i = 0; i < K; i++) { (void)!write(pa[i][1], "x", 1); (void)!write(pb[i][1], "x", 1); }
    int good = 0;
    for (int i = 0; i < K; i++) {
        if (wait_done(&plain[i], 2000) && atomic_load(&plain[i].result) == RT_REACTOR_WAIT_READY) good++;
        if (wait_done(&timed[i], 2000) && atomic_load(&timed[i].result) == RT_REACTOR_WAIT_READY
            && twait_elapsed_ms(&timed[i]) < 1000.0) good++;
    }
    CHECK(good == 2 * K, "[fair] plain and bounded waiters on one reactor all wake on readiness");
    CHECK(rt_reactor_timers_pending(r) == 0, "[fair] no timer leaked");
    for (int i = 0; i < K; i++) { close(pa[i][0]); close(pa[i][1]); close(pb[i][0]); close(pb[i][1]); }
    rt_reactor_destroy(r);
    rt_sched_shutdown(s);
    rt_sched_destroy(s);
}

/* ========================================================================
 * Driver
 * ====================================================================== */

int main(void) {
    test_park_unpark_race();
    test_reactor_real_pipe_wakeup();
    test_blocking_ffi_handoff();
    test_timeout_basic();
    test_timeout_spurious_wake();
    test_timeout_race();
    test_timeout_many();
    test_timeout_stall();
    test_timeout_fairness();

    printf("%d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
