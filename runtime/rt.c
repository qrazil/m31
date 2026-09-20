/* Runtime implementation.
 *
 * Compiled separately from emitted code and linked. Do NOT make rc_dec a
 * static inline in rt.h, and do NOT build with -flto: either lets the C
 * compiler see a free() whose argument is a static string literal object, and
 * it warns -- correctly, as far as it can tell -- with -Wfree-nonheap-object.
 * The RC_IMMORTAL guard makes that path unreachable at runtime, but the
 * compiler cannot prove it. Keeping the runtime in its own translation unit
 * is what hides the static objects from that analysis.
 *
 * Measured 2026-09-18: separate TU is clean under gcc and clang at -O0 and
 * -O2 with -Wall -Wextra; -flto reintroduces the warning.
 */
#include "rt.h"
#include "rc_debug.h"

#include <inttypes.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void rc_inc(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    o->rc++;
}

void rc_dec(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    RC_ASSERT(o->rc > 0, "decrement below zero");
    if (--o->rc == 0) {
        /* Release what this object holds before releasing the object. A type
         * with no reference-typed fields has no drop function at all, so the
         * common case is one predictable branch, not a call. */
        if (o->ty != NULL && o->ty->drop != NULL) {
            o->ty->drop(o);
        }
        RC_TRACK_FREE();
        free(o);
    }
}

const TypeInfo rt_str_type = { NULL, NULL };

Obj *rt_alloc_immortal(size_t size, const TypeInfo *ty) {
    Obj *o = malloc(size);
    if (o == NULL) rt_trap("out of memory");
    o->rc = RC_IMMORTAL;
    o->ty = ty;
    return o;
}

Obj *rt_alloc(size_t size, const TypeInfo *ty) {
    Obj *o = malloc(size);
    if (o == NULL) rt_trap("out of memory");
    o->rc = 1;
    o->ty = ty;
    RC_TRACK_ALLOC();
    return o;
}

int64_t rt_len(Obj *o) {
    return ((Str *)o)->len;
}

Obj *rt_concat(Obj *a, Obj *b) {
    const Str *x = (const Str *)a;
    const Str *y = (const Str *)b;

    int64_t n;
    if (__builtin_add_overflow(x->len, y->len, &n)) rt_trap("string too long");

    /* One block: header, then bytes, then a NUL so the data is also a valid
     * C string. Strings hold no references, so no drop function. */
    Str *s = (Str *)rt_alloc(sizeof(Str) + (size_t)n + 1, &rt_str_type);

    char *buf = (char *)(s + 1);
    memcpy(buf, x->data, (size_t)x->len);
    memcpy(buf + x->len, y->data, (size_t)y->len);
    buf[n] = '\0';

    s->len = n;
    s->data = buf;
    return (Obj *)s;
}

bool rt_str_eq(Obj *a, Obj *b) {
    const Str *x = (const Str *)a;
    const Str *y = (const Str *)b;
    if (x == y) return true;
    if (x->len != y->len) return false;
    return memcmp(x->data, y->data, (size_t)x->len) == 0;
}

void rt_print(int64_t v) {
    printf("%" PRId64 "\n", v);
}

void rt_print_bool(bool v) {
    puts(v ? "true" : "false");
}

void rt_print_str(Obj *o) {
    const Str *s = (const Str *)o;
    /* fwrite rather than puts: the string may contain NUL bytes, and len is
     * authoritative. */
    fwrite(s->data, 1, (size_t)s->len, stdout);
    putchar('\n');
}

/* ---- concurrency ------------------------------------------------------ */

struct Chan {
    Obj             hdr;
    pthread_mutex_t lock;
    pthread_cond_t  not_empty;
    pthread_cond_t  not_full;
    int64_t        *buf;
    int64_t         cap;
    int64_t         len;
    int64_t         head;
    bool            closed;
};

static void chan_drop(Obj *o) {
    Chan *c = (Chan *)o;
    pthread_mutex_destroy(&c->lock);
    pthread_cond_destroy(&c->not_empty);
    pthread_cond_destroy(&c->not_full);
    free(c->buf);
}

static const TypeInfo rt_chan_type = { chan_drop, NULL };

Chan *rt_chan_new(int64_t capacity) {
    if (capacity < 1) rt_trap("channel capacity must be at least 1");
    Chan *c = (Chan *)rt_alloc_immortal(sizeof(Chan), &rt_chan_type);
    /* NOTE: channels are IMMORTAL, and that is a deliberate v0 simplification.
     *
     * A channel is the one value that must be reachable from several threads
     * at once -- that is its whole job -- so it is exempt from the move rule
     * that keeps everything else single-threaded. But that exemption is
     * exactly what would make its own refcount race, and refcounts are
     * non-atomic by design.
     *
     * So a channel is never freed. A program creates few of them, and the
     * leak is bounded by that count. The real fix is an atomic refcount for
     * this one type, which is the same machinery a threaded refcount needs
     * in general -- see docs/concurrency-decision.md. */

    c->buf = malloc(sizeof(int64_t) * (size_t)capacity);
    if (c->buf == NULL) rt_trap("out of memory");
    c->cap = capacity;
    c->len = 0;
    c->head = 0;
    c->closed = false;
    pthread_mutex_init(&c->lock, NULL);
    pthread_cond_init(&c->not_empty, NULL);
    pthread_cond_init(&c->not_full, NULL);
    return c;
}

void rt_chan_send(Chan *c, int64_t slot) {
    pthread_mutex_lock(&c->lock);
    while (c->len == c->cap && !c->closed) {
        pthread_cond_wait(&c->not_full, &c->lock);
    }
    if (c->closed) {
        pthread_mutex_unlock(&c->lock);
        rt_trap("send on a closed channel");
    }
    c->buf[(c->head + c->len) % c->cap] = slot;
    c->len++;
    pthread_cond_signal(&c->not_empty);
    pthread_mutex_unlock(&c->lock);
}

int64_t rt_chan_recv(Chan *c) {
    pthread_mutex_lock(&c->lock);
    while (c->len == 0 && !c->closed) {
        pthread_cond_wait(&c->not_empty, &c->lock);
    }
    if (c->len == 0) {
        pthread_mutex_unlock(&c->lock);
        rt_trap("receive on a closed and empty channel");
    }
    int64_t v = c->buf[c->head];
    c->head = (c->head + 1) % c->cap;
    c->len--;
    pthread_cond_signal(&c->not_full);
    pthread_mutex_unlock(&c->lock);
    return v;
}

void rt_chan_close(Chan *c) {
    pthread_mutex_lock(&c->lock);
    c->closed = true;
    pthread_cond_broadcast(&c->not_empty);
    pthread_cond_broadcast(&c->not_full);
    pthread_mutex_unlock(&c->lock);
}

void rt_chan_drop(Chan *c) {
    rc_dec((Obj *)c);
}

/* Spawned threads are tracked so the program can wait for them. A fixed
 * table keeps this dependency-free; exceeding it is a trap rather than a
 * silent drop, because a lost thread is a lost result. */
#define RT_MAX_THREADS 1024
static pthread_t   rt_threads[RT_MAX_THREADS];
static int         rt_nthreads = 0;
static pthread_mutex_t rt_threads_lock = PTHREAD_MUTEX_INITIALIZER;

void rt_spawn(void *(*entry)(void *), void *arg) {
    pthread_mutex_lock(&rt_threads_lock);
    if (rt_nthreads >= RT_MAX_THREADS) {
        pthread_mutex_unlock(&rt_threads_lock);
        rt_trap("too many spawned threads");
    }
    int slot = rt_nthreads++;
    pthread_mutex_unlock(&rt_threads_lock);

    if (pthread_create(&rt_threads[slot], NULL, entry, arg) != 0) {
        rt_trap("could not spawn a thread");
    }
}

void rt_wait_all(void) {
    for (int i = 0; i < rt_nthreads; i++) {
        pthread_join(rt_threads[i], NULL);
    }
}

/* Flush stdout first so anything already printed is not lost behind the trap
 * message -- the corpus compares both streams. abort() gives a deterministic
 * exit status of 134 under gcc and clang.
 *
 * abort() skips atexit handlers, so a trapping program never prints
 * __rc_live. run.sh therefore does not require the refcount invariant on
 * corpus/traps/ programs. */
_Noreturn void rt_trap(const char *msg) {
    fflush(stdout);
    fprintf(stderr, "trap: %s\n", msg);
    abort();
}
