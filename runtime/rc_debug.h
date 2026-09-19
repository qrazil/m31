/* Refcount invariant checking for debug builds.
 *
 * Compiled in with -DRC_DEBUG; compiles to nothing otherwise, so release
 * builds pay zero. run.sh requires the __rc_live=0 line on every program.
 *
 * This is the one oracle layer that needs no second implementation: it
 * catches leaks, double-frees and use-after-free from the memory model
 * itself rather than by comparing against anything.
 */
#ifndef RC_DEBUG_H
#define RC_DEBUG_H

#include <stdio.h>
#include <stdlib.h>

#ifdef RC_DEBUG

static long __rc_live = 0;

static void __rc_report(void) {
    printf("__rc_live=%ld\n", __rc_live);
}

static void __rc_init(void) __attribute__((constructor));
static void __rc_init(void) {
    atexit(__rc_report);
}

#define RC_TRACK_ALLOC() (__rc_live++)
#define RC_TRACK_FREE()  (__rc_live--)

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
