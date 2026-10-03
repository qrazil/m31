/* Phase 2 green-thread scheduler (docs/concurrency-decision.md, "Scheduler
 * queues" and "Phases" -- this is Phase 2, built on Phase 1's primitives:
 * runtime/greenthread.h's context switch, slab stack allocator, and
 * byte-per-thread state table).
 *
 * This is a STANDALONE component. It is not #included by rt.c, it is not
 * reachable from a compiled .m31 program, and `spawn` still means an OS
 * thread (rt.h's "concurrency" section) exactly as it did after Phase 1.
 * Wiring the language's `spawn` keyword to use this scheduler instead of a
 * raw OS thread is a deliberate, separate, later decision -- see the task
 * that produced this file. Everything here is exercised only by
 * runtime/scheduler_test.c, its own test harness, which links this file,
 * runtime/rt.c and runtime/ctx_switch_x86_64.S directly.
 *
 * The design, summarised (docs/concurrency-decision.md has the full
 * reasoning and the rejected alternatives):
 *
 *   - One shared, bounded, mutex-protected global queue. Every spawn --
 *     regardless of origin -- routes through it. Push blocks when full
 *     (real backpressure, not silent unbounded growth).
 *   - Each carrier (one OS thread per core by default) keeps a small local
 *     buffer: a claimed batch of work, never a landing spot for new spawns.
 *     NOTHING ever reaches into another carrier's local buffer -- no
 *     stealing, in any form, anywhere. The local buffer is a plain,
 *     unsynchronized ring buffer for exactly this reason: only the owning
 *     carrier's own OS thread ever touches it, so it needs no lock.
 *   - Self-service draw: when a carrier's local buffer empties, it draws
 *     min(available, fuel_size) from the shared queue -- even a single
 *     available item, immediately, with no minimum threshold.
 *   - Wake targeting: a shared, persistent random permutation over carrier
 *     indices, walked on every push and advanced past non-idle carriers,
 *     waking the first idle one found via ITS OWN wake primitive (a
 *     semaphore) -- never a single shared condvar, whose wakeup order is
 *     not actually unspecified in practice (glibc wakes FIFO), which is
 *     exactly the bug simulation found and fixed. Every carrier's idle flag
 *     starts true, reflecting reality at t=0, not false-until-proven -- the
 *     other bug simulation found and fixed. A short periodic timeout on the
 *     semaphore wait is the correctness-floor fallback, not the primary
 *     mechanism.
 *   - `LANG_FUEL_SIZE` (default 4) and `LANG_GLOBAL_QUEUE_CAP` (default 64,
 *     clamped up to `LANG_FUEL_SIZE` with a warning if set lower) are read
 *     once, from the environment, when a scheduler is created -- fail-soft
 *     to the default on anything unparseable, never a crash.
 *
 * rt_scheduler_t and rt_green_t are opaque, the same convention rt.h uses
 * for Chan: everything needed from outside this file is a function below. */
#ifndef RT_SCHEDULER_H
#define RT_SCHEDULER_H

#include <stdbool.h>
#include <stdint.h>

#include "greenthread.h" /* rt_green_state_t, for rt_sched_park's parameter */

typedef struct rt_scheduler rt_scheduler_t;

/* Create a scheduler and start every carrier OS thread before returning.
 *
 * `n_carriers`: pass 0 to auto-detect (LANG_NUM_CARRIERS if set and valid,
 * else sysconf(_SC_NPROCESSORS_ONLN), else 1), or a specific count.
 *
 * LANG_FUEL_SIZE and LANG_GLOBAL_QUEUE_CAP are read here, once, from the
 * environment -- not re-read for the lifetime of this scheduler. This is
 * "read once at process startup" scoped to one scheduler instance rather
 * than a single process-wide read, which is what lets a test harness (or
 * any program that, unusually, wants more than one scheduler) configure
 * each one independently; a real deployment has exactly one scheduler, so
 * the two readings coincide there. */
rt_scheduler_t *rt_sched_create(uint32_t n_carriers);

/* Spawn a green thread running `entry(arg)`. Always routes through the
 * shared global queue -- never lands directly on any carrier's local
 * buffer, regardless of which thread calls this. Blocks if the queue is at
 * capacity, until a carrier's draw frees room (real backpressure).
 *
 * `arg`'s lifetime is the caller's responsibility, exactly like rt_spawn
 * (rt.h) -- this scheduler does not copy or free it.
 *
 * Returns the green thread's id in the byte-per-thread state table
 * (runtime/greenthread.h, Part 4) -- readable with rt_gtstate_get for
 * diagnostics/tests. Production code has no reason to need this back; it
 * is returned because it is free to provide and genuinely useful for
 * exactly that. */
uint32_t rt_sched_spawn(rt_scheduler_t *s, void (*entry)(void *), void *arg);

/* Callable only from inside a running green thread's own call stack (code
 * called, directly or transitively, by a function passed to
 * rt_sched_spawn). Suspends the calling green thread, marks it Runnable
 * again, and switches back to this green thread's OWN carrier's scheduling
 * loop so it can run something else -- the green thread resumes later, on
 * the SAME carrier, never any other (no migration, which is part of what
 * "no stealing" means here). Traps if called from outside a green thread
 * (e.g. from a carrier's own loop, or an ordinary OS thread). */
void rt_sched_yield(void);

/* Which carrier (0 .. n_carriers-1) is executing right now. Valid from a
 * carrier's own scheduling loop or from inside a green thread running on
 * it; returns UINT32_MAX from any other thread. Exists for diagnostics and
 * tests (e.g. confirming a green thread never migrates carriers across a
 * yield, and confirming wake-target variance). */
uint32_t rt_sched_current_carrier(void);

/* The id (runtime/greenthread.h, Part 4) of the green thread currently
 * running on THIS OS thread. Callable only from inside a running green
 * thread's own call stack -- traps otherwise, same as rt_sched_yield. Exists
 * for a future I/O primitive (the epoll reactor, runtime/reactor.h) that
 * needs to know who is about to park without the caller threading an id
 * through by hand. */
uint32_t rt_sched_current_green_id(void);

/* Caller-owned replacement for what used to be a second, shared,
 * mutex-protected lookup table in the epoll reactor (runtime/reactor_epoll.c
 * -- see its own top comment and rt_reactor_wait for the full reasoning):
 * "has EPOLL_CTL_ADD already been done for whichever fd this green thread is
 * currently waiting on, or does this registration need EPOLL_CTL_ADD rather
 * than EPOLL_CTL_MOD". That bit is read and written only by the one green
 * thread it describes, only while that green thread is the one actually
 * running -- the exact same safety argument rt_sched_current_green_id
 * already rests on (nothing else can be touching this green thread's own
 * control block right now) -- so it can live directly on rt_green_t instead
 * of in a structure anything else ever needs to lock. Starts false at
 * rt_sched_spawn and is set true by rt_sched_current_reactor_mark_added;
 * nothing ever resets it back to false, which is still correct even for a
 * green thread that ends up waiting on more than one fd over its life --
 * see reactor_epoll.c's rt_reactor_wait for why. Both trap if called from
 * outside a running green thread, same as rt_sched_current_green_id. */
bool rt_sched_current_reactor_added(void);
void rt_sched_current_reactor_mark_added(void);

/* ========================================================================
 * Park / unpark -- Phase 3 (docs/concurrency-decision.md, "Phases": epoll
 * reactor, park/unpark, blocking-FFI handoff). New surface, additive to
 * Phase 2: rt_sched_yield (above) is cooperative multitasking -- it
 * suspends a green thread and immediately re-marks it Runnable on the SAME
 * carrier, because the only thing it is ever waiting for is "my turn again".
 * Park/unpark is different in kind, not degree: a green thread parks
 * PENDING AN EXTERNAL EVENT (data arriving on a fd, a timer firing, another
 * green thread sending on a channel) and is woken by a DIFFERENT thread --
 * typically not a carrier at all, e.g. the epoll reactor's own dedicated OS
 * thread -- calling rt_sched_unpark by id, at a time no carrier controls or
 * predicts. That is a real, new kind of suspension Phase 2 has no surface
 * for at all, and it is the one place in this whole phase where getting the
 * ordering wrong produces the worst possible failure: a green thread that
 * sleeps forever because the wakeup that was meant for it arrived a moment
 * too early and found nobody listening yet (the "lost wakeup" race --
 * docs/concurrency-decision.md, "What this costs").
 *
 * THE MECHANISM, PRECISELY: every green thread carries one atomic state
 * word for its whole life. The textbook version of this (java.util.
 * concurrent.locks.LockSupport, Rust's std::thread::park/unpark) is THREE
 * states -- EMPTY / PARKED / NOTIFIED. This scheduler needs FOUR --
 * EMPTY / ARMED / PARKED / NOTIFIED -- and the extra state is not
 * decoration: a three-state version was tried first here, looked correct,
 * passed every ordinary test, and was then caught by this project's own
 * TSan gate producing a real, reproducible stack-corruption SEGV (full
 * account in scheduler.c, on the enum itself). The one-sentence version:
 * a green thread's CAS necessarily runs BEFORE it has actually switched
 * stacks (switching is what the CAS is deciding whether to do at all), so
 * a three-state version publishes "externally resumable" a moment before
 * it is actually true, and an unpark racing into that exact window can get
 * a second carrier to resume a context that has not finished being saved
 * yet -- two OS threads on one stack. ARMED exists to name that moment
 * honestly: "decided to park, not yet safely off my own stack."
 *
 *   rt_sched_park, stage 1 (own stack, BEFORE switching away):
 *     CAS  EMPTY    -> ARMED     (succeeds: proceed to actually switch away)
 *     CAS  NOTIFIED -> EMPTY     (fails above: already notified -- consume
 *                                 it, return at once, WITHOUT ever
 *                                 suspending; no switch happens in this
 *                                 path, so there is no stack-sharing hazard
 *                                 to worry about here)
 *
 *   carrier_dispatch, stage 2 (AFTER rt_fiber_switch returns there, i.e.
 *   provably once this green thread is off its own stack):
 *     CAS  ARMED    -> PARKED    (succeeds: genuinely, safely parked now --
 *                                 NOW it is safe for an unpark to resume
 *                                 g->ctx from any carrier)
 *     CAS  NOTIFIED -> EMPTY     (fails above: an unpark raced in during
 *                                 the gap between stage 1 and stage 2 --
 *                                 nobody else will ever call unpark again
 *                                 for this episode, so carrier_dispatch
 *                                 itself requeues the thread, right here)
 *
 *   rt_sched_unpark, any thread, any time:
 *     exchange -> NOTIFIED, learn the previous value:
 *       prev == EMPTY or ARMED: not yet safely parked (may still be
 *                                mid-switch, actually on its own stack right
 *                                now) -- record the notification only;
 *                                physically touching g here would be the
 *                                exact bug above. Whichever of stage 1 or
 *                                stage 2 checks next will find NOTIFIED and
 *                                requeue the thread itself.
 *       prev == PARKED         : genuinely, safely suspended right now --
 *                                actually push it back into real
 *                                circulation.
 *       prev == NOTIFIED       : already notified -- idempotent, no double
 *                                wake.
 *
 * Whichever of {the green thread's own stage-1 CAS, carrier_dispatch's
 * stage-2 CAS, an external rt_sched_unpark} reaches the word first is
 * handled correctly, and the others see consistent state -- this is the
 * whole of the fix, now closing both the original lost-wakeup race AND the
 * stack-corruption race a naive three-state version has. In particular,
 * rt_sched_unpark is always safe to call at ANY point relative to the
 * matching rt_sched_park -- before it is even reached, mid-switch, or
 * after it has genuinely parked: that is exactly the race this closes
 * (e.g. an epoll reactor observing readiness in the gap between a green
 * thread's EAGAIN and its own park call -- docs/concurrency-decision.md's
 * own framing of this race). See scheduler.c's "park/unpark" section (the
 * park_word enum's own comment, rt_sched_park, and carrier_dispatch's
 * "parked" branch) for the full implementation and the registry that makes
 * a green thread findable by id from any thread, including one that never
 * called rt_sched_spawn.
 *
 * A parked thread's saved context lives exactly where a yielded thread's
 * does: in its own rt_ctx_t (runtime/greenthread.h) inside its own
 * rt_green_t control block -- parking does not move or copy it anywhere.
 * "Runnable again" is implemented by reusing the EXISTING shared global
 * queue (scheduler.c's rt_squeue_t) as the resume path: rt_sched_unpark
 * pushes the SAME rt_green_t pointer a fresh rt_sched_spawn would have
 * created, and whichever carrier draws it calls the SAME carrier_dispatch
 * function a fresh spawn uses. No second "resume this saved context" entry
 * kind was needed -- rt_fiber_switch resumes a previously-parked rt_ctx_t
 * exactly as it would a brand new one built by rt_ctx_make, so one pointer
 * type already serves both purposes. Unlike an ordinary yield, a parked
 * thread CAN resume on a different carrier than before: nothing but the
 * reactor/timer/whatever is driving when it becomes runnable again, and
 * there is no reason to insist it lands back on the specific carrier that
 * happened to be running it when it parked. */

/* Must be called only from inside a running green thread's own call stack.
 * Suspends the calling green thread, marking it `parked_state`
 * (RT_GT_PARKED_IO / RT_GT_PARKED_CHAN / RT_GT_PARKED_TIMER / RT_GT_PARKED_QUEUE --
 * runtime/greenthread.h) in the shared state table -- UNLESS a matching
 * rt_sched_unpark(this green thread's id) already happened (the CAS
 * above), in which case this returns immediately without suspending at
 * all, having consumed that notification. Does not requeue the thread
 * anywhere by itself; it resumes only once some thread calls
 * rt_sched_unpark naming its id. Traps if called from outside a running
 * green thread, same as rt_sched_yield. */
void rt_sched_park(rt_green_state_t parked_state);

/* Callable from ANY thread -- a carrier, the parked thread's own former
 * carrier, an epoll reactor's dedicated OS thread, a timer thread, an
 * ordinary OS thread with no relationship to this scheduler at all.
 * Idempotent and safe to call before, during, or after the matching
 * rt_sched_park (see the CAS protocol above) -- that is the whole point.
 *
 * Returns false if `green_id` names no currently-live green thread (already
 * finished and freed, or never spawned by this scheduler) -- NOT a trap:
 * an external event racing a green thread's own ordinary completion is
 * expected, not a programming error, and the caller (e.g. a reactor whose
 * fd happened to become ready just as the green thread that owned it
 * finished for an unrelated reason) needs a way to find that out without
 * the whole process aborting. Returns true otherwise, whether or not this
 * particular call was the one that actually woke anything (see "prev ==
 * EMPTY" above: recording a notification for a thread that has not parked
 * YET is also success). */
bool rt_sched_unpark(rt_scheduler_t *s, uint32_t green_id);

/* ---- introspection, for production diagnostics and for this phase's own
 * tests -- all safe to call from any thread at any time. ---- */

uint32_t rt_sched_ncarriers(rt_scheduler_t *s);
uint32_t rt_sched_fuel_size(rt_scheduler_t *s);
uint32_t rt_sched_queue_cap(rt_scheduler_t *s);

uint64_t rt_sched_spawned(rt_scheduler_t *s);   /* total ever pushed */
uint64_t rt_sched_completed(rt_scheduler_t *s); /* total that finished */

/* The largest single draw any carrier has pulled from the shared queue so
 * far -- must never exceed fuel_size; test-visible proof the cap is real. */
uint32_t rt_sched_max_draw_seen(rt_scheduler_t *s);

bool     rt_sched_carrier_idle(rt_scheduler_t *s, uint32_t carrier);
uint64_t rt_sched_carrier_dispatched(rt_scheduler_t *s, uint32_t carrier);

/* Current length of the shared global queue (a momentary snapshot, taken
 * under its lock) -- diagnostics/tests only. */
uint32_t rt_sched_queue_len(rt_scheduler_t *s);

/* ========================================================================
 * Blocking-FFI handoff -- Phase 3 (docs/concurrency-decision.md, "Blocking
 * FFI"). src/emit_c.rs wraps every genuine foreign (`prim`) call site in
 * rt_enter_blocking()/rt_exit_blocking() (runtime/rt.h) -- unconditionally,
 * in every compiled program, whether or not it ever touches this scheduler.
 * This is the other half, scoped to here: a monitor OS thread that watches
 * every carrier's blocking record and, if one has been stuck past a
 * timeout, reuses Phase 2's own carrier/local-buffer structures to rescue
 * the OTHER green threads still queued on that carrier -- the whole point
 * being that one green thread's blocking syscall must not starve its
 * siblings that happen to share its carrier.
 *
 * Mechanism, precisely: a carrier's local buffer (rt_carrier_t.local) is,
 * by Phase 2's own construction, untouched by that carrier's own OS thread
 * for the ENTIRE duration of one dispatch -- the thread is either running
 * carrier_main's loop (between dispatches, local buffer live) or deep
 * inside rt_fiber_switch running a green thread's code (mid-dispatch,
 * local buffer completely idle until that dispatch returns). A carrier
 * "stuck" in a blocking FFI call is, by definition, mid-dispatch, so its
 * local buffer is safe for another thread to drain PROVIDED that handoff
 * is properly synchronised against the carrier's own thread eventually
 * waking up and resuming its post-dispatch bookkeeping -- which is exactly
 * what the new per-carrier lock in scheduler.c exists for (see that file's
 * "blocking-FFI handoff" section for the exact race this closes and why a
 * single timestamp re-check under the lock is sufficient). Every item
 * drained out is pushed onto the SAME shared global queue an ordinary
 * spawn uses, so any other (non-stuck) carrier picks it up through its
 * completely ordinary self-service draw -- no new "backup carrier" type is
 * needed; an already-running sibling carrier IS the backup.
 *
 * Opt-in, not automatic: rt_sched_create does not start this on its own,
 * so every existing Phase 2 test and use of this scheduler is unaffected
 * unless it explicitly asks for monitoring. */

/* Starts the monitor thread. `timeout_ns`: how long a carrier's blocking
 * record may stay nonzero before its local buffer is drained.
 * `poll_interval_ns`: how often the monitor re-scans every carrier --
 * shutdown latency is bounded by this, same trade-off as the carrier's own
 * 5ms idle-semaphore fallback in scheduler.c. May be called at most once
 * per scheduler (traps otherwise, loudly, rather than leaking a second
 * monitor thread silently). */
void rt_sched_start_blocking_monitor(rt_scheduler_t *s, uint64_t timeout_ns,
                                      uint64_t poll_interval_ns);

/* Stops the monitor thread and waits for it to actually exit. Safe to call
 * even if the monitor was never started (a no-op then). rt_sched_destroy
 * also calls this defensively if the caller forgot, but relying on that is
 * not recommended -- call it explicitly once the monitor is no longer
 * needed, the same discipline rt_sched_shutdown already asks for. */
void rt_sched_stop_blocking_monitor(rt_scheduler_t *s);

/* How many times the monitor has actually drained a stuck carrier's local
 * buffer (only counted when there was at least one other green thread
 * queued there to rescue), and the total number of green threads rescued
 * that way across the scheduler's whole life. Test-visible proof the
 * handoff is real, not merely wired up. */
uint64_t rt_sched_handoff_count(rt_scheduler_t *s);
uint64_t rt_sched_handoff_items(rt_scheduler_t *s);

/* Ask every carrier to stop once it next finds its own local buffer AND the
 * shared queue empty, and block until all of them have actually exited.
 * Test-harness / process-shutdown lifecycle only: Phase 2 has no I/O
 * reactor for a genuinely-idle carrier to integrate with, so "sitting idle
 * forever" is correct end-state behaviour for this phase (see
 * docs/concurrency-decision.md, Phase 3) and this is simply how a test (or
 * an eventual process exit) asks every carrier to stop doing that and
 * return. Caller must have no in-flight rt_sched_spawn calls that could
 * still be blocked on backpressure, or they will never unblock. */
void rt_sched_shutdown(rt_scheduler_t *s);

/* Frees everything. Call only after rt_sched_shutdown has returned -- every
 * carrier thread must have actually exited first, or this destroys
 * synchronisation primitives (mutexes, the per-carrier semaphores) while
 * they are still in use, which is undefined behaviour. */
void rt_sched_destroy(rt_scheduler_t *s);

#endif /* RT_SCHEDULER_H */
