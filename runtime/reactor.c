/* Phase 3 epoll reactor implementation. See reactor.h for the contract and
 * the design reasoning (dedicated OS thread, why no second CAS state
 * machine lives here, the one-waiter-per-fd limitation).
 *
 * Linux-only (epoll, eventfd) -- matching this phase's own stated scope
 * (docs/concurrency-decision.md, "Phases": Phase 3 is Linux/epoll; kqueue
 * is explicitly Phase 4).
 */
#include "reactor.h"
#include "rt.h"

#include <errno.h>
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
    uint32_t green_id;
    bool     added_to_epoll;
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
                                uint32_t green_id, bool added) {
    uint32_t i = fd % m->cap;
    while (m->slots[i].fd != RT_WAITER_EMPTY && m->slots[i].fd != RT_WAITER_TOMB) {
        i = (i + 1) % m->cap;
    }
    m->slots[i].fd = fd;
    m->slots[i].green_id = green_id;
    m->slots[i].added_to_epoll = added;
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
                                old[i].added_to_epoll);
        }
    }
    m->used = m->live;
    free(old);
}

/* Create-or-update: if `fd` already has a waiter, update its green_id (a
 * previous wait on this fd completed and the same green thread -- or a
 * different one, after the fd changed hands -- is waiting again) and say
 * `*need_add = false` (EPOLL_CTL_MOD is right). Otherwise insert a fresh
 * entry and say `*need_add = true` (EPOLL_CTL_ADD is needed). */
static void wmap_upsert(rt_waiter_map_t *m, uint32_t fd, uint32_t green_id,
                         bool *need_add) {
    pthread_mutex_lock(&m->lock);
    rt_waiter_t *w = wmap_find_locked(m, fd);
    if (w != NULL) {
        w->green_id = green_id;
        *need_add = !w->added_to_epoll;
    } else {
        wmap_grow_if_needed_locked(m);
        wmap_insert_locked(m, fd, green_id, false);
        m->used++;
        m->live++;
        *need_add = true;
    }
    pthread_mutex_unlock(&m->lock);
}

static void wmap_mark_added(rt_waiter_map_t *m, uint32_t fd) {
    pthread_mutex_lock(&m->lock);
    rt_waiter_t *w = wmap_find_locked(m, fd);
    if (w != NULL) w->added_to_epoll = true;
    pthread_mutex_unlock(&m->lock);
}

static bool wmap_green_id_for(rt_waiter_map_t *m, uint32_t fd, uint32_t *out) {
    pthread_mutex_lock(&m->lock);
    rt_waiter_t *w = wmap_find_locked(m, fd);
    bool found = (w != NULL);
    if (found) *out = w->green_id;
    pthread_mutex_unlock(&m->lock);
    return found;
}

/* ========================================================================
 * The reactor itself.
 * ====================================================================== */

struct rt_reactor {
    rt_scheduler_t *sched;
    int             epfd;
    int             shutdown_fd; /* eventfd -- write once to stop the loop */
    pthread_t       thread;
    rt_waiter_map_t waiters;
};

static void *reactor_loop(void *argp) {
    rt_reactor_t *r = (rt_reactor_t *)argp;
    struct epoll_event evs[64];

    for (;;) {
        int n = epoll_wait(r->epfd, evs, 64, -1);
        if (n < 0) {
            if (errno == EINTR) continue;
            rt_trap("rt_reactor: epoll_wait failed");
        }
        for (int i = 0; i < n; i++) {
            int fd = evs[i].data.fd;
            if (fd == r->shutdown_fd) {
                return NULL;
            }
            /* The entire race-closing logic lives in rt_sched_unpark's own
             * CAS protocol (runtime/scheduler.c) -- this is just "who was
             * waiting on this fd", looked up and handed off. If nobody
             * is (the waiter already moved on, or this is a stray/second
             * readiness event under level-triggered semantics before the
             * next rt_reactor_wait re-arms it -- EPOLLONESHOT specifically
             * prevents that second case), there is nothing to do. */
            uint32_t green_id;
            if (wmap_green_id_for(&r->waiters, (uint32_t)fd, &green_id)) {
                rt_sched_unpark(r->sched, green_id);
            }
        }
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

    bool need_add;
    wmap_upsert(&r->waiters, (uint32_t)fd, green_id, &need_add);

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

    int op = need_add ? EPOLL_CTL_ADD : EPOLL_CTL_MOD;
    if (epoll_ctl(r->epfd, op, fd, &ev) != 0) {
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
         * this self-healing case, and still traps. */
        if (op == EPOLL_CTL_MOD && errno == ENOENT) {
            if (epoll_ctl(r->epfd, EPOLL_CTL_ADD, fd, &ev) != 0) {
                rt_trap("rt_reactor_wait: epoll_ctl ADD failed even after an "
                        "ENOENT-triggered retry");
            }
            need_add = true;
        } else {
            rt_trap("rt_reactor_wait: epoll_ctl failed");
        }
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
    wmap_destroy(&r->waiters);
    free(r);
}
