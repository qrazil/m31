/* Phase 3 epoll reactor (docs/concurrency-decision.md, "Phases" -- Phase 3:
 * epoll reactor, park/unpark, blocking-FFI handoff). Built on top of the
 * Phase 2 scheduler's park/unpark surface (runtime/scheduler.h) -- this file
 * does not touch scheduler.c/.h itself at all, it only calls the additive
 * API that lives there.
 *
 * This is a STANDALONE component, the same convention runtime/scheduler.h
 * documents for itself: not #included by rt.c, not reachable from a
 * compiled .src program, exercised only by runtime/reactor_test.c. Wiring a
 * future I/O primitive (`lib/net.src`/`lib/io.src`) to actually call this is
 * explicitly out of scope for this phase -- see the task that produced this
 * file.
 *
 * WHAT THIS IS: a component that watches registered file descriptors for
 * readiness via a dedicated OS thread running epoll_wait in a loop, and, on
 * a ready fd, looks up which green thread (if any) is waiting on it and
 * calls rt_sched_unpark for it.
 *
 * WHERE THE REACTOR LOOP RUNS, AND WHY: one dedicated OS thread per
 * rt_reactor_t, never a carrier. A carrier's whole job is dispatching green
 * threads; epoll_wait blocking for an arbitrary time is exactly the kind of
 * foreign block a carrier must never do on its own thread (that is what
 * Part 3's blocking-FFI handoff exists to rescue carriers FROM -- using the
 * reactor's own carrier thread to run epoll_wait would need to rescue
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
 * Registers interest with epoll for `fd` and parks the calling green thread
 * (rt_sched_park) until `fd` becomes ready for at least one requested
 * event -- UNLESS the reactor's own thread already observed that readiness
 * before this call reached rt_sched_park, in which case rt_sched_park's own
 * CAS protocol makes this return immediately without ever suspending. See
 * this header's own top comment for why no second race-closing mechanism
 * is needed in this file for that to be correct.
 *
 * Traps if called from outside a running green thread (rt_sched_park's own
 * contract) or if epoll_ctl fails for a reason other than the ordinary
 * "this fd is new to this epoll instance" case this function already
 * handles (e.g. a closed or otherwise invalid fd is a caller bug, not a
 * condition this function can usefully recover from). */
void rt_reactor_wait(rt_reactor_t *r, int fd, uint32_t events);

/* Stops the reactor's OS thread and frees everything. The caller must have
 * no green thread currently parked via this reactor (same lifecycle
 * discipline rt_sched_shutdown already asks of its own callers). */
void rt_reactor_destroy(rt_reactor_t *r);

#endif /* RT_REACTOR_H */
