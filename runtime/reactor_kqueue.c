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
#include "rt.h"

#include <errno.h>
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
                                uint64_t seq) {
    uint32_t i = fd % m->cap;
    while (m->slots[i].fd != RT_WAITER_EMPTY && m->slots[i].fd != RT_WAITER_TOMB) {
        i = (i + 1) % m->cap;
    }
    m->slots[i].fd = fd;
    m->slots[i].green_id = green_id;
    m->slots[i].armed_mask = armed_mask;
    m->slots[i].seq = seq;
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
                                old[i].armed_mask, old[i].seq);
        }
    }
    /* Growing naturally compacts away tombstones too: only LIVE entries are
     * reinserted above, so `used` collapses back down to exactly `live`. */
    m->used = m->live;
    free(old);
}

/* Create-or-replace fd's registration with {green_id, new_mask}, minting a
 * fresh sequence number for it. Returns that new seq (to stamp into every
 * kevent udata this registration submits) and hands back the PREVIOUS
 * armed_mask in `*out_old_mask` (0 if there was no previous entry) so the
 * caller can tell which previously-armed filter(s), if any, are no longer
 * wanted and should be best-effort EV_DELETEd (see rt_reactor_wait). */
static uint64_t wmap_upsert(rt_waiter_map_t *m, uint32_t fd, uint32_t green_id,
                             uint32_t new_mask, uint32_t *out_old_mask) {
    pthread_mutex_lock(&m->lock);
    uint64_t seq = ++m->next_seq;
    rt_waiter_t *w = wmap_find_locked(m, fd);
    uint32_t old_mask = 0;
    if (w != NULL) {
        old_mask = w->armed_mask;
        w->green_id = green_id;
        w->armed_mask = new_mask;
        w->seq = seq;
    } else {
        wmap_grow_if_needed_locked(m);
        wmap_insert_locked(m, fd, green_id, new_mask, seq);
        m->used++;
        m->live++;
    }
    pthread_mutex_unlock(&m->lock);
    *out_old_mask = old_mask;
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
static bool wmap_consume(rt_waiter_map_t *m, uint32_t fd, uint64_t seq,
                          uint32_t filter_bit, uint32_t *out_green_id,
                          uint32_t *out_remaining) {
    pthread_mutex_lock(&m->lock);
    rt_waiter_t *w = wmap_find_locked(m, fd);
    bool matched = (w != NULL && w->seq == seq);
    if (matched) {
        *out_green_id = w->green_id;
        *out_remaining = w->armed_mask & ~filter_bit;
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

struct rt_reactor {
    rt_scheduler_t *sched;
    int             kq;
    pthread_t       thread;
    rt_waiter_map_t waiters;
};

static void *reactor_loop(void *argp) {
    rt_reactor_t *r = (rt_reactor_t *)argp;
    struct kevent evs[64];

    for (;;) {
        int n = kevent(r->kq, NULL, 0, evs, 64, NULL);
        if (n < 0) {
            if (errno == EINTR) continue;
            rt_trap("rt_reactor: kevent wait failed");
        }
        for (int i = 0; i < n; i++) {
            if (evs[i].filter == EVFILT_USER) {
                /* The shutdown signal (rt_reactor_destroy). Told apart from
                 * a real fd event by FILTER, not by comparing fd numbers --
                 * see RT_REACTOR_SHUTDOWN_IDENT's own comment. */
                return NULL;
            }

            uint32_t fd  = (uint32_t)evs[i].ident;
            uint64_t seq = (uint64_t)(uintptr_t)evs[i].udata;
            uint32_t filter_bit = (evs[i].filter == EVFILT_WRITE)
                                       ? RT_REACTOR_WRITE : RT_REACTOR_READ;

            uint32_t green_id, remaining;
            if (wmap_consume(&r->waiters, fd, seq, filter_bit, &green_id,
                              &remaining)) {
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

    wmap_init(&r->waiters, 64);

    if (pthread_create(&r->thread, NULL, reactor_loop, r) != 0) {
        rt_trap("rt_reactor_create: could not start the reactor thread");
    }
    return r;
}

void rt_reactor_wait(rt_reactor_t *r, int fd, uint32_t events) {
    /* Traps if not called from inside a running green thread -- the same
     * contract rt_sched_park (called below) already enforces; asking here
     * too gives a clearer message naming THIS function if it is misused. */
    uint32_t green_id = rt_sched_current_green_id();

    uint32_t old_mask;
    uint64_t seq = wmap_upsert(&r->waiters, (uint32_t)fd, green_id, events,
                                &old_mask);

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
    if (na > 0 && kevent(r->kq, add, na, NULL, 0, NULL) != 0) {
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
    wmap_destroy(&r->waiters);
    free(r);
}
