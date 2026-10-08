/* macOS/BSD backend of the reactor: kqueue/kevent. See reactor.h for the
 * cross-platform contract (shared with runtime/reactor_epoll.c, the Linux
 * backend) and the design reasoning common to both. This file is the
 * OS-specific half only -- same function signatures, same park/unpark
 * semantics, same thread-safety guarantees as the epoll backend; nothing
 * elsewhere in the runtime needs to know which one a given build linked.
 *
 * STATUS: written and statically reviewed against kqueue(2)/kevent(2) as
 * documented (no x86-64/Linux box can exercise this code at all -- there is
 * nothing here for TSan or gates.sh to catch), and NOT YET validated on
 * real macOS/BSD hardware. The GitHub Actions workflow
 * (.github/workflows/macos.yml) that would do that validation is part of
 * this same change, but a human has to push a branch/tag to actually run it
 * on GitHub's real macOS runners -- see docs/concurrency-decision.md,
 * "Phase 3.5", for the exact honesty statement this project expects for
 * code in this state.
 *
 * ============================================================================
 * THE EPOLL -> KQUEUE MAPPING, AND WHERE IT STOPS BEING A STRAIGHT PORT
 * ============================================================================
 *
 * epoll_create1/epoll_ctl/epoll_wait -> kqueue()/kevent(). kevent() does
 * both registration (a "changelist" in) and waiting (an "eventlist" out) in
 * one call, and this file mostly uses it as two separate calls: a
 * register-only call (eventlist NULL, nevents 0 -- applies changes and
 * returns immediately, no blocking) from whichever green thread's own OS
 * thread calls rt_reactor_wait, and a wait-only call (changelist NULL,
 * nchanges 0) from the reactor's own dedicated thread in reactor_loop,
 * blocking indefinitely. Per kevent(2) (both the FreeBSD and the Darwin/XNU
 * man page say the same thing here), a kqueue descriptor is safe for
 * concurrent kevent() calls from multiple threads -- one thread blocked
 * waiting while others register changes is exactly the standard shape every
 * production kqueue reactor (libevent, libuv, Go's older darwin netpoller)
 * uses, and is NOT something this file invented. This specific claim is one
 * of the things that can only be truly confirmed by the pending real
 * macOS CI run, not by reading a man page in a training corpus.
 *
 * EPOLLIN/EPOLLOUT -> EVFILT_READ/EVFILT_WRITE. The mapping itself is
 * direct, but the GRANULARITY is not: epoll has ONE registration per fd
 * (a single struct epoll_event whose `events` field ORs EPOLLIN/EPOLLOUT
 * together), while kqueue has a SEPARATE, independent knote per (fd,
 * filter) pair -- registering for both read and write readiness on one fd
 * is ONE epoll_ctl call but TWO kevent changelist entries. This is the one
 * place a line-for-line port would have silently introduced a real bug --
 * see "THE SPURIOUS-WAKEUP HAZARD" below.
 *
 * EPOLLONESHOT -> EV_ONESHOT. These are close but not identical:
 *   - EPOLLONESHOT: the fd stays registered in the epoll interest list
 *     after firing once, but DISABLED, until explicitly re-armed with
 *     EPOLL_CTL_MOD. A second EPOLL_CTL_ADD on it fails EEXIST.
 *   - EV_ONESHOT: "Causes the event to return only once... After the event
 *     is retrieved, it will automatically be deleted" (kevent(2)). The
 *     knote is fully DELETED from the kqueue after it fires, not merely
 *     disabled. Re-arming is therefore always EV_ADD again -- there is no
 *     separate "MOD" opcode in kqueue at all; EV_ADD is documented upsert
 *     semantics either way ("re-adding an existing event will modify the
 *     parameters of the original event, and not result in a duplicate
 *     entry" -- kevent(2)), so it is simultaneously the ADD and the MOD
 *     operation epoll needs two different opcodes for.
 *   One concrete, favorable consequence: epoll_epoll.c's ENOENT
 *   self-healing fallback (EPOLL_CTL_MOD racing a closed-and-recycled fd)
 *   has no kqueue equivalent to write, because this file never has to
 *   guess ADD vs MOD in the first place -- every rt_reactor_wait call just
 *   issues EV_ADD unconditionally, and EV_ADD's own upsert semantics handle
 *   both "fresh fd" and "re-arming" identically. There is no ADD-vs-MOD
 *   branch here for a stale cached bit to get wrong.
 *
 * eventfd -> EVFILT_USER, not a self-pipe. kqueue has no fd-based
 * cross-thread wakeup primitive, so the two real options are a classic
 * self-pipe (an extra pipe(2) pair, write one byte to wake the blocked
 * kevent() call, read-and-discard it on the other end) or EVFILT_USER (a
 * kqueue-native, software-triggered filter with no backing fd at all:
 * EV_ADD once at creation, then any thread "fires" it with a changelist
 * entry carrying NOTE_TRIGGER in fflags). This file uses EVFILT_USER:
 *   - No extra fd, no read-side draining logic, no EINTR/partial-write
 *     bookkeeping a self-pipe needs to get right.
 *   - kqueue namespaces idents PER FILTER, not globally (ident, filter) is
 *     the actual key -- so an EVFILT_USER registration with ident=1 can
 *     never collide with a real fd=1 (stdout) registered under
 *     EVFILT_READ/EVFILT_WRITE. reactor_loop tells the shutdown event
 *     apart from a real fd event by checking `.filter == EVFILT_USER`,
 *     never by comparing fd numbers the way the epoll backend compares
 *     against a sentinel shutdown_fd.
 *   This is the one piece of this file with the least amount of
 *   from-documentation confidence: the exact fflags control-bits
 *   (NOTE_FFNOP/NOTE_FFAND/NOTE_FFOR/NOTE_FFCOPY) that govern how a
 *   trigger's fflags combine with whatever is already stored are
 *   genuinely easy to misremember, and this file uses the plain
 *   `fflags = NOTE_TRIGGER` form (no explicit control bits ORed in) on the
 *   belief that the default (no control bits set, i.e. NOTE_FFNOP) still
 *   honors NOTE_TRIGGER for a single one-shot fire-and-forget signal --
 *   which is also the only thing this reactor ever uses EVFILT_USER for
 *   (shutdown fires it exactly once, ever, and the thread that receives it
 *   exits immediately without looping back into kevent() again). If this
 *   turns out to be wrong, the failure mode is narrow and loud: the
 *   reactor's dedicated thread simply never wakes up and rt_reactor_destroy
 *   hangs in pthread_join -- not a silent data-race, a hang a test timeout
 *   would catch immediately. This is exactly the kind of claim the pending
 *   macOS CI run (not a read of this comment) is what actually settles.
 *
 * ============================================================================
 * THE SPURIOUS-WAKEUP HAZARD THIS FILE CLOSES (specific to kqueue's
 * per-filter split, i.e. it is NOT a hazard the epoll backend has)
 * ============================================================================
 *
 * rt_sched_unpark (runtime/scheduler.h) is a per-green-thread, 4-state CAS
 * word with NO notion of "which episode" a notification belongs to -- it is
 * one bit of memory per green thread id, full stop. Calling it for a
 * green_id that is not currently expecting THIS wakeup does real damage:
 * if that green thread is in its EMPTY state doing something unrelated
 * (not parked at all yet), the call still lands as a pre-recorded NOTIFIED,
 * and the NEXT time that same thread calls rt_sched_park for an entirely
 * different, legitimate reason (a channel recv, a timer), stage 1's CAS
 * finds NOTIFIED and returns immediately WITHOUT actually waiting --
 * exactly the lost/misattributed-wakeup failure mode this whole park/unpark
 * design exists to prevent, just arriving from a different direction than
 * the one scheduler.h's own comment discusses.
 *
 * Why kqueue specifically creates a NEW opportunity for this: because read
 * and write are independent knotes here, a single rt_reactor_wait(fd,
 * READ|WRITE) call creates TWO EV_ONESHOT registrations. If READ fires
 * first, this reactor must treat that as "the wait is satisfied" (exactly
 * one unpark, matching what one combined epoll registration would have
 * done) -- but the WRITE knote is still physically armed in the kernel,
 * independent of whatever this green thread does next. Left alone, it can
 * fire LATER, for a green thread that has long since moved on to waiting on
 * something else entirely, or whose green_id has even been reused by a
 * brand-new, unrelated green thread.
 *
 * The fix is a per-registration SEQUENCE NUMBER, not the green_id itself,
 * as the token carried in each kevent's udata. r->waiters is keyed by fd
 * and stores {green_id, armed_mask, seq}; every rt_reactor_wait call mints
 * a fresh, strictly-increasing seq (reactor.waiters.lock-protected, one
 * counter for the whole reactor) and stamps it into udata for every
 * changelist entry that call submits. reactor_loop only acts on a firing
 * event if the CURRENT map entry for that fd still carries the exact same
 * seq the event's udata names (wmap_consume). This is strictly stronger
 * than matching on green_id alone: a stale sibling knote that fires after
 * the fd has been re-armed -- even if re-armed for the SAME green_id on the
 * SAME fd, which the earlier green_id-only design this file went through
 * first does NOT safely handle -- carries the OLD seq, which can never
 * equal the NEW map entry's seq (seq is never reused), so it is correctly
 * a no-op every time, independent of fd/green_id reuse patterns and
 * independent of whether the best-effort EV_DELETE cleanup below ever
 * actually runs or succeeds.
 *
 * Given that seq-matching is what makes this correct, the EV_DELETE calls
 * in this file that clean up a now-unwanted sibling knote (either the
 * no-longer-requested direction when a new rt_reactor_wait call narrows an
 * existing registration, or the not-yet-fired direction after the other
 * one fires) are deliberately best-effort: their return value is ignored,
 * and they are issued as their OWN kevent() call, never batched into the
 * same kevent() call as a must-succeed EV_ADD. This matters because
 * kevent(2) documents that when nevents==0 (no eventlist to report
 * per-entry EV_ERROR into), an error partway through a changelist aborts
 * the whole call and returns -1 -- batching an ENOENT-prone cleanup delete
 * together with a real registration could, in principle, make the real
 * registration silently not apply. Keeping them as separate calls means a
 * cleanup failure can never affect whether the real registration succeeded.
 */
#include "reactor.h"
#include "reactor_timers.h"
#include "rt.h"

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/time.h>
#include <sys/types.h>
#include <unistd.h>

/* ========================================================================
 * The waiter map: fd -> {which green thread id is waiting, which of
 * RT_REACTOR_READ/WRITE are currently armed as live EV_ONESHOT knotes for
 * it, and the sequence number that names THIS specific registration}.
 *
 * Mutex-protected open addressing, storing each waiter BY VALUE in its own
 * slot, the same "correctness over cleverness" choice runtime/reactor_epoll.c's
 * own waiter map makes (and for the same reason: not a hot path). Unlike
 * the epoll backend's map, entries here ARE actively erased on consume
 * (tombstoned) -- see wmap_consume's own comment for why a stale/duplicate
 * firing must see "not found", not a leftover entry. */
typedef struct {
    uint32_t fd;             /* key; sentinels below */
    uint32_t green_id;
    uint32_t armed_mask;      /* RT_REACTOR_READ/WRITE bits with a live,
                                 not-yet-fired EV_ONESHOT knote right now */
    uint64_t seq;             /* names this exact registration; see this
                                 file's top comment, "THE SPURIOUS-WAKEUP
                                 HAZARD" */
    /* The bounded wait that owns this registration (runtime/
     * reactor_timers.h), NULL for an unbounded one. Whoever claims the
     * wait -- readiness or the deadline -- clears it, under `lock`. */
    rt_timer_node_t *node;
} rt_waiter_t;

#define RT_WAITER_EMPTY UINT32_MAX
#define RT_WAITER_TOMB  (UINT32_MAX - 1)
/* A real fd is always small and non-negative (RLIMIT_NOFILE), so both
 * sentinels are unreachable as real keys. */

typedef struct {
    pthread_mutex_t lock;
    rt_waiter_t     *slots;
    uint32_t         cap;
    uint32_t         used; /* occupied, including tombstones */
    uint32_t         live;
    uint64_t         next_seq; /* monotonic, never reused; 0 never issued */
} rt_waiter_map_t;

static void wmap_init(rt_waiter_map_t *m, uint32_t cap) {
    m->cap = cap;
    m->slots = malloc(sizeof(rt_waiter_t) * cap);
    if (m->slots == NULL) rt_trap("out of memory: reactor waiter map");
    for (uint32_t i = 0; i < cap; i++) m->slots[i].fd = RT_WAITER_EMPTY;
    m->used = 0;
    m->live = 0;
    m->next_seq = 0;
    pthread_mutex_init(&m->lock, NULL);
}

static void wmap_destroy(rt_waiter_map_t *m) {
    pthread_mutex_destroy(&m->lock);
    free(m->slots);
}

/* Lock held. Finds `fd`'s slot, or NULL if absent (EMPTY ends the probe
 * chain; a TOMBSTONE does not -- it is a hole left by a consumed
 * registration for a fd that may still have a live entry further down the
 * same chain, or may not, exactly like any open-addressing delete). */
static rt_waiter_t *wmap_find_locked(rt_waiter_map_t *m, uint32_t fd) {
    uint32_t i = fd % m->cap;
    uint32_t steps = 0;
    while (m->slots[i].fd != RT_WAITER_EMPTY && steps < m->cap) {
        if (m->slots[i].fd == fd) return &m->slots[i];
        i = (i + 1) % m->cap;
        steps++;
    }
    return NULL;
}

/* Lock held. `fd` assumed not already present (reuses a TOMBSTONE slot if
 * the probe chain has one, same as a fresh EMPTY slot). */
static void wmap_insert_locked(rt_waiter_map_t *m, uint32_t fd,
                                uint32_t green_id, uint32_t armed_mask,
                                uint64_t seq, rt_timer_node_t *node) {
    uint32_t i = fd % m->cap;
    while (m->slots[i].fd != RT_WAITER_EMPTY && m->slots[i].fd != RT_WAITER_TOMB) {
        i = (i + 1) % m->cap;
    }
    m->slots[i].fd = fd;
    m->slots[i].green_id = green_id;
    m->slots[i].armed_mask = armed_mask;
    m->slots[i].seq = seq;
    m->slots[i].node = node;
}

static void wmap_grow_if_needed_locked(rt_waiter_map_t *m) {
    if ((uint64_t)(m->used + 1) * 2 <= m->cap) return;

    uint32_t old_cap = m->cap;
    rt_waiter_t *old = m->slots;

    uint32_t new_cap = old_cap * 2;
    m->slots = malloc(sizeof(rt_waiter_t) * new_cap);
    if (m->slots == NULL) rt_trap("out of memory: growing reactor waiter map");
    for (uint32_t i = 0; i < new_cap; i++) m->slots[i].fd = RT_WAITER_EMPTY;
    m->cap = new_cap;

    for (uint32_t i = 0; i < old_cap; i++) {
        if (old[i].fd != RT_WAITER_EMPTY && old[i].fd != RT_WAITER_TOMB) {
            wmap_insert_locked(m, old[i].fd, old[i].green_id,
                                old[i].armed_mask, old[i].seq, old[i].node);
        }
    }
    /* Growing naturally compacts away tombstones too: only LIVE entries are
     * reinserted above, so `used` collapses back down to exactly `live`. */
    m->used = m->live;
    free(old);
}

/* Lock held. Create-or-replace fd's registration with {green_id, new_mask,
 * node}, minting a fresh sequence number for it. Returns that new seq (to
 * stamp into every kevent udata this registration submits) and hands back the
 * PREVIOUS armed_mask in `*out_old_mask` (0 if there was no previous entry)
 * so the caller can tell which previously-armed filter(s), if any, are no
 * longer wanted and should be best-effort EV_DELETEd (see kq_arm). A
 * previous owner's `node` is not touched (it stays in the heap and ends at
 * its own deadline -- the one-waiter-per-fd limitation, reactor.h). */
static uint64_t wmap_upsert_locked(rt_waiter_map_t *m, uint32_t fd,
                                    uint32_t green_id, uint32_t new_mask,
                                    rt_timer_node_t *node,
                                    uint32_t *out_old_mask) {
    uint64_t seq = ++m->next_seq;
    rt_waiter_t *w = wmap_find_locked(m, fd);
    uint32_t old_mask = 0;
    if (w != NULL) {
        old_mask = w->armed_mask;
        w->green_id = green_id;
        w->armed_mask = new_mask;
        w->seq = seq;
        w->node = node;
    } else {
        wmap_grow_if_needed_locked(m);
        wmap_insert_locked(m, fd, green_id, new_mask, seq, node);
        m->used++;
        m->live++;
    }
    *out_old_mask = old_mask;
    return seq;
}

static uint64_t wmap_upsert(rt_waiter_map_t *m, uint32_t fd, uint32_t green_id,
                             uint32_t new_mask, uint32_t *out_old_mask) {
    pthread_mutex_lock(&m->lock);
    uint64_t seq = wmap_upsert_locked(m, fd, green_id, new_mask, NULL,
                                      out_old_mask);
    pthread_mutex_unlock(&m->lock);
    return seq;
}

/* Called from the reactor thread when a kevent fires naming (fd, seq,
 * filter_bit). Only acts if fd's CURRENT map entry still carries this exact
 * seq -- see this file's top comment, "THE SPURIOUS-WAKEUP HAZARD", for why
 * a mismatch here must be a silent no-op (a stale sibling knote, or a
 * genuinely stray event) rather than ever touching rt_sched_unpark. On a
 * match, erases the entry (so a second event for the SAME seq arriving
 * later in this same kevent() batch -- both read and write became ready
 * "simultaneously" -- finds nothing and also no-ops) and hands back the
 * green_id to unpark and whichever OTHER filter bit(s), if any, were armed
 * alongside this one and have not fired yet. */
static bool wmap_consume(rt_waiter_map_t *m, rt_timer_heap_t *timers,
                          uint32_t fd, uint64_t seq,
                          uint32_t filter_bit, uint32_t *out_green_id,
                          uint32_t *out_remaining) {
    pthread_mutex_lock(&m->lock);
    rt_waiter_t *w = wmap_find_locked(m, fd);
    bool matched = (w != NULL && w->seq == seq);
    if (matched) {
        *out_green_id = w->green_id;
        *out_remaining = w->armed_mask & ~filter_bit;
        if (w->node != NULL) {
            /* A bounded wait: readiness claims it, so its deadline leaves
             * the heap and the deadline pass cannot also fire for it. */
            rt_timer_node_t *node = w->node;
            w->node = NULL;
            rt_timer_heap_remove(timers, node);
            node->outcome = RT_TIMER_READY; /* last: `node` may be freed once unlocked */
        }
        w->fd = RT_WAITER_TOMB;
        m->live--;
    }
    pthread_mutex_unlock(&m->lock);
    return matched;
}

/* ========================================================================
 * The reactor itself.
 * ====================================================================== */

/* EVFILT_USER has no backing fd -- its ident lives in a namespace scoped to
 * EVFILT_USER alone (kqueue keys a knote by (ident, filter) together), so
 * this can never collide with a real fd=1 registered under
 * EVFILT_READ/EVFILT_WRITE. The exact value is arbitrary. */
#define RT_REACTOR_SHUTDOWN_IDENT ((uintptr_t)1)
/* A second EVFILT_USER knote, the epoll backend's wake_fd: fired when a new
 * earliest deadline needs the sleeping reactor thread to re-aim its kevent
 * timeout. A different ident, so reactor_loop tells it from shutdown. */
#define RT_REACTOR_WAKE_IDENT ((uintptr_t)2)

struct rt_reactor {
    rt_scheduler_t *sched;
    int             kq;
    pthread_t       thread;
    rt_waiter_map_t waiters;
    /* Deadlines of the bounded waits, guarded by waiters.lock -- the same
     * lock that guards the map, so that "claim this wait" is one critical
     * section whichever of readiness and the deadline gets there. */
    rt_timer_heap_t timers;
};

/* Claims up to `max` bounded waits whose deadline has passed, and returns the
 * green ids to unpark (the caller does that AFTER this returns, lock
 * released: rt_sched_unpark can block on a full shared queue, and a carrier
 * may be waiting for this lock in rt_reactor_wait_timeout). Each claimed
 * wait's registration is erased, so a knote that fires later finds no entry
 * with its seq and is a no-op -- the same property that makes the sibling
 * knote cleanup in reactor_loop best-effort. */
static size_t expire_timers(rt_reactor_t *r, uint32_t *gids, size_t max) {
    size_t k = 0;
    pthread_mutex_lock(&r->waiters.lock);
    uint64_t now = rt_timer_now_ns();
    rt_timer_node_t *n;
    while (k < max && (n = rt_timer_heap_pop_expired(&r->timers, now)) != NULL) {
        int fd = n->fd;
        gids[k++] = n->green_id;
        if (fd >= 0) {
            rt_waiter_t *w = wmap_find_locked(&r->waiters, (uint32_t)fd);
            /* Only if the registration is still THIS wait's: a later wait
             * on the same fd took it over otherwise, and is not ours. */
            if (w != NULL && w->node == n) {
                struct kevent del[2];
                int nd = 0;
                if (w->armed_mask & RT_REACTOR_READ) {
                    EV_SET(&del[nd++], fd, EVFILT_READ, EV_DELETE, 0, 0, NULL);
                }
                if (w->armed_mask & RT_REACTOR_WRITE) {
                    EV_SET(&del[nd++], fd, EVFILT_WRITE, EV_DELETE, 0, 0, NULL);
                }
                /* Best effort, its own call -- see this file's top comment. */
                if (nd > 0) (void)kevent(r->kq, del, nd, NULL, 0, NULL);
                w->fd = RT_WAITER_TOMB;
                r->waiters.live--;
            }
        }
        n->outcome = RT_TIMER_EXPIRED; /* last: `n` may be freed once we unlock */
    }
    pthread_mutex_unlock(&r->waiters.lock);
    return k;
}

static void *reactor_loop(void *argp) {
    rt_reactor_t *r = (rt_reactor_t *)argp;
    struct kevent evs[64];

    for (;;) {
        /* Sleep until an event, or until the nearest deadline. A deadline
         * that appears while this is already asleep wakes it through the
         * EVFILT_USER wake knote. */
        pthread_mutex_lock(&r->waiters.lock);
        int wait_ms = rt_timer_heap_wait_ms(&r->timers, rt_timer_now_ns());
        pthread_mutex_unlock(&r->waiters.lock);
        struct timespec ts;
        struct timespec *tsp = NULL;
        if (wait_ms >= 0) {
            ts.tv_sec = wait_ms / 1000;
            ts.tv_nsec = (long)(wait_ms % 1000) * 1000000L;
            tsp = &ts;
        }

        int n = kevent(r->kq, NULL, 0, evs, 64, tsp);
        if (n < 0) {
            if (errno == EINTR) continue;
            rt_trap("rt_reactor: kevent wait failed");
        }
        for (int i = 0; i < n; i++) {
            if (evs[i].filter == EVFILT_USER) {
                /* Told apart from a real fd event by FILTER, not by
                 * comparing fd numbers -- see RT_REACTOR_SHUTDOWN_IDENT's
                 * own comment. The shutdown signal (rt_reactor_destroy)
                 * ends the loop; the wake knote (EV_CLEAR, so it re-arms by
                 * itself) only means "recompute the timeout", which the top
                 * of the next pass does. */
                if (evs[i].ident == RT_REACTOR_SHUTDOWN_IDENT) return NULL;
                continue;
            }

            uint32_t fd  = (uint32_t)evs[i].ident;
            uint64_t seq = (uint64_t)(uintptr_t)evs[i].udata;
            uint32_t filter_bit = (evs[i].filter == EVFILT_WRITE)
                                       ? RT_REACTOR_WRITE : RT_REACTOR_READ;

            uint32_t green_id, remaining;
            if (wmap_consume(&r->waiters, &r->timers, fd, seq, filter_bit,
                              &green_id, &remaining)) {
                if (remaining != 0) {
                    /* The sibling direction (this fd was armed for both
                     * read and write, and only one fired) is still a live,
                     * independent EV_ONESHOT knote -- unlike epoll, where
                     * one combined registration disarms as a whole. This
                     * cleanup is best-effort ONLY: wmap_consume's seq match
                     * is what actually makes a late or failed-to-cancel
                     * firing safe (it will find no matching entry and
                     * no-op), not this delete succeeding. Issued as its own
                     * kevent() call -- see this file's top comment for why
                     * it is never batched with a must-succeed EV_ADD. */
                    struct kevent del;
                    int rfilt = (remaining & RT_REACTOR_WRITE) ? EVFILT_WRITE
                                                                : EVFILT_READ;
                    EV_SET(&del, fd, rfilt, EV_DELETE, 0, 0, NULL);
                    (void)kevent(r->kq, &del, 1, NULL, 0, NULL);
                }
                rt_sched_unpark(r->sched, green_id);
            }
            /* else: stale (a superseded or already-consumed registration)
             * or nobody is waiting any more -- exactly the epoll backend's
             * own "nothing to do" case. */
        }
        /* Every pass, whether it woke for events, for a deadline, or both:
         * a busy fd cannot starve a deadline, and a deadline pass costs one
         * clock read when nothing has expired. */
        uint32_t due[64];
        size_t k;
        do {
            k = expire_timers(r, due, 64);
            for (size_t j = 0; j < k; j++) rt_sched_unpark(r->sched, due[j]);
        } while (k == 64);
    }
}

rt_reactor_t *rt_reactor_create(rt_scheduler_t *sched) {
    rt_reactor_t *r = calloc(1, sizeof *r);
    if (r == NULL) rt_trap("out of memory: reactor");
    r->sched = sched;

    r->kq = kqueue();
    if (r->kq < 0) rt_trap("rt_reactor_create: kqueue() failed");

    struct kevent ev;
    EV_SET(&ev, RT_REACTOR_SHUTDOWN_IDENT, EVFILT_USER, EV_ADD | EV_CLEAR, 0,
           0, NULL);
    if (kevent(r->kq, &ev, 1, NULL, 0, NULL) != 0) {
        rt_trap("rt_reactor_create: could not register the EVFILT_USER "
                "shutdown event");
    }

    EV_SET(&ev, RT_REACTOR_WAKE_IDENT, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0,
           NULL);
    if (kevent(r->kq, &ev, 1, NULL, 0, NULL) != 0) {
        rt_trap("rt_reactor_create: could not register the EVFILT_USER "
                "wake event");
    }

    wmap_init(&r->waiters, 64);

    if (pthread_create(&r->thread, NULL, reactor_loop, r) != 0) {
        rt_trap("rt_reactor_create: could not start the reactor thread");
    }
    return r;
}

/* Arm `fd` for `events` as EV_ONESHOT knotes stamped with `seq`, first
 * best-effort deleting any filter a PREVIOUS wait on this fd armed that this
 * one does not want (`old_mask`). Returns 0 or the errno. No lock needed:
 * kevent registration is safe against the reactor thread's kevent wait. */
static int kq_arm(rt_reactor_t *r, int fd, uint32_t events, uint64_t seq,
                  uint32_t old_mask) {
    /* A filter armed by a PREVIOUS rt_reactor_wait call on this fd that
     * this call does not want any more. Unlike epoll's single combined
     * registration (one EPOLL_CTL_MOD atomically replaces the whole
     * interest mask), kqueue's independent knotes mean narrowing from
     * "both" to "one" leaves the other physically armed unless deleted
     * explicitly. Best-effort and issued as its own call -- see this
     * file's top comment. */
    uint32_t stale = old_mask & ~events;
    if (stale != 0) {
        struct kevent del[2];
        int nd = 0;
        if (stale & RT_REACTOR_READ) {
            EV_SET(&del[nd++], fd, EVFILT_READ, EV_DELETE, 0, 0, NULL);
        }
        if (stale & RT_REACTOR_WRITE) {
            EV_SET(&del[nd++], fd, EVFILT_WRITE, EV_DELETE, 0, 0, NULL);
        }
        (void)kevent(r->kq, del, nd, NULL, 0, NULL);
    }

    /* EV_ONESHOT: exactly one readiness notification per registration, then
     * the knote is deleted outright (not merely disabled, unlike
     * EPOLLONESHOT -- see this file's top comment). udata carries `seq`,
     * not `green_id`: that is the whole fix for the spurious-wakeup hazard
     * documented at this file's top. */
    struct kevent add[2];
    int na = 0;
    void *udata = (void *)(uintptr_t)seq;
    if (events & RT_REACTOR_READ) {
        EV_SET(&add[na++], fd, EVFILT_READ, EV_ADD | EV_ONESHOT, 0, 0, udata);
    }
    if (events & RT_REACTOR_WRITE) {
        EV_SET(&add[na++], fd, EVFILT_WRITE, EV_ADD | EV_ONESHOT, 0, 0, udata);
    }
    if (na > 0 && kevent(r->kq, add, na, NULL, 0, NULL) != 0) return errno;
    return 0;
}

void rt_reactor_wait(rt_reactor_t *r, int fd, uint32_t events) {
    /* Traps if not called from inside a running green thread -- the same
     * contract rt_sched_park (called below) already enforces; asking here
     * too gives a clearer message naming THIS function if it is misused. */
    uint32_t green_id = rt_sched_current_green_id();

    uint32_t old_mask;
    uint64_t seq = wmap_upsert(&r->waiters, (uint32_t)fd, green_id, events,
                                &old_mask);

    if (kq_arm(r, fd, events, seq, old_mask) != 0) {
        /* No ADD-vs-MOD branch exists here to have gotten wrong (see this
         * file's top comment on EV_ADD's upsert semantics) -- any failure
         * here is a genuine caller bug (e.g. a closed or invalid fd), the
         * same class of error the epoll backend traps on too. */
        rt_trap("rt_reactor_wait: kevent EV_ADD failed");
    }

    /* THE race-closing step: entirely rt_sched_park's own CAS protocol
     * (runtime/scheduler.c, runtime/scheduler.h) -- identical to the epoll
     * backend, unaffected by anything above. If the reactor thread's
     * kevent() wait already observed `fd` ready and called rt_sched_unpark
     * for `green_id` in the gap between the kevent() call above and this
     * line, rt_sched_park's CAS finds NOTIFIED (not EMPTY) and returns
     * immediately, having never suspended at all. See reactor.h's top
     * comment for why no second CAS state machine is needed in this file
     * for that composition to be correct. */
    rt_sched_park(RT_GT_PARKED_IO);
}

int rt_reactor_wait_timeout(rt_reactor_t *r, int fd, uint32_t events,
                            int64_t timeout_ms) {
    bool sleep_only = (fd < 0 || events == 0);
    if (timeout_ms < 0) {
        if (sleep_only) return -EINVAL;
        rt_reactor_wait(r, fd, events);
        return RT_REACTOR_WAIT_READY;
    }
    if (timeout_ms == 0) {
        /* An immediate poll: nothing to park, nothing to register. */
        if (sleep_only) return RT_REACTOR_WAIT_TIMEOUT;
        struct pollfd p;
        p.fd = fd;
        p.events = (short)(((events & RT_REACTOR_READ) ? POLLIN : 0) |
                           ((events & RT_REACTOR_WRITE) ? POLLOUT : 0));
        p.revents = 0;
        int rc;
        do {
            rc = poll(&p, 1, 0);
        } while (rc < 0 && errno == EINTR);
        if (rc < 0) return -errno;
        if (rc > 0 && (p.revents & POLLNVAL)) return -EBADF;
        return rc > 0 ? RT_REACTOR_WAIT_READY : RT_REACTOR_WAIT_TIMEOUT;
    }

    rt_timer_node_t node;
    node.deadline_ns = rt_timer_deadline_ns(timeout_ms);
    node.green_id = rt_sched_current_green_id();
    node.fd = sleep_only ? -1 : fd;
    node.heap_idx = 0;
    node.outcome = RT_TIMER_PENDING;

    /* Registration and deadline go in under ONE hold of the lock, for the
     * reason given in reactor_epoll.c: the reactor thread can claim neither
     * until it is released, so there is no window in which one exists
     * without the other. */
    pthread_mutex_lock(&r->waiters.lock);
    if (!sleep_only) {
        uint32_t old_mask;
        uint64_t seq = wmap_upsert_locked(&r->waiters, (uint32_t)fd,
                                          node.green_id, events, &node,
                                          &old_mask);
        int e = kq_arm(r, fd, events, seq, old_mask);
        if (e != 0) {
            rt_waiter_t *w = wmap_find_locked(&r->waiters, (uint32_t)fd);
            if (w != NULL && w->seq == seq) {
                w->fd = RT_WAITER_TOMB;
                r->waiters.live--;
            }
            pthread_mutex_unlock(&r->waiters.lock);
            return -e;
        }
    }
    bool earliest = rt_timer_heap_push(&r->timers, &node);
    pthread_mutex_unlock(&r->waiters.lock);

    if (earliest) {
        /* The reactor thread may be asleep until some later deadline (or
         * for ever): fire the wake knote. EV_CLEAR resets it once seen, and
         * a trigger that lands while the thread is between kevent calls is
         * still delivered to the next one. */
        struct kevent wake;
        EV_SET(&wake, RT_REACTOR_WAKE_IDENT, EVFILT_USER, 0, NOTE_TRIGGER, 0,
               NULL);
        (void)kevent(r->kq, &wake, 1, NULL, 0, NULL);
    }

    /* Park until the wait is CLAIMED; see reactor_epoll.c for why a park
     * that returns early is absorbed here. */
    for (;;) {
        rt_sched_park(sleep_only ? RT_GT_PARKED_TIMER : RT_GT_PARKED_IO);
        pthread_mutex_lock(&r->waiters.lock);
        int oc = node.outcome;
        pthread_mutex_unlock(&r->waiters.lock);
        if (oc != RT_TIMER_PENDING) {
            return oc == RT_TIMER_READY ? RT_REACTOR_WAIT_READY
                                        : RT_REACTOR_WAIT_TIMEOUT;
        }
    }
}

size_t rt_reactor_timers_pending(rt_reactor_t *r) {
    pthread_mutex_lock(&r->waiters.lock);
    size_t n = r->timers.len;
    pthread_mutex_unlock(&r->waiters.lock);
    return n;
}

void rt_reactor_destroy(rt_reactor_t *r) {
    struct kevent ev;
    EV_SET(&ev, RT_REACTOR_SHUTDOWN_IDENT, EVFILT_USER, 0, NOTE_TRIGGER, 0,
           NULL);
    if (kevent(r->kq, &ev, 1, NULL, 0, NULL) != 0) {
        rt_trap("rt_reactor_destroy: could not signal the reactor thread to "
                "stop (EVFILT_USER trigger failed)");
    }
    pthread_join(r->thread, NULL);
    close(r->kq);
    free(r->timers.a);
    wmap_destroy(&r->waiters);
    free(r);
}
