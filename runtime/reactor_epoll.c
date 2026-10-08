/* Linux backend of the reactor: epoll_wait + eventfd. See reactor.h for the
 * contract and the design reasoning (dedicated OS thread, why no second CAS
 * state machine lives here, the one-waiter-per-fd limitation) -- that
 * contract is shared with runtime/reactor_kqueue.c (the macOS/BSD backend,
 * docs/concurrency-decision.md "Phase 3.5": kqueue port), and nothing
 * outside these two files knows or cares which one a given build links.
 * runtime/arch.sh is the single place that decides which one, per
 * `uname -s`.
 */
#include "reactor.h"
#include "reactor_timers.h"
#include "rt.h"

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

/* ========================================================================
 * The waiter map: fd -> which green thread id is waiting on it, plus
 * whether this fd has already been EPOLL_CTL_ADD-ed to this reactor's
 * epoll instance (every later (re-)registration on the same fd must use
 * EPOLL_CTL_MOD instead -- EPOLLONESHOT leaves a fd registered-but-disabled
 * after it fires, not removed, and ADD on an fd already in the interest
 * list fails with EEXIST).
 *
 * Mutex-protected open addressing, storing each waiter BY VALUE in its own
 * slot -- no per-waiter allocation at all, so there is nothing to leak and
 * nothing to free individually. Same "correctness over cleverness" choice
 * runtime/scheduler.c's own registry makes, for the same reason: this is
 * not a hot path (one upsert per rt_reactor_wait call, one lookup per
 * ready fd the reactor thread observes), so a plain locked table is simpler
 * and no slower in any way that matters here. */
typedef struct {
    uint32_t fd;             /* key; sentinels below */
    uint32_t green_id;       /* RT_NO_GREEN once the wait has been claimed */
    bool     added_to_epoll;
    /* The bounded wait that owns this registration (runtime/
     * reactor_timers.h), NULL for an unbounded one. Whoever claims the wait
     * -- readiness or the deadline -- clears it, under `lock`. */
    rt_timer_node_t *node;
} rt_waiter_t;

#define RT_WAITER_EMPTY UINT32_MAX
#define RT_WAITER_TOMB  (UINT32_MAX - 1)
/* A real fd is always small and non-negative (RLIMIT_NOFILE), so both
 * sentinels are unreachable as real keys. */

/* green_id of a registration nobody is waiting on any more: a readiness
 * event that finds it must not unpark anyone. (Green ids are handed out
 * counting up from 1 and never reused, so this is not a real id.) */
#define RT_NO_GREEN UINT32_MAX

typedef struct {
    pthread_mutex_t lock;
    rt_waiter_t     *slots;
    uint32_t         cap;
    uint32_t         used; /* occupied, including tombstones */
    uint32_t         live;
} rt_waiter_map_t;

static void wmap_init(rt_waiter_map_t *m, uint32_t cap) {
    m->cap = cap;
    m->slots = malloc(sizeof(rt_waiter_t) * cap);
    if (m->slots == NULL) rt_trap("out of memory: reactor waiter map");
    for (uint32_t i = 0; i < cap; i++) m->slots[i].fd = RT_WAITER_EMPTY;
    m->used = 0;
    m->live = 0;
    pthread_mutex_init(&m->lock, NULL);
}

static void wmap_destroy(rt_waiter_map_t *m) {
    pthread_mutex_destroy(&m->lock);
    free(m->slots);
}

/* Lock held. Finds `fd`'s slot, or NULL if absent. */
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

/* Lock held. `fd` assumed not already present. */
static void wmap_insert_locked(rt_waiter_map_t *m, uint32_t fd,
                                uint32_t green_id, bool added,
                                rt_timer_node_t *node) {
    uint32_t i = fd % m->cap;
    while (m->slots[i].fd != RT_WAITER_EMPTY && m->slots[i].fd != RT_WAITER_TOMB) {
        i = (i + 1) % m->cap;
    }
    m->slots[i].fd = fd;
    m->slots[i].green_id = green_id;
    m->slots[i].added_to_epoll = added;
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
                                old[i].added_to_epoll, old[i].node);
        }
    }
    m->used = m->live;
    free(old);
}

/* Lock held. Create-or-update: if `fd` already has a waiter, update its
 * green_id (a previous wait on this fd completed and the same green thread
 * -- or a different one, after the fd changed hands -- is waiting again) and
 * say `*need_add = false` (EPOLL_CTL_MOD is right). Otherwise insert a fresh
 * entry and say `*need_add = true` (EPOLL_CTL_ADD is needed). `node` is the
 * bounded wait that owns the registration, or NULL; a previous owner's node
 * is not touched here (it stays in the heap and ends at its own deadline --
 * the one-waiter-per-fd limitation, reactor.h). Returns the slot. */
static rt_waiter_t *wmap_upsert_locked(rt_waiter_map_t *m, uint32_t fd,
                                        uint32_t green_id, rt_timer_node_t *node,
                                        bool *need_add) {
    rt_waiter_t *w = wmap_find_locked(m, fd);
    if (w != NULL) {
        w->green_id = green_id;
        w->node = node;
        *need_add = !w->added_to_epoll;
        return w;
    }
    wmap_grow_if_needed_locked(m);
    wmap_insert_locked(m, fd, green_id, false, node);
    m->used++;
    m->live++;
    *need_add = true;
    return wmap_find_locked(m, fd);
}

static void wmap_upsert(rt_waiter_map_t *m, uint32_t fd, uint32_t green_id,
                         bool *need_add) {
    pthread_mutex_lock(&m->lock);
    (void)wmap_upsert_locked(m, fd, green_id, NULL, need_add);
    pthread_mutex_unlock(&m->lock);
}

static void wmap_mark_added(rt_waiter_map_t *m, uint32_t fd) {
    pthread_mutex_lock(&m->lock);
    rt_waiter_t *w = wmap_find_locked(m, fd);
    if (w != NULL) w->added_to_epoll = true;
    pthread_mutex_unlock(&m->lock);
}

/* ========================================================================
 * The reactor itself.
 * ====================================================================== */

struct rt_reactor {
    rt_scheduler_t *sched;
    int             epfd;
    int             shutdown_fd; /* eventfd -- write once to stop the loop */
    int             wake_fd;     /* eventfd -- a new earliest deadline: re-aim */
    pthread_t       thread;
    rt_waiter_map_t waiters;
    /* Deadlines of the bounded waits, guarded by waiters.lock -- the same
     * lock that guards the map, so that "claim this wait" is one critical
     * section whichever of readiness and the deadline gets there. */
    rt_timer_heap_t timers;
};

/* Claims up to `max` bounded waits whose deadline has passed, and returns the
 * green ids to unpark (the caller does that AFTER this returns, lock
 * released). Each claimed wait's registration is released, so a readiness
 * arriving later cannot wake a thread that has moved on.
 *
 * The unpark is outside the lock because rt_sched_unpark can block (a full
 * shared queue, scheduler.c's squeue_push, backpressures a non-green
 * caller), and a carrier running a green thread may be waiting for this very
 * lock in rt_reactor_wait_timeout: unparking under it could deadlock the
 * carrier that has to drain the queue. What the waiter reads, it reads under
 * the lock, so it sees `outcome` only once the claim is complete; the unpark
 * that follows is the (idempotent) wake-up, tolerated before, during or after
 * the park. */
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
            /* Only if the registration is still THIS wait's: a later wait on
             * the same fd took it over otherwise, and is not ours to undo. */
            if (w != NULL && w->node == n) {
                w->node = NULL;
                w->green_id = RT_NO_GREEN;
                if (w->added_to_epoll) {
                    /* Best effort, and the fd may be closed already (which
                     * removed it from epoll by itself): either way it is no
                     * longer armed. The next wait on it ADDs afresh. */
                    (void)epoll_ctl(r->epfd, EPOLL_CTL_DEL, fd, NULL);
                    w->added_to_epoll = false;
                }
            }
        }
        n->outcome = RT_TIMER_EXPIRED; /* last: `n` may be freed once we unlock */
    }
    pthread_mutex_unlock(&r->waiters.lock);
    return k;
}

static void *reactor_loop(void *argp) {
    rt_reactor_t *r = (rt_reactor_t *)argp;
    struct epoll_event evs[64];

    for (;;) {
        /* Sleep until an event, or until the nearest deadline. A deadline
         * that appears while this is already asleep wakes it through
         * wake_fd. */
        pthread_mutex_lock(&r->waiters.lock);
        int wait_ms = rt_timer_heap_wait_ms(&r->timers, rt_timer_now_ns());
        pthread_mutex_unlock(&r->waiters.lock);

        int n = epoll_wait(r->epfd, evs, 64, wait_ms);
        if (n < 0) {
            if (errno == EINTR) continue;
            rt_trap("rt_reactor: epoll_wait failed");
        }
        for (int i = 0; i < n; i++) {
            int fd = evs[i].data.fd;
            if (fd == r->shutdown_fd) {
                return NULL;
            }
            if (fd == r->wake_fd) {
                uint64_t drained;
                ssize_t got = read(r->wake_fd, &drained, sizeof drained);
                (void)got; /* EAGAIN: already drained */
                continue;
            }
            /* The entire race-closing logic lives in rt_sched_unpark's own
             * CAS protocol (runtime/scheduler.c) -- this is just "who was
             * waiting on this fd", looked up and handed off. If nobody
             * is (the waiter already moved on, or this is a stray/second
             * readiness event under level-triggered semantics before the
             * next rt_reactor_wait re-arms it -- EPOLLONESHOT specifically
             * prevents that second case), there is nothing to do.
             *
             * A bounded wait is claimed here, under the lock: its deadline
             * leaves the heap, so the deadline pass below cannot also fire
             * for it. The unpark itself is after the lock is released, for
             * the reason given at expire_timers. */
            pthread_mutex_lock(&r->waiters.lock);
            rt_waiter_t *w = wmap_find_locked(&r->waiters, (uint32_t)fd);
            bool claimed = (w != NULL && w->green_id != RT_NO_GREEN);
            uint32_t green_id = 0;
            if (claimed) {
                green_id = w->green_id;
                w->green_id = RT_NO_GREEN;
                if (w->node != NULL) {
                    rt_timer_node_t *node = w->node;
                    w->node = NULL;
                    rt_timer_heap_remove(&r->timers, node);
                    node->outcome = RT_TIMER_READY;
                }
            }
            pthread_mutex_unlock(&r->waiters.lock);
            if (claimed) rt_sched_unpark(r->sched, green_id);
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

    r->epfd = epoll_create1(0);
    if (r->epfd < 0) rt_trap("rt_reactor_create: epoll_create1 failed");

    r->shutdown_fd = eventfd(0, EFD_NONBLOCK);
    if (r->shutdown_fd < 0) rt_trap("rt_reactor_create: eventfd failed");

    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = r->shutdown_fd;
    if (epoll_ctl(r->epfd, EPOLL_CTL_ADD, r->shutdown_fd, &ev) != 0) {
        rt_trap("rt_reactor_create: could not register the shutdown eventfd");
    }

    r->wake_fd = eventfd(0, EFD_NONBLOCK);
    if (r->wake_fd < 0) rt_trap("rt_reactor_create: eventfd failed");
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.fd = r->wake_fd;
    if (epoll_ctl(r->epfd, EPOLL_CTL_ADD, r->wake_fd, &ev) != 0) {
        rt_trap("rt_reactor_create: could not register the wake eventfd");
    }

    wmap_init(&r->waiters, 64);

    if (pthread_create(&r->thread, NULL, reactor_loop, r) != 0) {
        rt_trap("rt_reactor_create: could not start the reactor thread");
    }
    return r;
}

/* (Re-)arm `fd` in the epoll set for `events`, one-shot. `*need_add` says
 * whether EPOLL_CTL_ADD (true) or MOD is right, and comes back true if the
 * self-healing fallback below had to ADD after all. Returns 0 or the errno.
 * Needs no lock: epoll_ctl is safe against the reactor thread's epoll_wait. */
static int epoll_arm(rt_reactor_t *r, int fd, uint32_t events, bool *need_add) {
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    /* EPOLLONESHOT: exactly one readiness notification per registration,
     * then disabled until explicitly re-armed (EPOLL_CTL_MOD) by the next
     * rt_reactor_wait call on this fd. Without this, a level-triggered fd
     * that stays ready (the ordinary case: a socket with more buffered
     * data than one read drained) would fire repeatedly before the green
     * thread it already woke has had a chance to re-register, which this
     * single-waiter-per-fd design has no way to attribute correctly. */
    ev.events = EPOLLONESHOT;
    if (events & RT_REACTOR_READ) ev.events |= EPOLLIN;
    if (events & RT_REACTOR_WRITE) ev.events |= EPOLLOUT;
    ev.data.fd = fd;

    int op = *need_add ? EPOLL_CTL_ADD : EPOLL_CTL_MOD;
    if (epoll_ctl(r->epfd, op, fd, &ev) == 0) return 0;
    /* Self-healing fallback: closing a file descriptor removes it from
     * every epoll instance's interest list automatically (epoll(7)),
     * but this reactor's waiter map has no way to know that happened --
     * it only finds out the fd is gone when the OS later hands that
     * same (small, recycled) fd number to a brand new, unrelated file.
     * The map's stale "already added" bit then wrongly asks for MOD,
     * which fails ENOENT because the kernel genuinely has no record of
     * it. Recover by actually adding it, which is what should have
     * happened. Any other errno (a genuinely invalid or already-closed
     * fd passed by the caller, for instance) is a real caller bug, not
     * this self-healing case. */
    if (op == EPOLL_CTL_MOD && errno == ENOENT) {
        if (epoll_ctl(r->epfd, EPOLL_CTL_ADD, fd, &ev) != 0) return errno;
        *need_add = true;
        return 0;
    }
    return errno;
}

void rt_reactor_wait(rt_reactor_t *r, int fd, uint32_t events) {
    /* Traps if not called from inside a running green thread -- the same
     * contract rt_sched_park (called below) already enforces; asking here
     * too gives a clearer message naming THIS function if it is misused. */
    uint32_t green_id = rt_sched_current_green_id();

    bool need_add;
    wmap_upsert(&r->waiters, (uint32_t)fd, green_id, &need_add);

    /* A closed or invalid fd passed here is a caller bug and traps; a caller
     * for whom that is an ordinary event uses rt_reactor_wait_timeout. */
    if (epoll_arm(r, fd, events, &need_add) != 0) {
        rt_trap("rt_reactor_wait: epoll_ctl failed");
    }
    if (need_add) {
        wmap_mark_added(&r->waiters, (uint32_t)fd);
    }

    /* THE race-closing step: entirely rt_sched_park's own CAS protocol
     * (runtime/scheduler.c, runtime/scheduler.h). If the reactor thread's
     * epoll_wait already observed `fd` ready and called rt_sched_unpark
     * for `green_id` in the gap between the epoll_ctl call above and this
     * line, rt_sched_park's CAS finds NOTIFIED (not EMPTY) and returns
     * immediately, having never suspended at all -- exactly the fix for
     * the lost-wakeup race this whole phase is built around. See
     * reactor.h's top comment for why no second CAS state machine is
     * needed in this file for that composition to be correct. */
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

    /* Registration and deadline go in under ONE hold of the lock. The
     * reactor thread can claim neither until it is released, so by the time
     * anything can fire, both the fd's slot and the heap entry exist and
     * point at the same node -- there is no window in which a deadline can
     * expire for a wait whose registration is not in place yet (or the other
     * way round). epoll_ctl is a short syscall; holding the lock across it
     * costs the reactor thread a moment, not a block. */
    pthread_mutex_lock(&r->waiters.lock);
    if (!sleep_only) {
        bool need_add;
        rt_waiter_t *w = wmap_upsert_locked(&r->waiters, (uint32_t)fd,
                                            node.green_id, &node, &need_add);
        int e = epoll_arm(r, fd, events, &need_add);
        if (e != 0) {
            w->green_id = RT_NO_GREEN;
            w->node = NULL;
            pthread_mutex_unlock(&r->waiters.lock);
            return -e;
        }
        if (need_add) w->added_to_epoll = true;
    }
    bool earliest = rt_timer_heap_push(&r->timers, &node);
    pthread_mutex_unlock(&r->waiters.lock);

    if (earliest) {
        /* The reactor thread may be asleep until some later deadline (or
         * for ever). Level-triggered, so it is seen even if the thread is
         * between epoll_wait calls right now. EAGAIN (counter full) means a
         * wake is pending already. */
        uint64_t one = 1;
        ssize_t w = write(r->wake_fd, &one, sizeof one);
        (void)w;
    }

    /* Park until the wait is CLAIMED. rt_sched_park may return early on a
     * notification left over from some earlier, unrelated unpark (the stale
     * unpark hazard reactor_kqueue.c documents); that must not end the
     * wait, so it re-checks the outcome -- under the lock the claimer
     * writes it under -- and parks again. (The one residue: a waiter that
     * wakes on such a stale notification in the instant between a claim and
     * its unpark returns early and leaves that unpark pending as a stale
     * notification for its NEXT park. That is the same hazard, no wider, and
     * every caller of a park already retries on a wake that found nothing
     * done.) */
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
    uint64_t one = 1;
    ssize_t written = write(r->shutdown_fd, &one, sizeof one);
    if (written != (ssize_t)sizeof one) {
        rt_trap("rt_reactor_destroy: could not signal the reactor thread to "
                "stop (write to the shutdown eventfd failed)");
    }
    pthread_join(r->thread, NULL);
    close(r->epfd);
    close(r->shutdown_fd);
    close(r->wake_fd);
    free(r->timers.a);
    wmap_destroy(&r->waiters);
    free(r);
}
