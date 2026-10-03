/* Linux backend of the reactor: epoll_wait + eventfd. See reactor.h for the
 * contract and the design reasoning (dedicated OS thread, why no second CAS
 * state machine lives here, the one-waiter-per-fd limitation) -- that
 * contract is shared with runtime/reactor_kqueue.c (the macOS/BSD backend,
 * docs/concurrency-decision.md "Phase 3.5": kqueue port), and nothing
 * outside these two files knows or cares which one a given build links.
 * runtime/arch.sh is the single place that decides which one, per
 * `uname -s`.
 *
 * ========================================================================
 * WHY THERE IS NO SHARED WAITER MAP HERE ANY MORE (there used to be one --
 * see git history/the task that produced this comment for the measurement
 * that motivated removing it).
 *
 * The old design kept a second, mutex-protected, fd -> green_id lookup
 * table (`rt_waiter_map_t`) purely so the reactor thread's wake path
 * (reactor_loop, below) could translate "fd N is ready" into "green thread
 * G is ready" after epoll_wait returned. That translation does not need a
 * lookup at all: `struct epoll_event`'s `data` field is a union
 * (`int fd; uint32_t u32; uint64_t u64; void *ptr;`) that the KERNEL treats
 * as 100% opaque caller payload -- `epoll_ctl` takes `fd` as its own
 * separate, explicit argument, so nothing about how epoll itself works
 * depends on what gets put in `data`. So instead of writing `fd` into
 * `data` and looking `green_id` up from a shared table on the way out, this
 * file now writes `green_id` into `data.u64` directly (rt_reactor_wait,
 * below) and reads it straight back out (reactor_loop, below) -- zero
 * lookups, zero locks, on the one OS thread that handles every single
 * readiness event this process has.
 *
 * This was worth doing because that lookup's lock was found, by reading
 * this file and runtime/scheduler.c end to end against a benchmark showing
 * m31 falling behind an equivalent Go server as concurrency/event-rate
 * climbed, to be one of three global mutexes serially acquired, by this
 * one dedicated reactor thread, for EVERY event in EVERY epoll_wait batch,
 * before any of the 12 carrier OS threads could touch the resulting work.
 * The other two -- rt_sched_unpark's registry lock and squeue_push's
 * run-queue lock, both in runtime/scheduler.c -- are a separate, far
 * bigger piece of work (real green-thread lifetime/reclamation safety, and
 * deliberate cross-carrier load-balancing, respectively) and are
 * DELIBERATELY left exactly as they were; this file's own rt_sched_unpark
 * call below still goes through both, unchanged.
 *
 * THE WRINKLE THIS CREATED, AND HOW IT IS SOLVED: the waiter map did a
 * second job besides the wake-path lookup -- `wmap_upsert`'s `need_add`
 * output told rt_reactor_wait whether to use EPOLL_CTL_ADD (first time this
 * fd is registered) or EPOLL_CTL_MOD (already registered, just re-arming
 * after EPOLLONESHOT). Losing the map loses that bookkeeping too, and it
 * cannot simply move into `data` alongside the green_id (there is nowhere
 * left to put it once `data.u64` already holds a full green_id, and it is
 * exactly the kind of mutable-after-registration state epoll's own `data`
 * is not meant to carry). The fix is to notice that this bit does not need
 * to be SHARED at all: one fd's registration history belongs to exactly one
 * green thread for that fd's entire life (one connection, one green thread
 * reading it) -- nothing else ever concurrently touches whether THIS
 * specific fd has already been added to this epoll instance. So it is now
 * tracked as a plain bool living directly on the calling green thread's own
 * control block (runtime/scheduler.c's rt_green_t, via
 * rt_sched_current_reactor_added/_mark_added, runtime/scheduler.h) instead
 * of in any structure a second thread could ever need to lock to read.
 * Exactly the same safety argument rt_sched_current_green_id already rests
 * on applies: nothing else can be touching a green thread's own fields
 * while that green thread is the one currently running.
 *
 * One green thread occasionally waiting on more than one fd over its life
 * (not the common case this runtime is built around -- one connection, one
 * green thread, one fd for its whole life -- but not forbidden either)
 * still works correctly even though this bool is never reset back to false
 * once set: switching to a new, never-registered fd while the bool already
 * reads "added" makes rt_reactor_wait try EPOLL_CTL_MOD first, which the
 * kernel fails with ENOENT (no such registration) -- and the self-healing
 * ENOENT-triggered EPOLL_CTL_ADD retry a few lines below (unchanged from
 * before this redesign) recovers exactly that case. The only direction
 * that fallback does NOT cover -- believing "not added" when the kernel
 * already has a registration for this fd -- cannot happen: the bool starts
 * false only once, at rt_sched_spawn, before this green thread has ever
 * called rt_reactor_wait at all.
 *
 * THE SHUTDOWN FD: reactor_loop used to recognize the shutdown event by
 * comparing the raw fd it read back out of `data.fd` against
 * `r->shutdown_fd`. Now that `data` carries a green_id instead of a raw fd,
 * that comparison is replaced by a reserved sentinel value,
 * RT_REACTOR_SHUTDOWN_SENTINEL, registered as the shutdown eventfd's own
 * `data.u64` and checked for first in reactor_loop. UINT64_MAX is genuinely
 * impossible as a real green_id, not merely unlikely: green ids are
 * uint32_t, and rt_sched_spawn (runtime/scheduler.c) traps before ever
 * handing one out that is `>= RT_SCHED_ID_CAPACITY` (1u << 22) -- so no
 * real green_id this process will ever see comes anywhere close to even
 * UINT32_MAX, let alone UINT64_MAX.
 * ======================================================================== */
#include "reactor.h"
#include "rt.h"

#include <errno.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

/* See this file's top comment. No real green_id can ever equal this -- see
 * that comment for why -- so it can never collide with one. */
#define RT_REACTOR_SHUTDOWN_SENTINEL UINT64_MAX

struct rt_reactor {
    rt_scheduler_t *sched;
    int             epfd;
    int             shutdown_fd; /* eventfd -- write once to stop the loop */
    pthread_t       thread;
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
            uint64_t payload = evs[i].data.u64;
            if (payload == RT_REACTOR_SHUTDOWN_SENTINEL) {
                return NULL;
            }
            /* The entire race-closing logic lives in rt_sched_unpark's own
             * CAS protocol (runtime/scheduler.c) -- this just hands off the
             * green_id epoll_ctl was told to carry for us (rt_reactor_wait,
             * below). No "is anyone actually still waiting on this" check
             * is needed here any more: rt_sched_unpark's own registry
             * lookup already safely no-ops (returns false) if `green_id` no
             * longer refers to a live green thread, which is exactly the
             * "nobody is waiting" case the old waiter-map lookup used to
             * gate this call on. */
            rt_sched_unpark(r->sched, (uint32_t)payload);
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
    ev.data.u64 = RT_REACTOR_SHUTDOWN_SENTINEL;
    if (epoll_ctl(r->epfd, EPOLL_CTL_ADD, r->shutdown_fd, &ev) != 0) {
        rt_trap("rt_reactor_create: could not register the shutdown eventfd");
    }

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

    /* ADD-vs-MOD: see this file's top comment for why this is now a plain
     * read of a bool owned by the calling green thread itself, not a
     * lookup in any shared structure. */
    bool need_add = !rt_sched_current_reactor_added();

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
    /* Carry the green_id itself through the kernel instead of the fd --
     * see this file's top comment for the full reasoning. epoll_ctl takes
     * `fd` as its own explicit argument below, so nothing about epoll's own
     * bookkeeping depends on what `data` holds. */
    ev.data.u64 = (uint64_t)green_id;

    int op = need_add ? EPOLL_CTL_ADD : EPOLL_CTL_MOD;
    if (epoll_ctl(r->epfd, op, fd, &ev) != 0) {
        /* Self-healing fallback: closing a file descriptor removes it from
         * every epoll instance's interest list automatically (epoll(7)),
         * but this reactor has no way to know that happened -- it only
         * finds out the fd is gone when the OS later hands that same
         * (small, recycled) fd number to a brand new, unrelated file. The
         * calling green thread's "already added" bit then wrongly asks for
         * MOD, which fails ENOENT because the kernel genuinely has no
         * record of it. Recover by actually adding it, which is what
         * should have happened. Any other errno (a genuinely invalid or
         * already-closed fd passed by the caller, for instance) is a real
         * caller bug, not this self-healing case, and still traps. */
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
        rt_sched_current_reactor_mark_added();
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
    free(r);
}
