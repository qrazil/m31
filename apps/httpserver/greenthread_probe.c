/* greenthread_probe.c -- a purely additive, read-only instrumentation file
 * for the green-thread scaling test (apps/httpserver/SCALING.md) and the
 * fuel_size/wake-permutation investigation (the non-monotonic throughput
 * dip found past c=500 or so in that document's "Follow-up" sections). It
 * does NOT modify runtime/scheduler.c or runtime/rt.c in any way; it only
 * links against already-public, already-exported functions those files
 * expose for exactly this kind of external introspection:
 *
 *   rt_scheduler_t *rt_global_scheduler(void);               (runtime/rt.c)
 *   uint64_t   rt_sched_spawned(rt_scheduler_t *);            (scheduler.c)
 *   uint64_t   rt_sched_completed(rt_scheduler_t *);          (scheduler.c)
 *   uint32_t   rt_sched_ncarriers(rt_scheduler_t *);          (scheduler.c)
 *   uint64_t   rt_sched_carrier_dispatched(rt_scheduler_t *, uint32_t);
 *                                                              (scheduler.c)
 *
 * rt_sched_spawned()/rt_sched_completed() are literally the two counters
 * rt_run_program's own quiescence loop in runtime/rt.c already uses
 * ("spawned count caught up with completed count" -- see that file around
 * rt_run_program). "currently live green threads" is simply
 * spawned - completed at an instant; this file does not invent a new
 * counter or touch any scheduler-internal state, it just reads atomics
 * that already existed and were already acquire-loaded elsewhere in this
 * runtime, from a SIGUSR1 handler, so a benchmark driver can sample a
 * running `httpserver` process without stopping it.
 *
 * rt_sched_carrier_dispatched(s, i) is the same idea for one new question:
 * how evenly is work actually landing across the 12 carriers? The
 * mechanism that decides which idle carrier wakes next (notify_new_work,
 * scheduler.c: a shared, mutex-protected, Fisher-Yates-reshuffled
 * permutation walked at most n_carriers steps per push) is the one
 * genuinely randomized part of dispatch, and a per-carrier dispatch count
 * sampled before and after a load is the direct way to see whether it is
 * actually spreading work evenly at scale, or whether a few carriers are
 * doing most of it while others sit comparatively idle -- rather than
 * inferring this indirectly from end-to-end throughput alone.
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
 *   [greenthread-probe] dispatched: c0=15230 c1=14877 c2=9 c3=15102 ...
 *
 * `write(2, ...)` is used for the actual output (not fprintf/snprintf's
 * formatting, which is not guaranteed async-signal-safe) -- the integers
 * are formatted into a stack buffer by hand first, which only uses
 * division/modulo and byte stores, both signal-safe. A carrier count past
 * what this buffer can hold is not expected on any host this project
 * targets (RT_SCHED's own n_carriers is bounded by detected core count),
 * but the loop below bounds its own output defensively regardless.
 */
#include <signal.h>
#include <stdint.h>
#include <unistd.h>

typedef struct rt_scheduler rt_scheduler_t; /* opaque here; defined in scheduler.c */

extern rt_scheduler_t *rt_global_scheduler(void);
extern uint64_t rt_sched_spawned(rt_scheduler_t *s);
extern uint64_t rt_sched_completed(rt_scheduler_t *s);
extern uint32_t rt_sched_ncarriers(rt_scheduler_t *s);
extern uint64_t rt_sched_carrier_dispatched(rt_scheduler_t *s, uint32_t carrier);

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

    /* Second line: how many green threads each carrier has actually
     * dispatched so far, in carrier-index order. A snapshot taken once
     * before a load and once after (two SIGUSR1s) turns into a per-carrier
     * delta the caller can compute itself -- this probe stays a dumb
     * counter dump, not a diffing tool, the same "one job" discipline
     * rt_sched_spawned/rt_sched_completed above already follow. */
    char buf2[4096];
    char *q = buf2;
    char *const buf2_end = buf2 + sizeof buf2 - 32; /* room for one more
                                                      * "cNNN=<20 digits> "
                                                      * entry plus the
                                                      * trailing newline,
                                                      * checked before each
                                                      * one is appended. */
    const char *prefix2 = "[greenthread-probe] dispatched:";
    while (*prefix2) *q++ = *prefix2++;
    for (uint32_t i = 0; i < carriers && q < buf2_end; i++) {
        *q++ = ' ';
        *q++ = 'c';
        q = fmt_u64(q, (uint64_t)i);
        *q++ = '=';
        q = fmt_u64(q, rt_sched_carrier_dispatched(s, i));
    }
    *q++ = '\n';

    write(2, buf2, (size_t)(q - buf2));
}

__attribute__((constructor)) static void install_greenthread_probe(void) {
    struct sigaction sa;
    sa.sa_handler = on_sigusr1;
    sa.sa_flags = SA_RESTART;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR1, &sa, NULL);
}
