/* Tests for Phase 2 of docs/concurrency-decision.md: the green-thread
 * scheduler built on Phase 1's primitives (runtime/scheduler.c/.h). Run by
 * runtime/scheduler_test.sh, which also runs a dedicated ThreadSanitizer
 * build separately (see that file and the report this work produced for
 * why TSan needs its own build rather than piggybacking on the ASan+UBSan
 * one runtime/greenthread_test.sh uses).
 *
 * This creates and runs REAL green threads end to end through the real
 * scheduler, with real OS-thread carriers actually running concurrently --
 * not a single-threaded stand-in -- the same spirit as
 * runtime/greenthread_test.c testing Phase 1's primitives directly rather
 * than through a compiled .m31 program (which cannot reach this scheduler
 * at all: nothing wires `spawn` to it in this phase, by design).
 */
#include "rt.h"
#include "greenthread.h"
#include "scheduler.h"

#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

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

/* Generic "poll until true or give up" helper -- every wait in this file
 * has a real, bounded timeout, so a real bug (a lost thread, a missed
 * wake) makes a test FAIL loudly instead of hanging the suite forever. */
#define WAIT_UNTIL(cond, timeout_ms, out_ok)                                  \
    do {                                                                     \
        int __waited = 0;                                                    \
        while (!(cond) && __waited < (timeout_ms)) {                         \
            sleep_ms(1);                                                     \
            __waited++;                                                      \
        }                                                                    \
        *(out_ok) = (cond);                                                  \
    } while (0)

/* ========================================================================
 * Shared small worker bodies used by several tests below.
 * ====================================================================== */

/* A gate: occupies whichever carrier dispatches it, blocked, until the test
 * releases it -- the only way to deterministically force backlog to
 * accumulate in the shared queue for the fuel_size / backpressure tests
 * below, given carriers start consuming the instant anything is pushed.
 *
 * NOT a raw POSIX unnamed sem_t: macOS does not properly support
 * sem_init/sem_wait for unnamed semaphores (the same gap
 * runtime/scheduler.c's rt_wake_sem_t, in that file, was built to work
 * around for the runtime itself -- see its own comment there). This
 * matters here specifically because gate_release() must be safe to call
 * BEFORE gate_worker() reaches its wait (the test releases the gate from
 * the main thread without synchronizing on the worker having started) --
 * exactly the "post remembered even if no one is waiting yet" guarantee a
 * degraded macOS sem_wait can silently drop, which would make the gate
 * never actually block and the backlog drain to 0 instead of accumulating,
 * precisely what a real macOS CI run of this file showed. A small local
 * mutex+condvar+count mirrors that same fix, scoped to this test file. */
typedef struct {
    pthread_mutex_t mu;
    pthread_cond_t  cv;
    int             count;
} gate_t;

static void gate_init(gate_t *g) {
    pthread_mutex_init(&g->mu, NULL);
    pthread_cond_init(&g->cv, NULL);
    g->count = 0;
}
static void gate_release(gate_t *g) {
    pthread_mutex_lock(&g->mu);
    g->count++;
    pthread_cond_signal(&g->cv);
    pthread_mutex_unlock(&g->mu);
}
static void gate_destroy(gate_t *g) {
    pthread_mutex_destroy(&g->mu);
    pthread_cond_destroy(&g->cv);
}
static void gate_worker(void *argp) {
    gate_t *g = (gate_t *)argp;
    pthread_mutex_lock(&g->mu);
    while (g->count == 0) {
        pthread_cond_wait(&g->cv, &g->mu);
    }
    g->count--;
    pthread_mutex_unlock(&g->mu);
}

typedef struct { _Atomic uint32_t *completed; } noop_arg_t;
static void noop_worker(void *argp) {
    noop_arg_t *a = (noop_arg_t *)argp;
    atomic_fetch_add_explicit(a->completed, 1, memory_order_relaxed);
    free(a);
}

static bool wait_dispatched_at_least(rt_scheduler_t *s, uint32_t carrier,
                                      uint64_t n, int timeout_ms) {
    bool ok;
    WAIT_UNTIL(rt_sched_carrier_dispatched(s, carrier) >= n, timeout_ms, &ok);
    return ok;
}

static bool wait_completed_at_least(rt_scheduler_t *s, uint64_t n, int timeout_ms) {
    bool ok;
    WAIT_UNTIL(rt_sched_completed(s) >= n, timeout_ms, &ok);
    return ok;
}

/* ========================================================================
 * Test 1 -- completion invariant: spawn N, confirm all N complete exactly
 * once, no duplicates, none lost. Run under light and heavy load, and with
 * an explicit carrier count > 1 so this is genuinely concurrent carriers,
 * not a single-threaded stand-in.
 * ====================================================================== */

typedef struct {
    uint32_t          idx;
    atomic_bool      *ran_once;
    _Atomic uint32_t *completed;
    _Atomic uint32_t *duplicates;
} completion_arg_t;

static void completion_worker(void *argp) {
    completion_arg_t *a = (completion_arg_t *)argp;
    bool was = atomic_exchange_explicit(&a->ran_once[a->idx], true,
                                         memory_order_relaxed);
    if (was) {
        atomic_fetch_add_explicit(a->duplicates, 1, memory_order_relaxed);
    }
    /* A little real work, and one yield, so this exercises run-to-
     * completion AND the yield path in the same test. */
    volatile uint64_t acc = 0;
    for (int i = 0; i < 256; i++) acc += (uint64_t)i * (uint64_t)a->idx;
    rt_sched_yield();
    acc += 1;
    (void)acc;
    atomic_fetch_add_explicit(a->completed, 1, memory_order_relaxed);
    free(a);
}

static void run_completion_test(const char *label, uint32_t n_carriers,
                                 uint32_t n_workers) {
    rt_scheduler_t *s = rt_sched_create(n_carriers);

    atomic_bool *ran_once = calloc(n_workers, sizeof *ran_once);
    CHECK(ran_once != NULL, "test harness allocation for ran_once");
    _Atomic uint32_t completed = 0;
    _Atomic uint32_t duplicates = 0;

    for (uint32_t i = 0; i < n_workers; i++) {
        completion_arg_t *a = malloc(sizeof *a);
        a->idx = i;
        a->ran_once = ran_once;
        a->completed = &completed;
        a->duplicates = &duplicates;
        rt_sched_spawn(s, completion_worker, a);
    }

    bool ok = wait_completed_at_least(s, n_workers, 60000);
    char msg[128];
    snprintf(msg, sizeof msg,
             "[%s] all %u spawned green threads must complete (got %u)",
             label, n_workers, atomic_load_explicit(&completed, memory_order_relaxed));
    CHECK(ok, msg);
    CHECK(atomic_load_explicit(&completed, memory_order_relaxed) == n_workers,
          "[completion] own counter equals N exactly");
    CHECK(rt_sched_completed(s) == n_workers,
          "[completion] scheduler's own completed counter matches N");
    CHECK(rt_sched_spawned(s) == n_workers,
          "[completion] scheduler's own spawned counter matches N");
    CHECK(atomic_load_explicit(&duplicates, memory_order_relaxed) == 0,
          "[completion] no green thread was ever dispatched twice");

    rt_sched_shutdown(s);
    rt_sched_destroy(s);
    free(ran_once);
}

static void test_completion_invariant(void) {
    run_completion_test("light load, 4 carriers", 4, 200);
    run_completion_test("heavy load, 8 carriers", 8, 8000);
    run_completion_test("auto-detected carrier count", 0, 1000);
}

/* ========================================================================
 * Test 2 -- no stealing, structurally: a green thread that yields several
 * times must be resumed on the SAME carrier every single time, never any
 * other -- proof, from outside the implementation, that nothing ever
 * migrates a green thread between carriers (which is what "no stealing"
 * means operationally here; see also scheduler.c's own code-review comment
 * on rt_carrier_t for the static half of this claim). Also confirms more
 * than one carrier is actually doing real work, not just carrier 0.
 * ====================================================================== */

#define PIN_YIELDS 5

typedef struct {
    uint32_t          idx;
    uint32_t         *first_carrier;
    bool             *consistent;
    _Atomic uint32_t *completed;
} pin_arg_t;

static void pin_worker(void *argp) {
    pin_arg_t *a = (pin_arg_t *)argp;
    uint32_t c0 = rt_sched_current_carrier();
    a->first_carrier[a->idx] = c0;
    a->consistent[a->idx] = true;
    for (int i = 0; i < PIN_YIELDS; i++) {
        rt_sched_yield();
        if (rt_sched_current_carrier() != c0) {
            a->consistent[a->idx] = false;
        }
    }
    atomic_fetch_add_explicit(a->completed, 1, memory_order_relaxed);
    free(a);
}

/* One run of the 300-worker sweep. `*distinct_out` is only meaningful when
 * this returns true (no migration) -- see test_no_stealing's own comment on
 * why migration is a hard invariant that never gets a retry, while the
 * distinct-carrier count does. */
static bool no_stealing_sweep(uint32_t n_carriers, uint32_t n_workers,
                               uint32_t *distinct_out, int attempt) {
    rt_scheduler_t *s = rt_sched_create(n_carriers);
    uint32_t *first_carrier = calloc(n_workers, sizeof *first_carrier);
    bool *consistent = calloc(n_workers, sizeof *consistent);
    CHECK(first_carrier != NULL && consistent != NULL,
          "test harness allocation for pin tracking");
    _Atomic uint32_t completed = 0;

    for (uint32_t i = 0; i < n_workers; i++) {
        pin_arg_t *a = malloc(sizeof *a);
        a->idx = i;
        a->first_carrier = first_carrier;
        a->consistent = consistent;
        a->completed = &completed;
        rt_sched_spawn(s, pin_worker, a);
    }

    bool ok = wait_completed_at_least(s, n_workers, 60000);
    CHECK(ok, "[no-stealing] all pin workers must complete");

    bool any_inconsistent = false;
    bool carriers_seen[64] = { 0 };
    uint32_t distinct = 0;
    for (uint32_t i = 0; i < n_workers; i++) {
        if (!consistent[i]) any_inconsistent = true;
        uint32_t c = first_carrier[i];
        if (c < 64 && !carriers_seen[c]) {
            carriers_seen[c] = true;
            distinct++;
        }
    }
    CHECK(!any_inconsistent,
          "[no-stealing] every green thread stayed on the SAME carrier "
          "across every one of its yields -- never migrated");
    printf("    [no-stealing] attempt %d/3: %u distinct carriers actually "
           "ran work (of %u)\n", attempt, distinct, n_carriers);

    rt_sched_shutdown(s);
    rt_sched_destroy(s);
    free(first_carrier);
    free(consistent);

    *distinct_out = distinct;
    return !any_inconsistent;
}

/* The distinct-carrier-count half of this test retries for the identical
 * reason test_wake_variance's own sweep does -- see that function's long
 * comment for the full mechanism (each idle carrier's independent 5ms
 * periodic fallback timer, skewed by OS thread-scheduling jitter, racing
 * notify_new_work's own wake selection). Reproduced locally under real CPU
 * contention (`taskset -c 0-1` plus background load) that THIS check,
 * not only wake-variance's, goes to distinct=1 under -- work concentrating
 * onto whichever carrier the OS happens to schedule fastest is the same
 * underlying phenomenon, just observed through 300 pinned workers instead
 * of one lone arrival. The migration check (`any_inconsistent`) is NOT
 * retried under any condition: that is a hard structural invariant with no
 * legitimate contention-dependent exception, so a single failure there
 * fails the test immediately, on any attempt. */
static void test_no_stealing(void) {
    const uint32_t n_carriers = 6;
    const uint32_t n_workers = 300;
    uint32_t distinct = 0;

    for (int attempt = 1; attempt <= 3; attempt++) {
        bool no_migration = no_stealing_sweep(n_carriers, n_workers, &distinct, attempt);
        if (!no_migration) return; /* CHECK inside the sweep already failed this */
        if (distinct >= 2) break;
    }

    CHECK(distinct >= 2,
          "[no-stealing] more than one carrier actually did real work "
          "(not just carrier 0), in any of 3 independent attempts");
}

/* ========================================================================
 * Test 3 -- wake-target variance: with every carrier genuinely idle, spawn
 * exactly one green thread and record which carrier actually ran it.
 * Repeated across many trials (fresh scheduler, freshly reshuffled
 * permutation each time), the winner must NOT be the same carrier every
 * time -- the real-world analog of the simulation's seed-to-seed check
 * that caught the original fixed-order bug.
 * ====================================================================== */

typedef struct {
    _Atomic uint32_t *winner;
    _Atomic bool      *done;
} wake_arg_t;

static void wake_worker(void *argp) {
    wake_arg_t *a = (wake_arg_t *)argp;
    atomic_store_explicit(a->winner, rt_sched_current_carrier(), memory_order_relaxed);
    /* Release here, acquire on the main thread's gating read below: the
     * winner store above must be visible once `done` is observed true.
     * Plain relaxed/relaxed on two SEPARATE atomics gives no such
     * guarantee (each one is individually race-free, but nothing orders
     * one against the other) -- this is the same class of bug TSan found
     * in scheduler.c's own completion counters. */
    atomic_store_explicit(a->done, true, memory_order_release);
}

static bool all_carriers_idle(rt_scheduler_t *s, uint32_t n) {
    for (uint32_t i = 0; i < n; i++) {
        if (!rt_sched_carrier_idle(s, i)) return false;
    }
    return true;
}

/* One 30-trial sweep. Returns the number of distinct carriers that won the
 * lone-arrival race -- the caller decides what to do with that number. */
static uint32_t wake_variance_sweep(uint32_t n_carriers, int n_trials, int attempt) {
    uint32_t histogram[64] = { 0 };

    for (int t = 0; t < n_trials; t++) {
        rt_scheduler_t *s = rt_sched_create(n_carriers);

        bool idle_ok;
        WAIT_UNTIL(all_carriers_idle(s, n_carriers), 2000, &idle_ok);
        CHECK(idle_ok, "[wake-variance] every carrier reaches idle before the trial's arrival");

        _Atomic uint32_t winner = UINT32_MAX;
        _Atomic bool done = false;
        wake_arg_t arg = { &winner, &done };
        rt_sched_spawn(s, wake_worker, &arg);

        bool done_ok;
        WAIT_UNTIL(atomic_load_explicit(&done, memory_order_acquire), 2000, &done_ok);
        CHECK(done_ok, "[wake-variance] the lone arrival must actually run promptly");

        uint32_t w = atomic_load_explicit(&winner, memory_order_relaxed);
        if (w < 64) histogram[w]++;

        rt_sched_shutdown(s);
        rt_sched_destroy(s);
    }

    uint32_t distinct = 0;
    printf("    [wake-variance] attempt %d/3, winner histogram over %d trials:",
           attempt, n_trials);
    for (uint32_t i = 0; i < n_carriers; i++) {
        printf(" carrier%u=%u", i, histogram[i]);
        if (histogram[i] > 0) distinct++;
    }
    printf("\n");
    return distinct;
}

/* This check's own history is why it retries. The permutation this exercises
 * (notify_new_work's wake-selection walk, scheduler.c) is not the only path
 * that can draw the lone spawned item: every idle carrier also has its own
 * independent 5ms periodic fallback timer (carrier_main's own comment), and
 * those timers start at WHATEVER wall-clock moment each carrier happened to
 * go idle -- skewed by ordinary OS thread-creation/scheduling jitter, not
 * synchronized to each other at all. Under real CPU contention (confirmed by
 * reproducing this locally with `taskset -c 0-1` plus background load,
 * repeatedly: roughly half of runs failed, never all of them), the
 * explicitly-notified carrier can be slow enough to actually get scheduled
 * that a DIFFERENT carrier's own, already-ticking fallback timer fires first
 * and steals the one item instead -- a genuine, bounded race between two
 * individually-correct wake paths, not a logic bug in the permutation
 * itself, and not something a seed/RNG fix can touch (confirmed: this
 * recurred identically after two independent seeding fixes, including one
 * that moved to real kernel entropy, see runtime/scheduler.c's own
 * seed_from_entropy comment).
 *
 * The discriminator a retry exploits: a REAL fixed-order bug (the one this
 * check was originally written to catch, see its own module comment) would
 * reproduce identically on every attempt -- there is no scheduling noise
 * involved in a broken permutation always starting at the same place. This
 * contention race is probabilistic, so three independent 30-trial sweeps
 * overwhelmingly won't all land on distinct=1 unless the underlying
 * selection genuinely never varies. */
static void test_wake_variance(void) {
    const uint32_t n_carriers = 6;
    const int n_trials = 30;
    uint32_t distinct = 0;

    for (int attempt = 1; attempt <= 3; attempt++) {
        distinct = wake_variance_sweep(n_carriers, n_trials, attempt);
        if (distinct >= 2) break;
    }

    CHECK(distinct >= 2,
          "[wake-variance] the woken carrier is not fixed across trials -- "
          "at least two different carriers won the lone-arrival race, "
          "in any of 3 independent 30-trial attempts");
}

/* ========================================================================
 * Test 4 -- configurability: LANG_FUEL_SIZE actually changes the largest
 * batch a carrier draws, and LANG_GLOBAL_QUEUE_CAP actually changes where
 * backpressure engages. A single carrier plus a gate worker is what makes
 * "a real backlog bigger than fuel_size actually accumulates in the shared
 * queue" deterministic rather than a race against the carrier's own speed.
 * ====================================================================== */

static void test_fuel_size_changes_draw_size(uint32_t fuel) {
    char buf[32];
    snprintf(buf, sizeof buf, "%u", fuel);
    setenv("LANG_FUEL_SIZE", buf, 1);
    setenv("LANG_GLOBAL_QUEUE_CAP", "256", 1);

    rt_scheduler_t *s = rt_sched_create(1);
    CHECK(rt_sched_fuel_size(s) == fuel, "[fuel] effective fuel_size matches LANG_FUEL_SIZE");

    gate_t gate;
    gate_init(&gate);
    rt_sched_spawn(s, gate_worker, &gate);
    CHECK(wait_dispatched_at_least(s, 0, 1, 2000),
          "[fuel] the gate worker must actually start running (and then block)");

    _Atomic uint32_t completed = 0;
    uint32_t backlog = fuel * 3 + 5; /* comfortably more than one fuel's worth */
    for (uint32_t i = 0; i < backlog; i++) {
        noop_arg_t *a = malloc(sizeof *a);
        a->completed = &completed;
        rt_sched_spawn(s, noop_worker, a);
    }
    sleep_ms(20); /* the sole carrier is gated; nothing can drain this */
    char msg[160];
    snprintf(msg, sizeof msg,
             "[fuel=%u] the whole backlog (%u) must sit in the shared queue "
             "while the only carrier is gated (got %u)",
             fuel, backlog, rt_sched_queue_len(s));
    CHECK(rt_sched_queue_len(s) == backlog, msg);

    gate_release(&gate);
    CHECK(wait_completed_at_least(s, (uint64_t)backlog + 1, 30000),
          "[fuel] gate worker plus every noop must complete");
    snprintf(msg, sizeof msg,
             "[fuel=%u] max single draw observed must equal fuel_size exactly "
             "(got %u)",
             fuel, rt_sched_max_draw_seen(s));
    CHECK(rt_sched_max_draw_seen(s) == fuel, msg);

    rt_sched_shutdown(s);
    rt_sched_destroy(s);
    gate_destroy(&gate);
    unsetenv("LANG_FUEL_SIZE");
    unsetenv("LANG_GLOBAL_QUEUE_CAP");
}

typedef struct {
    rt_scheduler_t    *s;
    _Atomic uint32_t  *completed;
    _Atomic bool      *push_returned;
} pusher_arg_t;

static void *blocking_pusher(void *argp) {
    pusher_arg_t *pa = (pusher_arg_t *)argp;
    noop_arg_t *a = malloc(sizeof *a);
    a->completed = pa->completed;
    rt_sched_spawn(pa->s, noop_worker, a); /* expected to BLOCK here */
    atomic_store_explicit(pa->push_returned, true, memory_order_relaxed);
    return NULL;
}

static void test_queue_cap_and_backpressure(void) {
    setenv("LANG_FUEL_SIZE", "2", 1);
    setenv("LANG_GLOBAL_QUEUE_CAP", "4", 1);

    rt_scheduler_t *s = rt_sched_create(1);
    CHECK(rt_sched_queue_cap(s) == 4, "[backpressure] effective cap matches LANG_GLOBAL_QUEUE_CAP");

    gate_t gate;
    gate_init(&gate);
    rt_sched_spawn(s, gate_worker, &gate);
    CHECK(wait_dispatched_at_least(s, 0, 1, 2000),
          "[backpressure] the gate worker must start running before filling the queue");

    _Atomic uint32_t completed = 0;
    for (uint32_t i = 0; i < 4; i++) {
        noop_arg_t *a = malloc(sizeof *a);
        a->completed = &completed;
        rt_sched_spawn(s, noop_worker, a); /* must NOT block: cap is 4 */
    }
    CHECK(rt_sched_queue_len(s) == 4, "[backpressure] queue reaches exactly its configured capacity");

    _Atomic bool push_returned = false;
    pusher_arg_t pa = { s, &completed, &push_returned };
    pthread_t pusher;
    CHECK(pthread_create(&pusher, NULL, blocking_pusher, &pa) == 0,
          "[backpressure] test harness could start the blocking-pusher thread");

    sleep_ms(80);
    CHECK(atomic_load_explicit(&push_returned, memory_order_relaxed) == false,
          "[backpressure] a push against a full queue genuinely blocks "
          "rather than returning immediately");
    CHECK(rt_sched_queue_len(s) == 4,
          "[backpressure] queue length is unchanged while a push is blocked");

    gate_release(&gate); /* carrier drains, frees room, the blocked push can proceed */

    bool unblocked;
    WAIT_UNTIL(atomic_load_explicit(&push_returned, memory_order_relaxed), 5000, &unblocked);
    CHECK(unblocked, "[backpressure] the blocked push unblocks once a draw frees room");

    pthread_join(pusher, NULL);
    CHECK(wait_completed_at_least(s, 1 /* gate */ + 5 /* 4 + the blocked one */, 10000),
          "[backpressure] every spawned green thread completes -- no lost push");
    CHECK(rt_sched_completed(s) == 6,
          "[backpressure] exact completion count: nothing lost, nothing duplicated");

    rt_sched_shutdown(s);
    rt_sched_destroy(s);
    gate_destroy(&gate);
    unsetenv("LANG_FUEL_SIZE");
    unsetenv("LANG_GLOBAL_QUEUE_CAP");
}

static void test_invalid_env_fails_soft(void) {
    setenv("LANG_FUEL_SIZE", "banana", 1);
    setenv("LANG_GLOBAL_QUEUE_CAP", "-5", 1);
    rt_scheduler_t *s = rt_sched_create(1);
    CHECK(rt_sched_fuel_size(s) == 4, "[config] non-numeric LANG_FUEL_SIZE falls back to the default (4)");
    CHECK(rt_sched_queue_cap(s) == 64, "[config] negative LANG_GLOBAL_QUEUE_CAP falls back to the default (64)");
    rt_sched_shutdown(s);
    rt_sched_destroy(s);

    setenv("LANG_FUEL_SIZE", "0", 1);
    setenv("LANG_GLOBAL_QUEUE_CAP", "0", 1);
    rt_scheduler_t *s2 = rt_sched_create(1);
    CHECK(rt_sched_fuel_size(s2) == 4, "[config] LANG_FUEL_SIZE=0 falls back to the default (0 is not a usable fuel size)");
    CHECK(rt_sched_queue_cap(s2) == 64, "[config] LANG_GLOBAL_QUEUE_CAP=0 falls back to the default");
    rt_sched_shutdown(s2);
    rt_sched_destroy(s2);

    setenv("LANG_FUEL_SIZE", "10", 1);
    setenv("LANG_GLOBAL_QUEUE_CAP", "2", 1);
    rt_scheduler_t *s3 = rt_sched_create(1);
    CHECK(rt_sched_fuel_size(s3) == 10, "[config] a valid, larger LANG_FUEL_SIZE is honoured");
    CHECK(rt_sched_queue_cap(s3) == 10,
          "[config] a queue cap below fuel_size is clamped UP to fuel_size, not left undersized");
    rt_sched_shutdown(s3);
    rt_sched_destroy(s3);

    unsetenv("LANG_FUEL_SIZE");
    unsetenv("LANG_GLOBAL_QUEUE_CAP");
}

static void test_num_carriers_config(void) {
    /* An explicit count passed to rt_sched_create always wins, regardless
     * of the environment -- this is the "or a specific count" half of
     * rt_sched_create's contract, checked first so the rest of this test
     * knows that part is trustworthy. */
    setenv("LANG_NUM_CARRIERS", "3", 1);
    rt_scheduler_t *explicit_s = rt_sched_create(5);
    CHECK(rt_sched_ncarriers(explicit_s) == 5,
          "[config] an explicit n_carriers argument overrides LANG_NUM_CARRIERS");
    rt_sched_shutdown(explicit_s);
    rt_sched_destroy(explicit_s);

    /* 0 means auto-detect, and LANG_NUM_CARRIERS, when valid, wins there. */
    rt_scheduler_t *s = rt_sched_create(0);
    CHECK(rt_sched_ncarriers(s) == 3,
          "[config] LANG_NUM_CARRIERS overrides auto-detection when 0 is passed");
    rt_sched_shutdown(s);
    rt_sched_destroy(s);

    /* An invalid value falls back to core-count detection, soft -- not a
     * crash, and not zero carriers either. */
    setenv("LANG_NUM_CARRIERS", "not-a-number", 1);
    rt_scheduler_t *s2 = rt_sched_create(0);
    CHECK(rt_sched_ncarriers(s2) > 0,
          "[config] invalid LANG_NUM_CARRIERS falls back to core-count "
          "detection rather than crashing or producing 0 carriers");
    rt_sched_shutdown(s2);
    rt_sched_destroy(s2);

    unsetenv("LANG_NUM_CARRIERS");
}

/* ========================================================================
 * Test 5 -- the byte-per-thread state table (Phase 1, Part 4) is genuinely
 * exercised by this scheduler: Runnable at spawn, Running while dispatched,
 * Dead once finished.
 * ====================================================================== */

static void test_state_table_transitions(void) {
    rt_scheduler_t *s = rt_sched_create(1);
    gate_t gate;
    gate_init(&gate);

    uint32_t id = rt_sched_spawn(s, gate_worker, &gate);
    CHECK(wait_dispatched_at_least(s, 0, 1, 2000),
          "[state-table] the gated green thread must start running");
    CHECK(rt_gtstate_get(id) == RT_GT_RUNNING,
          "[state-table] a currently-dispatched, blocked-but-not-finished "
          "green thread reads back Running");

    gate_release(&gate);
    CHECK(wait_completed_at_least(s, 1, 5000),
          "[state-table] the gated green thread must finish once released");
    CHECK(rt_gtstate_get(id) == RT_GT_DEAD,
          "[state-table] a finished green thread reads back Dead");

    rt_sched_shutdown(s);
    rt_sched_destroy(s);
    gate_destroy(&gate);
}

/* ========================================================================
 * Failure-mode submode, run by the .sh harness as a separate process
 * because it is meant to abort (SIGABRT, exit 134) -- same convention
 * runtime/greenthread_test.c uses for its `overflow`/`poison` submodes.
 * ====================================================================== */

/* `./scheduler_test yield_outside`: rt_sched_yield from a context that is
 * not a running green thread (here, main() itself) must trap rather than
 * silently doing nothing or crashing some other way. */
static int run_yield_outside_submode(void) {
    rt_sched_yield();
    fprintf(stderr, "FAIL: rt_sched_yield returned instead of trapping\n");
    return 1;
}

/* ========================================================================
 * Driver
 * ====================================================================== */

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "yield_outside") == 0) {
        return run_yield_outside_submode();
    }

    test_completion_invariant();
    test_no_stealing();
    test_wake_variance();
    test_fuel_size_changes_draw_size(1);
    test_fuel_size_changes_draw_size(4);
    test_fuel_size_changes_draw_size(8);
    test_queue_cap_and_backpressure();
    test_invalid_env_fails_soft();
    test_num_carriers_config();
    test_state_table_transitions();

    printf("%d checks, %d failures\n", checks, failures);
    return failures == 0 ? 0 : 1;
}
