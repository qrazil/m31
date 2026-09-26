/* Refcount invariant checking for debug builds.
 *
 * Compiled in with -DRC_DEBUG; compiles to nothing otherwise, so release
 * builds pay zero. run.sh requires the __rc_live=0 line on every program.
 *
 * This is the one oracle layer that needs no second implementation: it
 * catches leaks, double-frees and use-after-free from the memory model
 * itself rather than by comparing against anything.
 *
 * The counter is ATOMIC even though the language's own refcounts are not.
 * Object refcounts are non-atomic because the move rule guarantees one
 * thread can reach a value at a time; this counter has no such guarantee --
 * every thread allocates and frees, and they all touch this one word. It was
 * a plain `long`, and it lost updates: corpus/core/035 reported a spurious
 * `__rc_live=1` in roughly one run in eighty. A flaky failure is the
 * visible half of that. The invisible half is worse -- a lost increment
 * cancels a real leak and the gate passes when it should not.
 */
#ifndef RC_DEBUG_H
#define RC_DEBUG_H

#include <stdio.h>
#include <stdlib.h>

#ifdef RC_DEBUG

/* Relaxed ordering is enough: nothing is published through this counter,
 * and it is only read after every thread has been joined. */
static long __rc_live = 0;

/* Standard output belongs to the runtime's own buffer (rt.c), not to C
 * stdio, so the report goes through it. It flushes for itself: it runs from
 * atexit, possibly after the runtime's own exit flush has already run. */
void rt_out_line(const char *p, size_t n);
void rt_out_flush(void);

/* Every allocation ever made, not just the ones still live -- the number a
 * measurement wants when the question is "how much did this loop allocate".
 * Reported only under -DRC_COUNT_ALLOCS, on a line of its own, so that the
 * corpus's `__rc_live=0` output is byte-for-byte what it always was. Counted
 * unconditionally under RC_DEBUG because the increment is already there. */
static long __rc_allocs = 0;

static void __rc_report(void) {
    long n = __atomic_load_n(&__rc_live, __ATOMIC_RELAXED);
    char buf[48];
    int k;
#ifdef RC_COUNT_ALLOCS
    k = snprintf(buf, sizeof buf, "__rc_allocs=%ld",
                 __atomic_load_n(&__rc_allocs, __ATOMIC_RELAXED));
    rt_out_line(buf, (size_t)k);
#endif
    k = snprintf(buf, sizeof buf, "__rc_live=%ld", n);
    rt_out_line(buf, (size_t)k);
    rt_out_flush();
}

static void __rc_init(void) __attribute__((constructor));
static void __rc_init(void) {
    atexit(__rc_report);
}

#define RC_TRACK_ALLOC()                                            \
    do {                                                            \
        __atomic_fetch_add(&__rc_live, 1, __ATOMIC_RELAXED);        \
        __atomic_fetch_add(&__rc_allocs, 1, __ATOMIC_RELAXED);      \
    } while (0)
#define RC_TRACK_FREE()  __atomic_fetch_sub(&__rc_live, 1, __ATOMIC_RELAXED)

#define RC_ASSERT(cond, msg)                                    \
    do {                                                        \
        if (!(cond)) {                                          \
            rt_out_flush();                                     \
            fprintf(stderr, "rc violation: %s at %s:%d\n",      \
                    (msg), __FILE__, __LINE__);                 \
            abort();                                            \
        }                                                       \
    } while (0)

#else

#define RC_TRACK_ALLOC() ((void)0)
#define RC_TRACK_FREE()  ((void)0)
#define RC_ASSERT(cond, msg) ((void)0)

#endif /* RC_DEBUG */

#endif /* RC_DEBUG_H */
