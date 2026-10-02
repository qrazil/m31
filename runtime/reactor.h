/* The reactor (docs/concurrency-decision.md, "Phases" -- Phase 3: epoll
 * reactor, park/unpark, blocking-FFI handoff; "Phase 3.5": kqueue port for
 * macOS/BSD). Built on top of the Phase 2 scheduler's park/unpark surface
 * (runtime/scheduler.h) -- neither backend touches scheduler.c/.h itself at
 * all, each only calls the additive API that lives there.
 *
 * ONE CONTRACT, TWO BACKENDS: this header is the entire cross-platform
 * contract. Exactly one of runtime/reactor_epoll.c (Linux: epoll_wait,
 * eventfd) or runtime/reactor_kqueue.c (macOS/BSD: kevent, EVFILT_USER) is
 * compiled into any given build -- runtime/arch.sh picks which, per
 * `uname -s`, the same way it already picks the per-architecture
 * context-switch file. Nothing outside those two .c files -- not rt.c, not
 * scheduler.c, not this header's own declarations below -- knows or needs
 * to know which backend a given build linked; `struct rt_reactor` is
 * opaque here specifically so each backend can give it entirely different
 * fields. See whichever backend file is actually linked for the OS-specific
 * design reasoning (the epoll<->kqueue mapping, why EVFILT_USER instead of
 * a self-pipe, the EV_ONESHOT-vs-EPOLLONESHOT edge case that does NOT carry
 * over unchanged -- kqueue splits read/write into independent knotes where
 * epoll has one combined registration, which is the one place a naive
 * line-for-line port would have introduced a spurious-wakeup bug).
 *
 * This is a STANDALONE component, the same convention runtime/scheduler.h
 * documents for itself: not #included by rt.c, not reachable from a
 * compiled .src program, exercised only by runtime/reactor_test.c. Wiring a
 * future I/O primitive (`lib/net.src`/`lib/io.src`) to actually call this is
 * explicitly out of scope for this phase -- see the task that produced this
 * file.
 *
 * WHAT THIS IS: a component that watches registered file descriptors for
 * readiness via a dedicated OS thread running the platform's blocking
 * readiness-wait syscall (epoll_wait or kevent) in a loop, and, on a ready
 * fd, looks up which green thread (if any) is waiting on it and calls
 * rt_sched_unpark for it.
 *
 * WHERE THE REACTOR LOOP RUNS, AND WHY: one dedicated OS thread per
 * rt_reactor_t, never a carrier. A carrier's whole job is dispatching green
 * threads; blocking in epoll_wait/kevent for an arbitrary time is exactly
 * the kind of foreign block a carrier must never do on its own thread (that
 * is what Part 3's blocking-FFI handoff exists to rescue carriers FROM --
 * using the reactor's own carrier thread to run it would need to rescue
 * itself from itself, which is circular). One dedicated thread is also
 * simply the standard shape for a reactor in exactly this family of
 * designs (Go's netpoller, Node's libuv, Rust's tokio reactor all use a
 * single dedicated OS thread, or a small fixed pool of them, never a
 * worker thread that also runs user code) and needs no further
 * justification here.
 *
 * THE RACE THIS CLOSES, AND WHERE THE FIX ACTUALLY LIVES: the lost-wakeup
 * race between a green thread's EAGAIN and its park call is closed entirely
 * by rt_sched_park/rt_sched_unpark's own CAS protocol (scheduler.h), not by
 * anything in this file. This reactor adds no second state machine of its
 * own on top -- it only needs a registration map (fd -> which green thread
 * id is waiting), because rt_sched_unpark already tolerates being called
 * before, during, or after the matching rt_sched_park for the SAME id. See
 * this file's own rt_reactor_wait for exactly how that composition works,
 * and the top-level report this task produced for the fuller argument of
 * why a second CAS here would be redundant, not merely simpler without
 * one.
 *
 * LIMITATION, STATED PLAINLY: this reactor supports at most one outstanding
 * wait per fd at a time (one `rt_waiter_t` per fd, overwritten by the next
 * rt_reactor_wait call on the same fd) -- correct for the common case a
 * future I/O primitive would use (one green thread owns a socket fd and
 * waits on it, one direction at a time), not a general multi-waiter
 * readiness broadcaster. */
#ifndef RT_REACTOR_H
#define RT_REACTOR_H

#include <stdint.h>

#include "scheduler.h"

typedef struct rt_reactor rt_reactor_t;

/* Event kinds a future I/O primitive would ask to wait for. OR-able. */
#define RT_REACTOR_READ  1u
#define RT_REACTOR_WRITE 2u

/* Starts the reactor's dedicated OS thread. `sched` must outlive the
 * reactor (rt_sched_unpark is called against it for as long as the reactor
 * runs). */
rt_reactor_t *rt_reactor_create(rt_scheduler_t *sched);

/* Called from inside a running green thread, after a non-blocking operation
 * on `fd` returned EAGAIN/EWOULDBLOCK and the caller wants to wait for
 * `events` (RT_REACTOR_READ/RT_REACTOR_WRITE, OR'd) before retrying.
 * Registers interest with the OS readiness mechanism (epoll or kqueue, per
 * backend) for `fd` and parks the calling green thread (rt_sched_park)
 * until `fd` becomes ready for at least one requested event -- UNLESS the
 * reactor's own thread already observed that readiness before this call
 * reached rt_sched_park, in which case rt_sched_park's own CAS protocol
 * makes this return immediately without ever suspending. See this header's
 * own top comment for why no second race-closing mechanism is needed in
 * either backend for that to be correct.
 *
 * Traps if called from outside a running green thread (rt_sched_park's own
 * contract) or if the backend's registration call fails for a reason other
 * than the ordinary "this fd is new to this reactor's interest set" case
 * each backend already handles (e.g. a closed or otherwise invalid fd is a
 * condition this function can usefully recover from). */
void rt_reactor_wait(rt_reactor_t *r, int fd, uint32_t events);

/* Stops the reactor's OS thread and frees everything. The caller must have
 * no green thread currently parked via this reactor (same lifecycle
 * discipline rt_sched_shutdown already asks of its own callers). */
void rt_reactor_destroy(rt_reactor_t *r);

#endif /* RT_REACTOR_H */
