/* Phase 2 green-thread scheduler (docs/concurrency-decision.md, "Scheduler
 * queues" and "Phases" -- this is Phase 2, built on Phase 1's primitives:
 * runtime/greenthread.h's context switch, slab stack allocator, and
 * byte-per-thread state table).
 *
 * This is a STANDALONE component. It is not #included by rt.c, it is not
 * reachable from a compiled .src program, and `spawn` still means an OS
 * thread (rt.h's "concurrency" section) exactly as it did after Phase 1.
 * Wiring the language's `spawn` keyword to use this scheduler instead of a
 * raw OS thread is a deliberate, separate, later decision -- see the task
 * that produced this file. Everything here is exercised only by
 * runtime/scheduler_test.c, its own test harness, which links this file,
 * runtime/rt.c and runtime/ctx_switch_x86_64.s directly.
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
