/* Phase 2 scheduler implementation. See scheduler.h for the public contract
 * and the design summary, and docs/concurrency-decision.md's "Scheduler
 * queues" section for the full reasoning this file is implementing exactly.
 *
 * Compiled as its own translation unit -- NOT #included by rt.c, unlike
 * greenthread.c. That matters for the same reason greenthread.h's rt_ctx_make
 * is `static inline` rather than living in rt.c's own compiled text: this
 * file calls rt_ctx_make, which takes the address of rt_ctx_trampoline
 * (defined only in ctx_switch_x86_64.S). If this file's object code lived
 * inside rt.c's translation unit, every program that links runtime/rt.c --
 * which is every build line in this repository, build.sh included -- would
 * suddenly need to link ctx_switch_x86_64.o too, or fail at link time. Kept
 * separate, only this phase's own test harness (runtime/scheduler_test.c)
 * links it, alongside rt.c and ctx_switch_x86_64.S directly.
 */
#include "rt.h"
#include "greenthread.h"
#include "scheduler.h"

#include <errno.h>
#include <pthread.h>
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

/* Forward-declared so squeue_push (just below) can ask "is my caller a green
 * thread or an ordinary OS thread" without needing struct rt_green's full
 * definition visible yet -- that definition (and tls_current_green, which
 * this answers from) comes much later in this file, where every OTHER
 * green-thread-identity question this file asks is already answered the
 * same way (rt_sched_current_carrier/rt_sched_current_green_id, both public,
 * both also defined down there). Defined right next to tls_current_green. */
static bool rt_sched_in_green_thread(void);

/* A FIFO of green-thread ids parked on "the queue is full" -- the exact
 * same shape as rt.c's own Chan waiter queue (rt_chan_wqueue_t there), kept
 * as its own small copy here rather than shared across the rt.c/scheduler.c
 * layering boundary, the same "correctness over cleverness, no shared
 * plumbing across layers" choice reactor.c's own waiter map already makes.
 * See squeue_push/squeue_draw below for why this exists at all: a condvar's
 * wait queue is only a safe place to block an ordinary OS thread, never a
 * green thread, which must never block its carrier. */
typedef struct rt_squeue_waiter {
    uint32_t                  id;
    struct rt_squeue_waiter  *next;
} rt_squeue_waiter_t;

static void sqwq_push(rt_squeue_waiter_t **head, rt_squeue_waiter_t **tail,
                       uint32_t id) {
    rt_squeue_waiter_t *n = malloc(sizeof *n);
    if (n == NULL) rt_trap("out of memory: scheduler queue waiter");
    n->id = id;
    n->next = NULL;
    if (*tail != NULL) (*tail)->next = n; else *head = n;
    *tail = n;
}

/* Detach the whole waiter list -- every one of them re-checks its own
 * while-loop condition on resume (same reasoning as squeue_draw's existing
 * pthread_cond_broadcast, just for the green-thread side of the same
 * backpressure signal), so waking all of them whenever ANY room frees up is
 * simply the green-thread-safe equivalent of that broadcast, not a
 * separate, weaker guarantee. */
static rt_squeue_waiter_t *sqwq_drain(rt_squeue_waiter_t **head,
                                       rt_squeue_waiter_t **tail) {
    rt_squeue_waiter_t *n = *head;
    *head = *tail = NULL;
    return n;
}

typedef struct {
    pthread_mutex_t      lock;
    pthread_cond_t        not_full;
    rt_green_t           **buf;
    uint32_t              cap;
    uint32_t              head;
    uint32_t              len;
    /* Green-thread pushers parked here instead of on `not_full` -- see
     * squeue_push's own comment for why the two cases cannot share one
     * waiting mechanism. */
    rt_squeue_waiter_t   *full_head;
    rt_squeue_waiter_t   *full_tail;
} rt_squeue_t;

static void squeue_init(rt_squeue_t *q, uint32_t cap) {
    q->buf = malloc(sizeof(rt_green_t *) * cap);
    if (q->buf == NULL) rt_trap("out of memory: scheduler global queue");
    q->cap = cap;
    q->head = 0;
    q->len = 0;
    q->full_head = q->full_tail = NULL;
    pthread_mutex_init(&q->lock, NULL);
    pthread_cond_init(&q->not_full, NULL);
}

static void squeue_destroy(rt_squeue_t *q) {
    pthread_mutex_destroy(&q->lock);
    pthread_cond_destroy(&q->not_full);
    free(q->buf);
    rt_squeue_waiter_t *n = q->full_head;
    while (n != NULL) { rt_squeue_waiter_t *next = n->next; free(n); n = next; }
}

/* Blocks while full -- real backpressure, matching the design doc's stated
 * semantics exactly: "a push blocks until space frees", never silent
 * unbounded growth and never a dropped item.
 *
 * TWO DIFFERENT WAYS TO BLOCK, chosen by who is calling -- a real,
 * reproduced deadlock this distinction fixes, not a theoretical one. An
 * ordinary OS thread (nothing has ever `spawn`ed from it, or a carrier's own
 * post-dispatch bookkeeping, which runs with no green thread "currently
 * running" on that OS thread -- see tls_current_green) blocking here via
 * `pthread_cond_wait` is correct: some OTHER thread -- another carrier -- is
 * always free to drain the queue and signal `not_full`. But when the caller
 * IS a green thread (`spawn` called from ordinary program code, which always
 * runs AS a green thread -- rt_run_program wraps even the top level as one),
 * blocking the CARRIER this green thread happens to be running on is wrong
 * the instant that carrier is the only one that could ever call
 * squeue_draw and free room: the carrier is now blocked waiting on itself,
 * forever. `LANG_NUM_CARRIERS=1` plus a tight `spawn` loop past the queue's
 * capacity reproduces this exactly (docs/concurrency-decision.md, "Phase
 * 3.5"). The fix mirrors Chan's own send/recv exactly (rt.c): park the
 * GREEN THREAD instead of blocking the carrier, which frees the carrier to
 * go dispatch other work (including, eventually, whatever drains this very
 * queue) while this one waits its turn. The enqueue (sqwq_push) happens
 * BEFORE the unlock, the same lost-wakeup fix Chan's own comment explains:
 * a squeue_draw that runs in the gap between this unlock and the actual
 * rt_sched_park call will already find this id on the list and unpark it,
 * which rt_sched_park's own CAS protocol (scheduler.h) is documented to
 * handle correctly no matter which order those two happen in. */
static void squeue_push(rt_squeue_t *q, rt_green_t *g) {
    bool in_green = rt_sched_in_green_thread();

    pthread_mutex_lock(&q->lock);
    while (q->len == q->cap) {
        if (in_green) {
            sqwq_push(&q->full_head, &q->full_tail, rt_sched_current_green_id());
            pthread_mutex_unlock(&q->lock);
            rt_sched_park(RT_GT_PARKED_QUEUE);
            pthread_mutex_lock(&q->lock);
        } else {
            pthread_cond_wait(&q->not_full, &q->lock);
        }
    }
    q->buf[(q->head + q->len) % q->cap] = g;
    q->len++;
    pthread_mutex_unlock(&q->lock);
}

/* Non-blocking: draws min(available, max_n) into out[0..n), no minimum
 * threshold -- a single available item is drawn immediately. Returns the
 * count actually drawn, which may be 0. */
static uint32_t squeue_draw(rt_scheduler_t *s, rt_squeue_t *q,
                             rt_green_t **out, uint32_t max_n) {
    pthread_mutex_lock(&q->lock);
    uint32_t n = q->len < max_n ? q->len : max_n;
    for (uint32_t i = 0; i < n; i++) {
        out[i] = q->buf[(q->head + i) % q->cap];
    }
    q->head = (q->head + n) % q->cap;
    q->len -= n;
    rt_squeue_waiter_t *woken = NULL;
    if (n > 0) {
        /* Freed room: every blocked pusher re-checks its own while-loop
         * condition, so a broadcast here is simply correct, not merely
         * convenient -- no lost wakeup, no lost push. Same for the
         * green-thread side: drain and unpark every one of them, below,
         * after releasing this lock (rt_sched_unpark must never be called
         * while holding a lock a park/unpark participant might need). */
        pthread_cond_broadcast(&q->not_full);
        woken = sqwq_drain(&q->full_head, &q->full_tail);
    }
    pthread_mutex_unlock(&q->lock);
    while (woken != NULL) {
        rt_squeue_waiter_t *next = woken->next;
        rt_sched_unpark(s, woken->id);
        free(woken);
        woken = next;
    }
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

/* CI found a real weakness here (both an x86_64 and an aarch64 runner, same
 * push, same symptom: 30/30 trials of scheduler_test.c's [wake-variance]
 * check woke the identical carrier): time(NULL) has 1-SECOND resolution,
 * and every caller of this function (rt_sched_create, repeatedly, in a
 * tight loop -- that same test creates 30 schedulers back to back) can
 * easily call it many times within one second, making this term a
 * constant in practice rather than real entropy. getpid() and &s (this
 * function's own stack slot, identical on every call from the same call
 * site) are likewise constant across such a loop, so in that case the
 * ENTIRE seed's diversity was resting on `salt` alone -- a plain
 * arithmetic progression (salt, salt+C, salt+2C, ...), which xorshift64*
 * does not always diffuse as well as independent seeds. A monotonic clock
 * read at nanosecond resolution does not have this failure mode: it
 * genuinely differs between calls microseconds apart, which is exactly
 * the case that broke. */
static uint64_t seed_from_entropy(void) {
    static _Atomic uint64_t salt = 1;
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    uint64_t s = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
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
 * A portable "wake semaphore": mutex + condvar + an explicit count,
 * emulating exactly the one property of an unnamed POSIX semaphore
 * (sem_init/sem_post/sem_timedwait/sem_destroy) this file actually needs --
 * a post that happens before the matching wait is REMEMBERED, not lost, so
 * `rt_wake_sem_timedwait` below can still return immediately even if the
 * matching `rt_wake_sem_post` ran first. A bare condvar does NOT have this
 * property (a `pthread_cond_signal` with nobody blocked on it yet vanishes),
 * which is exactly why this needs its own count rather than being a condvar
 * alone -- see carrier_main's own comment, below, for why that property is
 * load-bearing for closing a real lost-wakeup window, not a nicety.
 *
 * This replaces a real `sem_t wake_sem` this file used to carry directly.
 * Found and replaced the same night as the aarch64 port and the kqueue
 * reactor, by this branch's own macOS CI run (`.github/workflows/macos.yml`,
 * a real macos-14 runner) failing at compile time on `sem_timedwait` --
 * "call to undeclared function" -- NOT found by inspection beforehand.
 * macOS/Darwin's <semaphore.h> declares the unnamed-semaphore API only
 * incompletely (sem_timedwait is missing outright on recent SDKs; sem_init
 * exists but is documented by Apple as deprecated and, per long-standing
 * reports, returns ENOSYS at runtime) -- only NAMED semaphores (sem_open)
 * are actually supported there, which this runtime has no use for (naming
 * a kernel-wide semaphore per carrier, then unlinking it correctly on every
 * exit path including a crash, is real extra complexity this file does not
 * need). A mutex+condvar+count is POSIX-standard on both platforms (no
 * platform-specific attribute is set on either the mutex or the condvar, so
 * both default to behavior POSIX guarantees identically -- in particular
 * the condvar's default clock is CLOCK_REALTIME on both glibc and Darwin's
 * libpthread, matching the CLOCK_REALTIME-based absolute `struct timespec`
 * this file already builds for the timed wait, unchanged from what
 * sem_timedwait itself required), so one implementation covers both
 * platforms -- no #ifdef, per this project's standing "no two ways to do
 * the same thing" preference (docs/concurrency-decision.md). */
typedef struct rt_wake_sem {
    pthread_mutex_t lock;
    pthread_cond_t  cond;
    unsigned int    count;
} rt_wake_sem_t;

static int rt_wake_sem_init(rt_wake_sem_t *ws) {
    if (pthread_mutex_init(&ws->lock, NULL) != 0) return -1;
    if (pthread_cond_init(&ws->cond, NULL) != 0) {
        pthread_mutex_destroy(&ws->lock);
        return -1;
    }
    ws->count = 0;
    return 0;
}

static void rt_wake_sem_destroy(rt_wake_sem_t *ws) {
    pthread_mutex_destroy(&ws->lock);
    pthread_cond_destroy(&ws->cond);
}

/* Increments the count under the lock, then signals -- exactly
 * sem_post's own "remembered until consumed" contract: a post that lands
 * with nobody waiting yet is not lost, it just makes the count nonzero for
 * the next rt_wake_sem_timedwait to find. */
static void rt_wake_sem_post(rt_wake_sem_t *ws) {
    pthread_mutex_lock(&ws->lock);
    ws->count++;
    pthread_mutex_unlock(&ws->lock);
    pthread_cond_signal(&ws->cond);
}

/* sem_timedwait's replacement: block until the count is nonzero (consuming
 * one post) or `abstime` (a CLOCK_REALTIME-based absolute deadline, same
 * shape the old sem_timedwait call built) passes, whichever comes first.
 * Returns true if a post was consumed, false on timeout. The `while
 * (count == 0)` -- not `if` -- is the same spurious-wakeup-safe shape every
 * condvar wait in this codebase already uses elsewhere, and it is also
 * exactly what re-checks the deadline on every spurious or stolen wakeup
 * rather than trusting a single wait call to mean one real event. */
static bool rt_wake_sem_timedwait(rt_wake_sem_t *ws, const struct timespec *abstime) {
    bool got_post = false;
    pthread_mutex_lock(&ws->lock);
    while (ws->count == 0) {
        int rc = pthread_cond_timedwait(&ws->cond, &ws->lock, abstime);
        if (rc != 0) break; /* ETIMEDOUT, or an error -- either way, stop waiting */
    }
    if (ws->count > 0) {
        ws->count--;
        got_post = true;
    }
    pthread_mutex_unlock(&ws->lock);
    return got_post;
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
 * other thread ever touches are `idle` (atomic) and `wake_sem` (a mutex/
 * condvar/count wake primitive, see `rt_wake_sem_t` above) -- the wake
 * primitive, not the run queue -- which is exactly
 * what "individually addressable wake primitive" in the design doc means
 * and nothing more.
 *
 * PHASE 3 AMENDS THIS, NARROWLY AND DELIBERATELY: the blocking-FFI monitor
 * (see "blocking-FFI handoff" below) is a second, rare, explicitly-admitted
 * exception to "nothing reaches into another carrier's local buffer" --
 * reaching in only ever through `local_lock`, which this carrier's own
 * thread ALSO takes around every one of its own touches of
 * `local`/`local_head`/`local_len`: `local_push` (carrier_dispatch's
 * post-switch bookkeeping) AND carrier_main's own draw-and-reset/pop in its
 * dispatch loop, below. This used to be narrower -- only local_push took
 * the lock, and carrier_main's own draw/pop stayed lock-free on the belief
 * that try_handoff only ever runs while this carrier is PROVABLY not
 * concurrently executing its own loop at all (genuinely blocked inside a
 * real syscall). That belief was found FALSE and fixed:
 * docs/concurrency-decision.md's Phase 3.5 section ("the try_handoff/
 * carrier_main race") has the full, TSan-confirmed account -- in short, a
 * `prim` that parks the green thread (switching this carrier straight back
 * into carrier_main's loop) without perfectly clearing `blocking_since_ns`
 * for every call shape can leave this carrier looking "stuck" to the
 * monitor while it is actually right here, racing try_handoff's locked
 * mutation of these exact fields with its own unlocked ones. Locking every
 * touch, not just local_push, closes this unconditionally -- correct no
 * matter what makes try_handoff fire, not merely whenever the "stuck"
 * belief happens to be true. Still one mutex, taken at most a few times per
 * green-thread dispatch (once per local_pop, once per draw, once per
 * local_push), not a general-purpose lock held across anything blocking --
 * see try_handoff's own comment for why it is released before its
 * squeue_push calls, and carrier_main's loop for why the draw and the pop
 * are each their own short critical section. `blocking_rec` is this
 * carrier's own blocking-FFI record (runtime/rt.h), registered into this OS
 * thread's TLS once, at carrier_main startup. */

typedef struct rt_carrier {
    struct rt_scheduler *sched;
    uint32_t             index;
    pthread_t            os_thread;

    rt_wake_sem_t wake_sem;
    atomic_bool   idle;

    rt_green_t **local;
    uint32_t     local_cap;
    uint32_t     local_head;
    uint32_t     local_len;

    /* Phase 3: see the comment above for exactly what this does and does
     * not protect. */
    pthread_mutex_t   local_lock;
    rt_blocking_rec_t blocking_rec;

    _Atomic uint64_t dispatched;
} rt_carrier_t;

/* ========================================================================
 * Phase 3 -- the live-green-thread registry: id -> rt_green_t*, for the
 * WHOLE life of every green thread this scheduler ever spawns, not just
 * while parked.
 *
 * Why the whole life, not just while parked: rt_sched_unpark must be safe
 * to call BEFORE the matching rt_sched_park (that is the entire lost-wakeup
 * fix -- see scheduler.h's "park/unpark" section), which means the
 * information "is green thread N parked, and if so, which control block is
 * it" has to be findable by id from the moment a thread exists, not only
 * from the moment it actually parks -- there is no earlier hook to
 * populate a park-only map from.
 *
 * Mutex-protected open addressing, not lock-free: "correctness over
 * cleverness" is this project's own stated preference (see the shared
 * global queue just above, and runtime/greenthread.c's slab free-list
 * comment making the identical choice), and this is not a hot path --
 * inserted once per spawn, removed once per completion, looked up once per
 * rt_sched_unpark call, none of which happen anywhere near as often as an
 * ordinary dispatch.
 *
 * THE OTHER JOB THIS LOCK DOES: rt_sched_unpark must never touch a
 * `rt_green_t` that carrier_dispatch has already freed. Removal (always
 * paired with the eventual free, in carrier_dispatch's finished branch)
 * happens-before the free, under this same lock, and rt_sched_unpark reads
 * the map and touches the green thread's `park_word` in one critical
 * section under this same lock -- so either a lookup completes entirely
 * before a racing removal (sees the live pointer, which cannot be freed
 * until this lock is released and removal gets its turn), or entirely
 * after it (finds nothing, returns false). No lookup can ever observe a
 * pointer whose removal has already started. */
typedef struct {
    pthread_mutex_t lock;
    uint32_t       *keys;   /* RT_REG_EMPTY / RT_REG_TOMBSTONE / a real id */
    rt_green_t    **vals;
    uint32_t        cap;
    uint32_t        used;   /* occupied slots INCLUDING tombstones */
    uint32_t        live;   /* occupied slots EXCLUDING tombstones */
} rt_registry_t;

#define RT_REG_EMPTY     UINT32_MAX
#define RT_REG_TOMBSTONE (UINT32_MAX - 1)
/* Real ids never reach either sentinel: rt_sched_spawn traps before
 * next_id could ever exceed RT_SCHED_ID_CAPACITY (far below both). */

static void registry_init(rt_registry_t *r, uint32_t cap) {
    r->cap = cap;
    r->keys = malloc(sizeof(uint32_t) * cap);
    r->vals = malloc(sizeof(rt_green_t *) * cap);
    if (r->keys == NULL || r->vals == NULL) {
        rt_trap("out of memory: scheduler green-thread registry");
    }
    for (uint32_t i = 0; i < cap; i++) r->keys[i] = RT_REG_EMPTY;
    r->used = 0;
    r->live = 0;
    pthread_mutex_init(&r->lock, NULL);
}

static void registry_destroy(rt_registry_t *r) {
    pthread_mutex_destroy(&r->lock);
    free(r->keys);
    free(r->vals);
}

/* Lock already held. Insert into THIS table (used directly, and again by
 * resize's rehash below). `id` is assumed not already present -- true for
 * every real caller: a fresh spawn's id has never been seen before
 * (next_id is monotonic and never reused), and resize only ever re-inserts
 * keys that were already distinct in the old table. */
static void registry_insert_locked(rt_registry_t *r, uint32_t id, rt_green_t *g) {
    uint32_t i = id % r->cap;
    while (r->keys[i] != RT_REG_EMPTY && r->keys[i] != RT_REG_TOMBSTONE) {
        i = (i + 1) % r->cap;
    }
    r->keys[i] = id;
    r->vals[i] = g;
}

/* Lock already held. Doubles capacity once `used` (occupied, including
 * tombstones -- a tombstone still costs a probe step, so it counts against
 * the load factor exactly like a live entry) would exceed half of it.
 * Rehashing drops every tombstone, which is also the only thing that ever
 * reclaims the space a removal leaves behind. */
static void registry_maybe_grow_locked(rt_registry_t *r) {
    if ((uint64_t)(r->used + 1) * 2 <= r->cap) return;

    uint32_t old_cap = r->cap;
    uint32_t *old_keys = r->keys;
    rt_green_t **old_vals = r->vals;

    uint32_t new_cap = old_cap * 2;
    r->keys = malloc(sizeof(uint32_t) * new_cap);
    r->vals = malloc(sizeof(rt_green_t *) * new_cap);
    if (r->keys == NULL || r->vals == NULL) {
        rt_trap("out of memory: growing the scheduler green-thread registry");
    }
    for (uint32_t i = 0; i < new_cap; i++) r->keys[i] = RT_REG_EMPTY;
    r->cap = new_cap;

    for (uint32_t i = 0; i < old_cap; i++) {
        if (old_keys[i] != RT_REG_EMPTY && old_keys[i] != RT_REG_TOMBSTONE) {
            registry_insert_locked(r, old_keys[i], old_vals[i]);
        }
    }
    r->used = r->live; /* every tombstone was just dropped */
    free(old_keys);
    free(old_vals);
}

static void registry_insert(rt_registry_t *r, uint32_t id, rt_green_t *g) {
    pthread_mutex_lock(&r->lock);
    registry_maybe_grow_locked(r);
    registry_insert_locked(r, id, g);
    r->used++;
    r->live++;
    pthread_mutex_unlock(&r->lock);
}

static void registry_remove(rt_registry_t *r, uint32_t id) {
    pthread_mutex_lock(&r->lock);
    uint32_t i = id % r->cap;
    uint32_t steps = 0;
    while (r->keys[i] != RT_REG_EMPTY && steps < r->cap) {
        if (r->keys[i] == id) {
            r->keys[i] = RT_REG_TOMBSTONE;
            r->vals[i] = NULL;
            r->live--;
            break;
        }
        i = (i + 1) % r->cap;
        steps++;
    }
    pthread_mutex_unlock(&r->lock);
}

/* Lock already held (by the caller -- rt_sched_unpark takes it, does this
 * lookup, and acts on the result -- the exchange on g->park_word, and
 * nothing that can block -- all before releasing it; see that function for
 * exactly why staying under the lock for that long, and no longer, is what
 * makes it safe). `*found` says whether `id` is present at all; the
 * returned pointer is meaningless when it is false. */
static rt_green_t *registry_lookup_locked(rt_registry_t *r, uint32_t id,
                                           bool *found) {
    uint32_t i = id % r->cap;
    uint32_t steps = 0;
    while (r->keys[i] != RT_REG_EMPTY && steps < r->cap) {
        if (r->keys[i] == id) {
            *found = true;
            return r->vals[i];
        }
        i = (i + 1) % r->cap;
        steps++;
    }
    *found = false;
    return NULL;
}

/* ========================================================================
 * The scheduler itself.
 * ====================================================================== */

struct rt_scheduler {
    uint32_t      n_carriers;
    rt_carrier_t *carriers;

    rt_squeue_t    global;
    rt_wake_perm_t perm;
    rt_registry_t  registry;

    uint32_t fuel_size;
    uint32_t queue_cap;

    atomic_bool shutdown;

    _Atomic uint32_t next_id;
    _Atomic uint64_t total_spawned;
    _Atomic uint64_t total_completed;
    _Atomic uint32_t max_draw_seen;

    /* DIAGNOSTIC ONLY, not for master -- see
     * .claude/worktrees/diagnostics-fuel-dip-investigation's own commits.
     * Direct evidence for the fuel_size/notify_new_work contention
     * hypothesis: how many times the shared queue is actually drawn from
     * and how many items each draw gets (average batch size vs fuel_size),
     * and how many steps notify_new_work's wake-permutation walk takes per
     * push, including how often a full lap finds nobody idle. Relaxed
     * atomics: these are approximate counters for a diagnostic probe, not
     * a correctness mechanism, so no ordering guarantee is needed beyond
     * each individual add being atomic. */
    _Atomic uint64_t diag_draw_calls;
    _Atomic uint64_t diag_draw_items;
    _Atomic uint64_t diag_notify_calls;
    _Atomic uint64_t diag_notify_steps;
    _Atomic uint64_t diag_notify_full_laps;

    /* EXPERIMENTAL FIX v2, not for master without further validation. A
     * single shared, deliberately asymmetric hint, not a precise count --
     * v1 (reverted, see that commit) tried to keep an EXACT idle_count by
     * turning two plain stores on carrier_main's hottest path into atomic
     * exchanges, which made things WORSE: those two sites fire on every
     * single dispatch cycle for every carrier, far hotter than
     * notify_new_work's own lock, so the RMW cost added there dwarfed the
     * lock cost it was meant to avoid. This version touches the BUSY
     * transition not at all -- only "a carrier went idle" sets this (a
     * plain, unconditional, relaxed store: always writing `true` needs no
     * old-value check, so no RMW), and only a full permutation walk that
     * confirms nobody is actually idle clears it, piggybacking on work
     * notify_new_work is already doing in that case rather than adding a
     * new write anywhere else. A stale `true` costs one extra walk; it can
     * never cause a missed wakeup the existing 5ms periodic fallback
     * (carrier_main) does not already cover. */
    _Atomic bool maybe_idle;

    /* Blocking-FFI handoff monitor (opt-in; see
     * rt_sched_start_blocking_monitor in scheduler.h). */
    pthread_t         monitor_thread;
    atomic_bool       monitor_started;
    atomic_bool       monitor_stop;
    uint64_t          monitor_timeout_ns;
    uint64_t          monitor_poll_ns;
    _Atomic uint64_t  total_handoffs;
    _Atomic uint64_t  total_handoff_items;
};

/* Park/unpark's state word. FOUR states, not three -- scheduler.h's own
 * comment describes the three-state EMPTY/PARKED/NOTIFIED shape this is
 * based on (the standard LockSupport/std::thread::park shape), but a plain
 * three-state version has a real bug this project's own TSan gate caught
 * directly (see this enum's own trailing comment for the exact mechanism),
 * which is why a fourth, ARMED, state exists.
 *
 * THE BUG A THREE-STATE VERSION HAS: rt_sched_park's CAS necessarily runs
 * on the green thread's OWN stack, BEFORE it has actually switched away
 * (rt_fiber_switch has not even been called yet at the point the CAS would
 * need to run, to decide whether to switch away AT ALL). If that CAS
 * directly published PARKED -- "externally resumable now" -- an unpark
 * racing in during the (real, nonzero) window between the CAS and the
 * ACTUAL context switch completing could push `g` onto the shared queue,
 * have a DIFFERENT carrier draw it, and call rt_fiber_switch INTO g->ctx
 * while g->ctx is still the STALE context from the last time it was saved
 * (or, on a thread's very first park, the original entry-point context
 * rt_ctx_make built) -- NOT the context this park call is in the middle of
 * creating. That second carrier would then start running g's code FROM
 * WHEREVER THAT STALE CONTEXT POINTS, on the SAME 64 KiB stack the first
 * carrier's OS thread is STILL ACTIVELY EXECUTING ON. Two OS threads
 * running on one stack at once is immediate, silent corruption -- observed
 * directly, under TSan, as a SEGV inside rt_gtstate_set/green_trampoline
 * with a faulting address right at a stack/page boundary, while Phase 2's
 * own TSan gate (scheduler_tsan.sh, no park/unpark at all) stayed clean
 * across 10 consecutive runs -- conclusive evidence this is a Phase 3 park/
 * unpark bug, not a pre-existing fiber-switch/TSan interaction.
 *
 * THE FIX: splitting "decided to park" (ARMED) from "actually safely
 * parked, off this thread's own stack, genuinely resumable elsewhere"
 * (PARKED) into two distinct states, with the EMPTY/ARMED -> PARKED
 * transition made ONLY by carrier_dispatch, AFTER rt_fiber_switch has
 * already returned there -- i.e. only once g->ctx is provably saved and
 * this OS thread is provably no longer running on g's stack. rt_sched_unpark
 * treats EMPTY and ARMED identically (neither is safe to act on physically;
 * both just record NOTIFIED for whoever checks next to find), and only
 * ever pushes `g` onto the shared queue when it finds the thread was
 * ALREADY, provably, PARKED. See rt_sched_park and carrier_dispatch's
 * "parked" branch for the two sides of this. */
enum {
    RT_PARK_EMPTY = 0,
    RT_PARK_ARMED = 1,
    RT_PARK_PARKED = 2,
    RT_PARK_NOTIFIED = 3,
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

    /* Phase 3 -- see scheduler.h's "park/unpark" section. */
    rt_scheduler_t *sched;      /* set once, at spawn; never changes */
    _Atomic int      park_word; /* RT_PARK_* */
    bool             parked;    /* set immediately before the ONE switch-away
                                  * this value describes -- see rt_sched_park
                                  * and rt_sched_yield, and carrier_dispatch's
                                  * post-switch branch, which reads it exactly
                                  * once per dispatch, right after the switch
                                  * that is either a park or a yield returns. */
    bool             notified_before_park; /* set alongside `parked`; see
                                  * rt_sched_park's own comment on why EVERY
                                  * call to it performs a real switch, even
                                  * when it already knows, before switching,
                                  * that it must requeue itself immediately
                                  * rather than genuinely wait. */

    /* Diagnostic guard for the unresolved rt_stack_limit race
     * (docs/concurrency-decision.md, "Phase 3.5"). Hypothesis (b) there --
     * the same green thread briefly live on two carriers at once -- was
     * never caught in the act because nothing makes that condition loud:
     * if it happens, both carriers silently run rt_fiber_switch against the
     * same g->ctx/g->stack, and whichever corruption follows surfaces far
     * from its actual cause. This CAS's only job is to turn that silent
     * double-dispatch into an immediate, pinpointed rt_trap instead --
     * CAS false->true in carrier_dispatch right before rt_fiber_switch,
     * false->... reset to false right after it returns. Not a fix; a smoke
     * detector. */
    _Atomic bool     claimed_by_carrier;
};

/* Which green thread (if any) is running on THIS OS thread right now, and
 * which carrier index this OS thread is. Both thread-local for the same
 * reason rt_stack_limit (rt.h) is: one OS thread runs one green thread's
 * code at a time, and a carrier's own identity does not change for the
 * life of that OS thread. */
static _Thread_local rt_green_t *tls_current_green = NULL;
static _Thread_local uint32_t    tls_carrier_index = UINT32_MAX;

#if RT_TSAN_BUILD
/* This carrier OS thread's own native TSan fiber identity -- captured once,
 * in carrier_main, before this thread ever switches into a green thread's
 * fiber for the first time. See greenthread.h's "A note on ThreadSanitizer"
 * comment for the full account of why this runtime needs fiber identities
 * at all; this is the "switch back to being plain old carrier N" side of
 * every pair, used by rt_sched_yield/rt_sched_park/green_trampoline right
 * before each of their own switch-aways, mirroring carrier_dispatch's own
 * switch-in (which targets g->stack.tsan_fiber instead). Thread-local for
 * the same reason tls_current_green/tls_carrier_index are: this identity
 * belongs to the OS thread, not to whichever green thread it happens to be
 * running right now. */
static _Thread_local void *tls_carrier_tsan_fiber = NULL;
#endif

/* squeue_push's own forward declaration, above, explains why this exists:
 * "is the calling OS thread currently running a green thread, or is it some
 * other kind of caller" (an ordinary OS thread, or a carrier's own
 * post-dispatch bookkeeping, which runs with tls_current_green already
 * cleared -- see carrier_dispatch). */
static bool rt_sched_in_green_thread(void) {
    return tls_current_green != NULL;
}

/* ---- the wake notification itself -------------------------------------- */

/* On every push, advance through the shared permutation, skip any carrier
 * whose idle flag is false, wake the first idle one found via ITS OWN
 * semaphore, then clear its idle flag -- atomically, via CAS, so a
 * concurrent notify (from another simultaneous push) cannot also claim the
 * same carrier. If a full lap (n_carriers steps) finds nobody idle, do
 * nothing: everyone is already busy, exactly per the design doc. */
static void notify_new_work(rt_scheduler_t *s) {
    rt_wake_perm_t *p = &s->perm;
    /* DIAGNOSTIC ONLY, not for master -- see diag_notify_calls' own comment. */
    atomic_fetch_add_explicit(&s->diag_notify_calls, 1, memory_order_relaxed);

    /* EXPERIMENTAL FIX v2, not for master without further validation -- see
     * maybe_idle's own comment for why this is safe and why v1 (reverted)
     * was not merely slower but actually counter-productive. One relaxed
     * load, no lock, no RMW: skips straight to "nothing to do" in exactly
     * the 80-95%-of-calls case already measured, at zero cost to the
     * carrier-side dispatch path that cost v1 the whole benefit. */
    if (!atomic_load_explicit(&s->maybe_idle, memory_order_relaxed)) {
        return;
    }

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
            rt_wake_sem_post(&c->wake_sem);
            pthread_mutex_unlock(&p->lock);
            /* DIAGNOSTIC ONLY, not for master. */
            atomic_fetch_add_explicit(&s->diag_notify_steps, steps + 1,
                                       memory_order_relaxed);
            return;
        }
    }
    pthread_mutex_unlock(&p->lock);
    /* EXPERIMENTAL FIX v2, not for master without further validation. This
     * walk just confirmed, for real, that nobody is idle -- piggyback that
     * confirmation onto maybe_idle rather than paying for a separate check
     * anywhere else. Plain store: the next carrier to go idle sets this
     * back to true independently and unconditionally, so there is no
     * old-value race to get wrong here either. */
    atomic_store_explicit(&s->maybe_idle, false, memory_order_relaxed);
    /* DIAGNOSTIC ONLY, not for master. */
    atomic_fetch_add_explicit(&s->diag_notify_steps, s->n_carriers,
                               memory_order_relaxed);
    atomic_fetch_add_explicit(&s->diag_notify_full_laps, 1,
                               memory_order_relaxed);
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
#if RT_TSAN_BUILD
    /* Tell TSan this OS thread is about to stop being green thread g's
     * fiber and go back to being plain carrier tls_carrier_index, BEFORE
     * the actual stack switch below performs it -- see greenthread.h's "A
     * note on ThreadSanitizer" comment for why this has to be a single
     * call made by the originating side, not a pair bracketing the switch
     * the way ASan's (unused) fiber API would need. */
    __tsan_switch_to_fiber(tls_carrier_tsan_fiber, 0);
#endif
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
    /* Freshly false before THIS switch-away, so carrier_dispatch's
     * post-switch read (which fires the instant this call below returns
     * control to it) sees "ordinary yield", not a stale true left over
     * from some earlier, unrelated park cycle on this same green thread --
     * see struct rt_green's own comment on `parked` for why every
     * switch-away has to set this fresh rather than relying on whatever
     * the field already held. */
    g->parked = false;
#if RT_TSAN_BUILD
    /* Tell TSan this OS thread is about to stop being green thread g's
     * fiber and go back to being plain carrier tls_carrier_index, BEFORE
     * the actual stack switch below performs it -- see greenthread.h's "A
     * note on ThreadSanitizer" comment for why this has to be a single
     * call made by the originating side, not a pair bracketing the switch
     * the way ASan's (unused) fiber API would need. */
    __tsan_switch_to_fiber(tls_carrier_tsan_fiber, 0);
#endif
    rt_fiber_switch(&g->ctx, g->carrier_ctx, 0);
    /* Resumed: carrier_dispatch already set RT_GT_RUNNING and
     * tls_current_green again before switching back in, below -- nothing
     * to do here. */
}

uint32_t rt_sched_current_carrier(void) {
    return tls_carrier_index;
}

uint32_t rt_sched_current_green_id(void) {
    rt_green_t *g = tls_current_green;
    if (g == NULL) {
        rt_trap("rt_sched_current_green_id called from outside a running "
                "green thread");
    }
    return g->id;
}

/* ========================================================================
 * Park / unpark -- see scheduler.h's own, much longer, comment for the full
 * protocol and the race it closes. This is the implementation of exactly
 * that protocol and nothing more.
 * ====================================================================== */

void rt_sched_park(rt_green_state_t parked_state) {
    rt_green_t *g = tls_current_green;
    if (g == NULL) {
        rt_trap("rt_sched_park called from outside a running green thread");
    }

    rt_gtstate_set(g->id, parked_state);

    /* Stage 1 of 2 -- see the park_word enum's own long comment for why
     * this is split into two stages at all and exactly what bug a single
     * CAS here (straight to PARKED) actually has. This CAS only ever goes
     * EMPTY -> ARMED, NEVER straight to PARKED: ARMED means "decided to
     * park, about to switch away", and is NOT yet safe for an external
     * rt_sched_unpark to act on physically, because the actual context
     * switch below has not happened yet -- g->ctx does not yet hold a
     * valid, safely-resumable saved state. Only this green thread's own
     * carrier ever calls rt_sched_park for it, and only ever sequentially,
     * so the only two values this word can hold at this exact point are
     * EMPTY (ordinary case) or NOTIFIED (an rt_sched_unpark for this exact
     * id already landed in the gap before we got here -- the lost-wakeup
     * race, closed). ARMED or PARKED here would mean this function
     * re-entered itself without an intervening full park/unpark cycle,
     * which cannot happen and would be a real bug in this file -- rt_trap
     * says so loudly rather than silently corrupting the state machine. */
    int expected = RT_PARK_EMPTY;
    bool already_notified = false;
    if (!atomic_compare_exchange_strong_explicit(
            &g->park_word, &expected, RT_PARK_ARMED, memory_order_acq_rel,
            memory_order_acquire)) {
        if (expected != RT_PARK_NOTIFIED) {
            rt_trap("rt_sched_park: impossible park_word state (not EMPTY, "
                    "not NOTIFIED) -- scheduler.c's park/unpark invariant "
                    "is broken");
        }
        /* Already notified before we ever got here: consume it now --
         * reset to EMPTY so the next park cycle starts clean -- but still
         * fall through to an ACTUAL switch-away below rather than
         * returning in place.
         *
         * An earlier version of this function returned immediately here,
         * without ever calling rt_fiber_switch, as a deliberate fast path
         * (correct in principle: nothing is lost, the notification is
         * consumed, there is no stack-sharing hazard since nothing
         * suspends). This project's own TSan gate caught that fast path
         * being measurably LESS stable than the uniform one below: a green
         * thread whose entire dispatch runs start-to-finish through
         * rt_ctx_trampoline's synthetic entry point, doing several atomic
         * CAS/exchange operations on park_word along the way, with no
         * actual context switch ever happening in between, intermittently
         * crashed INSIDE ThreadSanitizer's own stack-trace-capture code
         * (__sanitizer::StackDepotBase::Put) -- not in any function this
         * file defines. That is consistent with TSan's own stack-trace
         * unwinder getting confused specifically by that one shape (a
         * trampoline-rooted call chain that never performs the hand-rolled
         * switch its own machinery expects), the same general class of
         * problem already documented for ASan's fiber-switch handling in
         * runtime/greenthread.h, just surfacing as a hard crash instead of
         * a benign warning. Always switching away here, even when the
         * outcome (requeue immediately) is already decided before the
         * switch, keeps every rt_sched_park call shaped like the ordinary,
         * already-proven-stable (Phase 2's own TSan gate) pattern: do the
         * work, then rt_fiber_switch, then let carrier_dispatch finish up
         * on the other side. `notified_before_park` tells carrier_dispatch
         * which of its two post-switch paths to take -- requeue
         * immediately itself (nobody else is ever going to call unpark
         * again for this episode) rather than the ordinary ARMED -> PARKED
         * promotion. */
        atomic_store_explicit(&g->park_word, RT_PARK_EMPTY,
                               memory_order_release);
        already_notified = true;
    }

    /* Genuinely parking (or, if `already_notified`, about to be
     * immediately self-requeued by carrier_dispatch the instant this
     * switch returns there) -- see struct rt_green's comment on `parked`
     * for why these are set fresh, immediately before this specific
     * switch-away. Stage 2 -- promoting ARMED to the genuinely-
     * externally-resumable PARKED, or performing the immediate self-requeue
     * -- happens in carrier_dispatch, AFTER this switch has returned
     * THERE (not here): that is the earliest point at which this green
     * thread is provably off its own stack. */
    g->parked = true;
    g->notified_before_park = already_notified;
#if RT_TSAN_BUILD
    /* Tell TSan this OS thread is about to stop being green thread g's
     * fiber and go back to being plain carrier tls_carrier_index, BEFORE
     * the actual stack switch below performs it -- see greenthread.h's "A
     * note on ThreadSanitizer" comment for why this has to be a single
     * call made by the originating side, not a pair bracketing the switch
     * the way ASan's (unused) fiber API would need. */
    __tsan_switch_to_fiber(tls_carrier_tsan_fiber, 0);
#endif
    rt_fiber_switch(&g->ctx, g->carrier_ctx, 0);
    /* Resumed -- either rt_sched_unpark, or carrier_dispatch's own
     * immediate self-requeue path, pushed us back through the shared queue,
     * and some carrier (not necessarily the one that parked us) dispatched
     * us again; carrier_dispatch already set RT_GT_RUNNING and
     * tls_current_green before switching back in. `park_word` was already
     * reset to EMPTY before we were pushed in every case, so nothing to do
     * to it here. */
}

bool rt_sched_unpark(rt_scheduler_t *s, uint32_t green_id) {
    rt_green_t *g;
    int prev;

    /* The entire critical section is: find `g`, and -- while still holding
     * the SAME lock that removal-before-free (carrier_dispatch's finished
     * branch) also takes -- do the one atomic exchange on its park_word.
     * Nothing in here can block (no squeue_push yet), so holding the
     * registry lock for it is cheap and, more importantly, is what makes
     * touching `g` safe at all: see rt_registry_t's own comment for why a
     * lookup that completes under this lock can never observe a green
     * thread whose removal has already started. */
    pthread_mutex_lock(&s->registry.lock);
    bool found;
    g = registry_lookup_locked(&s->registry, green_id, &found);
    if (!found) {
        pthread_mutex_unlock(&s->registry.lock);
        return false;
    }
    /* Exchange to NOTIFIED, learning the previous value -- see
     * scheduler.h's "park/unpark" section for what each outcome means.
     * acq_rel: acquire pairs with rt_sched_park's own release when it
     * reset this word to EMPTY (or, on the very first park cycle, with the
     * spawn-time atomic_init below); release publishes this write, and
     * everything sequenced before it on THIS thread (nothing, yet, but see
     * below), to whoever next acquires it. */
    prev = atomic_exchange_explicit(&g->park_word, RT_PARK_NOTIFIED,
                                     memory_order_acq_rel);
    pthread_mutex_unlock(&s->registry.lock);

    if (prev != RT_PARK_PARKED) {
        /* prev == EMPTY: recorded for the green thread's own upcoming
         * rt_sched_park to find (the lost-wakeup fix itself).
         * prev == NOTIFIED: already notified -- idempotent, no double
         * wake. Either way, nothing further to do: g is not suspended via
         * this scheduler's queue right now, so there is nothing to push. */
        return true;
    }

    /* Genuinely suspended: pull it back into real circulation. Reset to
     * EMPTY first (g cannot be touched by anything else until WE push it
     * below -- we are the unique winner of the CAS race by construction of
     * atomic_exchange, so no other rt_sched_unpark call can also have
     * observed PARKED for this same park cycle), then reuse the exact same
     * path an ordinary spawn uses: push onto the shared queue, notify the
     * wake permutation. carrier_dispatch does not care whether g->ctx is a
     * fresh trampoline context or a previously-parked one -- rt_fiber_switch
     * resumes either identically -- so no second "resume this saved
     * context" entry kind is needed; g itself already serves as that
     * entry, exactly as it does for a fresh spawn. */
    atomic_store_explicit(&g->park_word, RT_PARK_EMPTY, memory_order_release);
    rt_gtstate_set(g->id, RT_GT_RUNNABLE);
    squeue_push(&s->global, g);
    notify_new_work(s);
    return true;
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

    /* claimed_by_carrier's own comment: this is the actual guard, not just
     * its declaration. A failed CAS here means some OTHER carrier's own
     * rt_fiber_switch into this SAME g->ctx is concurrently in flight right
     * now -- exactly the unresolved double-dispatch hypothesis for the
     * rt_stack_limit race -- caught here, loudly, instead of silently
     * corrupting whichever stack happens to be adjacent. */
    bool not_claimed = false;
    if (!atomic_compare_exchange_strong_explicit(
            &g->claimed_by_carrier, &not_claimed, true, memory_order_acq_rel,
            memory_order_acquire)) {
        rt_trap("carrier_dispatch: green thread already claimed by another "
                "carrier -- double-dispatch detected (see claimed_by_carrier's "
                "comment and docs/concurrency-decision.md's Phase 3.5)");
    }

#if RT_TSAN_BUILD
    /* Tell TSan this OS thread is about to become green thread g's fiber,
     * BEFORE the actual stack switch below performs it -- see
     * greenthread.h's "A note on ThreadSanitizer" comment. g->stack.tsan_fiber
     * was created in rt_stack_alloc (rt_sched_spawn, above) and is destroyed
     * in rt_stack_free once g finishes (this function's own `finished`
     * branch, below) -- never while it could still be the current fiber of
     * any thread, since every switch-away from it (rt_sched_yield/
     * rt_sched_park/green_trampoline) already switches this thread's
     * identity back to tls_carrier_tsan_fiber first. */
    __tsan_switch_to_fiber(g->stack.tsan_fiber, 0);
#endif
    rt_fiber_switch(&loop_ctx, &g->ctx, (uintptr_t)g->stack.base);

    /* Released the instant this carrier is provably done directly driving
     * g's context for this dispatch episode -- before anything below could
     * make g visible to another carrier again (the requeue paths further
     * down, or registry_remove/free in the finished branch). */
    atomic_store_explicit(&g->claimed_by_carrier, false, memory_order_release);

    tls_current_green = NULL;

    if (g->finished) {
        /* Remove from the registry BEFORE freeing -- rt_sched_unpark looks
         * a green thread up and touches it entirely under the registry
         * lock, so this removal (also under that lock) happening-before
         * the free below is what makes that safe: see rt_registry_t's own
         * comment for the full argument. */
        registry_remove(&c->sched->registry, g->id);
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
    } else if (g->parked) {
        if (g->notified_before_park) {
            /* rt_sched_park already knew, before it even switched away,
             * that it had been notified in the gap before reaching its own
             * CAS -- park_word is already back to EMPTY (reset there).
             * Nobody else is ever going to call rt_sched_unpark again for
             * this episode, so requeue g ourselves, immediately -- exactly
             * the same requeue rt_sched_unpark's own "prev == PARKED" path
             * takes, just reached from the other side of the race. See
             * rt_sched_park's own long comment on `already_notified` for
             * why this still goes through a real switch instead of
             * returning in place. */
            rt_gtstate_set(g->id, RT_GT_RUNNABLE);
            squeue_push(&c->sched->global, g);
            notify_new_work(c->sched);
        } else {
            /* Stage 2 of park/unpark -- see the park_word enum's own long
             * comment and rt_sched_park's "Stage 1" comment for why this
             * exists and the exact corruption it prevents. g is now
             * PROVABLY off its own stack (this rt_fiber_switch call just
             * returned), which is the one thing Stage 1's CAS, running
             * before the switch, could not guarantee. Promote ARMED ->
             * PARKED now, making it genuinely safe for an external
             * rt_sched_unpark to resume g->ctx from here on. */
            int expected = RT_PARK_ARMED;
            if (!atomic_compare_exchange_strong_explicit(
                    &g->park_word, &expected, RT_PARK_PARKED,
                    memory_order_acq_rel, memory_order_acquire)) {
                /* expected == NOTIFIED: an unpark raced in during the
                 * window between rt_sched_park's own CAS (Stage 1) and this
                 * promotion -- the external event this was waiting for
                 * already happened, and whatever caused it (a reactor's
                 * readiness callback, a channel send, a timer) will not
                 * call rt_sched_unpark a SECOND time for the same episode.
                 * Nobody else is ever going to requeue g, so we must, right
                 * now, ourselves. */
                if (expected != RT_PARK_NOTIFIED) {
                    rt_trap("carrier_dispatch: impossible park_word state "
                            "promoting ARMED (not NOTIFIED) -- "
                            "scheduler.c's park/unpark invariant is broken");
                }
                atomic_store_explicit(&g->park_word, RT_PARK_EMPTY,
                                       memory_order_release);
                rt_gtstate_set(g->id, RT_GT_RUNNABLE);
                squeue_push(&c->sched->global, g);
                notify_new_work(c->sched);
            }
            /* Otherwise: genuinely, safely parked now. Already findable by
             * id through the registry (inserted at spawn, for its whole
             * life), so whichever thread eventually calls rt_sched_unpark
             * for it will push it back onto the shared queue itself.
             * Deliberately NOT requeued here in that case -- that omission
             * is the entire difference between this branch and the one
             * below. */
        }
    } else {
        /* Ordinary yield: stays on THIS carrier, never anywhere else --
         * the whole of "no stealing" for an already-running green thread.
         * Taken under local_lock -- see rt_carrier_t's own comment for
         * exactly what this one lock does and does not protect: it is the
         * ONLY thing standing between this write and the blocking-FFI
         * monitor (below) draining this same local buffer from another
         * thread, which can only legitimately happen while THIS carrier is
         * stuck in a DIFFERENT dispatch's blocking call -- never this one,
         * since we are plainly not stuck, we just returned -- but it is
         * the lock, not that argument, that makes it true under TSan. */
        pthread_mutex_lock(&c->local_lock);
        local_push(c, g);
        pthread_mutex_unlock(&c->local_lock);
    }
}

static void *carrier_main(void *argp) {
    rt_carrier_t *c = (rt_carrier_t *)argp;
    tls_carrier_index = c->index;
#if RT_TSAN_BUILD
    /* Captured once, before this OS thread ever switches into a green
     * thread's fiber -- see tls_carrier_tsan_fiber's own comment above. */
    tls_carrier_tsan_fiber = __tsan_get_current_fiber();
#endif
    /* Phase 3: so that any `prim` call any green thread this carrier ever
     * dispatches makes can find ITS carrier's own blocking record through
     * TLS, with no argument to pass -- see runtime/rt.h's "blocking FFI"
     * section for why this has to work with zero information at the call
     * site. Registered once, here, for this OS thread's whole life. */
    rt_blocking_register(&c->blocking_rec);

    for (;;) {
        /* Every touch of local/local_head/local_len in this loop -- the
         * emptiness check, the draw-and-reset, and the pop -- is now taken
         * under local_lock, the SAME mutex local_push (carrier_dispatch,
         * above) already takes and try_handoff (below) already takes.
         *
         * This loop used to touch these fields with no lock at all, resting
         * on the invariant that try_handoff only ever runs while this
         * carrier is PROVABLY not concurrently executing this very loop --
         * true for a genuine blocking syscall (the OS thread is inside the
         * kernel, doing nothing else), but NOT guaranteed for every `prim`:
         * runtime/rt.c's rt_wait_io comment documents one concrete way a
         * `prim` can park the green thread -- switching this carrier
         * straight back into this loop -- while leaving blocking_since_ns
         * set, which is exactly what makes the carrier look "stuck" to the
         * monitor while it is actually running right here. rt_wait_io's own
         * exit/enter-blocking dance closes that specific case, but not
         * (per this project's own reading of it, docs/concurrency-decision.md
         * Phase 3.5) every other `prim` call shape, and this was a real,
         * TSan-confirmed data race between this loop and try_handoff as a
         * result. Taking local_lock here closes it unconditionally: no
         * matter what makes try_handoff fire, it and this loop can no
         * longer touch local/local_head/local_len at the same time. */
        rt_green_t *g = NULL;
        bool drew = false;
        uint32_t drew_n = 0;

        pthread_mutex_lock(&c->local_lock);
        if (c->local_len == 0) {
            uint32_t n = squeue_draw(c->sched, &c->sched->global, c->local, c->local_cap);
            /* DIAGNOSTIC ONLY, not for master -- see diag_draw_calls' own
             * comment (scheduler.h). Counted here, not inside squeue_draw
             * itself, because rt_scheduler_t is still an incomplete type at
             * squeue_draw's own definition (it is declared well before
             * `struct rt_scheduler` is), while this call site already has
             * the complete type via c->sched. */
            atomic_fetch_add_explicit(&c->sched->diag_draw_calls, 1,
                                       memory_order_relaxed);
            atomic_fetch_add_explicit(&c->sched->diag_draw_items, n,
                                       memory_order_relaxed);
            if (n > 0) {
                c->local_head = 0;
                c->local_len = n;
                drew = true;
                drew_n = n;
            }
        }
        if (c->local_len > 0) {
            g = local_pop(c);
        }
        pthread_mutex_unlock(&c->local_lock);

        if (g == NULL) {
            if (atomic_load_explicit(&c->sched->shutdown,
                                      memory_order_relaxed)) {
                return NULL;
            }
            /* Genuinely nothing anywhere: go idle. Set the flag BEFORE
             * waiting, not after -- rt_wake_sem_post/rt_wake_sem_timedwait's
             * own count means a notify that lands between this store and
             * the wait call below is not lost (the wake semaphore remembers
             * the post, exactly like a real sem_t would), which is what
             * actually closes the lost-wakeup window; the periodic timeout
             * is the remaining correctness floor for anything this still
             * misses. */
            atomic_store_explicit(&c->idle, true, memory_order_relaxed);
            /* EXPERIMENTAL FIX v2, not for master without further
             * validation -- see maybe_idle's own comment. Plain,
             * unconditional store: always writing `true` needs no
             * old-value check, so this is exactly as cheap as the store
             * above it, not an RMW -- the busy-transition site below
             * (`if (drew) { atomic_store_explicit(&c->idle, false, ...` is
             * deliberately left untouched, zero added cost there. */
            atomic_store_explicit(&c->sched->maybe_idle, true,
                                   memory_order_relaxed);

            struct timespec ts;
            clock_gettime(CLOCK_REALTIME, &ts);
            ts.tv_nsec += 5 * 1000 * 1000; /* 5ms periodic fallback */
            if (ts.tv_nsec >= 1000000000L) {
                ts.tv_sec += 1;
                ts.tv_nsec -= 1000000000L;
            }
            rt_wake_sem_timedwait(&c->wake_sem, &ts);
            /* Either a real post or the 5ms fallback firing -- both mean
             * exactly the same thing here: go back to the top and try to
             * find work again. Unlike the old sem_timedwait call, there is
             * no EINTR to retry on and no clock-edge EINVAL to shrug off:
             * pthread_cond_timedwait is specified to never return EINTR,
             * and rt_wake_sem_timedwait's own `while (count == 0)` loop
             * already absorbs any spurious condvar wakeup internally. */
            continue;
        }

        if (drew) {
            atomic_store_explicit(&c->idle, false, memory_order_relaxed);
            update_max_draw_seen(c->sched, drew_n);
        }
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
    /* DIAGNOSTIC ONLY, not for master. */
    atomic_init(&s->diag_draw_calls, (uint64_t)0);
    atomic_init(&s->diag_draw_items, (uint64_t)0);
    atomic_init(&s->diag_notify_calls, (uint64_t)0);
    atomic_init(&s->diag_notify_steps, (uint64_t)0);
    atomic_init(&s->diag_notify_full_laps, (uint64_t)0);
    /* EXPERIMENTAL FIX v2, not for master without further validation. Every
     * carrier starts idle=true, so this starts true to match -- a false
     * start would make the very first notify_new_work call short-circuit
     * despite nobody having run yet. */
    atomic_init(&s->maybe_idle, true);

    /* Phase 3: the live-green-thread registry (park/unpark) and the
     * blocking-FFI monitor's own bookkeeping -- the monitor thread itself
     * is NOT started here (opt-in; see rt_sched_start_blocking_monitor). A
     * small initial capacity: this grows (doubling) exactly like the
     * shared queue's own backing store would if it needed to, and most
     * callers of this scheduler today (every existing Phase 2 test) never
     * put more than a few hundred green threads in flight at once. */
    registry_init(&s->registry, 256);
    atomic_init(&s->monitor_started, false);
    atomic_init(&s->monitor_stop, false);
    s->monitor_timeout_ns = 0;
    s->monitor_poll_ns = 0;
    atomic_init(&s->total_handoffs, (uint64_t)0);
    atomic_init(&s->total_handoff_items, (uint64_t)0);

    s->carriers = calloc(n_carriers, sizeof(rt_carrier_t));
    if (s->carriers == NULL) rt_trap("out of memory: carriers");

    for (uint32_t i = 0; i < n_carriers; i++) {
        rt_carrier_t *c = &s->carriers[i];
        c->sched = s;
        c->index = i;
        if (rt_wake_sem_init(&c->wake_sem) != 0) {
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
        pthread_mutex_init(&c->local_lock, NULL);
        atomic_init(&c->blocking_rec.blocking_since_ns, (uint64_t)0);
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

    /* Phase 3: park/unpark surface, live for this green thread's whole
     * life. `sched` and the registry entry exist from here on so that
     * rt_sched_unpark(s, id) is well-defined for this id immediately --
     * even before this function has returned the id to its caller, let
     * alone before this green thread ever calls rt_sched_park -- which is
     * exactly the property the lost-wakeup fix depends on (scheduler.h's
     * "park/unpark" section). */
    g->sched = s;
    atomic_init(&g->park_word, RT_PARK_EMPTY);
    g->parked = false;
    g->notified_before_park = false;
    atomic_init(&g->claimed_by_carrier, false);
    registry_insert(&s->registry, g->id, g);

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

/* DIAGNOSTIC ONLY, not for master -- see diag_draw_calls' own comment. */
uint64_t rt_sched_diag_draw_calls(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->diag_draw_calls, memory_order_relaxed);
}
uint64_t rt_sched_diag_draw_items(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->diag_draw_items, memory_order_relaxed);
}
uint64_t rt_sched_diag_notify_calls(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->diag_notify_calls, memory_order_relaxed);
}
uint64_t rt_sched_diag_notify_steps(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->diag_notify_steps, memory_order_relaxed);
}
uint64_t rt_sched_diag_notify_full_laps(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->diag_notify_full_laps, memory_order_relaxed);
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

/* ========================================================================
 * Blocking-FFI handoff monitor -- see scheduler.h's "Blocking-FFI handoff"
 * section for the full mechanism and why it is safe. This is that
 * mechanism's implementation.
 * ====================================================================== */

static uint64_t monitor_now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* Attempt to rescue carrier `c`'s local buffer, having just observed its
 * blocking record stuck at `since` for longer than the configured timeout.
 * Safe to call even if `c` has since become unstuck -- see the re-check
 * under the lock below, which is what makes that true. */
static void try_handoff(rt_scheduler_t *s, rt_carrier_t *c, uint64_t since) {
    rt_green_t *drained_buf[256]; /* fuel_size is realistically tiny
                                    * (default 4; design doc's own
                                    * reasoning never argues for anything
                                    * near this large) -- a fixed-size
                                    * on-stack scratch buffer avoids a
                                    * malloc on this already-slow path
                                    * while comfortably covering any
                                    * LANG_FUEL_SIZE a real deployment would
                                    * plausibly set. Falls back to malloc
                                    * below on the (extreme) chance
                                    * local_cap exceeds it. */
    rt_green_t **drained = drained_buf;
    bool heap = false;
    uint32_t n = 0;

    pthread_mutex_lock(&c->local_lock);
    /* Re-check UNDER THE LOCK: the whole point. If this carrier's blocking
     * call returned in the time it took to get here, `blocking_since_ns`
     * is either 0 (it finished and exited_blocking) or a DIFFERENT nonzero
     * value (a brand new blocking call started since) -- either way, this
     * is no longer the same stuck episode the caller observed, and acting
     * on a stale observation is exactly the race this lock exists to
     * close (see rt_carrier_t's and scheduler.h's own comments). Abandon
     * cleanly; the next poll will re-evaluate whatever is actually true. */
    uint64_t still = atomic_load_explicit(&c->blocking_rec.blocking_since_ns,
                                           memory_order_acquire);
    if (still != since) {
        pthread_mutex_unlock(&c->local_lock);
        return;
    }

    n = c->local_len;
    if (n > 0) {
        if (n > (uint32_t)(sizeof(drained_buf) / sizeof(drained_buf[0]))) {
            drained = malloc(sizeof(rt_green_t *) * n);
            if (drained == NULL) rt_trap("out of memory: blocking-FFI handoff");
            heap = true;
        }
        for (uint32_t i = 0; i < n; i++) {
            drained[i] = c->local[(c->local_head + i) % c->local_cap];
        }
        c->local_head = 0;
        c->local_len = 0;
    }
    pthread_mutex_unlock(&c->local_lock);

    if (n == 0) return; /* genuinely stuck, but nothing else was queued here */

    /* Outside the lock: squeue_push can block under backpressure, and
     * local_lock must never be held across a blocking call (every other
     * carrier's own post-dispatch local_push, which also takes this same
     * lock, must never be made to wait on this). Reused verbatim from
     * rt_sched_unpark/rt_sched_spawn: any other (non-stuck) carrier's
     * completely ordinary self-service draw is the "backup carrier" -- no
     * second thread type is needed. */
    for (uint32_t i = 0; i < n; i++) {
        squeue_push(&s->global, drained[i]);
    }
    notify_new_work(s);

    atomic_fetch_add_explicit(&s->total_handoffs, 1, memory_order_relaxed);
    atomic_fetch_add_explicit(&s->total_handoff_items, n, memory_order_relaxed);

    if (heap) free(drained);
}

static void *monitor_main(void *argp) {
    rt_scheduler_t *s = (rt_scheduler_t *)argp;

    while (!atomic_load_explicit(&s->monitor_stop, memory_order_relaxed)) {
        for (uint32_t i = 0; i < s->n_carriers; i++) {
            rt_carrier_t *c = &s->carriers[i];
            uint64_t since = atomic_load_explicit(
                &c->blocking_rec.blocking_since_ns, memory_order_acquire);
            if (since == 0) continue;
            uint64_t now = monitor_now_ns();
            /* now < since is possible only across a CLOCK_MONOTONIC
             * read racing right at the moment enter_blocking() wrote --
             * treat it as "not yet timed out" rather than wrapping a
             * uint64_t subtraction into a huge number. */
            if (now <= since || now - since < s->monitor_timeout_ns) continue;
            try_handoff(s, c, since);
        }

        struct timespec ts;
        ts.tv_sec = (time_t)(s->monitor_poll_ns / 1000000000ull);
        ts.tv_nsec = (long)(s->monitor_poll_ns % 1000000000ull);
        nanosleep(&ts, NULL);
    }
    return NULL;
}

void rt_sched_start_blocking_monitor(rt_scheduler_t *s, uint64_t timeout_ns,
                                      uint64_t poll_interval_ns) {
    bool expected = false;
    if (!atomic_compare_exchange_strong_explicit(
            &s->monitor_started, &expected, true, memory_order_acq_rel,
            memory_order_acquire)) {
        rt_trap("rt_sched_start_blocking_monitor: already started for this "
                "scheduler -- at most one monitor thread per scheduler");
    }
    s->monitor_timeout_ns = timeout_ns;
    s->monitor_poll_ns = poll_interval_ns;
    atomic_store_explicit(&s->monitor_stop, false, memory_order_relaxed);
    if (pthread_create(&s->monitor_thread, NULL, monitor_main, s) != 0) {
        rt_trap("could not start the blocking-FFI monitor thread");
    }
}

void rt_sched_stop_blocking_monitor(rt_scheduler_t *s) {
    if (!atomic_load_explicit(&s->monitor_started, memory_order_acquire)) {
        return; /* never started -- a no-op, per the documented contract */
    }
    atomic_store_explicit(&s->monitor_stop, true, memory_order_relaxed);
    pthread_join(s->monitor_thread, NULL);
    /* Allow a later rt_sched_start_blocking_monitor on this same scheduler,
     * should a caller ever want that -- not load-bearing for any test this
     * phase ships, but costs nothing and avoids a surprising permanent
     * "already started" trap after a legitimate stop. */
    atomic_store_explicit(&s->monitor_started, false, memory_order_release);
}

uint64_t rt_sched_handoff_count(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->total_handoffs, memory_order_relaxed);
}
uint64_t rt_sched_handoff_items(rt_scheduler_t *s) {
    return atomic_load_explicit(&s->total_handoff_items, memory_order_relaxed);
}

void rt_sched_shutdown(rt_scheduler_t *s) {
    atomic_store_explicit(&s->shutdown, true, memory_order_relaxed);
    for (uint32_t i = 0; i < s->n_carriers; i++) {
        rt_wake_sem_post(&s->carriers[i].wake_sem);
    }
    for (uint32_t i = 0; i < s->n_carriers; i++) {
        pthread_join(s->carriers[i].os_thread, NULL);
    }
}

void rt_sched_destroy(rt_scheduler_t *s) {
    /* Defensive: a caller that started the monitor and forgot to stop it
     * would otherwise leave a thread running against memory this function
     * is about to free out from under it. rt_sched_stop_blocking_monitor is
     * already a no-op if the monitor was never started, so this is safe to
     * call unconditionally. */
    rt_sched_stop_blocking_monitor(s);

    squeue_destroy(&s->global);
    pthread_mutex_destroy(&s->perm.lock);
    free(s->perm.order);
    registry_destroy(&s->registry);
    for (uint32_t i = 0; i < s->n_carriers; i++) {
        rt_wake_sem_destroy(&s->carriers[i].wake_sem);
        pthread_mutex_destroy(&s->carriers[i].local_lock);
        free(s->carriers[i].local);
    }
    free(s->carriers);
    free(s);
}
