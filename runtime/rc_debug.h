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

static void __rc_report(void) {
    long n = __atomic_load_n(&__rc_live, __ATOMIC_RELAXED);
    printf("__rc_live=%ld\n", n);
}

static void __rc_init(void) __attribute__((constructor));
static void __rc_init(void) {
    atexit(__rc_report);
}

#define RC_TRACK_ALLOC() __atomic_fetch_add(&__rc_live, 1, __ATOMIC_RELAXED)
#define RC_TRACK_FREE()  __atomic_fetch_sub(&__rc_live, 1, __ATOMIC_RELAXED)

#define RC_ASSERT(cond, msg)                                    \
    do {                                                        \
        if (!(cond)) {                                          \
            fflush(stdout);                                     \
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
