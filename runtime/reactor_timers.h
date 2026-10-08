/* The deadline half of a bounded wait, shared by both reactor backends
 * (runtime/reactor_epoll.c, runtime/reactor_kqueue.c). Header-only and
 * `static inline` on purpose: each backend is one translation unit that
 * includes this, so no build script learns about a new source file, and the
 * two backends cannot drift apart on the one piece that has nothing to do
 * with epoll or kqueue.
 *
 * docs/net-timeouts-decision.md has the reasoning; the shape, briefly:
 *
 *   - A bounded wait owns one `rt_timer_node_t`, and it lives on THE WAITING
 *     GREEN THREAD'S OWN STACK. That stack is exactly as alive as the wait
 *     (a parked green thread's stack does not move or go away), so a timer
 *     costs no allocation, and there is nothing to free and so nothing to
 *     leak: the heap holds pointers to nodes, and the invariant is that a
 *     node is in the heap from the moment it is registered until whoever
 *     CLAIMS it removes it, and the waiter does not return before a claim.
 *
 *   - There are exactly two claimers of one wait -- readiness (the reactor
 *     thread saw the fd ready) and the deadline (the reactor thread found it
 *     expired) -- and both are the reactor's own thread, both claim under the
 *     backend's one waiter-map lock, and a claim writes `outcome` once. The
 *     second claimer finds the node already gone from the heap and the
 *     waiter's slot already released, so it does nothing. That is the whole
 *     readiness-versus-timeout race, closed by a lock rather than by a CAS.
 *
 *   - The heap is intrusive-indexed (each node knows its slot), so a wait
 *     that ends early by readiness is removed in O(log n) at once instead
 *     of lingering until its deadline. A server with a 30 s timeout and
 *     thousands of short requests a second would otherwise hold tens of
 *     thousands of dead timers.
 *
 * Nothing here locks: every function is called with the backend's waiter-map
 * lock held. */
#ifndef RT_REACTOR_TIMERS_H
#define RT_REACTOR_TIMERS_H

#include <stdint.h>
#include <stdlib.h>
#include <time.h>

#include "rt.h"

/* What a bounded wait came to. PENDING means "not claimed yet": a woken
 * thread that finds it still PENDING was woken by a stale notification from
 * some earlier park (rt_sched_park may return early for exactly that), and
 * goes back to sleep. */
#define RT_TIMER_PENDING 0
#define RT_TIMER_READY   1
#define RT_TIMER_EXPIRED 2

typedef struct rt_timer_node {
    uint64_t deadline_ns;  /* CLOCK_MONOTONIC */
    uint32_t green_id;     /* who to unpark */
    int      fd;           /* the registration to release on expiry; -1: none */
    size_t   heap_idx;     /* valid only while in the heap */
    int      outcome;      /* RT_TIMER_*; written under the lock, once */
} rt_timer_node_t;

typedef struct {
    rt_timer_node_t **a;
    size_t            len;
    size_t            cap;
} rt_timer_heap_t;

static inline uint64_t rt_timer_now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static inline void rt_timer_heap_swap(rt_timer_heap_t *h, size_t i, size_t j) {
    rt_timer_node_t *t = h->a[i];
    h->a[i] = h->a[j];
    h->a[j] = t;
    h->a[i]->heap_idx = i;
    h->a[j]->heap_idx = j;
}

static inline void rt_timer_heap_up(rt_timer_heap_t *h, size_t i) {
    while (i > 0) {
        size_t p = (i - 1) / 2;
        if (h->a[p]->deadline_ns <= h->a[i]->deadline_ns) break;
        rt_timer_heap_swap(h, i, p);
        i = p;
    }
}

static inline void rt_timer_heap_down(rt_timer_heap_t *h, size_t i) {
    for (;;) {
        size_t l = 2 * i + 1, r = l + 1, m = i;
        if (l < h->len && h->a[l]->deadline_ns < h->a[m]->deadline_ns) m = l;
        if (r < h->len && h->a[r]->deadline_ns < h->a[m]->deadline_ns) m = r;
        if (m == i) break;
        rt_timer_heap_swap(h, i, m);
        i = m;
    }
}

/* Inserts `n`. Returns true if it became the earliest deadline (so a reactor
 * thread sleeping until some LATER deadline has to be woken to re-aim). */
static inline bool rt_timer_heap_push(rt_timer_heap_t *h, rt_timer_node_t *n) {
    if (h->len == h->cap) {
        size_t ncap = h->cap == 0 ? 64 : h->cap * 2;
        rt_timer_node_t **na = realloc(h->a, ncap * sizeof *na);
        if (na == NULL) rt_trap("out of memory: reactor timer heap");
        h->a = na;
        h->cap = ncap;
    }
    n->heap_idx = h->len;
    h->a[h->len++] = n;
    rt_timer_heap_up(h, n->heap_idx);
    return n->heap_idx == 0;
}

/* Removes `n`, which must be in the heap. */
static inline void rt_timer_heap_remove(rt_timer_heap_t *h, rt_timer_node_t *n) {
    size_t i = n->heap_idx;
    size_t last = h->len - 1;
    if (i != last) {
        rt_timer_heap_swap(h, i, last);
        h->len--;
        rt_timer_heap_down(h, i);
        rt_timer_heap_up(h, i);
    } else {
        h->len--;
    }
    /* Give the memory back once a burst has passed: the heap's footprint
     * follows the number of live waits, not the high-water mark. */
    if (h->cap > 256 && h->len < h->cap / 4) {
        size_t ncap = h->cap / 2;
        rt_timer_node_t **na = realloc(h->a, ncap * sizeof *na);
        if (na != NULL) {
            h->a = na;
            h->cap = ncap;
        }
    }
}

/* The earliest node whose deadline has passed at `now`, removed, or NULL. */
static inline rt_timer_node_t *rt_timer_heap_pop_expired(rt_timer_heap_t *h,
                                                         uint64_t now) {
    if (h->len == 0 || h->a[0]->deadline_ns > now) return NULL;
    rt_timer_node_t *n = h->a[0];
    rt_timer_heap_remove(h, n);
    return n;
}

/* Milliseconds a blocking readiness call may sleep before the earliest
 * deadline, rounded UP (rounding down would wake just short of the deadline
 * and spin on 0 until it passes). -1: no timers, sleep until an event. */
static inline int rt_timer_heap_wait_ms(const rt_timer_heap_t *h, uint64_t now) {
    if (h->len == 0) return -1;
    uint64_t d = h->a[0]->deadline_ns;
    if (d <= now) return 0;
    uint64_t ms = (d - now + 999999ull) / 1000000ull;
    return ms > 1000000000ull ? 1000000000 : (int)ms;
}

/* A timeout in milliseconds becomes an absolute deadline. Clamped so that
 * the multiplication cannot overflow: about 31 years is "forever" here. */
static inline uint64_t rt_timer_deadline_ns(int64_t timeout_ms) {
    if (timeout_ms > 1000000000000ll) timeout_ms = 1000000000000ll;
    return rt_timer_now_ns() + (uint64_t)timeout_ms * 1000000ull;
}

#endif /* RT_REACTOR_TIMERS_H */
