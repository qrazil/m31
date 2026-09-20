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
#include <stdint.h>
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

/* A str is immutable, so a copy is indistinguishable by value -- but not by
 * IDENTITY, and identity is what a thread boundary cares about. Sending a
 * string you also keep needs a second object, not a second reference. */
Obj *rt_str_clone(Obj *o) {
    const Str *x = (const Str *)o;
    Str *s = (Str *)rt_alloc(sizeof(Str) + (size_t)x->len + 1, &rt_str_type);
    char *buf = (char *)(s + 1);
    memcpy(buf, x->data, (size_t)x->len);
    buf[x->len] = '\0';
    s->len = x->len;
    s->data = buf;
    return (Obj *)s;
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

/* ---- collections ------------------------------------------------------ */

static void arr_drop_refs(Obj *o) {
    Arr *a = (Arr *)o;
    for (int64_t i = 0; i < a->len; i++) {
        rc_dec((Obj *)(intptr_t)a->data[i]);
    }
}

static void lst_drop_refs(Obj *o) {
    Lst *l = (Lst *)o;
    for (int64_t i = 0; i < l->len; i++) {
        rc_dec((Obj *)(intptr_t)l->data[i]);
    }
    free(l->data);
}

static void lst_drop_vals(Obj *o) {
    free(((Lst *)o)->data);
}

static const TypeInfo rt_arr_val_type = { NULL, NULL };
static const TypeInfo rt_arr_ref_type = { arr_drop_refs, NULL };
static const TypeInfo rt_lst_val_type = { lst_drop_vals, NULL };
static const TypeInfo rt_lst_ref_type = { lst_drop_refs, NULL };

Obj *rt_array_new(int64_t len, int64_t fill, bool elems_are_refs) {
    if (len < 0) rt_trap("array length cannot be negative");
    /* One allocation: header plus the elements. Every slot starts at `fill`
     * -- there is no null, so there is no such thing as an unset element. */
    Arr *a = (Arr *)rt_alloc(sizeof(Arr) + sizeof(int64_t) * (size_t)len,
                             elems_are_refs ? &rt_arr_ref_type : &rt_arr_val_type);
    a->len = len;
    for (int64_t i = 0; i < len; i++) {
        a->data[i] = fill;
        if (elems_are_refs) rc_inc((Obj *)(intptr_t)fill);
    }
    return (Obj *)a;
}

Obj *rt_list_new(bool elems_are_refs) {
    Lst *l = (Lst *)rt_alloc(sizeof(Lst),
                             elems_are_refs ? &rt_lst_ref_type : &rt_lst_val_type);
    l->len = 0;
    l->cap = 0;
    l->data = NULL;
    return (Obj *)l;
}

/* A list and an array share a header shape up to `len`, so one accessor
 * serves both. The compiler knows which it has; the runtime does not need
 * to, except to find the elements. */
static bool is_list(const Obj *o) {
    return o->ty == &rt_lst_val_type || o->ty == &rt_lst_ref_type;
}

static int64_t *slots(Obj *o) {
    return is_list(o) ? ((Lst *)o)->data : ((Arr *)o)->data;
}

int64_t rt_len_of(Obj *o) {
    return is_list(o) ? ((Lst *)o)->len : ((Arr *)o)->len;
}

int64_t rt_index_get(Obj *o, int64_t i) {
    int64_t n = rt_len_of(o);
    if (i < 0 || i >= n) rt_trap("index out of range");
    return slots(o)[i];
}

void rt_index_set(Obj *o, int64_t i, int64_t v) {
    int64_t n = rt_len_of(o);
    if (i < 0 || i >= n) rt_trap("index out of range");
    slots(o)[i] = v;
}

void rt_list_push(Obj *o, int64_t v) {
    Lst *l = (Lst *)o;
    if (l->len == l->cap) {
        int64_t cap = l->cap == 0 ? 4 : l->cap * 2;
        int64_t *buf = realloc(l->data, sizeof(int64_t) * (size_t)cap);
        if (buf == NULL) rt_trap("out of memory");
        l->data = buf;
        l->cap = cap;
    }
    l->data[l->len++] = v;
}

/* Shallow: the new collection holds the same elements, each retained once
 * more. Deep copying would have to know what an element's own copy means,
 * which is a question only the program can answer. */
Obj *rt_seq_clone(Obj *o) {
    int64_t n = rt_len_of(o);
    bool refs = o->ty == &rt_arr_ref_type || o->ty == &rt_lst_ref_type;
    int64_t *src = slots(o);

    if (is_list(o)) {
        Lst *l = (Lst *)rt_list_new(refs);
        for (int64_t i = 0; i < n; i++) {
            rt_list_push((Obj *)l, src[i]);
            if (refs) rc_inc((Obj *)(intptr_t)src[i]);
        }
        return (Obj *)l;
    }

    Arr *a = (Arr *)rt_alloc(sizeof(Arr) + sizeof(int64_t) * (size_t)n,
                             refs ? &rt_arr_ref_type : &rt_arr_val_type);
    a->len = n;
    for (int64_t i = 0; i < n; i++) {
        a->data[i] = src[i];
        if (refs) rc_inc((Obj *)(intptr_t)src[i]);
    }
    return (Obj *)a;
}

int64_t rt_list_pop(Obj *o) {
    Lst *l = (Lst *)o;
    if (l->len == 0) rt_trap("pop from an empty list");
    return l->data[--l->len];
}

/* ---- map --------------------------------------------------------------
 *
 * Open addressing with linear probing and a 70% load factor. One allocation
 * for the whole table, no per-entry node, and deletion leaves a tombstone so
 * a probe sequence is never broken.
 */
enum { SLOT_EMPTY = 0, SLOT_FULL = 1, SLOT_DEAD = 2 };

typedef struct {
    int64_t k;
    int64_t v;
    uint8_t state;
} Slot;

typedef struct {
    Obj     hdr;
    Slot   *slots;
    int64_t cap;
    int64_t len;      /* live entries */
    int64_t used;     /* live + tombstones, for the load factor */
    bool    key_is_str;
    bool    key_is_ref;
    bool    val_is_ref;
} Map;

static void map_drop(Obj *o) {
    Map *m = (Map *)o;
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->slots[i].state != SLOT_FULL) continue;
        if (m->key_is_ref) rc_dec((Obj *)(intptr_t)m->slots[i].k);
        if (m->val_is_ref) rc_dec((Obj *)(intptr_t)m->slots[i].v);
    }
    free(m->slots);
}

static const TypeInfo rt_map_type = { map_drop, NULL };

static uint64_t hash_int(int64_t x) {
    /* splitmix64's finaliser: cheap and mixes the low bits, which matters
     * because linear probing is sensitive to clustering. */
    uint64_t z = (uint64_t)x + 0x9e3779b97f4a7c15ULL;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static uint64_t hash_key(const Map *m, int64_t k) {
    if (!m->key_is_str) return hash_int(k);
    const Str *s = (const Str *)(intptr_t)k;
    uint64_t h = 1469598103934665603ULL;      /* FNV-1a */
    for (int64_t i = 0; i < s->len; i++) {
        h ^= (unsigned char)s->data[i];
        h *= 1099511628211ULL;
    }
    return h;
}

static bool key_eq(const Map *m, int64_t a, int64_t b) {
    if (!m->key_is_str) return a == b;
    return rt_str_eq((Obj *)(intptr_t)a, (Obj *)(intptr_t)b);
}

Obj *rt_map_new(bool key_is_str, bool key_is_ref, bool val_is_ref) {
    Map *m = (Map *)rt_alloc(sizeof(Map), &rt_map_type);
    m->slots = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
    m->key_is_str = key_is_str;
    m->key_is_ref = key_is_ref;
    m->val_is_ref = val_is_ref;
    return (Obj *)m;
}

/* Index of the slot holding `k`, or of the first free slot for it. */
static int64_t map_probe(const Map *m, int64_t k, bool *found) {
    uint64_t h = hash_key(m, k);
    int64_t mask = m->cap - 1;
    int64_t i = (int64_t)(h & (uint64_t)mask);
    int64_t first_dead = -1;
    for (;;) {
        if (m->slots[i].state == SLOT_EMPTY) {
            *found = false;
            return first_dead >= 0 ? first_dead : i;
        }
        if (m->slots[i].state == SLOT_DEAD) {
            if (first_dead < 0) first_dead = i;
        } else if (key_eq(m, m->slots[i].k, k)) {
            *found = true;
            return i;
        }
        i = (i + 1) & mask;
    }
}

static void map_grow(Map *m) {
    int64_t cap = m->cap == 0 ? 8 : m->cap * 2;
    Slot *old = m->slots;
    int64_t oldcap = m->cap;

    Slot *fresh = calloc((size_t)cap, sizeof(Slot));
    if (fresh == NULL) rt_trap("out of memory");
    m->slots = fresh;
    m->cap = cap;
    m->used = m->len;

    for (int64_t i = 0; i < oldcap; i++) {
        if (old[i].state != SLOT_FULL) continue;
        bool found;
        int64_t j = map_probe(m, old[i].k, &found);
        m->slots[j] = old[i];
    }
    free(old);
}

void rt_map_set(Obj *o, int64_t k, int64_t v) {
    Map *m = (Map *)o;
    /* Grow before probing, so a full table can never spin forever. */
    if (m->cap == 0 || (m->used + 1) * 10 >= m->cap * 7) map_grow(m);

    bool found;
    int64_t i = map_probe(m, k, &found);
    if (found) {
        /* Replacing a value releases the old one; the key stays as it was. */
        if (m->val_is_ref) rc_dec((Obj *)(intptr_t)m->slots[i].v);
        m->slots[i].v = v;
        if (m->val_is_ref) rc_inc((Obj *)(intptr_t)v);
        return;
    }
    if (m->slots[i].state == SLOT_EMPTY) m->used++;
    m->slots[i].state = SLOT_FULL;
    m->slots[i].k = k;
    m->slots[i].v = v;
    m->len++;
    if (m->key_is_ref) rc_inc((Obj *)(intptr_t)k);
    if (m->val_is_ref) rc_inc((Obj *)(intptr_t)v);
}

int64_t rt_map_get(Obj *o, int64_t k) {
    Map *m = (Map *)o;
    if (m->cap == 0) rt_trap("key not in map");
    bool found;
    int64_t i = map_probe(m, k, &found);
    if (!found) rt_trap("key not in map");
    return m->slots[i].v;
}

bool rt_map_has(Obj *o, int64_t k) {
    Map *m = (Map *)o;
    if (m->cap == 0) return false;
    bool found;
    map_probe(m, k, &found);
    return found;
}

void rt_map_remove(Obj *o, int64_t k) {
    Map *m = (Map *)o;
    if (m->cap == 0) return;
    bool found;
    int64_t i = map_probe(m, k, &found);
    if (!found) return;
    if (m->key_is_ref) rc_dec((Obj *)(intptr_t)m->slots[i].k);
    if (m->val_is_ref) rc_dec((Obj *)(intptr_t)m->slots[i].v);
    m->slots[i].state = SLOT_DEAD;   /* not EMPTY: a probe must not stop here */
    m->len--;
}

int64_t rt_map_len(Obj *o) {
    return ((Map *)o)->len;
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
void rt_check_unique(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    if (o->rc != 1) {
        rt_trap("value crossing a thread boundary is still referenced elsewhere; "
                "clone() it, or drop the other reference first");
    }
}

_Noreturn void rt_trap(const char *msg) {
    fflush(stdout);
    fprintf(stderr, "trap: %s\n", msg);
    abort();
}
