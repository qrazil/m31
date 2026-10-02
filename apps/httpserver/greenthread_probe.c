/* greenthread_probe.c -- a purely additive, read-only instrumentation file
 * for the green-thread scaling test (apps/httpserver/SCALING.md). It does
 * NOT modify runtime/scheduler.c or runtime/rt.c in any way; it only links
 * against three already-public, already-exported functions that those
 * files expose for exactly this kind of external introspection:
 *
 *   rt_scheduler_t *rt_global_scheduler(void);          (runtime/rt.c)
 *   uint64_t        rt_sched_spawned(rt_scheduler_t *);  (runtime/scheduler.c)
 *   uint64_t        rt_sched_completed(rt_scheduler_t *);(runtime/scheduler.c)
 *   uint32_t        rt_sched_ncarriers(rt_scheduler_t *);(runtime/scheduler.c)
 *
 * rt_sched_spawned()/rt_sched_completed() are literally the two counters
 * rt_run_program's own quiescence loop in runtime/rt.c already uses
 * ("spawned count caught up with completed count" -- see that file around
 * rt_run_program). "currently live green threads" is simply
 * spawned - completed at an instant; this file does not invent a new
 * counter or touch any scheduler-internal state, it just reads two atomics
 * that already existed and were already acquire-loaded elsewhere in this
 * runtime, from a SIGUSR1 handler, so a benchmark driver can sample the
 * concurrently-live green-thread count of a running `httpserver` process
 * without stopping it.
 *
 * rt_global_scheduler is not declared `static` in runtime/rt.c and is not
 * declared in runtime/rt.h either (only mentioned in a comment there), so
 * this file re-declares its prototype itself (the normal C way to use an
 * externally-linkable symbol whose header omits it) rather than editing
 * runtime/rt.h to add one.
 *
 * Usage: `kill -USR1 <pid>` against a build that links this file. Output
 * goes to stderr, one line per signal:
 *
 *   [greenthread-probe] carriers=12 spawned=183042 completed=182998 live=44
 *
 * `write(2, ...)` is used for the actual output (not fprintf/snprintf's
 * formatting, which is not guaranteed async-signal-safe) -- the integers
 * are formatted into a stack buffer by hand first, which only uses
 * division/modulo and byte stores, both signal-safe.
 */
#include <signal.h>
#include <stdint.h>
#include <unistd.h>

typedef struct rt_scheduler rt_scheduler_t; /* opaque here; defined in scheduler.c */

extern rt_scheduler_t *rt_global_scheduler(void);
extern uint64_t rt_sched_spawned(rt_scheduler_t *s);
extern uint64_t rt_sched_completed(rt_scheduler_t *s);
extern uint32_t rt_sched_ncarriers(rt_scheduler_t *s);

static char *fmt_u64(char *out, uint64_t v) {
    char tmp[20];
    int n = 0;
    if (v == 0) {
        tmp[n++] = '0';
    } else {
        while (v > 0) {
            tmp[n++] = (char)('0' + (v % 10));
            v /= 10;
        }
    }
    while (n > 0) {
        *out++ = tmp[--n];
    }
    return out;
}

static void on_sigusr1(int sig) {
    (void)sig;
    rt_scheduler_t *s = rt_global_scheduler();
    uint64_t spawned = rt_sched_spawned(s);
    uint64_t completed = rt_sched_completed(s);
    uint64_t live = spawned - completed;
    uint32_t carriers = rt_sched_ncarriers(s);

    char buf[160];
    char *p = buf;
    const char *prefix = "[greenthread-probe] carriers=";
    while (*prefix) *p++ = *prefix++;
    p = fmt_u64(p, (uint64_t)carriers);
    const char *s1 = " spawned=";
    while (*s1) *p++ = *s1++;
    p = fmt_u64(p, spawned);
    const char *s2 = " completed=";
    while (*s2) *p++ = *s2++;
    p = fmt_u64(p, completed);
    const char *s3 = " live=";
    while (*s3) *p++ = *s3++;
    p = fmt_u64(p, live);
    *p++ = '\n';

    write(2, buf, (size_t)(p - buf));
}

__attribute__((constructor)) static void install_greenthread_probe(void) {
    struct sigaction sa;
    sa.sa_handler = on_sigusr1;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR1, &sa, NULL);
}
