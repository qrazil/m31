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

const TypeInfo rt_str_type = { NULL, NULL, NULL };

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

/* ---- strings ----------------------------------------------------------
 *
 * Everything here works in BYTES, not characters. `size()` is a byte count,
 * `substr` takes byte offsets, and `to_upper` touches only ASCII. That is
 * Go's choice too, and it is the honest one for a type that carries bytes --
 * the alternative is pretending to understand an encoding the language has
 * no other opinion about. It is written down in the reference so nobody has
 * to discover it.
 */

/* Defined with the collections below; a string joins a List of them. */
static int64_t *slots(Obj *o);

/* One allocation, header and bytes together, with a NUL so the data is also
 * a valid C string. */
static Obj *str_new(const char *src, int64_t n) {
    Str *s = (Str *)rt_alloc(sizeof(Str) + (size_t)n + 1, &rt_str_type);
    char *buf = (char *)(s + 1);
    if (n > 0) memcpy(buf, src, (size_t)n);
    buf[n] = '\0';
    s->len = n;
    s->data = buf;
    return (Obj *)s;
}

/* Byte offsets, half-open, and an out-of-range one traps the way an
 * out-of-range index does: it is a bug at the call site, not a condition to
 * handle. */
Obj *rt_str_substr(Obj *o, int64_t from, int64_t to) {
    const Str *s = (const Str *)o;
    if (from < 0 || to < from || to > s->len) rt_trap("substring range out of bounds");
    return str_new(s->data + from, to - from);
}

/* -1 for absent; the lowering turns it into a None. An empty needle is found
 * at 0, which is what every library that answers this question says. */
int64_t rt_str_find(Obj *o, Obj *needle) {
    const Str *h = (const Str *)o;
    const Str *n = (const Str *)needle;
    if (n->len == 0) return 0;
    if (n->len > h->len) return -1;
    for (int64_t i = 0; i + n->len <= h->len; i++) {
        if (memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    }
    return -1;
}

bool rt_str_starts_with(Obj *o, Obj *p) {
    const Str *s = (const Str *)o;
    const Str *q = (const Str *)p;
    if (q->len > s->len) return false;
    return memcmp(s->data, q->data, (size_t)q->len) == 0;
}

bool rt_str_ends_with(Obj *o, Obj *p) {
    const Str *s = (const Str *)o;
    const Str *q = (const Str *)p;
    if (q->len > s->len) return false;
    return memcmp(s->data + (s->len - q->len), q->data, (size_t)q->len) == 0;
}

static bool is_space(char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v';
}

Obj *rt_str_trim(Obj *o) {
    const Str *s = (const Str *)o;
    int64_t a = 0;
    int64_t b = s->len;
    while (a < b && is_space(s->data[a])) a++;
    while (b > a && is_space(s->data[b - 1])) b--;
    return str_new(s->data + a, b - a);
}

/* ASCII only, deliberately: anything else needs a Unicode table this
 * language has no business carrying, and a half-done one would be worse
 * than an honest limit. */
Obj *rt_str_case(Obj *o, bool upper) {
    const Str *s = (const Str *)o;
    Obj *out = str_new(s->data, s->len);
    char *buf = (char *)(((Str *)out) + 1);
    for (int64_t i = 0; i < s->len; i++) {
        char c = buf[i];
        if (upper && c >= 'a' && c <= 'z') buf[i] = (char)(c - 32);
        if (!upper && c >= 'A' && c <= 'Z') buf[i] = (char)(c + 32);
    }
    return out;
}

Obj *rt_str_repeat(Obj *o, int64_t n) {
    const Str *s = (const Str *)o;
    if (n < 0) rt_trap("cannot repeat a string a negative number of times");
    int64_t total;
    if (__builtin_mul_overflow(s->len, n, &total)) rt_trap("string too long");
    Str *out = (Str *)rt_alloc(sizeof(Str) + (size_t)total + 1, &rt_str_type);
    char *buf = (char *)(out + 1);
    /* An empty string repeated any number of times is empty. Without this
     * the overflow guard above passes -- the total really is 0 -- and the
     * loop then runs n no-op iterations, which for a large n never ends. */
    if (s->len > 0) {
        for (int64_t i = 0; i < n; i++) {
            memcpy(buf + i * s->len, s->data, (size_t)s->len);
        }
    }
    buf[total] = '\0';
    out->len = total;
    out->data = buf;
    return (Obj *)out;
}

/* Splitting on an EMPTY separator would have no answer that is not
 * arbitrary, so it traps rather than picking one. */
Obj *rt_str_split(Obj *o, Obj *sep) {
    const Str *s = (const Str *)o;
    const Str *d = (const Str *)sep;
    if (d->len == 0) rt_trap("cannot split on an empty separator");

    Obj *out = rt_list_new(true);
    int64_t start = 0;
    for (int64_t i = 0; i + d->len <= s->len;) {
        if (memcmp(s->data + i, d->data, (size_t)d->len) == 0) {
            rt_list_push(out, (int64_t)(intptr_t)str_new(s->data + start, i - start));
            i += d->len;
            start = i;
        } else {
            i++;
        }
    }
    rt_list_push(out, (int64_t)(intptr_t)str_new(s->data + start, s->len - start));
    return out;
}

Obj *rt_str_join(Obj *parts, Obj *sep) {
    const Str *d = (const Str *)sep;
    int64_t n = rt_len_of(parts);
    int64_t *el = slots(parts);

    int64_t total = 0;
    for (int64_t i = 0; i < n; i++) {
        const Str *p = (const Str *)(intptr_t)el[i];
        if (__builtin_add_overflow(total, p->len, &total)) rt_trap("string too long");
    }
    if (n > 1) {
        int64_t gaps;
        if (__builtin_mul_overflow(d->len, n - 1, &gaps)) rt_trap("string too long");
        if (__builtin_add_overflow(total, gaps, &total)) rt_trap("string too long");
    }

    Str *out = (Str *)rt_alloc(sizeof(Str) + (size_t)total + 1, &rt_str_type);
    char *buf = (char *)(out + 1);
    int64_t at = 0;
    for (int64_t i = 0; i < n; i++) {
        const Str *p = (const Str *)(intptr_t)el[i];
        if (i > 0 && d->len > 0) {
            memcpy(buf + at, d->data, (size_t)d->len);
            at += d->len;
        }
        memcpy(buf + at, p->data, (size_t)p->len);
        at += p->len;
    }
    buf[total] = '\0';
    out->len = total;
    out->data = buf;
    return (Obj *)out;
}

void rt_print(int64_t v) {
    printf("%" PRId64 "\n", v);
}

/* Printed so it reads back as the same double and still looks like what was
 * written: the shortest precision that round-trips, tried in order. `%.17g`
 * always round-trips but renders 0.1 as 0.10000000000000001.
 *
 * All four oracle builds share a libc, so this is deterministic across them.
 * A Go twin is not comparable here -- Go prints shortest-round-trip by a
 * different algorithm and spells infinities `+Inf` -- so float programs stay
 * out of corpus/twin. */
void rt_print_float(double x) {
    if (x != x) {
        puts("nan");
        return;
    }
    if (x > 1.7976931348623157e308) {
        puts("inf");
        return;
    }
    if (x < -1.7976931348623157e308) {
        puts("-inf");
        return;
    }
    char buf[64];
    for (int p = 1; p <= 17; p++) {
        snprintf(buf, sizeof buf, "%.*g", p, x);
        if (strtod(buf, NULL) == x) break;
    }

    /* %g reaches for an exponent as soon as the exponent exceeds the
     * precision, so the shortest round-trip of 2500.0 is "2.5e+03". That is
     * correct and surprising. For magnitudes a reader would write out in
     * full, prefer the plain form -- which is what Go and Rust print too. */
    if (strpbrk(buf, "eE") != NULL) {
        double mag = x < 0 ? -x : x;
        if (mag >= 1e-4 && mag < 1e17) {
            char plain[64];
            for (int p = 0; p <= 17; p++) {
                snprintf(plain, sizeof plain, "%.*f", p, x);
                if (strtod(plain, NULL) == x) {
                    /* Only what was written. `sizeof buf` would copy the
                     * whole scratch array, most of which snprintf never
                     * touched -- harmless for puts, and still a read of
                     * uninitialised memory. */
                    memcpy(buf, plain, strlen(plain) + 1);
                    break;
                }
            }
        }
    }
    puts(buf);
}

double rt_i2f_val(int64_t n) {
    return (double)n;
}

/* Truncates toward zero. Casting a NaN, or a value outside the integer
 * range, is UNDEFINED in C -- so it is checked here rather than left to
 * whatever the target happens to do. The bound is written as a double on
 * purpose: INT64_MAX is not representable, and comparing against it after
 * conversion would be the same undefined cast again. */
int64_t rt_f2i_checked(double x) {
    if (x != x) rt_trap("cannot convert NaN to int");
    if (!(x >= -9223372036854775808.0 && x < 9223372036854775808.0)) {
        rt_trap("float is out of range for int");
    }
    return (int64_t)x;
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

/* Walks report references; they do not release them. A channel has none of
 * these because it is immortal, and the uniqueness check skips immortals. */
static void arr_walk_refs(Obj *o, VisitFn visit, void *ctx) {
    Arr *a = (Arr *)o;
    for (int64_t i = 0; i < a->len; i++) {
        visit(ctx, (Obj *)(intptr_t)a->data[i]);
    }
}

static void lst_walk_refs(Obj *o, VisitFn visit, void *ctx) {
    Lst *l = (Lst *)o;
    for (int64_t i = 0; i < l->len; i++) {
        visit(ctx, (Obj *)(intptr_t)l->data[i]);
    }
}

static const TypeInfo rt_arr_val_type = { NULL, NULL, NULL };
static const TypeInfo rt_arr_ref_type = { arr_drop_refs, NULL, arr_walk_refs };
static const TypeInfo rt_lst_val_type = { lst_drop_vals, NULL, NULL };
static const TypeInfo rt_lst_ref_type = { lst_drop_refs, NULL, lst_walk_refs };


/* Bytes for `n` slots plus a `head` header, trapping rather than wrapping.
 *
 * `sizeof(T) * n` is a silent wrap on a 64-bit size_t: 2^61 elements times 8
 * bytes is 0, which allocates a 16-byte block that the caller then writes as
 * if it held 2^61 elements. rt_concat already guards its length this way;
 * these paths did not. */
static size_t slot_bytes(size_t head, int64_t n) {
    size_t bytes;
    if (n < 0) rt_trap("length cannot be negative");
    if (__builtin_mul_overflow((size_t)n, sizeof(int64_t), &bytes))
        rt_trap("length too large");
    if (__builtin_add_overflow(bytes, head, &bytes))
        rt_trap("length too large");
    return bytes;
}

Obj *rt_array_new(int64_t len, int64_t fill, bool elems_are_refs) {
    if (len < 0) rt_trap("array length cannot be negative");
    /* One allocation: header plus the elements. Every slot starts at `fill`
     * -- there is no null, so there is no such thing as an unset element. */
    Arr *a = (Arr *)rt_alloc(slot_bytes(sizeof(Arr), len),
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
        int64_t *buf = realloc(l->data, slot_bytes(0, cap));
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

    Arr *a = (Arr *)rt_alloc(slot_bytes(sizeof(Arr), n),
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

/* `insert` and `remove_at` transfer ownership the same way push and pop do:
 * the caller hands a reference in, and gets one back out. Neither touches a
 * refcount here -- the lowering does it, so every container agrees. */
void rt_list_insert(Obj *o, int64_t i, int64_t v) {
    Lst *l = (Lst *)o;
    if (i < 0 || i > l->len) rt_trap("insert index out of range");
    rt_list_push(o, 0);              /* grow by one; the value is overwritten */
    for (int64_t j = l->len - 1; j > i; j--) {
        l->data[j] = l->data[j - 1];
    }
    l->data[i] = v;
}

int64_t rt_list_remove_at(Obj *o, int64_t i) {
    Lst *l = (Lst *)o;
    if (i < 0 || i >= l->len) rt_trap("index out of range");
    int64_t gone = l->data[i];
    for (int64_t j = i; j + 1 < l->len; j++) {
        l->data[j] = l->data[j + 1];
    }
    l->len--;
    return gone;
}

/* Releasing is the container's job here, because after this there is nothing
 * left to hand the references to. */
void rt_list_clear(Obj *o, bool elems_are_refs) {
    Lst *l = (Lst *)o;
    if (elems_are_refs) {
        for (int64_t i = 0; i < l->len; i++) {
            rc_dec((Obj *)(intptr_t)l->data[i]);
        }
    }
    l->len = 0;
}

/* In place, so no ownership changes: the same references, different order.
 * Works for an Array too -- only the slots move. */
void rt_seq_reverse(Obj *o) {
    int64_t n = rt_len_of(o);
    int64_t *d = slots(o);
    for (int64_t i = 0, j = n - 1; i < j; i++, j--) {
        int64_t t = d[i];
        d[i] = d[j];
        d[j] = t;
    }
}

/* Sorting is a STABLE merge sort: O(n log n) always, one scratch buffer, and
 * equal elements keep their order. Stability is worth promising -- sorting by
 * one key and then another is the ordinary way to get a compound order, and
 * it only works if the second sort leaves ties alone.
 *
 * Heapsort would need no buffer, but it is not stable, and an unstable sort
 * is the kind of thing that cannot be fixed after a freeze.
 *
 * The comparison is passed in rather than switched on inside, so the element
 * kinds stay in one place: the caller. */
typedef int64_t (*SortCmp)(int64_t, int64_t);

int64_t rt_cmp_int(int64_t a, int64_t b) {
    if (a < b) return -1;
    return a > b ? 1 : 0;
}

int64_t rt_cmp_str(int64_t a, int64_t b) {
    const Str *x = (const Str *)(intptr_t)a;
    const Str *y = (const Str *)(intptr_t)b;
    int64_t n = x->len < y->len ? x->len : y->len;
    int c = n == 0 ? 0 : memcmp(x->data, y->data, (size_t)n);
    if (c != 0) return c < 0 ? -1 : 1;
    /* A prefix sorts before what extends it. */
    if (x->len == y->len) return 0;
    return x->len < y->len ? -1 : 1;
}

static void merge_run(int64_t *d, int64_t *tmp, int64_t lo, int64_t mid,
                      int64_t hi, SortCmp cmp) {
    int64_t i = lo, j = mid, k = lo;
    while (i < mid && j < hi) {
        /* `<= 0` takes from the left on a tie, which is what makes it
         * stable. */
        tmp[k++] = cmp(d[i], d[j]) <= 0 ? d[i++] : d[j++];
    }
    while (i < mid) tmp[k++] = d[i++];
    while (j < hi) tmp[k++] = d[j++];
    for (int64_t x = lo; x < hi; x++) d[x] = tmp[x];
}

static void rt_sort_with(Obj *o, SortCmp cmp) {
    int64_t n = rt_len_of(o);
    if (n < 2) return;
    int64_t *d = slots(o);

    int64_t *tmp = malloc(slot_bytes(0, n));
    if (tmp == NULL) rt_trap("out of memory");

    /* Bottom-up, so there is no recursion and no stack depth to worry about
     * on a large list. */
    for (int64_t width = 1; width < n; width *= 2) {
        for (int64_t lo = 0; lo < n; lo += 2 * width) {
            int64_t mid = lo + width;
            int64_t hi = lo + 2 * width;
            if (mid >= n) break;
            if (hi > n) hi = n;
            merge_run(d, tmp, lo, mid, hi, cmp);
        }
    }
    free(tmp);
}

void rt_sort_int(Obj *o) {
    rt_sort_with(o, rt_cmp_int);
}

void rt_sort_str(Obj *o) {
    rt_sort_with(o, rt_cmp_str);
}

/* Sorting floats needs a TOTAL order, and `<` is not one: every comparison
 * with a NaN is false, so a NaN anywhere makes the merge's decisions
 * inconsistent and the result depends on the order elements happened to be
 * in. That is a silent wrong answer, not a crash.
 *
 * So: -inf < ... < -0.0 < +0.0 < ... < +inf < NaN. Zeroes compare equal, as
 * `==` says they are, and a stable sort then leaves them in the order they
 * arrived. NaNs sort to the end, together. This is the same shape as Rust's
 * total_cmp, minus distinguishing the two signs of zero. */
static int64_t rt_cmp_float(int64_t a, int64_t b) {
    double x = rt_i2f(a);
    double y = rt_i2f(b);
    bool xn = x != x;
    bool yn = y != y;
    if (xn || yn) {
        if (xn && yn) return 0;
        return xn ? 1 : -1;
    }
    if (x < y) return -1;
    return x > y ? 1 : 0;
}

void rt_sort_float(Obj *o) {
    rt_sort_with(o, rt_cmp_float);
}

/* Membership, following exactly the rule `==` follows so the two cannot
 * disagree: a str by value, a float as a float, anything else by word.
 *
 * A float has to be compared AS a float, not as the word holding it. The bit
 * patterns of 0.0 and -0.0 differ while the values are equal, and two NaNs
 * can share a bit pattern while comparing unequal. Both would be wrong the
 * other way round. */
/* -1 for absent. The sentinel does not escape: the lowering turns it into a
 * None before anyone can see it, which is the whole reason index_of waited
 * for Option rather than shipping a -1 into the language. */
int64_t rt_seq_index_of(Obj *o, int64_t v, int kind) {
    int64_t n = rt_len_of(o);
    int64_t *d = slots(o);
    for (int64_t i = 0; i < n; i++) {
        if (kind == SEQ_STR) {
            if (rt_str_eq((Obj *)(intptr_t)d[i], (Obj *)(intptr_t)v)) return i;
        } else if (kind == SEQ_FLOAT) {
            if (rt_i2f(d[i]) == rt_i2f(v)) return i;
        } else if (d[i] == v) {
            return i;
        }
    }
    return -1;
}

bool rt_seq_contains(Obj *o, int64_t v, int kind) {
    int64_t n = rt_len_of(o);
    int64_t *d = slots(o);
    for (int64_t i = 0; i < n; i++) {
        if (kind == SEQ_STR) {
            if (rt_str_eq((Obj *)(intptr_t)d[i], (Obj *)(intptr_t)v)) return true;
        } else if (kind == SEQ_FLOAT) {
            if (rt_i2f(d[i]) == rt_i2f(v)) return true;
        } else if (d[i] == v) {
            return true;
        }
    }
    return false;
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

static void map_walk(Obj *o, VisitFn visit, void *ctx) {
    Map *m = (Map *)o;
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->slots[i].state != SLOT_FULL) continue;
        if (m->key_is_ref) visit(ctx, (Obj *)(intptr_t)m->slots[i].k);
        if (m->val_is_ref) visit(ctx, (Obj *)(intptr_t)m->slots[i].v);
    }
}

static const TypeInfo rt_map_type = { map_drop, NULL, map_walk };

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

/* Rebuild the table at `cap`, dropping tombstones.
 *
 * `cap` is not always larger. A removed slot has to stay DEAD rather than
 * EMPTY so a probe does not stop short at it, and `used` counts those --
 * which means a map that is repeatedly filled and emptied grows without
 * bound while `len()` stays 0. Measured before this: 8M set/remove pairs on
 * a map that was never larger than one entry reached 100 MB resident.
 *
 * So when the load is tombstones rather than entries, rehash at the SAME
 * capacity and sweep them instead of doubling. */
static void map_rehash(Map *m, int64_t cap) {
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
    /* Rehash before probing, so a full table can never spin forever. Double
     * only when live entries are what fills it; when the load is mostly
     * tombstones, sweep them at the current capacity instead. */
    if (m->cap == 0) {
        map_rehash(m, 8);
    } else if ((m->used + 1) * 10 >= m->cap * 7) {
        map_rehash(m, (m->len + 1) * 10 >= m->cap * 4 ? m->cap * 2 : m->cap);
    }

    bool found;
    int64_t i = map_probe(m, k, &found);
    if (found) {
        /* Retain BEFORE releasing. `m.set(k, m.get(k))` hands us the value
         * the slot already holds; releasing first would free it and the
         * retain would then resurrect freed memory. Every other overwrite
         * path -- StoreField, rt_index_set -- orders it this way too. */
        Obj *old = (Obj *)(intptr_t)m->slots[i].v;
        if (m->val_is_ref) rc_inc((Obj *)(intptr_t)v);
        m->slots[i].v = v;
        if (m->val_is_ref) rc_dec(old);
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

/* Keys and values come out as a List, rather than the map being iterable
 * directly.
 *
 * A map's slots are sparse -- EMPTY and DEAD sit among the FULL ones -- so an
 * index-based loop over them would need a cursor that skips, which is a
 * different loop shape from the one `for .. in` has. Copying into a dense
 * List reuses that shape exactly, and the allocation is visible at the call
 * site rather than hidden in the loop.
 *
 * Insertion order is NOT preserved: the order is the table's, which changes
 * when the table rehashes. Nothing promises otherwise.
 *
 * The list holds its own reference to each element, like any container. */
static Obj *map_collect(Obj *o, bool want_keys) {
    Map *m = (Map *)o;
    bool refs = want_keys ? m->key_is_ref : m->val_is_ref;
    Obj *out = rt_list_new(refs);

    for (int64_t i = 0; i < m->cap; i++) {
        if (m->slots[i].state != SLOT_FULL) continue;
        int64_t e = want_keys ? m->slots[i].k : m->slots[i].v;
        rt_list_push(out, e);
        if (refs) rc_inc((Obj *)(intptr_t)e);
    }
    return out;
}

Obj *rt_map_keys(Obj *o) {
    return map_collect(o, true);
}

Obj *rt_map_values(Obj *o) {
    return map_collect(o, false);
}

void rt_map_clear(Obj *o) {
    Map *m = (Map *)o;
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->slots[i].state != SLOT_FULL) continue;
        if (m->key_is_ref) rc_dec((Obj *)(intptr_t)m->slots[i].k);
        if (m->val_is_ref) rc_dec((Obj *)(intptr_t)m->slots[i].v);
    }
    free(m->slots);
    m->slots = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
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

static const TypeInfo rt_chan_type = { chan_drop, NULL, NULL };

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

    c->buf = malloc(slot_bytes(0, capacity));
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

/* The handle and the count are published TOGETHER, under the lock.
 *
 * Reserving the slot first and writing the handle after unlocking made the
 * count say a thread existed before its handle did, so rt_wait_all could
 * join a slot that was still zero. ThreadSanitizer found the race on the
 * counter; the join was the part that mattered. */
void rt_spawn(void *(*entry)(void *), void *arg) {
    pthread_t t;
    if (pthread_create(&t, NULL, entry, arg) != 0) {
        rt_trap("could not spawn a thread");
    }
    pthread_mutex_lock(&rt_threads_lock);
    if (rt_nthreads >= RT_MAX_THREADS) {
        pthread_mutex_unlock(&rt_threads_lock);
        rt_trap("too many spawned threads");
    }
    rt_threads[rt_nthreads++] = t;
    pthread_mutex_unlock(&rt_threads_lock);
}

/* A spawned thread may spawn more, so the count is re-read after each pass
 * rather than snapshotted once. The lock is released before joining: a
 * thread that spawns while we hold it would otherwise deadlock. */
void rt_wait_all(void) {
    int joined = 0;
    for (;;) {
        pthread_mutex_lock(&rt_threads_lock);
        int n = rt_nthreads;
        pthread_mutex_unlock(&rt_threads_lock);
        if (joined >= n) return;
        for (int i = joined; i < n; i++) {
            pthread_join(rt_threads[i], NULL);
        }
        joined = n;
    }
}

/* Flush stdout first so anything already printed is not lost behind the trap
 * message -- the corpus compares both streams. abort() gives a deterministic
 * exit status of 134 under gcc and clang.
 *
 * abort() skips atexit handlers, so a trapping program never prints
 * __rc_live. run.sh therefore does not require the refcount invariant on
 * corpus/traps/ programs. */
/* ---- transitive uniqueness -------------------------------------------- */

/* Checking only the moved object is not enough.
 *
 *     type Holder { str s; }
 *     str shared = concat("ab", "cd");
 *     spawn eat(Holder(shared), n);      // repeated
 *
 * Each Holder really is unique, so a shallow check passes -- and every one
 * of them points at the same Str, whose non-atomic refcount is then
 * incremented and decremented from several threads at once. Measured before
 * this: eight spawns, eight runs, eight failures, split between a refcount
 * going negative, a corrupted malloc arena and a segfault.
 *
 * So the whole graph reachable from the moved value has to be unreachable
 * from anywhere else. The test is exact rather than "every rc is 1": count
 * the references INTO each object from within the graph, treating the
 * mover's own reference to the root as one, and require that count to equal
 * the object's refcount. An object shared twice *inside* the graph is fine
 * -- one thread still owns all of it -- and is what a conservative rc == 1
 * test would wrongly refuse.
 *
 * Immortal objects are skipped and not walked: literals are never freed, and
 * a channel is immortal precisely because it is the one thing threads are
 * meant to share.
 *
 * Cost is proportional to the graph, paid once per value crossing a
 * boundary. A boundary crossing already costs a lock and a condition
 * variable, and a moved value cannot be sent twice. */

typedef struct {
    Obj    **keys;   /* open addressing, NULL is empty */
    int64_t *cnt;    /* references seen into keys[i] */
    Obj    **todo;   /* objects whose walk has not run yet */
    int64_t  todo_len;
    int64_t  todo_cap;
    int64_t  cap;    /* a power of two */
    int64_t  len;
} Reach;

static void reach_add(Reach *r, Obj *o);

static void reach_grow(Reach *r) {
    int64_t cap = r->cap == 0 ? 64 : r->cap * 2;
    Obj **keys = calloc((size_t)cap, sizeof(Obj *));
    int64_t *cnt = calloc((size_t)cap, sizeof(int64_t));
    if (keys == NULL || cnt == NULL) rt_trap("out of memory");

    Obj **oldk = r->keys;
    int64_t *oldc = r->cnt;
    int64_t oldcap = r->cap;
    int64_t mask = cap - 1;

    for (int64_t i = 0; i < oldcap; i++) {
        if (oldk[i] == NULL) continue;
        int64_t j = (int64_t)((((uintptr_t)oldk[i]) >> 4) & (uintptr_t)mask);
        while (keys[j] != NULL) j = (j + 1) & mask;
        keys[j] = oldk[i];
        cnt[j] = oldc[i];
    }
    free(oldk);
    free(oldc);
    r->keys = keys;
    r->cnt = cnt;
    r->cap = cap;
}

/* Record one reference into `o`, and queue it for walking the first time. */
static void reach_add(Reach *r, Obj *o) {
    if (o == NULL || o->rc == RC_IMMORTAL) return;
    if (r->cap == 0 || (r->len + 1) * 10 >= r->cap * 7) reach_grow(r);

    int64_t mask = r->cap - 1;
    int64_t i = (int64_t)((((uintptr_t)o) >> 4) & (uintptr_t)mask);
    while (r->keys[i] != NULL && r->keys[i] != o) i = (i + 1) & mask;

    if (r->keys[i] == NULL) {
        r->keys[i] = o;
        r->cnt[i] = 1;
        r->len++;
        if (r->todo_len == r->todo_cap) {
            int64_t cap = r->todo_cap == 0 ? 64 : r->todo_cap * 2;
            Obj **bigger = realloc(r->todo, (size_t)cap * sizeof(Obj *));
            if (bigger == NULL) rt_trap("out of memory");
            r->todo = bigger;
            r->todo_cap = cap;
        }
        r->todo[r->todo_len++] = o;
    } else {
        r->cnt[i]++;
    }
}

static void reach_visit(void *ctx, Obj *child) {
    reach_add((Reach *)ctx, child);
}

void rt_check_unique(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;

    /* The common case by far: a leaf the mover alone holds. Answer it
     * without allocating anything. */
    if (o->ty == NULL || o->ty->walk == NULL) {
        if (o->rc != 1) {
            rt_trap("value crossing a thread boundary is still referenced "
                    "elsewhere; clone() it, or drop the other reference first");
        }
        return;
    }

    Reach r = { NULL, NULL, NULL, 0, 0, 0, 0 };

    /* Each object is walked exactly once, the first time it is seen, so a
     * cycle terminates. */
    reach_add(&r, o);
    while (r.todo_len > 0) {
        Obj *cur = r.todo[--r.todo_len];
        if (cur->ty != NULL && cur->ty->walk != NULL) {
            cur->ty->walk(cur, reach_visit, &r);
        }
    }

    bool ok = true;
    for (int64_t i = 0; i < r.cap; i++) {
        if (r.keys[i] == NULL) continue;
        if (r.keys[i]->rc != r.cnt[i]) {
            ok = false;
            break;
        }
    }
    free(r.keys);
    free(r.cnt);
    free(r.todo);

    if (!ok) {
        rt_trap("value crossing a thread boundary is still referenced "
                "elsewhere; clone() it, or drop the other reference first");
    }
}

_Noreturn void rt_trap(const char *msg) {
    fflush(stdout);
    fprintf(stderr, "trap: %s\n", msg);
    abort();
}
