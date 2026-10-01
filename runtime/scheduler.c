/* Phase 2 scheduler implementation. See scheduler.h for the public contract
 * and the design summary, and docs/concurrency-decision.md's "Scheduler
 * queues" section for the full reasoning this file is implementing exactly.
 *
 * Compiled as its own translation unit -- NOT #included by rt.c, unlike
 * greenthread.c. That matters for the same reason greenthread.h's rt_ctx_make
 * is `static inline` rather than living in rt.c's own compiled text: this
 * file calls rt_ctx_make, which takes the address of rt_ctx_trampoline
 * (defined only in ctx_switch_x86_64.s). If this file's object code lived
 * inside rt.c's translation unit, every program that links runtime/rt.c --
 * which is every build line in this repository, build.sh included -- would
 * suddenly need to link ctx_switch_x86_64.o too, or fail at link time. Kept
 * separate, only this phase's own test harness (runtime/scheduler_test.c)
 * links it, alongside rt.c and ctx_switch_x86_64.s directly.
 */
#include "rt.h"
#include "greenthread.h"
#include "scheduler.h"

#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* ========================================================================
 * Configuration: environment variables, read once per scheduler (see
 * scheduler.h for why "once per scheduler" rather than a single
 * process-wide read). Parse failures fall back to the default silently
 * except for the one case that needs a word of explanation -- the queue
 * capacity being clamped up to fuel_size -- which gets a one-line warning
 * on stderr so an operator who set a too-small value is not left guessing
 * why backpressure engages later than they configured.
 * ====================================================================== */

#define RT_SCHED_DEFAULT_FUEL      4u  /* explicitly provisional -- see the
                                         * design doc's own caveat: this was
                                         * tuned in a simulation that charges
                                         * zero cost per draw, so it cannot
                                         * see the real floor. */
#define RT_SCHED_DEFAULT_QUEUE_CAP 64u

/* How much of the green-thread id space to force-grow, up front, in a
 * single single-threaded call, before any carrier OS thread starts.
 *
 * Why this exists: runtime/greenthread.c's rt_gtstate_ensure grows its
 * backing array with realloc under a lock, but the FAST path that skips the
 * lock entirely -- `if ((size_t)id < g_gtstate_cap) return;` -- reads
 * g_gtstate_cap, and rt_gtstate_set/get then index g_gtstate[id], with NO
 * synchronisation at all once a slot already exists. Phase 1's own comment
 * on that function says exactly this: "a real concurrent-writer story is
 * Phase 2's problem, once there is a scheduler that could actually produce
 * one." This is that scheduler, so here is the answer -- but it is a
 * pre-sizing discipline in THIS file, not a patch to greenthread.c:
 *
 *   - Concurrent, unsynchronised reads/writes to DIFFERENT array elements
 *     are not a data race (they are different objects) -- and this
 *     scheduler's own design guarantees two carriers never touch the same
 *     green-thread id at once (no stealing; a green thread runs only on
 *     the carrier that drew it, for its entire lifetime).
 *   - The only way that becomes a *real* race is if the table has to GROW
 *     again concurrently with those accesses -- g_gtstate_cap and
 *     g_gtstate itself are plain globals on the fast path, read without a
 *     lock, and a concurrent realloc can both tear a read of the pointer
 *     and free memory another thread is still indexing into.
 *
 * So: grow the table ONCE, here, single-threaded, far enough that ordinary
 * use never triggers that path again, and refuse (via rt_trap, loudly, not
 * silently) a scheduler instance that is asked to outlive that budget
 * across its whole lifetime of spawns. 1 << 22 ids is a 4 MiB table and
 * comfortably covers this phase's heaviest test load (tens of thousands of
 * green threads) with enormous headroom; it is NOT the "millions of green
 * threads" figure the design doc targets for threads ALIVE AT ONCE, which
 * this scheduler can still do -- this cap is on lifetime ID issuance, a
 * monotonically increasing counter that is never reused, so it is really a
 * statement about total spawns over a process's life, not concurrency. A
 * production deployment that wants to lift it needs rt_gtstate_ensure
 * itself made genuinely thread-safe (e.g. an atomic pointer swap to a
 * freshly-grown copy, leaking the old one rather than freeing it under
 * concurrent readers) -- a real, scoped, Phase-1-file change deliberately
 * NOT made here, since Phase 1 is already merged and this scheduler can be
 * fully correct today without it, by simply not needing the growth path to
 * ever run concurrently. */
#define RT_SCHED_ID_CAPACITY (1u << 22)

static uint32_t parse_env_u32(const char *name, uint32_t fallback) {
    const char *v = getenv(name);
    if (v == NULL || *v == '\0') return fallback;

    /* strtoul accepts a leading '-' and silently negates (wraps) the result
     * rather than failing -- reject it explicitly up front rather than
     * relying on the wrapped value happening to exceed UINT32_MAX on every
     * platform this might run on. */
    const char *p = v;
    while (*p == ' ' || *p == '\t') p++;
    bool negative = (*p == '-');

    char *end = NULL;
    errno = 0;
    unsigned long n = negative ? 0 : strtoul(v, &end, 10);
    if (negative || errno != 0 || end == v || *end != '\0' || n == 0 ||
        n > UINT32_MAX) {
        fprintf(stderr,
                "warning: %s=\"%s\" is not a valid positive integer; using "
                "the default (%u)\n",
                name, v, fallback);
        return fallback;
    }
    return (uint32_t)n;
}

/* ========================================================================
 * The shared global queue -- bounded, mutex-protected, correctness over
 * cleverness on purpose (docs/concurrency-decision.md: "don't spend excess
 * effort optimizing its internal structure"). Shaped exactly like rt.c's
 * own Chan (rt_chan_send/recv): a ring buffer behind one mutex, one condvar
 * for "wait for room". There is deliberately no "wait for data" condvar --
 * a draw never blocks; an empty draw is itself the signal a carrier acts
 * on (go idle), which is the whole point of the self-service, no-minimum
 * design this is implementing.
 * ====================================================================== */

typedef struct rt_green rt_green_t;

typedef struct {
    pthread_mutex_t lock;
    pthread_cond_t  not_full;
    rt_green_t    **buf;
    uint32_t        cap;
    uint32_t        head;
    uint32_t        len;
} rt_squeue_t;

static void squeue_init(rt_squeue_t *q, uint32_t cap) {
    q->buf = malloc(sizeof(rt_green_t *) * cap);
    if (q->buf == NULL) rt_trap("out of memory: scheduler global queue");
    q->cap = cap;
    q->head = 0;
    q->len = 0;
    pthread_mutex_init(&q->lock, NULL);
    pthread_cond_init(&q->not_full, NULL);
}

static void squeue_destroy(rt_squeue_t *q) {
    pthread_mutex_destroy(&q->lock);
    pthread_cond_destroy(&q->not_full);
    free(q->buf);
}

/* Blocks while full -- real backpressure, matching the design doc's stated
 * semantics exactly: "a push blocks until space frees", never silent
 * unbounded growth and never a dropped item. */
static void squeue_push(rt_squeue_t *q, rt_green_t *g) {
    pthread_mutex_lock(&q->lock);
    while (q->len == q->cap) {
        pthread_cond_wait(&q->not_full, &q->lock);
    }
    q->buf[(q->head + q->len) % q->cap] = g;
    q->len++;
    pthread_mutex_unlock(&q->lock);
}

/* Non-blocking: draws min(available, max_n) into out[0..n), no minimum
 * threshold -- a single available item is drawn immediately. Returns the
 * count actually drawn, which may be 0. */
static uint32_t squeue_draw(rt_squeue_t *q, rt_green_t **out, uint32_t max_n) {
    pthread_mutex_lock(&q->lock);
    uint32_t n = q->len < max_n ? q->len : max_n;
    for (uint32_t i = 0; i < n; i++) {
        out[i] = q->buf[(q->head + i) % q->cap];
    }
    q->head = (q->head + n) % q->cap;
    q->len -= n;
    if (n > 0) {
        /* Freed room: every blocked pusher re-checks its own while-loop
         * condition, so a broadcast here is simply correct, not merely
         * convenient -- no lost wakeup, no lost push. */
        pthread_cond_broadcast(&q->not_full);
    }
    pthread_mutex_unlock(&q->lock);
    return n;
}

static uint32_t squeue_len(rt_squeue_t *q) {
    pthread_mutex_lock(&q->lock);
    uint32_t n = q->len;
    pthread_mutex_unlock(&q->lock);
    return n;
}

/* ========================================================================
 * The shared wake permutation -- one persistent, reshuffled-on-exhaustion
 * random walk over carrier indices, per docs/concurrency-decision.md's
 * "Resolved 2026-09-30" fix. A plain xorshift64* PRNG with its own state,
 * not libc's rand()/random(): those have process-wide internal state that
 * would race against anything else in the process calling them without
 * going through this same lock, which this file has no business assuming
 * never happens. Not cryptographic, and does not need to be -- only needs
 * to avoid a fixed, guessable sequence, which it does.
 * ====================================================================== */

typedef struct {
    pthread_mutex_t lock;
    uint32_t       *order;
    uint32_t        n;
    uint32_t        pos;
    uint64_t        rng;
} rt_wake_perm_t;

static uint64_t xorshift64star(uint64_t *state) {
    uint64_t x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    return x * 0x2545F4914F6CDD1DULL;
}

static uint64_t seed_from_entropy(void) {
    static _Atomic uint64_t salt = 1;
    uint64_t s = (uint64_t)time(NULL);
    s ^= ((uint64_t)(uintptr_t)&s) << 16;
    s ^= (uint64_t)getpid() << 32;
    s ^= atomic_fetch_add_explicit(&salt, 0x9E3779B97F4A7C15ULL,
                                    memory_order_relaxed);
    if (s == 0) s = 0xD1B54A32D192ED03ULL; /* xorshift needs nonzero state */
    return s;
}

/* Fisher-Yates. Called with the permutation's lock already held. */
static void perm_reshuffle(rt_wake_perm_t *p) {
    for (uint32_t i = p->n - 1; i > 0; i--) {
        uint64_t r = xorshift64star(&p->rng);
        uint32_t j = (uint32_t)(r % (uint64_t)(i + 1));
        uint32_t tmp = p->order[i];
        p->order[i] = p->order[j];
        p->order[j] = tmp;
    }
}

/* ========================================================================
 * One carrier: one OS thread, its own wake semaphore and idle flag, and its
 * own local buffer -- a ring buffer sized exactly fuel_size, which is
 * always enough: a draw only ever happens when the buffer is completely
 * empty (so it never needs to hold more than one draw's worth at once), and
 * a yielded green thread is popped before it can be pushed back, so the
 * length never exceeds fuel_size either.
 *
 * NO STEALING, STRUCTURALLY: `local`, `local_head` and `local_len` below
 * are read and written ONLY by carrier_main/carrier_dispatch running on
 * THIS carrier's own OS thread -- grep this file for `->local` and every
 * hit is inside a function that received `rt_carrier_t *c` meaning "the
 * carrier calling this, about itself". Nothing anywhere takes another
 * carrier's `c->local` by pointer, indexes into it, or iterates
 * `sched->carriers[j].local` for any j. The only per-carrier fields ANY
 * other thread ever touches are `idle` (atomic) and `wake_sem` (a
 * semaphore) -- the wake primitive, not the run queue -- which is exactly
 * what "individually addressable wake primitive" in the design doc means
 * and nothing more.
 * ====================================================================== */

typedef struct rt_carrier {
    struct rt_scheduler *sched;
    uint32_t             index;
    pthread_t            os_thread;

    sem_t       wake_sem;
    atomic_bool idle;

    rt_green_t **local;
    uint32_t     local_cap;
    uint32_t     local_head;
    uint32_t     local_len;

    _Atomic uint64_t dispatched;
} rt_carrier_t;

/* ========================================================================
 * The scheduler itself.
 * ====================================================================== */

struct rt_scheduler {
    uint32_t      n_carriers;
    rt_carrier_t *carriers;

    rt_squeue_t    global;
    rt_wake_perm_t perm;

    uint32_t fuel_size;
    uint32_t queue_cap;

    atomic_bool shutdown;

    _Atomic uint32_t next_id;
    _Atomic uint64_t total_spawned;
    _Atomic uint64_t total_completed;
    _Atomic uint32_t max_draw_seen;
};

/* One green thread. Opaque to callers of scheduler.h -- only this file
 * knows its shape, the same convention rt.h uses for Chan. */
struct rt_green {
    rt_ctx_t   ctx;
    rt_stack_t stack;
    uint32_t   id;
    void     (*entry)(void *);
    void      *arg;
    rt_ctx_t  *carrier_ctx; /* this dispatch's switch-back target; reset on
                              * every dispatch, see carrier_dispatch below */
    bool       finished;
};

/* Which green thread (if any) is running on THIS OS thread right now, and
 * which carrier index this OS thread is. Both thread-local for the same
 * reason rt_stack_limit (rt.h) is: one OS thread runs one green thread's
 * code at a time, and a carrier's own identity does not change for the
 * life of that OS thread. */
static _Thread_local rt_green_t *tls_current_green = NULL;
static _Thread_local uint32_t    tls_carrier_index = UINT32_MAX;

/* ---- the wake notification itself -------------------------------------- */

/* On every push, advance through the shared permutation, skip any carrier
 * whose idle flag is false, wake the first idle one found via ITS OWN
 * semaphore, then clear its idle flag -- atomically, via CAS, so a
 * concurrent notify (from another simultaneous push) cannot also claim the
 * same carrier. If a full lap (n_carriers steps) finds nobody idle, do
 * nothing: everyone is already busy, exactly per the design doc. */
static void notify_new_work(rt_scheduler_t *s) {
    rt_wake_perm_t *p = &s->perm;
    pthread_mutex_lock(&p->lock);
    for (uint32_t steps = 0; steps < s->n_carriers; steps++) {
        if (p->pos >= p->n) {
            perm_reshuffle(p);
            p->pos = 0;
        }
        uint32_t idx = p->order[p->pos++];
        rt_carrier_t *c = &s->carriers[idx];
        bool expected = true;
        if (atomic_compare_exchange_strong_explicit(
                &c->idle, &expected, false, memory_order_acq_rel,
                memory_order_relaxed)) {
            sem_post(&c->wake_sem);
            pthread_mutex_unlock(&p->lock);
            return;
        }
    }
    pthread_mutex_unlock(&p->lock);
}

static void update_max_draw_seen(rt_scheduler_t *s, uint32_t n) {
    uint32_t cur = atomic_load_explicit(&s->max_draw_seen, memory_order_relaxed);
    while (n > cur &&
           !atomic_compare_exchange_weak_explicit(
               &s->max_draw_seen, &cur, n, memory_order_relaxed,
               memory_order_relaxed)) {
        /* cur was refreshed by the failed CAS; retry if still a new max. */
    }
}

/* ---- the green-thread entry trampoline ---------------------------------
 *
 * What rt_ctx_make (greenthread.h) actually switches into. Runs the user's
 * entry, marks the green thread Dead, and switches back to whichever
 * carrier is currently running it -- `carrier_ctx` is set fresh by
 * carrier_dispatch on every single dispatch (first run AND every resume
 * after a yield), so this always lands back in the right place, on the
 * right carrier, whichever carrier that happens to be (always the same one
 * for this green thread's whole life -- see carrier_dispatch). */
static void green_trampoline(void *argp) {
    rt_green_t *g = (rt_green_t *)argp;
    g->entry(g->arg);
    rt_gtstate_set(g->id, RT_GT_DEAD);
    g->finished = true;
    rt_fiber_switch(&g->ctx, g->carrier_ctx, 0);
    /* Unreachable: a Dead green thread is never dispatched again. */
    rt_ctx_entry_returned();
}

void rt_sched_yield(void) {
    rt_green_t *g = tls_current_green;
    if (g == NULL) {
        rt_trap("rt_sched_yield called from outside a running green thread");
    }
    rt_gtstate_set(g->id, RT_GT_RUNNABLE);
    rt_fiber_switch(&g->ctx, g->carrier_ctx, 0);
    /* Resumed: carrier_dispatch already set RT_GT_RUNNING and
     * tls_current_green again before switching back in, below -- nothing
     * to do here. */
}

uint32_t rt_sched_current_carrier(void) {
    return tls_carrier_index;
}

/* ---- the carrier's own local buffer (ring, capacity == fuel_size) ------ */

static void local_push(rt_carrier_t *c, rt_green_t *g) {
    c->local[(c->local_head + c->local_len) % c->local_cap] = g;
    c->local_len++;
}

static rt_green_t *local_pop(rt_carrier_t *c) {
    rt_green_t *g = c->local[c->local_head];
    c->local_head = (c->local_head + 1) % c->local_cap;
    c->local_len--;
    return g;
}

/* Run `g` until it yields or finishes, on carrier `c`. */
static void carrier_dispatch(rt_carrier_t *c, rt_green_t *g) {
    rt_ctx_t loop_ctx; /* this dispatch's own return point -- a fresh one
                        * every call, living on THIS carrier's native OS
                        * stack frame for exactly the duration of this
                        * call, the same pattern greenthread_test.c's
                        * test_round_trip uses for its own main_ctx. */
    g->carrier_ctx = &loop_ctx;
    tls_current_green = g;
    rt_gtstate_set(g->id, RT_GT_RUNNING);

    /* RELEASE, not relaxed -- found by TSan on exactly this scheduler,
     * exercised by scheduler_test.c's state-table test: a caller on some
     * OTHER thread polls rt_sched_carrier_dispatched() as a "has this
     * green thread actually started running" signal and then reads
     * rt_gtstate_get(id) expecting to see the RT_GT_RUNNING this line just
     * set. Relaxed ordering made that a genuine, TSan-confirmed data race
     * -- "dispatched >= 1 is now visible" did not imply "the RUNNING write
     * right above it is now visible", because relaxed atomics only order
     * the atomic itself, never the plain memory around it. Release here,
     * paired with acquire in rt_sched_carrier_dispatched, closes exactly
     * that gap (same fix shape as rt_spawn's own comment above about the
     * thread-count race TSan found during Phase 0). */
    atomic_fetch_add_explicit(&c->dispatched, 1, memory_order_release);
    rt_fiber_switch(&loop_ctx, &g->ctx, (uintptr_t)g->stack.base);

    tls_current_green = NULL;

    if (g->finished) {
        rt_stack_free(&g->stack);
        free(g);
        /* Same reasoning as the dispatched counter above, for the same
         * TSan-confirmed reason: a caller elsewhere polls
         * rt_sched_completed() and then reads data this green thread (or
         * this scheduler's own bookkeeping, e.g. rt_gtstate_set(DEAD) in
         * green_trampoline) wrote before finishing -- RELEASE here,
         * ACQUIRE in rt_sched_completed, is what makes that safe instead
         * of merely usually-true on this hardware. */
        atomic_fetch_add_explicit(&c->sched->total_completed, 1,
                                   memory_order_release);
    } else {
        /* Yielded, not finished: stays on THIS carrier, never anywhere
         * else -- the whole of "no stealing" for an already-running green
         * thread. */
        local_push(c, g);
    }
}

static void *carrier_main(void *argp) {
    rt_carrier_t *c = (rt_carrier_t *)argp;
    tls_carrier_index = c->index;

    for (;;) {
        if (c->local_len == 0) {
            uint32_t n = squeue_draw(&c->sched->global, c->local, c->local_cap);
            if (n > 0) {
                c->local_head = 0;
                c->local_len = n;
                atomic_store_explicit(&c->idle, false, memory_order_relaxed);
                update_max_draw_seen(c->sched, n);
            } else {
                if (atomic_load_explicit(&c->sched->shutdown,
                                          memory_order_relaxed)) {
                    return NULL;
                }
                /* Genuinely nothing anywhere: go idle. Set the flag BEFORE
                 * waiting, not after -- sem_post/sem_timedwait's own count
                 * means a notify that lands between this store and the
                 * wait call below is not lost (the semaphore remembers the
                 * post), which is what actually closes the lost-wakeup
                 * window; the periodic timeout is the remaining
                 * correctness floor for anything this still misses. */
                atomic_store_explicit(&c->idle, true, memory_order_relaxed);

                struct timespec ts;
                clock_gettime(CLOCK_REALTIME, &ts);
                ts.tv_nsec += 5 * 1000 * 1000; /* 5ms periodic fallback */
                if (ts.tv_nsec >= 1000000000L) {
                    ts.tv_sec += 1;
                    ts.tv_nsec -= 1000000000L;
                }
                while (sem_timedwait(&c->wake_sem, &ts) != 0 && errno == EINTR) {
                    /* retry on signal interruption only */
                }
                /* Either a real post, or ETIMEDOUT (the fallback firing),
                 * or a spurious-looking EINVAL from a clock edge case --
                 * all three mean exactly the same thing here: go back to
                 * the top and try to find work again. */
                continue;
            }
        }

        rt_green_t *g = local_pop(c);
        carrier_dispatch(c, g);
    }
}

/* ========================================================================
 * Public API
 * ====================================================================== */

static uint32_t detect_ncarriers(void) {
    const char *v = getenv("LANG_NUM_CARRIERS");
    if (v != NULL && *v != '\0') {
        char *end = NULL;
        errno = 0;
        long n = strtol(v, &end, 10);
        if (errno == 0 && end != v && *end == '\0' && n > 0 && n <= 4096) {
            return (uint32_t)n;
        }
        fprintf(stderr,
                "warning: LANG_NUM_CARRIERS=\"%s\" is not a valid positive "
                "integer; detecting core count instead\n",
                v);
    }
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    return (n > 0) ? (uint32_t)n : 1u;
}

rt_scheduler_t *rt_sched_create(uint32_t n_carriers) {
    rt_scheduler_t *s = calloc(1, sizeof *s);
    if (s == NULL) rt_trap("out of memory: scheduler");

    /* Force one single-threaded growth of the whole green-thread id space
     * before any carrier thread exists -- see RT_SCHED_ID_CAPACITY above
     * for exactly why this matters and what it is standing in for. */
    rt_gtstate_set(RT_SCHED_ID_CAPACITY - 1, RT_GT_RUNNABLE);

    if (n_carriers == 0) n_carriers = detect_ncarriers();
    s->n_carriers = n_carriers;

    s->fuel_size = parse_env_u32("LANG_FUEL_SIZE", RT_SCHED_DEFAULT_FUEL);
    uint32_t cap = parse_env_u32("LANG_GLOBAL_QUEUE_CAP", RT_SCHED_DEFAULT_QUEUE_CAP);
    if (cap < s->fuel_size) {
        fprintf(stderr,
                "warning: LANG_GLOBAL_QUEUE_CAP=%u is below LANG_FUEL_SIZE=%u; "
                "clamping it up to %u\n",
                cap, s->fuel_size, s->fuel_size);
        cap = s->fuel_size;
    }
    s->queue_cap = cap;

    squeue_init(&s->global, s->queue_cap);

    s->perm.n = n_carriers;
    s->perm.order = malloc(sizeof(uint32_t) * n_carriers);
    if (s->perm.order == NULL) rt_trap("out of memory: wake permutation");
    for (uint32_t i = 0; i < n_carriers; i++) s->perm.order[i] = i;
    pthread_mutex_init(&s->perm.lock, NULL);
    s->perm.rng = seed_from_entropy();
    perm_reshuffle(&s->perm);
    s->perm.pos = 0;

    atomic_init(&s->shutdown, false);
    atomic_init(&s->next_id, 0u);
    atomic_init(&s->total_spawned, (uint64_t)0);
    atomic_init(&s->total_completed, (uint64_t)0);
    atomic_init(&s->max_draw_seen, 0u);

    s->carriers = calloc(n_carriers, sizeof(rt_carrier_t));
    if (s->carriers == NULL) rt_trap("out of memory: carriers");

    for (uint32_t i = 0; i < n_carriers; i++) {
        rt_carrier_t *c = &s->carriers[i];
        c->sched = s;
        c->index = i;
        if (sem_init(&c->wake_sem, 0, 0) != 0) {
            rt_trap("could not create a carrier's wake semaphore");
        }
        /* Every carrier starts idle=true: it genuinely is, before anything
         * has run -- the fix for the cold-start bug the design doc
         * describes (a diagnostic counting the wrong-at-startup case went
         * from num_carriers to exactly 0 once this was the initial value,
         * rather than false-until-a-carrier-discovers-it-the-hard-way). */
        atomic_init(&c->idle, true);
        atomic_init(&c->dispatched, (uint64_t)0);
        c->local_cap = s->fuel_size;
        c->local = malloc(sizeof(rt_green_t *) * c->local_cap);
        if (c->local == NULL) rt_trap("out of memory: carrier local buffer");
        c->local_head = 0;
        c->local_len = 0;
    }

    for (uint32_t i = 0; i < n_carriers; i++) {
        if (pthread_create(&s->carriers[i].os_thread, NULL, carrier_main,
                            &s->carriers[i]) != 0) {
            rt_trap("could not start a carrier OS thread");
        }
    }

    return s;
}

uint32_t rt_sched_spawn(rt_scheduler_t *s, void (*entry)(void *), void *arg) {
    rt_green_t *g = malloc(sizeof *g);
    if (g == NULL) rt_trap("out of memory: green thread control block");

    uint32_t id = atomic_fetch_add_explicit(&s->next_id, 1u, memory_order_relaxed);
    if (id >= RT_SCHED_ID_CAPACITY) {
        rt_trap("rt_sched_spawn: exceeded this scheduler's pre-sized "
                "green-thread id capacity (RT_SCHED_ID_CAPACITY, "
                "runtime/scheduler.c) -- see that constant's comment");
    }

    g->id = id;
    g->entry = entry;
    g->arg = arg;
    g->finished = false;
    g->stack = rt_stack_alloc();
    rt_ctx_make(&g->ctx, g->stack.base, RT_STACK_SIZE, green_trampoline, g);

    rt_gtstate_set(g->id, RT_GT_RUNNABLE);
    /* Release/acquire, matching total_completed below and for the same
     * reason: a caller elsewhere may poll rt_sched_spawned() and act on
     * that count. */
    atomic_fetch_add_explicit(&s->total_spawned, (uint64_t)1, memory_order_release);

    /* Every spawn -- local or external in origin -- routes through the
     * shared queue, never directly onto any carrier's local buffer. This
     * call blocks if the queue is full (real backpressure) until a draw
     * frees room. */
    squeue_push(&s->global, g);

    /* New-work event: advance the shared wake permutation and notify
     * whichever idle carrier it lands on, if any. */
    notify_new_work(s);

    return id;
}

uint32_t rt_sched_ncarriers(rt_scheduler_t *s) { return s->n_carriers; }
uint32_t rt_sched_fuel_size(rt_scheduler_t *s) { return s->fuel_size; }
uint32_t rt_sched_queue_cap(rt_scheduler_t *s) { return s->queue_cap; }

/* ACQUIRE on every one of these three loads -- see the matching RELEASE
 * comments at the write sites (rt_sched_spawn, carrier_dispatch) for why:
 * each of these counts is used, in this file's own tests, as a "this
 * already happened, so it's now safe to look at what it produced" signal
 * from a different thread, and TSan confirmed that relaxed ordering does
 * not actually provide that guarantee -- only acquire/release does. */
uint64_t rt_sched_spawned(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->total_spawned, memory_order_acquire);
}
uint64_t rt_sched_completed(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->total_completed, memory_order_acquire);
}
uint32_t rt_sched_max_draw_seen(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->max_draw_seen, memory_order_relaxed);
}
bool rt_sched_carrier_idle(rt_scheduler_t *s, uint32_t carrier) {
    return atomic_load_explicit(&s->carriers[carrier].idle, memory_order_relaxed);
}
uint64_t rt_sched_carrier_dispatched(rt_scheduler_t *s, uint32_t carrier) {
    return atomic_load_explicit(&s->carriers[carrier].dispatched,
                                 memory_order_acquire);
}
uint32_t rt_sched_queue_len(rt_scheduler_t *s) {
    return squeue_len(&s->global);
}

void rt_sched_shutdown(rt_scheduler_t *s) {
    atomic_store_explicit(&s->shutdown, true, memory_order_relaxed);
    for (uint32_t i = 0; i < s->n_carriers; i++) {
        sem_post(&s->carriers[i].wake_sem);
    }
    for (uint32_t i = 0; i < s->n_carriers; i++) {
        pthread_join(s->carriers[i].os_thread, NULL);
    }
}

void rt_sched_destroy(rt_scheduler_t *s) {
    squeue_destroy(&s->global);
    pthread_mutex_destroy(&s->perm.lock);
    free(s->perm.order);
    for (uint32_t i = 0; i < s->n_carriers; i++) {
        sem_destroy(&s->carriers[i].wake_sem);
        free(s->carriers[i].local);
    }
    free(s->carriers);
    free(s);
}
