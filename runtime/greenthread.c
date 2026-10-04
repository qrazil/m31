/* Phase 1 green-thread primitives: the slab stack allocator (Part 2) and the
 * byte-per-thread state table (Part 4). See runtime/greenthread.h for the
 * declarations and the full design rationale, and
 * docs/concurrency-decision.md for the decision these implement.
 *
 * #included by rt.c, never compiled on its own -- the contract is in
 * greenthread.h, the same convention runtime/sys_libc.c and
 * runtime/sys_linux.c already use for sys.h.
 *
 * rt_ctx_entry_returned also lives here rather than in the .s file: it is
 * ordinary C (just a trap), and keeping it here means ctx_switch_x86_64.S
 * stays nothing but the two things that genuinely have to be assembly.
 */

#include <sys/mman.h>

/* ========================================================================
 * Part 1 (the one piece of it that is plain C) -- what happens if a green
 * thread's entry function returns.
 * ====================================================================== */

_Noreturn void rt_ctx_entry_returned(void) {
    rt_trap("a green thread's entry function returned, which Phase 1 has no "
            "scheduler to resume into (docs/concurrency-decision.md, "
            "\"Phases\")");
}

/* ========================================================================
 * Part 2 -- slab stack allocator
 * ====================================================================== */

/* One slab: a single 64 MiB mmap holding RT_SLAB_STACKS fixed-size stacks,
 * plus a free list of which indices within it are not currently handed out.
 *
 * The free list is a plain array used as a LIFO stack of free indices
 * (free_idx[0 .. free_top) are the free ones) rather than anything
 * intrusive written into the stack memory itself -- this is not a hot path
 * yet, so the simplest correct structure wins over anything cleverer. */
struct rt_slab {
    void *mem; /* mmap base, RT_SLAB_BYTES long */
    struct rt_slab *next;
    uint32_t free_idx[RT_SLAB_STACKS];
    uint32_t free_top; /* free_idx[0 .. free_top) are free */

    /* Diagnostic guard for the unresolved rt_stack_limit race
     * (docs/concurrency-decision.md, "Phase 3.5"): an independent invariant
     * from free_idx/free_top above, checked on every alloc/free under the
     * same g_slab_lock, so a bug in the free-list bookkeeping itself (an
     * index pushed twice, or handed out while something still holds it)
     * traps loudly here instead of silently handing the same physical stack
     * memory to two live green threads at once. Not a fix; a smoke
     * detector, same spirit as struct rt_green's claimed_by_carrier
     * (runtime/scheduler.c). */
    bool in_use[RT_SLAB_STACKS];
};

/* DIAGNOSTIC ONLY, not for master -- see
 * .claude/worktrees/diagnostics-fuel-dip-investigation's own commits.
 * Investigating c=10000's specifically wide run-to-run variance (3x+
 * between trials under otherwise-identical, calm conditions): rt_stack_
 * alloc holds g_slab_lock -- one GLOBAL mutex -- for its entire duration,
 * including rt_slab_new's mmap(1 GiB) call on the ~1-in-1024 new-thread
 * event that needs a fresh slab. Every concurrent green-thread spawn,
 * across every carrier, contends for this same lock meanwhile. c=10000's
 * ramp-up needs roughly 10 such slab crossings; fewer levels need fewer,
 * more need more -- these counters measure how many happened and how long
 * each one actually held the lock, to find out whether THIS is where
 * c=10000's variance comes from. */
static _Atomic uint64_t diag_slab_allocs = 0;
static _Atomic uint64_t diag_slab_alloc_ns = 0;

static pthread_mutex_t g_slab_lock = PTHREAD_MUTEX_INITIALIZER;
/* Newest-first. A brand new slab is entirely free, so prepending it and
 * starting the next search there is what keeps allocation fast in the
 * common case; a slab that has filled up simply gets walked past. Freed
 * stacks go back to whichever slab they came from (rt_stack_t remembers),
 * never anywhere else, so an old, partly-freed slab further down the list
 * still gets reused -- just found by a slightly longer walk. Phase 1 is
 * explicit about not optimising this walk; see greenthread.h. */
static rt_slab_t *g_slabs = NULL;

static rt_slab_t *rt_slab_new(void) {
    void *mem = mmap(NULL, RT_SLAB_BYTES, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mem == MAP_FAILED) {
        rt_trap("out of memory: mmap failed allocating a 64 MiB stack slab");
    }
    rt_slab_t *slab = malloc(sizeof *slab);
    if (slab == NULL) {
        munmap(mem, RT_SLAB_BYTES);
        rt_trap("out of memory: could not allocate slab bookkeeping");
    }
    slab->mem = mem;
    slab->next = NULL;
    for (uint32_t i = 0; i < RT_SLAB_STACKS; i++) {
        slab->free_idx[i] = i;
        slab->in_use[i] = false;
    }
    slab->free_top = RT_SLAB_STACKS;
    return slab;
}

rt_stack_t rt_stack_alloc(void) {
    pthread_mutex_lock(&g_slab_lock);

    rt_slab_t *s = g_slabs;
    while (s != NULL && s->free_top == 0) {
        s = s->next;
    }
    if (s == NULL) {
        /* Every existing slab (if any) is full -- the "exhausted slab"
         * case. Allocate another; this is what lets the allocator scale to
         * the millions-of-green-threads target instead of hitting a ceiling
         * at 1024. */
        /* DIAGNOSTIC ONLY, not for master -- see diag_slab_allocs' own
         * comment. Timed and counted here, INSIDE g_slab_lock, on purpose:
         * this is exactly the window every other concurrent rt_stack_alloc
         * call (any carrier, any green thread) is blocked waiting through. */
        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        s = rt_slab_new();
        clock_gettime(CLOCK_MONOTONIC, &t1);
        uint64_t ns = (uint64_t)(t1.tv_sec - t0.tv_sec) * 1000000000ull +
                      (uint64_t)(t1.tv_nsec - t0.tv_nsec);
        atomic_fetch_add_explicit(&diag_slab_allocs, 1, memory_order_relaxed);
        atomic_fetch_add_explicit(&diag_slab_alloc_ns, ns, memory_order_relaxed);
        s->next = g_slabs;
        g_slabs = s;
    }

    uint32_t idx = s->free_idx[--s->free_top];

    if (s->in_use[idx]) {
        rt_trap("rt_stack_alloc: slab slot already in use -- handed out "
                "while something else still holds it (see rt_slab's "
                "in_use comment and docs/concurrency-decision.md's "
                "Phase 3.5)");
    }
    s->in_use[idx] = true;

    pthread_mutex_unlock(&g_slab_lock);

    rt_stack_t out;
    out.base = (char *)s->mem + (size_t)idx * RT_STACK_SIZE;
    out.top = (char *)out.base + RT_STACK_SIZE;
    out.slab = s;
    out.index = idx;
#if RT_TSAN_BUILD
    /* This lease's own TSan fiber identity -- see greenthread.h's "A note
     * on ThreadSanitizer" comment for why this is created per-lease (here)
     * rather than once per physical slot, and runtime/scheduler.c's
     * carrier_dispatch/rt_sched_spawn for where it is actually switched to
     * and (eventually) torn down. Created outside the slab lock on
     * purpose -- it has nothing to do with free-list bookkeeping, and
     * __tsan_create_fiber is not something that needs that lock's
     * protection. */
    out.tsan_fiber = __tsan_create_fiber(0);
#endif
    return out;
}

void rt_stack_free(rt_stack_t *s) {
#if RT_TSAN_BUILD
    /* Destroyed before the slot is returned to the free list, same
     * reasoning as above: this is about the lease's logical identity, not
     * the slot's physical reuse, and by the time a stack is freed
     * (runtime/scheduler.c's carrier_dispatch, `finished` branch) this OS
     * thread's current TSan fiber has already been switched back to the
     * carrier's own native fiber (green_trampoline's own switch-away),
     * so this is never the current fiber of any thread here. */
    __tsan_destroy_fiber(s->tsan_fiber);
#endif
    pthread_mutex_lock(&g_slab_lock);
    rt_slab_t *slab = s->slab;
    if (!slab->in_use[s->index]) {
        rt_trap("rt_stack_free: double free of a slab slot (see "
                "rt_slab's in_use comment)");
    }
    slab->in_use[s->index] = false;
    slab->free_idx[slab->free_top++] = s->index;
    pthread_mutex_unlock(&g_slab_lock);
}

/* DIAGNOSTIC ONLY, not for master -- see diag_slab_allocs' own comment. */
uint64_t rt_diag_slab_allocs(void) {
    return atomic_load_explicit(&diag_slab_allocs, memory_order_relaxed);
}
uint64_t rt_diag_slab_alloc_ns(void) {
    return atomic_load_explicit(&diag_slab_alloc_ns, memory_order_relaxed);
}

/* ========================================================================
 * Part 4 -- byte-per-thread state table
 * ====================================================================== */

static pthread_mutex_t g_gtstate_lock = PTHREAD_MUTEX_INITIALIZER;
static uint8_t *g_gtstate = NULL;
static size_t g_gtstate_cap = 0;

/* Grow the table so index `id` is valid, if it is not already. Freshly
 * grown bytes start at RT_GT_RUNNABLE (0), matching the enum's own
 * zero-value, so a green thread id that nothing has written yet reads back
 * as runnable rather than an arbitrary byte. */
static void rt_gtstate_ensure(uint32_t id) {
    if ((size_t)id < g_gtstate_cap) {
        return;
    }
    pthread_mutex_lock(&g_gtstate_lock);
    if ((size_t)id >= g_gtstate_cap) {
        size_t new_cap = g_gtstate_cap == 0 ? 1024 : g_gtstate_cap * 2;
        while (new_cap <= (size_t)id) {
            new_cap *= 2;
        }
        uint8_t *grown = realloc(g_gtstate, new_cap);
        if (grown == NULL) {
            pthread_mutex_unlock(&g_gtstate_lock);
            rt_trap("out of memory: could not grow the green-thread state table");
        }
        memset(grown + g_gtstate_cap, RT_GT_RUNNABLE, new_cap - g_gtstate_cap);
        g_gtstate = grown;
        g_gtstate_cap = new_cap;
    }
    pthread_mutex_unlock(&g_gtstate_lock);
}

void rt_gtstate_set(uint32_t id, rt_green_state_t s) {
    rt_gtstate_ensure(id);
    g_gtstate[id] = (uint8_t)s;
}

rt_green_state_t rt_gtstate_get(uint32_t id) {
    rt_gtstate_ensure(id);
    return (rt_green_state_t)g_gtstate[id];
}
