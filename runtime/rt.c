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

#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Every operating-system call the runtime makes goes through the sys layer
 * (runtime/sys.h, docs/sys-layer.md). Its implementation is #included here
 * rather than built as a file of its own, so the runtime stays one
 * translation unit and every build line keeps naming rt.c alone -- the
 * backend is a -D flag, not a different set of files to remember. */
#include "sys.h"
#ifdef RT_SYS_RAW
#include "sys_linux.c"
#else
#include "sys_libc.c"
#endif

void rc_inc(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    o->rc++;
}

void rc_dec(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    RC_ASSERT((o->rc & ~RC_FROZEN) > 0, "decrement below zero");
    /* The frozen flag sits above the count (rt.h), so the count is what is
     * below it: a frozen object is freed like any other when its last
     * reference goes. */
    if ((--o->rc & ~RC_FROZEN) == 0) {
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

const TypeInfo rt_str_type = { NULL, NULL, NULL, NULL };

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
 * A str is always valid UTF-8 (reference §3, docs/text-decision.md), and
 * every function here that makes one keeps it so. Sizes and offsets are in
 * BYTES -- O(1), and what files and sockets speak -- so `size()` is a byte
 * count and `substr` takes byte offsets, which must land on a character
 * boundary. Code points are language source, lib/__text.src.
 *
 * Why the rest cannot break validity, so nobody has to re-derive it: concat,
 * repeat and join put whole valid strings side by side; trim and the case
 * conversions touch only ASCII bytes, and UTF-8 never uses an ASCII byte
 * inside a multi-byte character; split and find match a valid needle, which
 * begins with a non-continuation byte and ends a character, so a match can
 * only start and end on boundaries. The doors from outside -- bytes.utf8(),
 * and through it io, fs and os -- check.
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

/* Is byte offset `i` of `s` the start of a character (or the end)? A str is
 * valid UTF-8, so the only non-boundary is a continuation byte, 10xxxxxx. */
static bool on_boundary(const Str *s, int64_t i) {
    return i == s->len || ((unsigned char)s->data[i] & 0xC0) != 0x80;
}

/* Byte offsets, half-open, and an out-of-range one traps the way an
 * out-of-range index does: it is a bug at the call site, not a condition to
 * handle.
 *
 * An offset inside a character traps too -- Rust's rule for &s[a..b]. The
 * alternatives were worse: a result holding half a character would break
 * the one promise a str makes (it is valid UTF-8, reference §3), and
 * rounding the offset to a boundary would hand back a string of a length the
 * caller did not ask for. Offsets a program gets from the language --
 * index_of, size(), a scan for an ASCII delimiter with byte_at -- are
 * always on a boundary, so only arithmetic on a guess lands here. */
Obj *rt_str_substr(Obj *o, int64_t from, int64_t to) {
    const Str *s = (const Str *)o;
    if (from < 0 || to < from || to > s->len) rt_trap("substring range out of bounds");
    int64_t bad = !on_boundary(s, from) ? from : !on_boundary(s, to) ? to : -1;
    if (bad >= 0) {
        char msg[96];
        snprintf(msg, sizeof msg,
                 "substring offset %" PRId64 " is inside a character, not on a UTF-8 boundary",
                 bad);
        rt_trap(msg);
    }
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

/* One byte, as an int. Out of range traps the way an index does: it is a bug
 * at the call site, not a condition to handle. This is the foundation a
 * library needs to do anything textual from inside the language. */
int64_t rt_str_byte_at(Obj *o, int64_t i) {
    const Str *s = (const Str *)o;
    if (i < 0 || i >= s->len) rt_trap("byte index out of range");
    return (unsigned char)s->data[i];
}

/* Parsing answers "did it parse", so it returns a sentinel the lowering turns
 * into an Option. The WHOLE string has to be consumed and it may not be
 * empty: "12abc" is not a number, and neither is "". Surrounding whitespace
 * is not accepted either -- trim first if that is what you meant. */
bool rt_str_parse_int(Obj *o, int64_t *out) {
    const Str *s = (const Str *)o;
    if (s->len == 0) return false;

    int64_t i = 0;
    bool neg = false;
    if (s->data[0] == '-' || s->data[0] == '+') {
        neg = s->data[0] == '-';
        i = 1;
        if (s->len == 1) return false;
    }
    int64_t n = 0;
    for (; i < s->len; i++) {
        char c = s->data[i];
        if (c < '0' || c > '9') return false;
        /* Overflow is "does not fit", not a wrap and not a trap: the caller
         * asked whether this parses, and out of range is one way it does
         * not. Accumulating negatively keeps INT64_MIN reachable. */
        if (__builtin_mul_overflow(n, (int64_t)10, &n)) return false;
        if (__builtin_sub_overflow(n, (int64_t)(c - '0'), &n)) return false;
    }
    if (!neg) {
        if (n == INT64_MIN) return false;
        n = -n;
    }
    *out = n;
    return true;
}

Obj *rt_int_to_str(int64_t n) {
    char buf[32];
    int k = snprintf(buf, sizeof buf, "%" PRId64, n);
    return str_new(buf, k);
}

Obj *rt_bool_to_str(bool b) {
    return b ? str_new("true", 4) : str_new("false", 5);
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

/* ---- standard output -------------------------------------------------- */

/* print goes to fd 1 through the sys layer, buffered here rather than by C
 * stdio, so that the raw backend really does print without the C library
 * and both backends buffer identically.
 *
 * The rules are stdio's, because they are the ones a program's output
 * ordering has always depended on:
 *
 *   - fully buffered when stdout is a pipe or a file, flushed when the
 *     buffer fills and at exit;
 *   - flushed after every line when stdout is a terminal, so an
 *     interactive program's output appears when it is printed;
 *   - flushed before anything is written to stderr and before a trap
 *     aborts, so the two streams interleave in program order.
 *
 * One print is one line, and it is appended under the lock as a whole, so
 * spawned threads printing at once interleave by line, never mid-line --
 * the guarantee glibc's locked stdio gave. 64 KiB is the default capacity
 * of a Linux pipe: a full buffer is one write that does not block on a
 * reader that is keeping up. */
#define OUT_CAP 65536
static char out_buf[OUT_CAP];
static size_t out_len;
static bool out_line_mode;
static pthread_mutex_t out_lock = PTHREAD_MUTEX_INITIALIZER;

/* All of it, across short writes and signals. Any other failure -- a closed
 * pipe, a full disk -- has nobody to report to: print returns nothing, and
 * stdio discarded the output the same way. */
static void write_all(int64_t fd, const char *p, size_t n) {
    while (n > 0) {
        int64_t r = sys_write(fd, p, (int64_t)n);
        if (r == -SYS_EINTR) continue;
        if (r <= 0) return;
        p += r;
        n -= (size_t)r;
    }
}

static void out_flush_locked(void) {
    write_all(1, out_buf, out_len);
    out_len = 0;
}

static void out_put_locked(const char *p, size_t n) {
    if (out_len + n > OUT_CAP) {
        out_flush_locked();
        /* Larger than the whole buffer: copying it through in pieces would
         * only add writes. */
        if (n > OUT_CAP) {
            write_all(1, p, n);
            return;
        }
    }
    memcpy(out_buf + out_len, p, n);
    out_len += n;
}

void rt_out_flush(void) {
    pthread_mutex_lock(&out_lock);
    out_flush_locked();
    pthread_mutex_unlock(&out_lock);
}

/* `p[0..n]` and a newline, as one line. */
void rt_out_line(const char *p, size_t n) {
    pthread_mutex_lock(&out_lock);
    out_put_locked(p, n);
    out_put_locked("\n", 1);
    if (out_line_mode) out_flush_locked();
    pthread_mutex_unlock(&out_lock);
}

/* Whether stdout is a terminal is asked once: stdio decides its buffering
 * mode at the first write and never again, and so does this. The exit
 * flush is registered here too. rc_debug.h's leak report is another atexit
 * handler, and handlers run in reverse order of registration, which is not
 * fixed between two constructors -- so that report flushes for itself
 * rather than relying on running before this one. */
__attribute__((constructor)) static void out_init(void) {
    out_line_mode = sys_isatty(1) == 1;
    atexit(rt_out_flush);
}

void rt_print(int64_t v) {
    /* Digits by hand rather than snprintf: this is the hottest output path
     * there is, and the unsigned magnitude is what makes INT64_MIN safe. */
    char buf[24];
    char *end = buf + sizeof buf, *p = end;
    uint64_t m = v < 0 ? (uint64_t)0 - (uint64_t)v : (uint64_t)v;
    do {
        *--p = (char)('0' + m % 10);
        m /= 10;
    } while (m != 0);
    if (v < 0) *--p = '-';
    rt_out_line(p, (size_t)(end - p));
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
    if (v) {
        rt_out_line("true", 4);
    } else {
        rt_out_line("false", 5);
    }
}

void rt_print_str(Obj *o) {
    const Str *s = (const Str *)o;
    /* By length, not by NUL: the string may contain NUL bytes, and len is
     * authoritative. */
    rt_out_line(s->data, (size_t)s->len);
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

const TypeInfo rt_arr_val_type = { NULL, NULL, NULL, NULL };
const TypeInfo rt_arr_ref_type = { arr_drop_refs, NULL, arr_walk_refs, NULL };
const TypeInfo rt_lst_val_type = { lst_drop_vals, NULL, NULL, NULL };
const TypeInfo rt_lst_ref_type = { lst_drop_refs, NULL, lst_walk_refs, NULL };


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

/* An array of `len` slots with nothing in them yet.
 *
 * `rt_array_new` needs a fill because there is no zero value, and it retains
 * that fill once per element. A literal has a real value for every slot and
 * is about to write them all, so a fill would be allocated and released for
 * nothing. Every slot IS written before anything can read one -- the literal
 * lowering emits one store per element -- so the gap is not observable.
 *
 * Zeroed rather than left as malloc found it: a reference slot that is
 * briefly NULL is a value the drop function can survive, and uninitialised
 * memory is not. */
Obj *rt_array_blank(int64_t len, bool elems_are_refs) {
    if (len < 0) rt_trap("array length cannot be negative");
    Arr *a = (Arr *)rt_alloc(slot_bytes(sizeof(Arr), len),
                             elems_are_refs ? &rt_arr_ref_type : &rt_arr_val_type);
    a->len = len;
    for (int64_t i = 0; i < len; i++) {
        a->data[i] = 0;
    }
    return (Obj *)a;
}

/* Write one slot of a freshly blank array, without releasing what was there:
 * nothing was. `rt_index_set` is for an array that already holds values. */
void rt_array_put(Obj *o, int64_t i, int64_t v) {
    Arr *a = (Arr *)o;
    if (i < 0 || i >= a->len) rt_trap("index out of range");
    a->data[i] = v;
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
    rt_check_mutable(o);
    int64_t n = rt_len_of(o);
    if (i < 0 || i >= n) rt_trap("index out of range");
    slots(o)[i] = v;
}

void rt_list_push(Obj *o, int64_t v) {
    rt_check_mutable(o);
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

/* A list of `n` copies of `fill`: the `[v; n]` literal in List position.
 *
 * The buffer is sized once, up front, so a negative or absurd `n` traps here
 * with the same message `rt_array_new` gives instead of pushing one element
 * at a time until the process is killed. Like `rt_array_new`, a reference
 * fill is retained once per slot. */
Obj *rt_list_repeat(int64_t n, int64_t fill, bool elems_are_refs) {
    if (n < 0) rt_trap("array length cannot be negative");
    Lst *l = (Lst *)rt_list_new(elems_are_refs);
    if (n == 0) return (Obj *)l;
    l->data = malloc(slot_bytes(0, n));
    if (l->data == NULL) rt_trap("out of memory");
    l->cap = n;
    l->len = n;
    for (int64_t i = 0; i < n; i++) {
        l->data[i] = fill;
        if (elems_are_refs) rc_inc((Obj *)(intptr_t)fill);
    }
    return (Obj *)l;
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
    rt_check_mutable(o);
    Lst *l = (Lst *)o;
    if (l->len == 0) rt_trap("pop from an empty list");
    return l->data[--l->len];
}

/* `insert` and `remove_at` transfer ownership the same way push and pop do:
 * the caller hands a reference in, and gets one back out. Neither touches a
 * refcount here -- the lowering does it, so every container agrees. */
void rt_list_insert(Obj *o, int64_t i, int64_t v) {
    rt_check_mutable(o);
    Lst *l = (Lst *)o;
    if (i < 0 || i > l->len) rt_trap("insert index out of range");
    rt_list_push(o, 0);              /* grow by one; the value is overwritten */
    for (int64_t j = l->len - 1; j > i; j--) {
        l->data[j] = l->data[j - 1];
    }
    l->data[i] = v;
}

int64_t rt_list_remove_at(Obj *o, int64_t i) {
    rt_check_mutable(o);
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
    rt_check_mutable(o);
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
    rt_check_mutable(o);
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
    rt_check_mutable(o);
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

/* ---- bytes ------------------------------------------------------------
 *
 * One byte per element, in a buffer of its own so push can grow it. The
 * methods mirror str's over octets and are written the same way; the
 * differences are the ones mutability forces -- a result is always a fresh
 * object, never a view, because a view of something that can change is not
 * a value.
 */

static void bytes_drop(Obj *o) {
    free(((Bytes *)o)->data);
}

/* No walk: a byte is not a reference, so a bytes is a leaf at a thread
 * boundary and rt_check_unique answers it from the count alone. */
const TypeInfo rt_bytes_type = { bytes_drop, NULL, NULL, NULL };

/* A value going INTO a bytes. Truncating 256 to 0 would be a silent wrong
 * answer in exactly the code -- codecs, checksums -- least able to notice. */
static uint8_t as_byte(int64_t v) {
    if (v < 0 || v > 255) rt_trap("byte value out of range 0..255");
    return (uint8_t)v;
}

/* Make room for `need` bytes in total. Doubling keeps push amortised O(1);
 * the size is a byte count, so no slot multiplication can wrap, but the
 * doubling itself is capped before it can overflow. */
static void bytes_reserve(Bytes *b, int64_t need) {
    if (need <= b->cap) return;
    int64_t cap = b->cap < 8 ? 8 : b->cap;
    while (cap < need) {
        if (cap > INT64_MAX / 2) {
            cap = need;
            break;
        }
        cap *= 2;
    }
    uint8_t *buf = realloc(b->data, (size_t)cap);
    if (buf == NULL) rt_trap("out of memory");
    b->data = buf;
    b->cap = cap;
}

Obj *rt_bytes_new(int64_t cap) {
    if (cap < 0) rt_trap("bytes length cannot be negative");
    Bytes *b = (Bytes *)rt_alloc(sizeof(Bytes), &rt_bytes_type);
    b->len = 0;
    b->cap = 0;
    b->data = NULL;
    /* Never a NULL buffer, even when empty: every method can then index
     * and memcmp without a special case, and C's pointer arithmetic on NULL
     * -- even `NULL + 0` -- is undefined. Eight bytes is the price. */
    bytes_reserve(b, cap > 0 ? cap : 1);
    return (Obj *)b;
}

/* A fresh bytes holding a copy of `n` octets. */
static Obj *bytes_of(const uint8_t *src, int64_t n) {
    Bytes *b = (Bytes *)rt_bytes_new(n);
    if (n > 0) memcpy(b->data, src, (size_t)n);
    b->len = n;
    return (Obj *)b;
}

Obj *rt_bytes_fill(int64_t n, int64_t v) {
    uint8_t x = as_byte(v);
    Bytes *b = (Bytes *)rt_bytes_new(n);
    if (n > 0) memset(b->data, x, (size_t)n);
    b->len = n;
    return (Obj *)b;
}

int64_t rt_bytes_len(Obj *o) {
    return ((Bytes *)o)->len;
}

int64_t rt_bytes_get(Obj *o, int64_t i) {
    Bytes *b = (Bytes *)o;
    if (i < 0 || i >= b->len) rt_trap("index out of range");
    return b->data[i];
}

/* The index is checked before the value, the order a reader sees them in
 * `b[i] = v`. */
void rt_bytes_set(Obj *o, int64_t i, int64_t v) {
    rt_check_mutable(o);
    Bytes *b = (Bytes *)o;
    if (i < 0 || i >= b->len) rt_trap("index out of range");
    b->data[i] = as_byte(v);
}

void rt_bytes_push(Obj *o, int64_t v) {
    rt_check_mutable(o);
    Bytes *b = (Bytes *)o;
    uint8_t x = as_byte(v);
    bytes_reserve(b, b->len + 1);
    b->data[b->len++] = x;
}

int64_t rt_bytes_pop(Obj *o) {
    rt_check_mutable(o);
    Bytes *b = (Bytes *)o;
    if (b->len == 0) rt_trap("pop from empty bytes");
    return b->data[--b->len];
}

/* Keeps the buffer: clearing is what a reused read buffer does between
 * reads, and giving the memory back only to ask for it again is waste. */
void rt_bytes_clear(Obj *o) {
    rt_check_mutable(o);
    ((Bytes *)o)->len = 0;
}

/* `b.extend(b)` is legal and doubles `b`. The length is read before growing
 * and the source pointer after, so a realloc that moves the buffer moves the
 * source with it. */
void rt_bytes_extend(Obj *o, Obj *more) {
    rt_check_mutable(o);
    Bytes *b = (Bytes *)o;
    int64_t n = ((Bytes *)more)->len;
    int64_t total;
    if (__builtin_add_overflow(b->len, n, &total)) rt_trap("bytes too long");
    bytes_reserve(b, total);
    if (n > 0) memmove(b->data + b->len, ((Bytes *)more)->data, (size_t)n);
    b->len = total;
}

bool rt_bytes_eq(Obj *a, Obj *b) {
    const Bytes *x = (const Bytes *)a;
    const Bytes *y = (const Bytes *)b;
    if (x == y) return true;
    if (x->len != y->len) return false;
    return memcmp(x->data, y->data, (size_t)x->len) == 0;
}

Obj *rt_bytes_clone(Obj *o) {
    const Bytes *b = (const Bytes *)o;
    return bytes_of(b->data, b->len);
}

Obj *rt_str_to_bytes(Obj *s) {
    const Str *x = (const Str *)s;
    return bytes_of((const uint8_t *)x->data, x->len);
}

Obj *rt_bytes_substr(Obj *o, int64_t from, int64_t to) {
    const Bytes *b = (const Bytes *)o;
    if (from < 0 || to < from || to > b->len) rt_trap("substring range out of bounds");
    return bytes_of(b->data + from, to - from);
}

/* -1 for absent, turned into a None by the lowering; an empty needle is
 * found at 0 -- str's rules exactly. */
int64_t rt_bytes_find(Obj *o, Obj *needle) {
    const Bytes *h = (const Bytes *)o;
    const Bytes *n = (const Bytes *)needle;
    if (n->len == 0) return 0;
    for (int64_t i = 0; i + n->len <= h->len; i++) {
        if (memcmp(h->data + i, n->data, (size_t)n->len) == 0) return i;
    }
    return -1;
}

bool rt_bytes_starts_with(Obj *o, Obj *p) {
    const Bytes *b = (const Bytes *)o;
    const Bytes *q = (const Bytes *)p;
    if (q->len > b->len) return false;
    return memcmp(b->data, q->data, (size_t)q->len) == 0;
}

bool rt_bytes_ends_with(Obj *o, Obj *p) {
    const Bytes *b = (const Bytes *)o;
    const Bytes *q = (const Bytes *)p;
    if (q->len > b->len) return false;
    return memcmp(b->data + (b->len - q->len), q->data, (size_t)q->len) == 0;
}

Obj *rt_bytes_trim(Obj *o) {
    const Bytes *b = (const Bytes *)o;
    int64_t lo = 0;
    int64_t hi = b->len;
    while (lo < hi && is_space((char)b->data[lo])) lo++;
    while (hi > lo && is_space((char)b->data[hi - 1])) hi--;
    return bytes_of(b->data + lo, hi - lo);
}

/* ASCII only, as on str: a byte above 127 is not a letter of anything this
 * runtime can know. */
Obj *rt_bytes_case(Obj *o, bool upper) {
    const Bytes *b = (const Bytes *)o;
    Bytes *out = (Bytes *)bytes_of(b->data, b->len);
    for (int64_t i = 0; i < out->len; i++) {
        uint8_t c = out->data[i];
        if (upper && c >= 'a' && c <= 'z') out->data[i] = (uint8_t)(c - 32);
        if (!upper && c >= 'A' && c <= 'Z') out->data[i] = (uint8_t)(c + 32);
    }
    return (Obj *)out;
}

Obj *rt_bytes_repeat(Obj *o, int64_t n) {
    const Bytes *b = (const Bytes *)o;
    if (n < 0) rt_trap("cannot repeat bytes a negative number of times");
    int64_t total;
    if (__builtin_mul_overflow(b->len, n, &total)) rt_trap("bytes too long");
    Bytes *out = (Bytes *)rt_bytes_new(total);
    /* Empty repeated any number of times is empty, without n no-op copies. */
    if (b->len > 0) {
        for (int64_t i = 0; i < n; i++) {
            memcpy(out->data + i * b->len, b->data, (size_t)b->len);
        }
    }
    out->len = total;
    return (Obj *)out;
}

/* An empty separator has no answer that is not arbitrary, so it traps, as
 * on str. Each part is its own bytes, owned by the list. */
Obj *rt_bytes_split(Obj *o, Obj *sep) {
    const Bytes *b = (const Bytes *)o;
    const Bytes *d = (const Bytes *)sep;
    if (d->len == 0) rt_trap("cannot split on an empty separator");

    Obj *out = rt_list_new(true);
    int64_t start = 0;
    for (int64_t i = 0; i + d->len <= b->len;) {
        if (memcmp(b->data + i, d->data, (size_t)d->len) == 0) {
            rt_list_push(out, (int64_t)(intptr_t)bytes_of(b->data + start, i - start));
            i += d->len;
            start = i;
        } else {
            i++;
        }
    }
    rt_list_push(out, (int64_t)(intptr_t)bytes_of(b->data + start, b->len - start));
    return out;
}

Obj *rt_bytes_join(Obj *parts, Obj *sep) {
    const Bytes *d = (const Bytes *)sep;
    int64_t n = rt_len_of(parts);
    int64_t *el = slots(parts);
    Bytes *out = (Bytes *)rt_bytes_new(0);
    for (int64_t i = 0; i < n; i++) {
        /* `sep` may be one of the parts, or the result of nothing at all;
         * extend reads it fresh each time, so either is fine. */
        if (i > 0) rt_bytes_extend((Obj *)out, (Obj *)d);
        rt_bytes_extend((Obj *)out, (Obj *)(intptr_t)el[i]);
    }
    return (Obj *)out;
}

/* Lowercase, two digits a byte, no separator: the form every checksum tool
 * prints, and one that reads back unambiguously. */
Obj *rt_bytes_hex(Obj *o) {
    static const char digits[] = "0123456789abcdef";
    const Bytes *b = (const Bytes *)o;
    int64_t n;
    if (__builtin_mul_overflow(b->len, (int64_t)2, &n)) rt_trap("string too long");
    Str *s = (Str *)rt_alloc(sizeof(Str) + (size_t)n + 1, &rt_str_type);
    char *buf = (char *)(s + 1);
    for (int64_t i = 0; i < b->len; i++) {
        buf[2 * i] = digits[b->data[i] >> 4];
        buf[2 * i + 1] = digits[b->data[i] & 15];
    }
    buf[n] = '\0';
    s->len = n;
    s->data = buf;
    return (Obj *)s;
}

/* Strict UTF-8, RFC 3629: no overlong forms, no surrogates (U+D800..DFFF),
 * nothing past U+10FFFF, no truncated sequence. Anything else is a refusal
 * rather than a U+FFFD substitution -- a decoder that repairs is a decoder
 * that hides the bug that produced the input. The second byte's range is
 * what rules out the overlong and out-of-range forms, so each lead byte
 * carries its own. */
static bool utf8_valid(const uint8_t *p, int64_t n) {
    int64_t i = 0;
    while (i < n) {
        uint8_t c = p[i];
        if (c < 0x80) {
            i++;
            continue;
        }
        int64_t more;
        uint8_t lo = 0x80, hi = 0xBF;
        if (c >= 0xC2 && c <= 0xDF) {
            more = 1;
        } else if (c >= 0xE0 && c <= 0xEF) {
            more = 2;
            if (c == 0xE0) lo = 0xA0;         /* overlong below U+0800 */
            if (c == 0xED) hi = 0x9F;         /* surrogates */
        } else if (c >= 0xF0 && c <= 0xF4) {
            more = 3;
            if (c == 0xF0) lo = 0x90;         /* overlong below U+10000 */
            if (c == 0xF4) hi = 0x8F;         /* past U+10FFFF */
        } else {
            return false;                     /* 80..C1 and F5..FF never lead */
        }
        if (i + more >= n) return false;      /* truncated at the end */
        if (p[i + 1] < lo || p[i + 1] > hi) return false;
        for (int64_t k = 2; k <= more; k++) {
            if (p[i + k] < 0x80 || p[i + k] > 0xBF) return false;
        }
        i += more + 1;
    }
    return true;
}

bool rt_bytes_utf8(Obj *o, Obj **out) {
    const Bytes *b = (const Bytes *)o;
    *out = NULL;
    if (!utf8_valid(b->data, b->len)) return false;
    *out = str_new((const char *)b->data, b->len);
    return true;
}

/* ---- map --------------------------------------------------------------
 *
 * Open addressing with linear probing and a 70% load factor. One allocation
 * for the whole table, no per-entry node, and deletion leaves a tombstone so
 * a probe sequence is never broken.
 *
 * The layout (Map, MapSlot) is in rt.h, because a module constant's map is
 * laid out by the compiler as static data. So is the hashing: the compiler
 * places each key where map_probe below will look for it, which makes
 * hash_int, hash_key and the probe order part of that contract. Change them
 * together with src/lower/consts.rs (`map_hash`), or every constant map
 * silently stops finding its keys -- corpus/core/742-const-map checks.
 */

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

const TypeInfo rt_map_type = { map_drop, NULL, map_walk, NULL };

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
    MapSlot *old = m->slots;
    int64_t oldcap = m->cap;

    MapSlot *fresh = calloc((size_t)cap, sizeof(MapSlot));
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
    rt_check_mutable(o);
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
    rt_check_mutable(o);
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
    rt_check_mutable(o);
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

/* `clone(m)`: a new map with the same entries, each key and value retained
 * once more -- shallow, like rt_seq_clone. It exists so that a constant map
 * has the same way out as a constant array: `clone(TABLE)` is the mutable
 * copy. The table is copied as it stands, tombstones and all; the copy
 * sweeps them on its first rehash the way the original would have. */
Obj *rt_map_clone(Obj *o) {
    const Map *m = (const Map *)o;
    Map *c = (Map *)rt_map_new(m->key_is_str, m->key_is_ref, m->val_is_ref);
    if (m->cap == 0) return (Obj *)c;
    c->slots = malloc((size_t)m->cap * sizeof(MapSlot));
    if (c->slots == NULL) rt_trap("out of memory");
    memcpy(c->slots, m->slots, (size_t)m->cap * sizeof(MapSlot));
    c->cap = m->cap;
    c->len = m->len;
    c->used = m->used;
    for (int64_t i = 0; i < c->cap; i++) {
        if (c->slots[i].state != SLOT_FULL) continue;
        if (c->key_is_ref) rc_inc((Obj *)(intptr_t)c->slots[i].k);
        if (c->val_is_ref) rc_inc((Obj *)(intptr_t)c->slots[i].v);
    }
    return (Obj *)c;
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

static const TypeInfo rt_chan_type = { chan_drop, NULL, NULL, NULL };

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
    /* Collecting for rt_freeze rather than a thread crossing: skip what is
     * already frozen and every str as well as immortals. Those may be shared
     * by a const, because nobody can change them; they may NOT be shared by
     * a value crossing threads, because two threads would still race on
     * their counts. */
    bool     freezing;
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
    if (r->freezing && ((o->rc & RC_FROZEN) != 0 || o->ty == &rt_str_type)) return;
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

/* Walk everything reachable from `o` (subject to r->freezing) and say
 * whether all of it is private to `o`: no object in it is referenced from
 * outside it. The count of references into each object from within the
 * graph has to equal its refcount. `r` is left filled in, for rt_freeze to
 * mark; the caller frees it. */
static bool reach_private(Reach *r, Obj *o) {
    /* Each object is walked exactly once, the first time it is seen, so a
     * cycle terminates. */
    reach_add(r, o);
    while (r->todo_len > 0) {
        Obj *cur = r->todo[--r->todo_len];
        if (cur->ty != NULL && cur->ty->walk != NULL) {
            cur->ty->walk(cur, reach_visit, r);
        }
    }
    for (int64_t i = 0; i < r->cap; i++) {
        if (r->keys[i] == NULL) continue;
        if ((r->keys[i]->rc & ~RC_FROZEN) != r->cnt[i]) return false;
    }
    return true;
}

static void reach_free(Reach *r) {
    free(r->keys);
    free(r->cnt);
    free(r->todo);
}

void rt_check_unique(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;

    /* The common case by far: a leaf the mover alone holds. Answer it
     * without allocating anything. The frozen flag is not part of the count
     * (rt.h): a frozen value crosses under the same rule as any other,
     * because its count is still not atomic. */
    if (o->ty == NULL || o->ty->walk == NULL) {
        if ((o->rc & ~RC_FROZEN) != 1) {
            rt_trap("value crossing a thread boundary is still referenced "
                    "elsewhere; clone() it, or drop the other reference first");
        }
        return;
    }

    Reach r = { NULL, NULL, NULL, 0, 0, 0, 0, false };
    bool ok = reach_private(&r, o);
    reach_free(&r);
    if (!ok) {
        rt_trap("value crossing a thread boundary is still referenced "
                "elsewhere; clone() it, or drop the other reference first");
    }
}

/* A `const` binding takes a frozen snapshot of its value -- the author's
 * rule, docs/const-decision.md.
 *
 * Constness has to belong to the object, not to the name: under reference
 * counting `List<int> b = a;` makes a second name for the same list, and a
 * rule about the name `a` would say nothing about `b`. So the object is
 * marked, and every mutation path checks the mark.
 *
 * Marking a value somebody else still holds would change it under their
 * feet, so the binding decides here, at run time, which of two things to do:
 *
 *   - the value's whole graph is reachable from it alone (the same count the
 *     thread-boundary check makes): freeze it in place, no copy. This is the
 *     literal, the fresh construction, the call result nobody else kept;
 *   - anything in it is held from outside: deep-copy it and freeze the copy.
 *     The original stays exactly as mutable as it was.
 *
 * Frozen and immortal objects and every str are left out of both the count
 * and the copy: nobody can change them, so sharing them is harmless. */

/* Old object -> its copy, open addressing on the old pointer. The new
 * objects are the keys' values, which is also the list to freeze. */
typedef struct {
    Obj    **old;
    Obj    **copy;
    int64_t  cap;    /* a power of two */
    int64_t  len;
} CopyMap;

static int64_t copymap_slot(const CopyMap *m, const Obj *o) {
    int64_t mask = m->cap - 1;
    int64_t i = (int64_t)((((uintptr_t)o) >> 4) & (uintptr_t)mask);
    while (m->old[i] != NULL && m->old[i] != o) i = (i + 1) & mask;
    return i;
}

void rt_copy_register(void *ctx, Obj *old, Obj *copy) {
    CopyMap *m = (CopyMap *)ctx;
    if (m->cap == 0 || (m->len + 1) * 10 >= m->cap * 7) {
        int64_t cap = m->cap == 0 ? 64 : m->cap * 2;
        CopyMap g = { calloc((size_t)cap, sizeof(Obj *)),
                      calloc((size_t)cap, sizeof(Obj *)), cap, 0 };
        if (g.old == NULL || g.copy == NULL) rt_trap("out of memory");
        for (int64_t i = 0; i < m->cap; i++) {
            if (m->old[i] == NULL) continue;
            int64_t j = copymap_slot(&g, m->old[i]);
            g.old[j] = m->old[i];
            g.copy[j] = m->copy[i];
            g.len++;
        }
        free(m->old);
        free(m->copy);
        *m = g;
    }
    int64_t i = copymap_slot(m, old);
    m->old[i] = old;
    m->copy[i] = copy;
    m->len++;
}

static Obj *copy_obj(CopyMap *m, Obj *o);

/* A child's copy, +1 for the field or slot that will hold it. Something
 * already copied is shared, so a diamond in the original stays a diamond
 * and a cycle closes onto the copy. */
Obj *rt_copy_child(void *ctx, Obj *c) {
    CopyMap *m = (CopyMap *)ctx;
    if (c == NULL) return NULL;
    if ((c->rc & RC_FROZEN) != 0 || c->ty == &rt_str_type) {
        rc_inc(c);
        return c;
    }
    if (m->cap > 0) {
        int64_t i = copymap_slot(m, c);
        if (m->old[i] == c) {
            rc_inc(m->copy[i]);
            return m->copy[i];
        }
    }
    return copy_obj(m, c);
}

static void copy_slots(CopyMap *m, int64_t *dst, const int64_t *src, int64_t n, bool refs) {
    for (int64_t i = 0; i < n; i++) {
        dst[i] = refs ? (int64_t)(intptr_t)rt_copy_child(m, (Obj *)(intptr_t)src[i]) : src[i];
    }
}

/* A new object, count 1, registered before its children are copied. The
 * runtime's collections are copied here; a user type by its emitted CopyFn.
 * Recursive, so a very deep chain of objects is bounded by the C stack. */
static Obj *copy_obj(CopyMap *m, Obj *o) {
    const TypeInfo *ty = o->ty;
    if (ty == &rt_arr_val_type || ty == &rt_arr_ref_type) {
        const Arr *a = (const Arr *)o;
        Arr *n = (Arr *)rt_alloc(slot_bytes(sizeof(Arr), a->len), ty);
        n->len = a->len;
        for (int64_t i = 0; i < a->len; i++) n->data[i] = 0;
        rt_copy_register(m, o, (Obj *)n);
        copy_slots(m, n->data, a->data, a->len, ty == &rt_arr_ref_type);
        return (Obj *)n;
    }
    if (ty == &rt_lst_val_type || ty == &rt_lst_ref_type) {
        const Lst *l = (const Lst *)o;
        Lst *n = (Lst *)rt_list_new(ty == &rt_lst_ref_type);
        if (l->len > 0) {
            n->data = calloc((size_t)l->len, sizeof(int64_t));
            if (n->data == NULL) rt_trap("out of memory");
            n->cap = l->len;
        }
        rt_copy_register(m, o, (Obj *)n);
        copy_slots(m, n->data, l->data, l->len, ty == &rt_lst_ref_type);
        n->len = l->len;
        return (Obj *)n;
    }
    if (ty == &rt_bytes_type) {
        Obj *n = rt_bytes_clone(o);
        rt_copy_register(m, o, n);
        return n;
    }
    if (ty == &rt_map_type) {
        const Map *src = (const Map *)o;
        Map *n = (Map *)rt_map_new(src->key_is_str, src->key_is_ref, src->val_is_ref);
        rt_copy_register(m, o, (Obj *)n);
        if (src->cap > 0) {
            n->slots = calloc((size_t)src->cap, sizeof(MapSlot));
            if (n->slots == NULL) rt_trap("out of memory");
            n->cap = src->cap;
            n->used = src->used;
            for (int64_t i = 0; i < src->cap; i++) {
                MapSlot s = src->slots[i];
                if (s.state == SLOT_FULL) {
                    if (src->key_is_ref) s.k = (int64_t)(intptr_t)rt_copy_child(m, (Obj *)(intptr_t)s.k);
                    if (src->val_is_ref) s.v = (int64_t)(intptr_t)rt_copy_child(m, (Obj *)(intptr_t)s.v);
                }
                n->slots[i] = s;
            }
            n->len = src->len;
        }
        return (Obj *)n;
    }
    if (ty != NULL && ty->copy != NULL) return ty->copy(o, m);
    rt_trap("internal: a value of this type cannot be copied for a const");
}

Obj *rt_snapshot(Obj *o) {
    if ((o->rc & RC_FROZEN) != 0 || o->ty == &rt_str_type) return o;
    if (o->ty == NULL || o->ty->walk == NULL) {
        if (o->rc == 1) {
            o->rc |= RC_FROZEN;
            return o;
        }
    } else {
        Reach r = { NULL, NULL, NULL, 0, 0, 0, 0, true };
        bool private = reach_private(&r, o);
        if (private) {
            for (int64_t i = 0; i < r.cap; i++) {
                if (r.keys[i] != NULL) r.keys[i]->rc |= RC_FROZEN;
            }
        }
        reach_free(&r);
        if (private) return o;
    }
    /* Shared: copy, then freeze exactly the objects the copy made -- they
     * are the map's values, and nothing outside holds any of them yet. */
    CopyMap m = { NULL, NULL, 0, 0 };
    Obj *snap = copy_obj(&m, o);
    for (int64_t i = 0; i < m.cap; i++) {
        if (m.old[i] != NULL) m.copy[i]->rc |= RC_FROZEN;
    }
    free(m.old);
    free(m.copy);
    rc_dec(o);
    return snap;
}

_Noreturn void rt_frozen_trap(void) {
    rt_trap("cannot modify a constant; clone() it for a copy that can be changed");
}

_Noreturn void rt_trap(const char *msg) {
    rt_out_flush();
    /* One write, assembled here, so the message cannot be split by another
     * thread's output; every trap message is a short literal, and one too
     * long for the buffer is cut rather than lost. */
    char buf[512];
    size_t n = strlen(msg), at = 6;
    memcpy(buf, "trap: ", 6);
    if (n > sizeof buf - at - 1) n = sizeof buf - at - 1;
    memcpy(buf + at, msg, n);
    at += n;
    buf[at++] = '\n';
    write_all(2, buf, at);
    abort();
}


/* ---- io primitives: lib/io.src, lib/fs.src ----------------------------- */
/* Each is ONE sys-layer call with the layer's convention passed straight
 * through: a non-negative value, or -errno in Linux numbering. Nothing here
 * loops, buffers, retries on EINTR, splits lines or decides what an errno
 * means -- that is all language source in lib/io.src and lib/fs.src. What
 * is left in C is only what C must do because the language cannot see it:
 *
 *   - a path becomes a C string. A str is NUL-terminated by construction
 *     (str_new), so it goes to the kernel as it is -- but a path holding a
 *     NUL would be silently truncated there and name a different file, so
 *     it is refused as EINVAL instead. So is the empty path, which names no
 *     file anybody meant.
 *   - a buffer range is checked. A primitive that writes into a caller's
 *     `bytes` (docs/stdlib-seam.md §2, amended) does so only inside
 *     [off, off + n), and that range is checked against the buffer's size
 *     HERE, never trusted from the caller: the kernel would happily write
 *     past the end of the allocation. A range outside it is a bug in the
 *     library, so it traps. The size of a `bytes` never changes here. */

static bool path_ok(Obj *path) {
    Str *p = (Str *)path;
    return p->len > 0 && memchr(p->data, '\0', (size_t)p->len) == NULL;
}

static void range_ok(int64_t size, int64_t off, int64_t n, const char *who) {
    if (off < 0 || n < 0 || off > size || n > size - off) rt_trap(who);
}

int64_t rt_open(Obj *path, int64_t flags, int64_t mode) {
    if (!path_ok(path)) return -SYS_EINVAL;
    return sys_open(((Str *)path)->data, flags, mode);
}

int64_t rt_read(int64_t fd, Obj *buf, int64_t off, int64_t n) {
    rt_check_mutable(buf);
    Bytes *b = (Bytes *)buf;
    range_ok(b->len, off, n, "__read: range outside the buffer");
    return sys_read(fd, b->data + off, n);
}

int64_t rt_write(int64_t fd, Obj *buf, int64_t off, int64_t n) {
    Bytes *b = (Bytes *)buf;
    range_ok(b->len, off, n, "__write: range outside the buffer");
    return sys_write(fd, b->data + off, n);
}

/* The same as rt_write from an immutable str, so writing text never copies
 * it into a `bytes` first. */
int64_t rt_write_str(int64_t fd, Obj *s, int64_t off, int64_t n) {
    Str *p = (Str *)s;
    range_ok(p->len, off, n, "__write_str: range outside the string");
    return sys_write(fd, p->data + off, n);
}

int64_t rt_close(int64_t fd) {
    return sys_close(fd);
}

int64_t rt_seek(int64_t fd, int64_t off, int64_t whence) {
    return sys_lseek(fd, off, whence);
}

/* A prim cannot return a struct, so SysStat's three fields are pushed in
 * order -- size, mode, mtime_ns -- and the library builds its own type. */
static void push_stat(Obj *out, const SysStat *st) {
    rt_list_push(out, st->size);
    rt_list_push(out, st->mode);
    rt_list_push(out, st->mtime_ns);
}

int64_t rt_fstat(int64_t fd, Obj *out) {
    SysStat st;
    int64_t r = sys_fstat(fd, &st);
    if (r == 0) push_stat(out, &st);
    return r;
}

int64_t rt_stat(Obj *path, bool follow, Obj *out) {
    if (!path_ok(path)) return -SYS_EINVAL;
    SysStat st;
    int64_t r = sys_stat(((Str *)path)->data, follow ? 1 : 0, &st);
    if (r == 0) push_stat(out, &st);
    return r;
}

int64_t rt_mkdir(Obj *path, int64_t mode) {
    if (!path_ok(path)) return -SYS_EINVAL;
    return sys_mkdir(((Str *)path)->data, mode);
}

int64_t rt_unlink(Obj *path) {
    if (!path_ok(path)) return -SYS_EINVAL;
    return sys_unlink(((Str *)path)->data);
}

int64_t rt_rmdir(Obj *path) {
    if (!path_ok(path)) return -SYS_EINVAL;
    return sys_rmdir(((Str *)path)->data);
}

int64_t rt_rename(Obj *from, Obj *to) {
    if (!path_ok(from) || !path_ok(to)) return -SYS_EINVAL;
    return sys_rename(((Str *)from)->data, ((Str *)to)->data);
}

/* The target is stored as written, not resolved, so it is any str without
 * a NUL -- empty included, which the kernel then refuses itself. */
int64_t rt_symlink(Obj *target, Obj *path) {
    Str *t = (Str *)target;
    if (memchr(t->data, '\0', (size_t)t->len) != NULL || !path_ok(path)) return -SYS_EINVAL;
    return sys_symlink(t->data, ((Str *)path)->data);
}

/* Fills the whole of `buf` at most; the library grows it and asks again
 * when the answer is larger than its size (sys.h, sys_listdir). */
int64_t rt_listdir(Obj *path, Obj *buf) {
    rt_check_mutable(buf);
    if (!path_ok(path)) return -SYS_EINVAL;
    Bytes *b = (Bytes *)buf;
    return sys_listdir(((Str *)path)->data, (char *)b->data, b->len);
}


/* ---- process primitives: lib/os.src, lib/date.src, lib/random.src ------ */
/* Each is one OS fact, handed back as a scalar or pushed onto the caller's
 * list. None of them decides anything: whether argv[0] is included, what an
 * unset variable means, how a clock reading becomes a date, how octets
 * become a die roll -- all of that is in the library, in source. */

static int    rt_argc_saved = 0;
static char **rt_argv_saved = NULL;

void rt_args_init(int argc, char **argv) {
    rt_argc_saved = argc;
    rt_argv_saved = argv;
}

/* Octets, not text: on Unix an argument is any run of non-NUL bytes, and a
 * str must be valid UTF-8. Pushing a str here would be the one way into the
 * type that nothing checked, so the library decodes with utf8() and decides
 * what a non-UTF-8 argument means (lib/os.src). */
void rt_args(Obj *out) {
    for (int i = 0; i < rt_argc_saved; i++) {
        const char *a = rt_argv_saved[i];
        rt_list_push(out, (int64_t)(intptr_t)bytes_of((const uint8_t *)a, (int64_t)strlen(a)));
    }
}

/* A name holding a NUL or an `=` cannot be a variable, and the C library
 * would look up a different one -- so it is simply not set. */
int64_t rt_env(Obj *name, Obj *out) {
    Str *n = (Str *)name;
    if (n->len == 0 || memchr(n->data, '\0', (size_t)n->len) != NULL ||
        memchr(n->data, '=', (size_t)n->len) != NULL) {
        return 0;
    }
    const char *v = getenv(n->data);
    if (v == NULL) return 0;
    /* Octets, for the reason rt_args gives: the value need not be UTF-8. */
    rt_list_push(out, (int64_t)(intptr_t)bytes_of((const uint8_t *)v, (int64_t)strlen(v)));
    return 1;
}

/* exit(), not _exit(): stdio is flushed and atexit handlers run, which is
 * how the -DRC_DEBUG report still appears. The range check (0..255) is in
 * lib/os.src, where the library can say what was wrong. */
_Noreturn void rt_exit(int64_t code) {
    rt_out_flush();
    exit((int)code);
}

/* `trap(msg)`: a program's way to say "this is a bug" -- the same trap the
 * language itself uses for an index out of range, with the program's own
 * message. Shaped like rt_trap: flush what `print` has buffered (it used
 * fflush(stdout), which is not where print's output waits, so a program's
 * last lines were lost whenever stdout was not a terminal), then one write
 * where the message fits, so another thread's output cannot split it. A
 * message too long for that is written in pieces rather than cut: unlike
 * rt_trap's literals, it is the program's text and may be long. */
_Noreturn void rt_panic(Obj *msg) {
    /* Through the runtime's own buffer and one write, as rt_trap does:
     * `print` does not use stdio, so an fflush(stdout) flushed nothing and a
     * trap lost the program's last lines. The length is the str's own, so an
     * embedded NUL does not cut the message short. */
    Str *m = (Str *)msg;
    size_t n = m->len > 0 ? (size_t)m->len : 0;
    rt_out_flush();
    char buf[4096];
    if (n + 7 <= sizeof buf) {
        memcpy(buf, "trap: ", 6);
        memcpy(buf + 6, m->data, n);
        buf[6 + n] = '\n';
        write_all(2, buf, n + 7);
    } else {
        write_all(2, "trap: ", 6);
        write_all(2, (const char *)m->data, n);
        write_all(2, "\n", 1);
    }
    abort();
}

/* CLOCK_REALTIME: wall-clock time since 1970-01-01T00:00:00Z. Seconds and
 * nanoseconds come from ONE reading, so they can never straddle a tick. */
void rt_clock(Obj *out) {
    /* Through the sys layer like every other OS fact, so the raw-syscall
     * backend needs no libc for it. One reading, split here. */
    int64_t ns = sys_clock_ns(SYS_CLOCK_REALTIME);
    if (ns < 0) rt_trap("the wall clock is unavailable");
    rt_list_push(out, ns / 1000000000);
    rt_list_push(out, ns % 1000000000);
}

/* The kernel's randomness, through sys_getrandom: getrandom(2) on Linux,
 * getentropy(3) elsewhere -- no file descriptor, so it works in a chroot with
 * no /dev. Chunks of 256 octets because getentropy answers no more per call.
 * Each octet is pushed as an int in 0..255 until random moves onto bytes. */
int64_t rt_entropy(int64_t n, Obj *out) {
    unsigned char buf[256];
    while (n > 0) {
        int64_t k = n > 256 ? 256 : n;
        int64_t got = sys_getrandom(buf, k);
        /* The sys layer answers -errno in Linux numbering; the prim
         * contract is a positive errno, as io's prims return. */
        if (got < 0) return -got;
        for (int64_t i = 0; i < k; i++) rt_list_push(out, (int64_t)buf[i]);
        n -= k;
    }
    return 0;
}
/* ---- end process primitives ------------------------------------------- */
