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
#include <time.h>

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

/* Phase 1 green-thread primitives (docs/concurrency-decision.md, "Phases"):
 * the slab stack allocator and the per-thread state table. #included for the
 * same reason the sys layer is, just above: the runtime stays one
 * translation unit, so this is exercised by gates.sh's "runtime compiles
 * clean" under every compiler and optimisation level it already checks
 * rt.c with, with no extra build line to remember.
 *
 * The context switch itself (runtime/ctx_switch_x86_64.S) is NOT pulled in
 * this way -- it is real assembly, not C, so it is a separate translation
 * unit by necessity, linked only by whatever actually calls rt_ctx_switch.
 * Nothing in rt.c does (see runtime/greenthread.h's rt_ctx_make and
 * rt_fiber_switch, both `static inline` for exactly this reason), so every
 * existing build line in this repository that links runtime/rt.c alone --
 * which is all of them -- keeps working unmodified without ever linking the
 * .s file it does not need. */
#include "greenthread.h"
#include "greenthread.c"

/* Phase 2/3 scheduler and reactor (docs/concurrency-decision.md). Unlike
 * greenthread.c above, scheduler.c and reactor.c are NOT #included here --
 * they stay their own translation units (see their own file headers for
 * why: scheduler.c takes the address of rt_ctx_trampoline, which only
 * ctx_switch_x86_64.S defines). Only their declarations are needed in this
 * file; every build line that links runtime/rt.c now also links
 * runtime/scheduler.c, runtime/reactor.c and runtime/ctx_switch_x86_64.S,
 * because `spawn` (rt_spawn, below) and `Chan` (rt_chan_send/recv/close,
 * "concurrency" section below) call into them unconditionally. This is the
 * one place that changes for every build line in the repository: `spawn`
 * meaning a green thread, not conditionally OS-thread-or-green-thread, is
 * the whole point (docs/concurrency-decision.md, "The decision being made
 * here, stated precisely"). */
#include "scheduler.h"
#include "reactor.h"

void rc_inc(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    o->rc++;
}

/* --- releasing a graph, without recursing ------------------------------
 *
 * An object's drop function releases what the object holds, so releasing a
 * chain used to cost one C stack frame per link: a 100 000-node linked list
 * segfaulted at -O0 and 200 000 at -O2, on the default 8 MiB stack
 * (docs/destructors-decision.md, "Recursion depth of release"). The depth is
 * the data's and the program chooses the data, so no stack size is the
 * answer; the pending work has to live on the heap.
 *
 * So the FIRST decrement to reach zero owns the release and runs a loop.
 * Every decrement that reaches zero underneath it finds `rc_releasing` set
 * and hands its object to the queue instead of calling back into rc_dec.
 * One frame, whatever the shape of the graph.
 *
 * A QUEUE and not a stack: an object's fields go in in the order its drop
 * function releases them, and a queue takes them out in that order. Across
 * objects the walk is breadth-first where it used to be depth-first. Nothing
 * promised depth-first; what reference §7.1 promises -- a destructor runs
 * before its own object's fields are released -- is unchanged, and so is the
 * order of two separate releases, because the first drains the queue before
 * it returns.
 *
 * Thread-local, because the counts are (reference §8.3): two threads never
 * reach one object, so they never share a queue either.
 *
 * --- and a destructor's body is top-level code -------------------------
 *
 * The queue is the STRUCTURAL walk's, and only the structural walk's: the
 * fields of an object being released, which is what could be a 10 000 000
 * link chain. It is NOT for what a destructor's own body drops.
 *
 * It used to be, because `rc_releasing` covered the whole drain, and that
 * made a destructor's body a different language from the same code at the
 * top level (docs/destructors-decision.md, "A destructor's body is ordinary
 * code"). Measured 2026-09-23, before this: a loop that opens and drops
 * 3 000 files runs fine at the top level and dies at iteration 1021 with
 * EMFILE inside a destructor, because every `File` was queued and none of
 * them closed; 3 000 000 short-lived objects peak at 1.5 MB at the top level
 * and at 212 MB inside a destructor; and an object made and dropped inside a
 * destructor body ran its own `drop` after the enclosing one returned, which
 * reference §4.4 says cannot happen.
 *
 * So `rt_drop_enter` / `rt_drop_leave` bracket the call to a user `drop`
 * body -- emitted by src/emit_c.rs, the only place the runtime's release
 * reaches the program -- and give the body a FRESH, empty queue: inside it
 * the first decrement to reach zero owns a drain of its own and runs it
 * before the body's next statement, exactly as at the top level. The outer
 * walk's pending objects are set aside untouched and picked up again
 * afterwards, so its order and its one-frame guarantee are unchanged.
 *
 * Draining the outer queue at that point instead would have been wrong twice
 * over: it releases objects the walk has not reached yet, and it does it
 * from inside the body, which is recursion once per link again.
 */
#define RC_Q_SMALL 64
static _Thread_local Obj  *rc_q_small[RC_Q_SMALL];
static _Thread_local Obj **rc_q;      /* rc_q_small, or a heap buffer */
static _Thread_local size_t rc_q_cap, rc_q_head, rc_q_tail;
static _Thread_local bool rc_releasing;
/* Whether an outer drain is holding rc_q_small. A nested drain -- one
 * started inside a destructor's body -- takes a heap buffer instead, so the
 * two never share the array. Only the nesting pays the malloc. */
static _Thread_local bool rc_q_small_busy;

/* Run what this object holds, then free it. A type with no reference-typed
 * fields has no drop function at all, so the common case is one predictable
 * branch, not a call. */
static void rc_release(Obj *o) {
    if (o->ty != NULL && o->ty->drop != NULL) {
        o->ty->drop(o);
    }
    RC_TRACK_FREE();
    free(o);
}

/* Hand `o` to the loop that is already running. */
static void rc_enqueue(Obj *o) {
    if (rc_q == NULL) {
        if (rc_q_small_busy) {
            /* An outer walk has the array; this drain was started inside a
             * destructor's body. Its own buffer, freed when it drains. */
            rc_q = (Obj **)malloc(RC_Q_SMALL * sizeof *rc_q);
            if (rc_q == NULL) {
                rc_release(o);
                return;
            }
        } else {
            rc_q = rc_q_small;
            rc_q_small_busy = true;
        }
        rc_q_cap = RC_Q_SMALL;
    }
    if (rc_q_tail == rc_q_cap && rc_q_head > 0) {
        /* Slide the live span down before growing. A chain enqueues and
         * dequeues one object at a time forever, and without this the
         * buffer would grow once per link. */
        memmove(rc_q, rc_q + rc_q_head, (rc_q_tail - rc_q_head) * sizeof *rc_q);
        rc_q_tail -= rc_q_head;
        rc_q_head = 0;
    }
    if (rc_q_tail == rc_q_cap) {
        size_t cap = rc_q_cap * 2;
        Obj **bigger = rc_q == rc_q_small ? (Obj **)malloc(cap * sizeof *rc_q)
                                          : (Obj **)realloc(rc_q, cap * sizeof *rc_q);
        if (bigger == NULL) {
            /* Out of memory in the middle of freeing memory. Release it here
             * instead, which recurses -- the behaviour this replaced, and
             * correct -- rather than trapping halfway through a graph. */
            rc_release(o);
            return;
        }
        if (rc_q == rc_q_small) {
            memcpy(bigger, rc_q_small, sizeof rc_q_small);
            /* Off the array and onto the heap: a nested drain may have it. */
            rc_q_small_busy = false;
        }
        rc_q = bigger;
        rc_q_cap = cap;
    }
    rc_q[rc_q_tail++] = o;
}

/* The last reference to a collection went while the runtime was using it.
 * Cold and out of line, like rt_frozen_trap and for the same reason. */
__attribute__((cold)) static _Noreturn void rt_busy_gone_trap(long rc) {
    if ((rc & RC_SORTING) != 0) {
        rt_trap("the last reference to the list went while it was being sorted: "
                "`cmp` dropped the very list `sort` was called on");
    }
    rt_trap("the last reference to the map went while it was being searched: "
            "`hash` or `eq` dropped the very map being looked in");
}

void rc_dec(Obj *o) {
    if (o->rc == RC_IMMORTAL) return;
    RC_ASSERT((o->rc & ~RC_FLAGS) > 0, "decrement below zero");
    /* The flags sit above the count (rt.h), so the count is what is below
     * them: a frozen object is freed like any other when its last reference
     * goes. */
    if ((--o->rc & ~RC_FLAGS) != 0) return;
    /* Letting go of the last reference to a collection the runtime is in the
     * middle of sorting or probing would free the buffer under it. The
     * callback that did it -- dropping the field the receiver was read from,
     * say -- is the same bug as changing it, and gets the same answer. */
    if (__builtin_expect((o->rc & RC_BUSY) != 0, 0)) rt_busy_gone_trap(o->rc);
    if (rc_releasing) {
        rc_enqueue(o);
        return;
    }
    rc_releasing = true;
    rc_release(o);
    while (rc_q_head < rc_q_tail) {
        rc_release(rc_q[rc_q_head++]);
    }
    rc_q_head = 0;
    rc_q_tail = 0;
    /* Give the buffer back, so a program is not left holding one a single big
     * graph needed and LeakSanitizer has nothing to report at exit, and so
     * the next drain -- or a drain nested inside a destructor's body -- finds
     * the small array free. */
    if (rc_q == rc_q_small) {
        rc_q_small_busy = false;
    } else if (rc_q != NULL) {
        free(rc_q);
    }
    rc_q = NULL;
    rc_q_cap = 0;
    rc_releasing = false;
}

/* A user destructor's body runs with a queue of its own: see the note above.
 * Nothing here can fail and nothing allocates -- the whole state is five
 * words, saved in the drop function's own frame. */
void rt_drop_enter(RcDrain *save) {
    save->q = rc_q;
    save->cap = rc_q_cap;
    save->head = rc_q_head;
    save->tail = rc_q_tail;
    save->releasing = rc_releasing;
    rc_q = NULL;
    rc_q_cap = 0;
    rc_q_head = 0;
    rc_q_tail = 0;
    rc_releasing = false;
}

void rt_drop_leave(RcDrain *save) {
    /* The body drained whatever it started, exactly as the top level does,
     * so there is nothing of its own left to carry. */
    RC_ASSERT(!rc_releasing && rc_q_head == rc_q_tail, "destructor body left a release pending");
    rc_q = save->q;
    rc_q_cap = save->cap;
    rc_q_head = save->head;
    rc_q_tail = save->tail;
    rc_releasing = save->releasing;
}

/* --- the runtime is using this collection ------------------------------
 *
 * docs/reentrancy-decision.md; reference §3.9, §4.4a, §7.4.
 *
 * Three of a type's methods are called BY the runtime, from inside its own
 * operations: `cmp` while a list is being sorted, `hash` and `eq` while a
 * map is being probed (rt.h, TypeInfo). While one of them runs, the runtime
 * is holding state the program could invalidate under it -- the element
 * buffer a `push` reallocates, a slot index a rehash makes meaningless -- so
 * for the length of the operation the receiver is marked, and every path
 * that changes an object refuses a marked one (rt.h, rt_check_mutable).
 *
 * The mark is a bit of the header word the frozen check already loads, so
 * the common case -- no callback at all, or one that touches nothing -- pays
 * two stores per whole operation and nothing per element: measured below 1%
 * on a 1 000 000-element sort, at the noise floor.
 *
 * READING the receiver stays legal, and so does changing anything else: only
 * the collection the runtime is working on is marked, so a callback may sort
 * or fill a different list, and may read this one -- the list is a valid
 * permutation of its elements throughout the merge, and the map is whole
 * until the probe returns.
 *
 * `take` reports whether it was this call that marked the object, so a
 * nested read of the same receiver -- `m.get(k)` inside that map's own `eq`
 * -- does not clear the outer mark when it returns.
 *
 * An immortal object is never marked: the bits could not be cleared again
 * (RC_IMMORTAL is every bit set), and it does not need them -- it reads as
 * frozen, so sorting it already traps, and a module-constant map's keys can
 * only be ints or strs, which run no program code.
 */
static bool rc_guard_take(Obj *o, long bit) {
    if (o->rc == RC_IMMORTAL || (o->rc & bit) != 0) return false;
    o->rc |= bit;
    return true;
}

static void rc_guard_drop(Obj *o, long bit, bool took) {
    if (took) o->rc &= ~bit;
}

/* A `const` block's runtime half (docs/const-decision.md, "`const` is also a
 * block"; rt.h has the full comment). Thin wrappers over the guard above,
 * applied to RC_FROZEN instead of RC_SORTING/RC_PROBING: the compiler is the
 * caller here, one call per name on entry and on every exit, rather than the
 * runtime bracketing its own callback. */
bool rt_freeze_enter(Obj *o) {
    return rc_guard_take(o, RC_FROZEN);
}

void rt_freeze_leave(Obj *o, bool took) {
    rc_guard_drop(o, RC_FROZEN, took);
}

const TypeInfo rt_str_type = { NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL };

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
 * boundary. Code points are language source, lib/__text.m31.
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
    /* Registered after the flush, so it runs BEFORE it: handlers run in
     * reverse order, and the last thing a program prints should go out to a
     * terminal that is already itself again. This is the exit path's half of
     * what rt_trap does for the abort path -- it is what covers `os.exit`,
     * which abandons every live object and so runs no destructor. */
    atexit(rt_term_restore);
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

/* These three -- and the emitted `drop_T` that releases a struct's fields --
 * release in place, walking a structure while its elements' destructors run.
 * That is the shape docs/reentrancy-decision.md forbids everywhere else, and
 * it is safe here for a reason that holds only here: a drop function runs
 * when the count has reached ZERO, so no reference to this object exists for
 * the program to reach it through. A destructor that could name it would be
 * holding one, and the count would not be zero. The one way to acquire one
 * during the walk -- storing `this` somewhere that outlives the call -- is
 * the resurrection trap (src/emit_c.rs). A cycle would be the exception, and
 * a cycle is never released at all (reference §7.1).
 *
 * So the elements' own destructors cannot see these containers, and nothing
 * detaches. Every path that releases while the structure IS reachable --
 * rt_list_clear, rt_map_clear, rt_map_remove, rt_map_set -- does. */
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

const TypeInfo rt_arr_val_type = { NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL };
const TypeInfo rt_arr_ref_type = { arr_drop_refs, NULL, arr_walk_refs, NULL, NULL,
                                   NULL, NULL, NULL };
const TypeInfo rt_lst_val_type = { lst_drop_vals, NULL, NULL, NULL, NULL,
                                   NULL, NULL, NULL };
const TypeInfo rt_lst_ref_type = { lst_drop_refs, NULL, lst_walk_refs, NULL, NULL,
                                   NULL, NULL, NULL };


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

/* rt_len_of, rt_index_get and rt_index_set moved to rt.h as `static inline`
 * (docs/perf-board.md item 1): included by the emitted C, they are in the
 * SAME translation unit as the caller there, so gcc/clang can inline the
 * bounds check and the dereference instead of calling out to this one. They
 * stay declared here too, in the sense that `is_list`/`slots` above exist for
 * rt.c's OWN internal callers (rt_seq_clone and friends, below), which have
 * no reason to duplicate the header's inline logic. */

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

/* rt_seq_clone of a range: a fresh collection of the SAME kind holding
 * [from, to), sharing the elements and retaining each, so an Array's slice
 * is an Array and a List's is a List.
 *
 * Half-open and bounds-checked exactly as rt_str_substr is -- that is the
 * contract this borrows, and a range the program computed wrong is a bug
 * that clamping would hide in whatever computed it. */
Obj *rt_seq_slice(Obj *o, int64_t from, int64_t to) {
    int64_t n = rt_len_of(o);
    if (from < 0 || to < from || to > n) rt_trap("slice range out of bounds");
    bool refs = o->ty == &rt_arr_ref_type || o->ty == &rt_lst_ref_type;
    int64_t *src = slots(o);
    int64_t m = to - from;

    if (is_list(o)) {
        Lst *l = (Lst *)rt_list_new(refs);
        for (int64_t i = 0; i < m; i++) {
            rt_list_push((Obj *)l, src[from + i]);
            if (refs) rc_inc((Obj *)(intptr_t)src[from + i]);
        }
        return (Obj *)l;
    }

    Arr *a = (Arr *)rt_alloc(slot_bytes(sizeof(Arr), m),
                             refs ? &rt_arr_ref_type : &rt_arr_val_type);
    a->len = m;
    for (int64_t i = 0; i < m; i++) {
        a->data[i] = src[from + i];
        if (refs) rc_inc((Obj *)(intptr_t)src[from + i]);
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
 * left to hand the references to.
 *
 * The buffer is DETACHED before anything is released -- the rule for every
 * release path, docs/reentrancy-decision.md "The release paths", and the
 * same shape rt_map_clear already had. An element's destructor is program
 * code; it may reach this very list, and it must find the empty list
 * `clear` promises rather than the one being walked.
 *
 * Releasing in place did not: a destructor that removed from the list being
 * cleared took a SECOND reference to the element already being destroyed
 * and released it again -- `drop` ran twice on it, then the resurrection
 * trap under -DRC_DEBUG, and a heap-use-after-free under AddressSanitizer
 * (corpus/core/1150).
 *
 * Nothing below touches `l` after the first release, which is the other
 * half of the rule. The caller does retain the receiver across a call that
 * can release (src/lower/hold.rs, `releases`), so the list cannot actually
 * go out from under this -- but the runtime does not have to know that, and
 * a `clear` that only ever reads its own locals after the first `rc_dec`
 * stays correct whichever side holds the reference.
 *
 * A list of values releases nothing, so no program code can run and there is
 * nothing to detach from: it keeps its buffer, as `bytes.clear` does, and a
 * scratch list cleared in a loop still allocates once. */
void rt_list_clear(Obj *o, bool elems_are_refs) {
    rt_check_mutable(o);
    Lst *l = (Lst *)o;
    if (!elems_are_refs) {
        l->len = 0;
        return;
    }
    int64_t *data = l->data;
    int64_t n = l->len;
    l->data = NULL;
    l->len = 0;
    l->cap = 0;
    for (int64_t i = 0; i < n; i++) {
        rc_dec((Obj *)(intptr_t)data[i]);
    }
    free(data);
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

    /* `d` and `n` are read once and used until the sort finishes, so the
     * list may not change while it does. A user `cmp` runs in the middle of
     * this (rt_cmp_obj); marking the list is what makes reaching back into
     * it a trap instead of a reallocated buffer or a freed element. The
     * mark is set for every sort, not only the ones that can call back:
     * two stores, and one rule is easier to state than two. */
    bool guard = rc_guard_take(o, RC_SORTING);
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
    rc_guard_drop(o, RC_SORTING, guard);
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

/* A list of a user type, ordered by the type's own `cmp` -- the same method
 * `<` desugars to, so a sorted list and a comparison cannot disagree.
 *
 * The runtime already holds everything it needs: each element is an Obj *,
 * whose header names its TypeInfo, which carries `cmp`. Nothing is passed in
 * and no function reference exists in the IR.
 *
 * The receiver decides. Every element of a `List<P>` is a P, so `a->ty->cmp`
 * and `b->ty->cmp` are the same function; the compiler refuses a list whose
 * element type is an interface, exactly so that stays true. */
static int64_t rt_cmp_obj(int64_t a, int64_t b) {
    Obj *x = (Obj *)(intptr_t)a;
    /* Unreachable unless the compiler and this file disagree: `sort` on a
     * type with no `cmp` is refused where it is written. */
    if (x->ty == NULL || x->ty->cmp == NULL)
        rt_trap("internal: sorting a type that has no `cmp`");
    return x->ty->cmp(x, (Obj *)(intptr_t)b);
}

void rt_sort_obj(Obj *o) {
    rt_sort_with(o, rt_cmp_obj);
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
const TypeInfo rt_bytes_type = { bytes_drop, NULL, NULL, NULL, NULL, NULL, NULL, NULL };

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

/* rt_bytes_len, rt_bytes_get and rt_bytes_set moved to rt.h as `static
 * inline`, for the same reason as rt_index_get/rt_index_set above. */

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

/* The two ways a buffer shrinks short of empty. Both keep the allocation,
 * as clear does: a read loop that has consumed a prefix says drop_front,
 * where before it had to build a copy with substr and throw the original
 * away -- an allocation and a copy per read.
 *
 * They disagree about an argument past the end because the questions do.
 * truncate(n) asks to be AT MOST n long, which a shorter buffer already is;
 * drop_front(n) asks for n bytes to be REMOVED, which a shorter buffer
 * cannot do, and whatever counted them has miscounted. */
void rt_bytes_truncate(Obj *o, int64_t n) {
    rt_check_mutable(o);
    if (n < 0) rt_trap("truncate: a length cannot be negative");
    Bytes *b = (Bytes *)o;
    if (n < b->len) b->len = n;
}

void rt_bytes_drop_front(Obj *o, int64_t n) {
    rt_check_mutable(o);
    if (n < 0) rt_trap("drop_front: a count cannot be negative");
    Bytes *b = (Bytes *)o;
    if (n > b->len) {
        char msg[80];
        snprintf(msg, sizeof msg,
                 "drop_front(%" PRId64 ") on %" PRId64 " bytes", n, b->len);
        rt_trap(msg);
    }
    /* memmove and not memcpy: the source and destination overlap whenever
     * more is kept than dropped, which is the ordinary case. */
    if (n > 0) memmove(b->data, b->data + n, (size_t)(b->len - n));
    b->len -= n;
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
 *
 * A MK_OBJ key is the one kind the compiler CANNOT lay out: the hash is the
 * program's own `hash` method, which does not exist until the program runs.
 * So a module-constant map keyed on a user type is refused where it is
 * written, and `map_hash` over there stays int-and-str only.
 */

/* In place, like arr_drop_refs and lst_drop_refs and for the same reason:
 * the count is zero, so nothing the program can run reaches this map. */
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

const TypeInfo rt_map_type = { map_drop, NULL, map_walk, NULL, NULL, NULL, NULL, NULL };

static uint64_t hash_int(int64_t x) {
    /* splitmix64's finaliser: cheap and mixes the low bits, which matters
     * because linear probing is sensitive to clustering. */
    uint64_t z = (uint64_t)x + 0x9e3779b97f4a7c15ULL;
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static uint64_t hash_key(const Map *m, int64_t k) {
    if (m->key == MK_OBJ) {
        Obj *o = (Obj *)(intptr_t)k;
        /* The compiler refuses a map whose key type has no `hash`, so this
         * can only fail if the compiler and this file have gone out of step
         * -- a trap, not a jump through NULL. */
        if (o->ty == NULL || o->ty->hash == NULL)
            rt_trap("internal: a map key whose type has no `hash`");
        /* MIXED, not used raw. A hand-written `hash` is usually a field or a
         * sum of fields, so its low bits cluster, and linear probing turns
         * clustering into long probe runs. Mixing costs three multiplies and
         * makes a lazy `hash` behave; it preserves collisions exactly, so two
         * keys the program means to collide still do. */
        return hash_int(o->ty->hash(o));
    }
    if (m->key != MK_STR) return hash_int(k);
    const Str *s = (const Str *)(intptr_t)k;
    uint64_t h = 1469598103934665603ULL;      /* FNV-1a */
    for (int64_t i = 0; i < s->len; i++) {
        h ^= (unsigned char)s->data[i];
        h *= 1099511628211ULL;
    }
    return h;
}

static bool key_eq(const Map *m, int64_t a, int64_t b) {
    if (m->key == MK_OBJ) {
        Obj *x = (Obj *)(intptr_t)a;
        /* The same `eq` that `==` calls, so a map cannot disagree with the
         * operator about which keys are the same one. */
        if (x->ty == NULL || x->ty->eq == NULL)
            rt_trap("internal: a map key whose type has no `eq`");
        return x->ty->eq(x, (Obj *)(intptr_t)b);
    }
    if (m->key != MK_STR) return a == b;
    return rt_str_eq((Obj *)(intptr_t)a, (Obj *)(intptr_t)b);
}

Obj *rt_map_new(int64_t key, bool key_is_ref, bool val_is_ref) {
    Map *m = (Map *)rt_alloc(sizeof(Map), &rt_map_type);
    m->slots = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
    m->key = (uint8_t)key;
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

/* Every entry point below marks the map for the length of its own probe
 * (rc_guard_take, above). `map_probe` runs the program's `hash` and `eq` and
 * then hands back a slot INDEX, which the caller reads and writes after the
 * callback has returned; a `set` or a `remove` from inside that callback
 * rehashes the table and the index means nothing any more. `map_rehash`
 * probes too, so a `set` is marked across the rehash as well as the probe.
 *
 * `get` and `has` change nothing but are marked all the same: they hold an
 * index across program code exactly as `set` does. */
void rt_map_set(Obj *o, int64_t k, int64_t v) {
    rt_check_mutable(o);
    bool guard = rc_guard_take(o, RC_PROBING);
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
        bool val_is_ref = m->val_is_ref;
        if (val_is_ref) rc_inc((Obj *)(intptr_t)v);
        m->slots[i].v = v;
        rc_guard_drop(o, RC_PROBING, guard);
        /* The mark comes off before the release: the value being replaced
         * may be the last of its kind, and its destructor is program code
         * that has every right to change this map. The slot already holds
         * the new value and `val_is_ref` is already in hand, so nothing here
         * reads `m` again after the release (docs/reentrancy-decision.md,
         * "The release paths"). */
        if (val_is_ref) rc_dec(old);
        return;
    }
    if (m->slots[i].state == SLOT_EMPTY) m->used++;
    m->slots[i].state = SLOT_FULL;
    m->slots[i].k = k;
    m->slots[i].v = v;
    m->len++;
    if (m->key_is_ref) rc_inc((Obj *)(intptr_t)k);
    if (m->val_is_ref) rc_inc((Obj *)(intptr_t)v);
    rc_guard_drop(o, RC_PROBING, guard);
}

int64_t rt_map_get(Obj *o, int64_t k) {
    Map *m = (Map *)o;
    if (m->cap == 0) rt_trap("key not in map");
    bool guard = rc_guard_take(o, RC_PROBING);
    bool found;
    int64_t i = map_probe(m, k, &found);
    if (!found) rt_trap("key not in map");
    int64_t v = m->slots[i].v;
    rc_guard_drop(o, RC_PROBING, guard);
    return v;
}

bool rt_map_has(Obj *o, int64_t k) {
    Map *m = (Map *)o;
    if (m->cap == 0) return false;
    bool guard = rc_guard_take(o, RC_PROBING);
    bool found;
    map_probe(m, k, &found);
    rc_guard_drop(o, RC_PROBING, guard);
    return found;
}

void rt_map_remove(Obj *o, int64_t k) {
    rt_check_mutable(o);
    Map *m = (Map *)o;
    if (m->cap == 0) return;
    bool guard = rc_guard_take(o, RC_PROBING);
    bool found;
    int64_t i = map_probe(m, k, &found);
    if (!found) {
        rc_guard_drop(o, RC_PROBING, guard);
        return;
    }
    Obj *dk = (Obj *)(intptr_t)m->slots[i].k;
    Obj *dv = (Obj *)(intptr_t)m->slots[i].v;
    /* Both flags are read BEFORE the first release, so that nothing after it
     * reads the map again: `m->val_is_ref` sat after `rc_dec(dk)`, and a key
     * destructor that dropped the last reference to this map would have made
     * that a read of freed memory. It cannot today -- the caller retains the
     * receiver across a call that can release (src/lower/hold.rs,
     * `releases`) -- and the runtime no longer relies on it either
     * (docs/reentrancy-decision.md, "The release paths"; corpus/core/1154). */
    bool key_is_ref = m->key_is_ref;
    bool val_is_ref = m->val_is_ref;
    m->slots[i].state = SLOT_DEAD;   /* not EMPTY: a probe must not stop here */
    m->len--;
    /* The slot is gone from the table before either release runs: a
     * destructor is program code and may look at this map, which must
     * already read as one entry shorter, and may change it, which the mark
     * must no longer refuse. */
    rc_guard_drop(o, RC_PROBING, guard);
    if (key_is_ref) rc_dec(dk);
    if (val_is_ref) rc_dec(dv);
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
    /* The table is detached BEFORE anything is released. A value's
     * destructor is program code, it may reach this map, and it must find an
     * empty one it can fill again rather than the table being walked. */
    MapSlot *old = m->slots;
    int64_t oldcap = m->cap;
    bool key_is_ref = m->key_is_ref;
    bool val_is_ref = m->val_is_ref;
    m->slots = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
    for (int64_t i = 0; i < oldcap; i++) {
        if (old[i].state != SLOT_FULL) continue;
        if (key_is_ref) rc_dec((Obj *)(intptr_t)old[i].k);
        if (val_is_ref) rc_dec((Obj *)(intptr_t)old[i].v);
    }
    free(old);
}

/* `clone(m)`: a new map with the same entries, each key and value retained
 * once more -- shallow, like rt_seq_clone. It exists so that a constant map
 * has the same way out as a constant array: `clone(TABLE)` is the mutable
 * copy. The table is copied as it stands, tombstones and all; the copy
 * sweeps them on its first rehash the way the original would have. */
Obj *rt_map_clone(Obj *o) {
    const Map *m = (const Map *)o;
    Map *c = (Map *)rt_map_new(m->key, m->key_is_ref, m->val_is_ref);
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

/* ---- concurrency ------------------------------------------------------
 *
 * `spawn` always means a green thread now (docs/concurrency-decision.md,
 * "The decision being made here, stated precisely"): a conditional
 * spawn -- OS thread sometimes, green thread other times -- would have
 * reintroduced the exact "two kinds of concurrency" split this language's
 * uncoloured design exists to reject, and it would have left `Chan`/`net`
 * unable to tell which kind of caller they had, which is the other half of
 * why the choice is unconditional rather than a runtime flag.
 *
 * One process-wide scheduler, created lazily on first use by whichever
 * happens first -- `rt_run_program` (always first in practice, since it
 * runs before the program's own first statement) or, defensively, a direct
 * `rt_spawn`/`Chan` call from a hand-written C caller that does not go
 * through `rt_run_program`. `pthread_once` makes this race-free without a
 * hand-rolled double-checked lock. */
static rt_scheduler_t *g_sched = NULL;
static pthread_once_t  g_sched_once = PTHREAD_ONCE_INIT;

static void init_global_scheduler(void) {
    /* KNOWN, HONEST LIMITATION -- read before touching this.
     *
     * rt_sched_spawn's backpressure (scheduler.c's squeue_push, when the
     * shared queue is full) is a real pthread_cond_wait on the CALLING OS
     * thread -- correct and harmless when the caller is an ordinary OS
     * thread, which is the only kind of caller every existing Phase 2/3
     * test ever used. It stops being harmless the moment the caller is
     * itself a green thread running on a carrier, which is exactly what
     * EVERY `spawn` now is (this task's own change). With only ONE carrier,
     * that carrier's OS thread can get stuck inside its own green thread's
     * `squeue_push` wait -- and the only thing that could ever free room in
     * that queue is a carrier drawing from it, which here means THIS SAME
     * now-blocked carrier. Genuine deadlock: found directly, while testing
     * this task's own "hundreds of green threads" requirement with
     * LANG_NUM_CARRIERS=1 and the default queue capacity (64) -- spawning
     * 300 workers in a tight loop from the top level hung forever. With two
     * or more carriers this does not happen in practice: whichever carrier
     * is NOT the one stuck spawning keeps draining the queue independently,
     * which is what every existing scheduler test exercised without ever
     * needing to know this path could block at all.
     *
     * The real fix is teaching rt_sched_spawn's own backpressure wait to
     * park the calling green thread instead of blocking its carrier
     * outright -- the exact same treatment this task already gave `Chan` --
     * which is scoped, real work inside scheduler.c itself and is
     * deliberately NOT done here: this task's brief is to reuse
     * scheduler.c's existing contract unchanged, not to extend it, and
     * scheduler.c's own standalone tests (every caller of rt_sched_spawn
     * before tonight) never needed it to be green-thread-safe.
     *
     * What IS done here, as an honest mitigation rather than a fix: the
     * queue capacity is raised, for this process-wide scheduler only, to
     * comfortably absorb a tight burst of spawns far past the scale this
     * task tests ("hundreds") on any carrier count including one -- UNLESS
     * an operator has already set LANG_GLOBAL_QUEUE_CAP, which is left
     * alone (scheduler.h's own env-var contract is respected, not
     * overridden). This narrows the deadlock window; it does not close it
     * -- an unyielding burst of more than this many spawns, with no
     * carrier ever free to drain concurrently, reproduces the exact same
     * hang. Phase 2/3's own test harnesses are unaffected: they create
     * their own schedulers directly with rt_sched_create, never through
     * this function, so their default (64) is exactly as before. */
    if (getenv("LANG_GLOBAL_QUEUE_CAP") == NULL) {
        setenv("LANG_GLOBAL_QUEUE_CAP", "1048576", 1);
    }
    g_sched = rt_sched_create(0); /* 0: auto-detect carrier count */
    /* The blocking-FFI handoff monitor is opt-in (scheduler.h) and is turned
     * on here, unconditionally, for every compiled program. Converting
     * net.src's sockets to non-blocking-plus-reactor (below, and
     * lib/net.src) covers the common, previously-UNBOUNDED blocking I/O
     * paths, but it is not exhaustive: regular-file I/O, `os.run`'s process
     * wait, DNS resolution (`__resolve`) and a terminal's canonical-mode
     * read are all still genuinely blocking syscalls a green thread can make
     * (unconverted -- out of this task's scope, which is `spawn`, `Chan` and
     * net.src specifically), and any one of them can now wait indefinitely
     * on a real event outside the process (another program's stdout, a
     * user's keypress). Before `spawn` meant a green thread, each such call
     * had its own OS thread and starved nobody; now several green threads
     * can share one carrier, so one stuck in any of those calls would freeze
     * every sibling queued behind it on the same carrier without this.  The
     * monitor hands a stuck carrier's queued-but-not-yet-run siblings to
     * another carrier -- it does not make the stuck call itself any faster,
     * only stops it from also stalling unrelated work. 20ms/5ms are chosen
     * to be short enough that an interactive or latency-sensitive program
     * does not visibly stall on one slow carrier, and long enough not to
     * mistake an ordinary brief syscall for a stuck one; like `fuel_size`,
     * these are a reasonable starting point rather than a hardware-tuned
     * figure. */
    rt_sched_start_blocking_monitor(g_sched, 20ull * 1000 * 1000,
                                     5ull * 1000 * 1000);
}

rt_scheduler_t *rt_global_scheduler(void) {
    pthread_once(&g_sched_once, init_global_scheduler);
    return g_sched;
}

/* The epoll reactor (runtime/reactor.h) that lib/net.src's `__wait_io`
 * parks against (rt_wait_io, in the net primitives section below). Lazily
 * created the same way as the scheduler, and bound to it: there is exactly
 * one of each per process, matching "a real deployment has exactly one
 * scheduler" (scheduler.h). */
static rt_reactor_t   *g_reactor = NULL;
static pthread_once_t  g_reactor_once = PTHREAD_ONCE_INIT;

static void init_global_reactor(void) {
    g_reactor = rt_reactor_create(rt_global_scheduler());
}

static rt_reactor_t *rt_global_reactor(void) {
    pthread_once(&g_reactor_once, init_global_reactor);
    return g_reactor;
}

/* rt_run_program's own teardown, below, needs to stop the reactor thread
 * (if one was ever started -- a program that never calls into `net` never
 * triggers init_global_reactor, and `g_reactor` stays NULL for its whole
 * life) before any scheduler memory it might still touch is freed. Reading
 * `g_reactor` here with no lock is safe only because rt_run_program is the
 * single, one-time, main-thread-only call site (its own header comment),
 * invoked strictly after every carrier OS thread has already been joined
 * (rt_sched_shutdown) -- so nothing concurrent can still be racing
 * init_global_reactor's own write to it by the time this runs. */
static void rt_global_reactor_destroy_if_created(void) {
    if (g_reactor != NULL) {
        rt_reactor_destroy(g_reactor);
        g_reactor = NULL;
    }
}

/* A FIFO queue of green-thread ids waiting to send or to receive on one
 * `Chan`. Plural matters: with OS-thread `spawn`, several threads could
 * block on `pthread_cond_wait` at once and the condvar's own wait queue
 * handled that for free. A green thread cannot block a carrier, so parking
 * it needs somewhere of our own to remember WHICH green thread(s) are
 * waiting, so the right one(s) get `rt_sched_unpark`-ed back -- not just
 * "someone, whoever the OS wakes next", which a condvar does not actually
 * promise in a useful order either.
 *
 * FIFO, not LIFO or unordered: a channel is a queue in the language's own
 * vocabulary (docs/concurrency-decision.md's accept-loop example hands
 * connections to workers through one), and waking the longest-waiting
 * green thread first is the one order that cannot starve a waiter
 * indefinitely while newer ones keep being satisfied first -- the same
 * fairness argument that makes Go's channels and POSIX's own documented
 * (if not always delivered) condvar semantics both pick FIFO. A plain
 * singly-linked list, one small malloc per park and one free per wake: this
 * is not a hot path relative to the rest of a channel operation (a lock
 * already taken, a potential context switch already paid for), so the
 * "correctness over cleverness" choice this codebase already makes for the
 * scheduler's own registry (runtime/scheduler.c) and the reactor's waiter
 * map (runtime/reactor.c) applies here too. */
typedef struct rt_chan_waiter {
    uint32_t               id;
    struct rt_chan_waiter *next;
} rt_chan_waiter_t;

typedef struct {
    rt_chan_waiter_t *head;
    rt_chan_waiter_t *tail;
} rt_chan_wqueue_t;

static void wq_push(rt_chan_wqueue_t *q, uint32_t id) {
    rt_chan_waiter_t *n = malloc(sizeof *n);
    if (n == NULL) rt_trap("out of memory: channel waiter");
    n->id = id;
    n->next = NULL;
    if (q->tail != NULL) q->tail->next = n; else q->head = n;
    q->tail = n;
}

/* True and `*out` set if a waiter was dequeued; false if the queue is
 * empty. Popping is the ONLY way an id ever leaves this queue -- there is
 * no separate removal path -- so every successful `rt_sched_unpark` a
 * send/recv does for a waiter corresponds to exactly one pop, which is what
 * makes the wake protocol below race-free without a condvar-style
 * "recheck, it might have been a spurious wakeup" step: the only way to be
 * woken via this queue is for the waker to have already popped this exact
 * id under this exact lock. */
static bool wq_pop(rt_chan_wqueue_t *q, uint32_t *out) {
    rt_chan_waiter_t *n = q->head;
    if (n == NULL) return false;
    q->head = n->next;
    if (q->head == NULL) q->tail = NULL;
    *out = n->id;
    free(n);
    return true;
}

/* Detach the whole queue (for `rt_chan_close`, which must wake EVERY
 * waiter, not one) and hand back its head; the caller walks and frees it
 * after unlocking, the same "collect under the lock, act after releasing
 * it" shape send/recv use below. */
static rt_chan_waiter_t *wq_drain(rt_chan_wqueue_t *q) {
    rt_chan_waiter_t *n = q->head;
    q->head = q->tail = NULL;
    return n;
}

struct Chan {
    Obj              hdr;
    pthread_mutex_t  lock;
    int64_t         *buf;
    int64_t          cap;
    int64_t          len;
    int64_t          head;
    bool             closed;
    rt_chan_wqueue_t recv_waiters; /* parked on `recv`: channel was empty */
    rt_chan_wqueue_t send_waiters; /* parked on `send`: channel was full */
};

static void chan_drop(Obj *o) {
    Chan *c = (Chan *)o;
    pthread_mutex_destroy(&c->lock);
    free(c->buf);
    /* Channels are immortal (rt_chan_new's own comment below), so in
     * practice this never runs with a live waiter on either queue -- a
     * process exits by waiting for every green thread to finish first
     * (rt_run_program), and a finished green thread cannot still be parked.
     * Freed defensively anyway, so this function has no hidden dependency
     * on that always being true. */
    rt_chan_waiter_t *n;
    n = wq_drain(&c->recv_waiters);
    while (n != NULL) { rt_chan_waiter_t *next = n->next; free(n); n = next; }
    n = wq_drain(&c->send_waiters);
    while (n != NULL) { rt_chan_waiter_t *next = n->next; free(n); n = next; }
}

static const TypeInfo rt_chan_type = { chan_drop, NULL, NULL, NULL, NULL,
                                       NULL, NULL, NULL };

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
    c->recv_waiters.head = c->recv_waiters.tail = NULL;
    c->send_waiters.head = c->send_waiters.tail = NULL;
    pthread_mutex_init(&c->lock, NULL);
    return c;
}

/* send/recv below both follow the same shape: lock, loop while the channel
 * cannot satisfy this call yet, enqueue THIS green thread's own id on the
 * relevant waiter queue, unlock, park, relock, recheck. The enqueue happens
 * BEFORE the unlock, which is what closes the lost-wakeup race: the id is
 * findable by the other side (under the same lock) from the moment it is
 * queued, so a send/recv on the other end that runs in the gap between this
 * thread's unlock and its own rt_sched_park call will already find this id
 * in the queue, pop it, and call rt_sched_unpark for it -- and
 * rt_sched_unpark is documented safe to call at any point relative to the
 * matching rt_sched_park, including before it (scheduler.h's "park/unpark"
 * section). The `while`, not `if`, is still needed even though wakes are
 * targeted rather than broadcast: the lock is released between "someone
 * freed a slot" and "this thread actually runs again", during which a
 * THIRD, never-parked green thread can walk straight in and take the slot
 * first (ordinary lock contention, not a flaw in the wake protocol), so the
 * condition is rechecked and the thread re-queues itself if it lost that
 * race -- the same reason a condvar wait is always written in a loop. */

void rt_chan_send(Chan *c, int64_t slot) {
    pthread_mutex_lock(&c->lock);
    while (c->len == c->cap && !c->closed) {
        uint32_t my_id = rt_sched_current_green_id();
        wq_push(&c->send_waiters, my_id);
        pthread_mutex_unlock(&c->lock);
        rt_sched_park(RT_GT_PARKED_CHAN);
        pthread_mutex_lock(&c->lock);
    }
    if (c->closed) {
        pthread_mutex_unlock(&c->lock);
        rt_trap("send on a closed channel");
    }
    c->buf[(c->head + c->len) % c->cap] = slot;
    c->len++;
    uint32_t wake_id;
    bool woke = wq_pop(&c->recv_waiters, &wake_id);
    pthread_mutex_unlock(&c->lock);
    if (woke) rt_sched_unpark(rt_global_scheduler(), wake_id);
}

int64_t rt_chan_recv(Chan *c) {
    pthread_mutex_lock(&c->lock);
    while (c->len == 0 && !c->closed) {
        uint32_t my_id = rt_sched_current_green_id();
        wq_push(&c->recv_waiters, my_id);
        pthread_mutex_unlock(&c->lock);
        rt_sched_park(RT_GT_PARKED_CHAN);
        pthread_mutex_lock(&c->lock);
    }
    if (c->len == 0) {
        pthread_mutex_unlock(&c->lock);
        rt_trap("receive on a closed and empty channel");
    }
    int64_t v = c->buf[c->head];
    c->head = (c->head + 1) % c->cap;
    c->len--;
    uint32_t wake_id;
    bool woke = wq_pop(&c->send_waiters, &wake_id);
    pthread_mutex_unlock(&c->lock);
    if (woke) rt_sched_unpark(rt_global_scheduler(), wake_id);
    return v;
}

void rt_chan_close(Chan *c) {
    pthread_mutex_lock(&c->lock);
    c->closed = true;
    /* Every waiter on both queues must wake -- closing flips `!c->closed` to
     * false for everyone's loop condition, regardless of how much room or
     * data there is -- so this is rt_sched_unpark called once per waiter,
     * not the single targeted wake send/recv do. Detached under the lock,
     * walked and unparked after releasing it, same shape as everywhere
     * else here. */
    rt_chan_waiter_t *recv_list = wq_drain(&c->recv_waiters);
    rt_chan_waiter_t *send_list = wq_drain(&c->send_waiters);
    pthread_mutex_unlock(&c->lock);

    rt_scheduler_t *s = rt_global_scheduler();
    while (recv_list != NULL) {
        rt_chan_waiter_t *next = recv_list->next;
        rt_sched_unpark(s, recv_list->id);
        free(recv_list);
        recv_list = next;
    }
    while (send_list != NULL) {
        rt_chan_waiter_t *next = send_list->next;
        rt_sched_unpark(s, send_list->id);
        free(send_list);
        send_list = next;
    }
}

void rt_chan_drop(Chan *c) {
    rc_dec((Obj *)c);
}

/* `arg`'s cast through `void *` and back is the one place this file relies
 * on the POSIX guarantee (not quite ISO C, but universal in practice --
 * `dlsym` depends on the same thing) that a function pointer round-trips
 * through `void *` unchanged; greenthread.h's rt_ctx_make already smuggles
 * `entry`/`arg` through raw registers for the identical reason, so this is
 * not a new kind of assumption for this runtime to make. */
static void rt_main_trampoline(void *argp) {
    void (*entry)(void) = (void (*)(void))(uintptr_t)argp;
    entry();
}

void rt_run_program(void (*entry)(void)) {
    rt_scheduler_t *s = rt_global_scheduler();
    rt_sched_spawn(s, rt_main_trampoline, (void *)(uintptr_t)entry);

    /* Wait for `entry` and everything it (transitively) spawns to finish.
     * "spawned count caught up with completed count" is an exact
     * quiescence signal here, not an approximation: the moment the two are
     * equal, EVERY green thread ever spawned by this scheduler has reached
     * RT_GT_DEAD, including green thread 0 running `entry` -- and a dead
     * green thread cannot spawn, so no new spawn can appear after that
     * instant without one already having been counted (whatever spawned it
     * was itself still live, hence not yet counted as completed, hence
     * spawned > completed at that moment). So there is no race between
     * observing equality and some other thread incrementing `spawned`
     * again a moment later: equality is a stable fixed point once reached.
     *
     * This also means a program that deadlocks -- a channel recv with
     * nobody left to send, the same case that hung a `pthread_join` forever
     * under the old OS-thread `spawn` -- hangs here forever too, which is
     * the same observable behaviour as before, not a regression: this loop
     * is the direct replacement for rt_wait_all's blocking joins, and it
     * is deliberately not a busy spin -- a short sleep between checks costs
     * nothing a program running real work would notice, and this is the
     * only place in a compiled program's life that ever polls it. */
    while (rt_sched_completed(s) < rt_sched_spawned(s)) {
        struct timespec ts;
        ts.tv_sec = 0;
        ts.tv_nsec = 1000 * 1000; /* 1ms */
        nanosleep(&ts, NULL);
    }

    rt_sched_shutdown(s);
    /* Stop and join the reactor thread (if `net` ever started one) BEFORE
     * rt_sched_destroy frees anything it could still touch. The reactor
     * thread is not one of the carriers rt_sched_shutdown above already
     * joined -- it is a separate OS thread (runtime/reactor.c) that calls
     * back into THIS scheduler, via rt_sched_unpark, every time epoll_wait
     * reports a ready fd. Without this call, that thread keeps running
     * (rt_reactor_destroy was never invoked anywhere else in this runtime)
     * right through rt_sched_destroy's pthread_mutex_destroy/free calls
     * below: a real event on any still-registered fd -- plausible even this
     * late, since a peer's last FIN/RST can arrive concurrently with this
     * process's own exit -- makes reactor_loop dereference `r->sched` and
     * touch `s->registry.lock`/`s->global`/`s->carriers[...]`, all freed or
     * about to be freed out from under it: a genuine TSan-confirmed
     * data race between the main thread inside rt_sched_destroy and the
     * reactor thread inside rt_sched_unpark (docs/concurrency-decision.md,
     * "Phase 3.5"). Ordered after rt_sched_shutdown (every carrier is
     * already stopped, so nothing else can still be pushing work the
     * reactor's own wakeups would need a live carrier to drain) and before
     * rt_sched_destroy (so the reactor thread has fully exited before any
     * of the memory it reads is freed). */
    rt_global_reactor_destroy_if_created();
    rt_sched_destroy(s);
}

void rt_spawn(void (*entry)(void *), void *arg) {
    rt_sched_spawn(rt_global_scheduler(), entry, arg);
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
        if ((r->keys[i]->rc & ~RC_FLAGS) != r->cnt[i]) return false;
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
        if ((o->rc & ~RC_FLAGS) != 1) {
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
        Map *n = (Map *)rt_map_new(src->key, src->key_is_ref, src->val_is_ref);
        rt_copy_register(m, o, (Obj *)n);
        /* The table is copied SLOT FOR SLOT, not rehashed, so every key
         * copy has to land where the original sat. For a user-typed key
         * that is the "equal keys hash equally" contract doing its work: the
         * copy is field-for-field equal to the original, so its `hash`
         * answers the same and a later probe finds it here. A `hash` built
         * on anything but the key's value -- an address, a counter -- breaks
         * that contract and would lose its entries here first. */
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

/* A value that owns a resource -- its type declares a destructor -- can be
 * neither frozen nor copied (docs/destructors-decision.md):
 *
 *   - a copy would own the same resource, and whichever of the two died
 *     first would release it under the other: a File's copy closes the
 *     descriptor, and the original's later writes land in whatever file
 *     reuses the number;
 *   - a frozen one would be half usable, in a way that depends on how its
 *     type happens to be written (a File that stores its read position in a
 *     field could be written but not read or closed), and its destructor
 *     would be the one change a constant allows.
 *
 * So a const may not hold one at all, shared or fresh. The compiler refuses
 * every binding whose static type can hold one; this is the backstop for a
 * value it cannot see into -- an interface, or a collection of them. It
 * runs over the whole unfrozen graph BEFORE anything is frozen or copied,
 * so a refused binding changes nothing (it traps anyway). Frozen objects
 * are skipped by the walk and need no check: nothing frozen owns a
 * resource, by this very rule. */
static void refuse_resource(const Obj *o) {
    if (o->ty == NULL || o->ty->resource == NULL) return;
    char msg[320];
    snprintf(msg, sizeof msg,
             "a const cannot hold a value of type `%s`: it owns a resource (it has a "
             "destructor), which a constant can neither freeze nor copy; bind it without `const`",
             o->ty->resource);
    rt_trap(msg);
}

Obj *rt_snapshot(Obj *o) {
    if ((o->rc & RC_FROZEN) != 0 || o->ty == &rt_str_type) return o;
    if (o->ty == NULL || o->ty->walk == NULL) {
        refuse_resource(o);
        if (o->rc == 1) {
            o->rc |= RC_FROZEN;
            return o;
        }
    } else {
        Reach r = { NULL, NULL, NULL, 0, 0, 0, 0, true };
        bool private = reach_private(&r, o);
        for (int64_t i = 0; i < r.cap; i++) {
            if (r.keys[i] != NULL) refuse_resource(r.keys[i]);
        }
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

/* One trap for every bit of the header that forbids a change (rt.h,
 * RC_FLAGS). Frozen first, because an immortal object has every bit set and
 * a constant is what it is. */
_Noreturn void rt_frozen_trap(long rc) {
    if ((rc & RC_FROZEN) != 0) {
        rt_trap("cannot modify a constant; clone() it for a copy that can be changed");
    }
    if ((rc & RC_SORTING) != 0) {
        rt_trap("the list changed while it was being sorted: `cmp` may read the "
                "list being sorted, but not change it");
    }
    rt_trap("the map changed while it was being searched: `hash` and `eq` may "
            "read the map being searched, but not change it");
}

_Noreturn void rt_trap(const char *msg) {
    /* Before anything is printed: a trap message written to a terminal still
     * in raw mode comes out as a staircase, with no carriage return between
     * the lines. Putting the settings back first is also the only chance
     * there is -- abort() runs no atexit handler and no destructor. */
    rt_term_restore();
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

/* ---- green threads, Phase 1: the probe's slow path ----------------------
 *
 * See rt.h for the contract this keeps with src/emit_c.rs's emitted probe
 * and docs/concurrency-decision.md for the design. The fast path (the
 * comparison itself) is inline in every emitted function on purpose, so it
 * costs one compare and one untaken branch there; everything that happens
 * only when the probe actually fires lives here instead, out of line, the
 * same reason rt_trap and rt_frozen_trap above are out of line.
 *
 * Thread-local, zero-initialised, so every OS thread -- including the
 * process's own main thread, which never runs on a slab stack at all --
 * starts with the probe permanently disabled (rt.h explains why 0 does
 * that) until something explicitly opts a carrier in via rt_fiber_switch
 * (runtime/greenthread.h). */
_Thread_local uintptr_t rt_stack_limit = 0;

/* `noinline` is declared on the rt.h prototype and repeated here -- see
 * rt_stack_limit's own comment in rt.h for exactly which bug this prevents.
 * A real, separate function call, so every call site gets a fresh
 * `%fs`-relative read of rt_stack_limit's address with nothing for the
 * optimiser to cache across: the caller never emits a TLS access of its own
 * to hoist in the first place. `local`'s address stands in for the caller's
 * current stack depth the same way the old inlined version's did -- one
 * real call frame deeper, which only ever makes the check more
 * conservative, never less. */
__attribute__((noinline)) void rt_stack_check(void) {
    int local;
    if ((uintptr_t)&local < rt_stack_limit) {
        rt_stack_probe_slow();
    }
}

void rt_stack_probe_slow(void) {
    if (rt_stack_limit == RT_STACK_LIMIT_POISON) {
        /* Phase 2 stub -- deliberately not implemented. The eventual design
         * poisons rt_stack_limit to force a probe to fire as a voluntary
         * yield point; yielding needs a scheduler to yield TO, and there is
         * none yet (docs/concurrency-decision.md, "Phases", Phase 2).
         *
         * Nothing in Phase 1 ever writes RT_STACK_LIMIT_POISON into
         * rt_stack_limit, so this branch should be unreachable today. It
         * traps rather than silently returning: silently returning here
         * would mean every subsequent call on this thread re-enters this
         * same branch forever (poisoning does not un-poison itself), which
         * is a worse failure than a clear trap naming exactly what is
         * missing. TODO(Phase 2): replace this trap with the real
         * park-and-yield, once there is a scheduler. The probe's own call
         * site does not need to change when that happens. */
        rt_trap("stack probe: preemption was requested, but no scheduler "
                "exists yet (Phase 2, docs/concurrency-decision.md)");
    }
    /* Not poisoned, so the probe fired because the comparison was honestly
     * true: the thread's stack pointer proxy really is below the lowest
     * valid address of its stack. That is a genuine overflow. */
    rt_trap("stack overflow");
}

/* ---- blocking FFI, Phase 3 ----------------------------------------------
 *
 * See rt.h for the full contract. NULL (the default on every OS thread that
 * never calls rt_blocking_register -- i.e. every program that does not use
 * runtime/scheduler.c) makes both functions below a null check and nothing
 * else, same discipline as rt_stack_limit defaulting to 0 above. */
_Thread_local rt_blocking_rec_t *rt_blocking_rec = NULL;

void rt_blocking_register(rt_blocking_rec_t *rec) {
    rt_blocking_rec = rec;
}

/* CLOCK_MONOTONIC, never CLOCK_REALTIME: wall-clock time can jump (NTP, a
 * manual clock change), which would make "how long has this carrier been
 * blocking" briefly wrong in either direction -- including, worst case,
 * appearing to leap backwards and never crossing the monitor's timeout at
 * all. Monotonic cannot do either. */
static uint64_t rt_monotonic_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

void rt_enter_blocking(void) {
    if (rt_blocking_rec == NULL) return;
    /* Release: the monitor thread's read of this value (acquire) must see
     * every write this carrier made before entering the blocking call --
     * not load-bearing for correctness today (the monitor only reads the
     * timestamp), but cheap to get right now rather than leave a relaxed
     * atomic here for the next reader to have to re-derive why it is safe. */
    atomic_store_explicit(&rt_blocking_rec->blocking_since_ns, rt_monotonic_ns(),
                           memory_order_release);
}

void rt_exit_blocking(void) {
    if (rt_blocking_rec == NULL) return;
    atomic_store_explicit(&rt_blocking_rec->blocking_since_ns, 0,
                           memory_order_release);
}

/* ---- io primitives: lib/io.m31, lib/fs.m31 ----------------------------- */
/* Each is ONE sys-layer call with the layer's convention passed straight
 * through: a non-negative value, or -errno in Linux numbering. Nothing here
 * loops, buffers, retries on EINTR, splits lines or decides what an errno
 * means -- that is all language source in lib/io.m31 and lib/fs.m31. What
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


/* ---- process primitives: lib/os.m31, lib/date.m31, lib/random.m31 ------ */
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
 * what a non-UTF-8 argument means (lib/os.m31). */
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

/* Every variable, as alternating name and value octets. `environ` is the
 * only way to enumerate them: getenv answers one name and the C standard
 * offers nothing else. An entry with no `=` is not a variable and is
 * skipped, as is one with an empty name, which matches what rt_env refuses
 * to look up. */
extern char **environ;

void rt_env_map(Obj *out) {
    for (char **e = environ; e != NULL && *e != NULL; e++) {
        const char *eq = strchr(*e, '=');
        if (eq == NULL || eq == *e) continue;
        int64_t nlen = (int64_t)(eq - *e);
        rt_list_push(out, (int64_t)(intptr_t)bytes_of((const uint8_t *)*e, nlen));
        rt_list_push(
            out, (int64_t)(intptr_t)bytes_of((const uint8_t *)(eq + 1), (int64_t)strlen(eq + 1)));
    }
}

/* exit(), not _exit(): stdio is flushed and atexit handlers run, which is
 * how the -DRC_DEBUG report still appears. The range check (0..255) is in
 * lib/os.m31, where the library can say what was wrong. */
_Noreturn void rt_exit(int64_t code) {
    rt_out_flush();
    exit((int)code);
}

/* Starts `argv` as a child process (lib/os.m31's os.run) -- not to be
 * confused with `spawn`, a green thread inside this process (rt_spawn,
 * above); sys.h's "process launch" section says why the two stay apart by
 * name.
 *
 * `argv` is built into a malloc'd, NUL-terminated char* array here rather
 * than read element by element inside the layer, for the reason rt_poll's
 * comment gives for its SysPollFd array: the layer itself may not allocate,
 * and a fixed bound would put a ceiling on how long a command line a
 * program may launch. Every element must already be a NUL-free run of
 * bytes -- Str.data is a C string (str_new), so the only way it could hold
 * an embedded NUL is a slice built from one, and threading a truncated
 * argument through to execve would run a different command than the one
 * asked for, so that is refused rather than silently cut short, the same
 * choice path_ok makes for a path.
 *
 * `environ` is read HERE, not in the layer: sys.h's comment on
 * sys_proc_start explains why the raw backend cannot reach for it itself
 * (runtime/sys_test.c's freestanding link would fail), so "the child
 * inherits this process's environment" is this wrapper's decision, made by
 * passing the same `environ` os.env_map already reads (above) straight
 * through.
 *
 * `rt_out_flush()` first, for the reason rt_trap and rt_exit already call it
 * before touching a descriptor directly: `print`'s buffer is this process's
 * own memory, not yet written to fd 1, and a child that inherits fd 1
 * writes to it directly and immediately once it execs. Skipping this made
 * a child's output appear BEFORE everything this program had already
 * printed but not yet flushed -- every line out of order -- which is
 * exactly the bug docs/sys-layer.md §8 added `__out_flush` to prevent for
 * `io`, one descriptor earlier. */
int64_t rt_proc_start(Obj *argv_list) {
    rt_out_flush();
    int64_t n = rt_len_of(argv_list);
    if (n <= 0) return -SYS_EINVAL;
    char **argv = malloc((size_t)(n + 1) * sizeof *argv);
    if (argv == NULL) return -SYS_ENOMEM;
    const int64_t *elems = slots(argv_list);
    for (int64_t i = 0; i < n; i++) {
        Str *s = (Str *)(intptr_t)elems[i];
        if (memchr(s->data, '\0', (size_t)s->len) != NULL) {
            free(argv);
            return -SYS_EINVAL;
        }
        argv[i] = (char *)s->data;
    }
    argv[n] = NULL;
    int64_t r = sys_proc_start(argv, environ);
    free(argv);
    return r;
}

/* Waits for the process `pid` -- one this process's own rt_proc_start
 * returned -- and reports how it ended, in sys.h's own encoding (see
 * sys_proc_wait's comment there): 0..255 is an exit code, 256 and up is
 * SYS_WAIT_SIGNAL_BASE plus the signal that killed it. lib/os.m31 decodes
 * this into `ExitStatus`, a real enum a `match` can be exhaustive over,
 * rather than a bare int a caller has to remember the encoding of. */
int64_t rt_proc_wait(int64_t pid) {
    return sys_proc_wait(pid);
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
    rt_term_restore();  /* as rt_trap does, and for the same reason */
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


/* ---- net primitives: lib/net.m31 --------------------------------------- */
/* The socket seam, designed in docs/sys-layer.md §9 and built to that list.
 * Like the io primitives above, each one is ONE sys-layer call with the
 * layer's convention passed straight through -- a value, or -errno in Linux
 * numbering -- and none of them decides anything. The address parser and
 * printer, the retry loops, SO_REUSEADDR's default, what an errno means and
 * the whole of `Conn` and `Listener` are language source in lib/net.m31.
 *
 * There are no primitives here for reading, writing or closing a socket: a
 * socket is a descriptor, so io's `__read`, `__write` and `__close` (§8)
 * already work on one. That is most of the reason sys.h hands back
 * descriptors rather than a socket handle of its own.
 *
 * What is left in C, and why it has to be:
 *
 *   - **An address comes apart at the seam.** A prim returns a scalar or a
 *     str, or pushes onto a collection it was handed (docs/stdlib-seam.md
 *     §2), so a SysAddr cannot cross whole. Family and port are ints in and
 *     pushed out; the 16 address bytes are written in place into a `bytes`
 *     the caller owns, under §2's amendment. `addr.size() >= 16` is checked
 *     HERE and never trusted from the caller, exactly as __read's range is:
 *     a shorter buffer would be written past. It traps rather than failing,
 *     because it is a bug in the library and not a condition in the world.
 *   - **__poll builds the kernel's array.** A SysPollFd is a 32-bit fd and
 *     two 16-bit masks, which the language cannot hold and the layer may not
 *     allocate. So the array is built here from two parallel List<int>s and
 *     freed here, and the language never sees a narrow integer.
 *   - **A path becomes a C string**, refusing an empty one or one holding a
 *     NUL, for the reason path_ok gives above.
 *
 * SYS_AF_UNIX is reachable through __bind_path, __connect_path and
 * __sockpath although lib/net.m31 is TCP only. That trio is what keeps a
 * path OUT of __bind's argument list, which is the design §9 settled on, and
 * building the runtime half of the set now is what lets the module that
 * wants Unix sockets be language source alone. */

/* The 16 address bytes a caller gets back. Never trusted: the kernel writes
 * all 16 of SysAddr.addr whatever the family. */
static unsigned char *addr16(Obj *addr, const char *who) {
    rt_check_mutable(addr);
    Bytes *b = (Bytes *)addr;
    if (b->len < 16) rt_trap(who);
    return b->data;
}

/* The same, read-only: a caller only handing an address over may pass a
 * frozen `bytes`, so this one does not ask for mutability. */
static const unsigned char *addr16_in(Obj *addr, const char *who) {
    Bytes *b = (Bytes *)addr;
    if (b->len < 16) rt_trap(who);
    return b->data;
}

/* An IP address as the layer's record. The port is a plain integer and the
 * backends do the htons (sys.h), so nothing here byte-swaps. */
static void to_sys_addr(SysAddr *a, int64_t family, int64_t port,
                        const unsigned char *bytes16) {
    memset(a, 0, sizeof *a);
    a->family = family;
    a->port = port;
    memcpy(a->addr, bytes16, 16);
}

/* And back: family and port pushed, the 16 bytes written in place. Called
 * only after a call that succeeded, so `out` grows by exactly two. */
static void from_sys_addr(const SysAddr *a, Obj *out, unsigned char *bytes16) {
    rt_list_push(out, a->family);
    rt_list_push(out, a->port);
    memcpy(bytes16, a->addr, 16);
}

/* A SysAddr naming a file system path, for the AF_UNIX pair. A path too long
 * for the layer's own field is refused here rather than truncated, because a
 * truncated path names a different socket; the backends check it against the
 * HOST's sun_path too, which is shorter on macOS. */
static int64_t to_unix_addr(SysAddr *a, Obj *path) {
    if (!path_ok(path)) return -SYS_EINVAL;
    Str *p = (Str *)path;
    if ((size_t)p->len + 1 > sizeof a->path) return -SYS_ENAMETOOLONG;
    memset(a, 0, sizeof *a);
    a->family = SYS_AF_UNIX;
    memcpy(a->path, p->data, (size_t)p->len + 1);
    return 0;
}

int64_t rt_socket(int64_t domain, int64_t type, int64_t protocol) {
    return sys_socket(domain, type, protocol);
}

int64_t rt_listen(int64_t fd, int64_t backlog) {
    return sys_listen(fd, backlog);
}

int64_t rt_shutdown(int64_t fd, int64_t how) {
    return sys_shutdown(fd, how);
}

int64_t rt_bind(int64_t fd, int64_t family, int64_t port, Obj *addr) {
    SysAddr a;
    to_sys_addr(&a, family, port, addr16_in(addr, "__bind: the address needs 16 bytes"));
    return sys_bind(fd, &a);
}

int64_t rt_connect(int64_t fd, int64_t family, int64_t port, Obj *addr) {
    SysAddr a;
    to_sys_addr(&a, family, port, addr16_in(addr, "__connect: the address needs 16 bytes"));
    return sys_connect(fd, &a);
}

int64_t rt_bind_path(int64_t fd, Obj *path) {
    SysAddr a;
    int64_t r = to_unix_addr(&a, path);
    if (r < 0) return r;
    return sys_bind(fd, &a);
}

int64_t rt_connect_path(int64_t fd, Obj *path) {
    SysAddr a;
    int64_t r = to_unix_addr(&a, path);
    if (r < 0) return r;
    return sys_connect(fd, &a);
}

/* The new descriptor, and the peer's address split across `out` and `addr`.
 * Nothing is pushed when the accept fails, so the result alone tells the two
 * apart and no caller ever reads a half-filled list. */
int64_t rt_accept(int64_t fd, Obj *out, Obj *addr) {
    unsigned char *p = addr16(addr, "__accept: the address needs 16 bytes");
    SysAddr peer;
    int64_t c = sys_accept(fd, &peer);
    if (c < 0) return c;
    from_sys_addr(&peer, out, p);
    return c;
}

/* This end's address, or the far end's when `peer` is non-zero. One
 * primitive and not two, because they differ by a single kernel call and a
 * caller always knows statically which it wants -- the trade sys_stat makes
 * with its `follow`. */
int64_t rt_sockname(int64_t fd, int64_t peer, Obj *out, Obj *addr) {
    unsigned char *p = addr16(addr, "__sockname: the address needs 16 bytes");
    SysAddr a;
    int64_t r = peer != 0 ? sys_getpeername(fd, &a) : sys_getsockname(fd, &a);
    if (r < 0) return r;
    from_sys_addr(&a, out, p);
    return 0;
}

/* The AF_UNIX path, as a str, and "" for anything without one -- an unbound
 * socket, an IP socket, or a failed call. A second call rather than a fifth
 * thing __sockname could push, because a prim returns a scalar OR a str and
 * cannot do both (docs/stdlib-seam.md §2), and because charging every TCP
 * accept for a str it will not read would be the worse trade.
 *
 * It cannot report why it failed, which is the price of returning a str.
 * That is affordable precisely because a caller has already made the
 * __sockname call that would have said; this one only fetches text. */
Obj *rt_sockpath(int64_t fd, int64_t peer) {
    SysAddr a;
    int64_t r = peer != 0 ? sys_getpeername(fd, &a) : sys_getsockname(fd, &a);
    if (r < 0 || a.family != SYS_AF_UNIX) return str_new("", 0);
    /* Not strlen: `path` is the last member of SysAddr, so a kernel that
     * filled every byte would send strlen off the end of the struct. */
    size_t n = 0;
    while (n < sizeof a.path && a.path[n] != 0) n++;
    return str_new(a.path, (int64_t)n);
}

int64_t rt_setsockopt(int64_t fd, int64_t opt, int64_t value) {
    return sys_setsockopt(fd, opt, value);
}

int64_t rt_getsockopt(int64_t fd, int64_t opt) {
    return sys_getsockopt(fd, opt);
}

/* Wait until one of several descriptors is ready.
 *
 * `fds` and `events` are read and must be the same length; `revents` is
 * cleared and given one value per descriptor, because its length is the
 * answer's and not the caller's. Different lengths trap: the three lists are
 * one table written three ways, and a caller that got them out of step has a
 * bug the kernel cannot see.
 *
 * The array is malloc'd rather than taken from a fixed buffer, so that
 * nothing here puts a ceiling on how many descriptors a server may wait on.
 * The layer itself may not allocate, which is exactly why this is the
 * wrapper's job (sys.h, sys_poll). */
int64_t rt_poll(Obj *fds, Obj *events, Obj *revents, int64_t timeout_ms) {
    int64_t n = rt_len_of(fds);
    if (rt_len_of(events) != n) rt_trap("__poll: fds and events are different lengths");
    rt_list_clear(revents, false);
    if (n == 0) return sys_poll(NULL, 0, timeout_ms);
    if ((uint64_t)n > (uint64_t)SIZE_MAX / sizeof(SysPollFd)) return -SYS_EINVAL;
    SysPollFd *pf = malloc((size_t)n * sizeof *pf);
    if (pf == NULL) return -SYS_ENOMEM;
    const int64_t *f = slots(fds);
    const int64_t *e = slots(events);
    for (int64_t i = 0; i < n; i++) {
        /* A descriptor wider than the kernel's 32-bit field, or a mask wider
         * than its 16-bit one, would be cut down to something that names a
         * different descriptor or waits for a different event. Refused
         * whole rather than truncated. */
        if (f[i] < INT32_MIN || f[i] > INT32_MAX || e[i] < 0 || e[i] > INT16_MAX) {
            free(pf);
            return -SYS_EINVAL;
        }
        pf[i].fd = (int32_t)f[i];
        pf[i].events = (int16_t)e[i];
        pf[i].revents = 0;
    }
    int64_t r = sys_poll(pf, n, timeout_ms);
    /* Every entry's mask comes back, the zeros included: sys_poll fills them
     * all, and a caller reading the table needs each row to line up with the
     * row it wrote. */
    for (int64_t i = 0; i < n; i++) rt_list_push(revents, pf[i].revents);
    free(pf);
    return r;
}

/* Every address a name has, as far as `addrs` holds them.
 *
 * `addrs` is 16 bytes per address and its size says how many fit; the result
 * is how many the name HAS, so a caller whose buffer was too small grows it
 * and asks again, exactly as __listdir does. Family and port are pushed onto
 * `out`, two per address written.
 *
 * This is the ONE primitive whose answer depends on the backend: the raw one
 * returns -SYS_ENOSYS always, because resolving a name is getaddrinfo and
 * getaddrinfo is glibc's NSS, which dlopens libnss_* at run time
 * (runtime/sys_linux.c). lib/net.m31 treats that as an ordinary error on
 * every path that uses a name, which is the right shape anyway. */
int64_t rt_resolve(Obj *host, int64_t port, int64_t family, Obj *out, Obj *addrs) {
    if (!path_ok(host)) return -SYS_EINVAL;
    rt_check_mutable(addrs);
    Bytes *b = (Bytes *)addrs;
    int64_t cap = b->len / 16;
    if (cap < 1) rt_trap("__resolve: the address buffer needs 16 bytes per address");
    if ((uint64_t)cap > (uint64_t)SIZE_MAX / sizeof(SysAddr)) return -SYS_EINVAL;
    SysAddr *got = malloc((size_t)cap * sizeof *got);
    if (got == NULL) return -SYS_ENOMEM;
    int64_t found = sys_resolve(((Str *)host)->data, port, family, got, cap);
    if (found > 0) {
        int64_t k = found < cap ? found : cap;
        for (int64_t i = 0; i < k; i++) from_sys_addr(&got[i], out, b->data + i * 16);
    }
    free(got);
    return found;
}

/* Set SIGPIPE to ignore, so that a write to a socket whose peer has gone
 * away comes back as -EPIPE instead of killing the process (sys.h).
 *
 * lib/net.m31 calls it every time it opens a socket rather than once at
 * startup: a module cannot hold the "already done" flag
 * (docs/module-state-decision.md), the disposition is per process and
 * idempotent, and one system call per socket is nothing beside the connect
 * or the accept next to it. */
int64_t rt_ignore_sigpipe(void) {
    return sys_ignore_sigpipe();
}

/* The seam lib/net.src uses to wait for a non-blocking socket instead of
 * blocking the carrier OS thread on the syscall itself (docs/
 * concurrency-decision.md's whole point for Phase 3's reactor). `events` is
 * RT_REACTOR_READ/RT_REACTOR_WRITE (runtime/reactor.h), OR'd -- lib/net.src
 * names its own constants with the same values (IO_READABLE/IO_WRITABLE)
 * rather than reusing POLL_IN/POLL_OUT, which are a different, unrelated
 * bit encoding (the kernel's poll(2) bits) that this primitive has nothing
 * to do with.
 *
 * Unlike `__poll`, this can never itself fail with a value the caller must
 * check: it parks the calling green thread (rt_reactor_wait ->
 * rt_sched_park) until `fd` is ready, and traps only for a genuine caller
 * bug (an invalid `fd`, or calling this from outside a green thread) --
 * which is exactly why lib/net.src uses it only for the UNBOUNDED wait (no
 * read/write timeout set): there is no way for it to report "gave up after
 * N ms", which is what makes it wrong for the bounded case. The bounded
 * case keeps using `__poll` with a timeout, exactly as it already did
 * (`net.Conn.fill`/`write`/`write_some`, lib/net.src) -- see that file for
 * the reasoning this split is built on.
 *
 * Always 0: there is nothing to report back; the fd is ready when this
 * returns, full stop. A prim still needs a return type, and `int` rather
 * than `void` keeps it uniform with every other primitive in this module.
 *
 * THE rt_enter_blocking/rt_exit_blocking PAIR THIS FUNCTION UNDOES AND
 * RE-ARMS INTERNALLY, AND WHY -- a real, reproducible bug found and fixed
 * while building this, serious enough to document in full:
 *
 * src/emit_c.rs wraps EVERY `prim` call site in rt_enter_blocking()/
 * rt_exit_blocking() unconditionally (docs/concurrency-decision.md,
 * "Blocking FFI") -- `__wait_io` is a `prim` like any other, so the
 * compiler emits `rt_enter_blocking(); v = rt_wait_io(...); rt_exit_
 * blocking();` at every call site, with no way yet to mark one primitive
 * as different (the compiler's own warning about this says so). That
 * wrapping is correct for a genuine blocking syscall: it marks the calling
 * carrier as stuck, so the blocking-FFI monitor (scheduler.c) can rescue
 * the OTHER green threads queued on that same carrier by moving them to
 * another one. But the monitor's safety argument for doing that
 * (scheduler.h, "Blocking-FFI handoff") rests entirely on one invariant:
 * the carrier is PROVABLY stuck, meaning its own OS thread is not
 * concurrently touching its own local buffer at all for as long as
 * blocking_since_ns stays set. That is true for a real blocking syscall
 * (the OS thread is inside the kernel, doing nothing else) and FALSE here:
 * rt_reactor_wait parks the GREEN THREAD via rt_sched_park, which
 * immediately switches the CARRIER back to its own ordinary dispatch loop
 * -- carrier_main, drawing and dispatching from `c->local` with no lock,
 * BY DESIGN, because nothing is supposed to be touching it concurrently.
 * The carrier is genuinely free and busy with other work for the whole
 * real-world duration of the park (which can be long -- waiting on a slow
 * peer), while the compiler's own rt_enter_blocking(), made just before
 * entering this function, has already told the monitor the opposite.
 *
 * Worse: the matching rt_exit_blocking() the compiler emits AFTER this
 * call returns runs on WHICHEVER carrier the green thread happens to
 * RESUME on -- which rt_sched_park's own contract explicitly allows to be
 * a DIFFERENT carrier than the one that called rt_enter_blocking
 * (scheduler.h: "a parked thread CAN resume on a different carrier than
 * before"). So the ORIGINAL carrier's blocking_since_ns can be left set
 * FOREVER: nothing on that carrier's own thread ever clears it, because
 * the green thread that would have cleared it is not coming back there.
 * That carrier then looks permanently stuck to the monitor, which
 * repeatedly calls try_handoff on it -- concurrently mutating
 * `c->local`/`local_head`/`local_len` under `c->local_lock` WHILE that
 * carrier's own OS thread is ALSO concurrently reading and writing the
 * exact same fields with no lock in its ordinary carrier_main loop,
 * because nothing was ever supposed to be racing it there.
 *
 * That is a genuine, unsynchronized data race on a carrier's own run
 * queue -- found directly, empirically, not merely suspected: reproducible
 * well over half the time with as few as 2 carriers and ~10 concurrent
 * real socket connections, manifesting as green threads dispatched through
 * a corrupted `rt_green_t *` and promptly hitting the stack probe at a
 * laughably shallow call depth (confirmed with gdb: several carriers
 * simultaneously faulting inside `Conn.close`, 3-4 frames into a fresh
 * green thread, with rt_stack_limit and the stack pointer both looking
 * individually sane). Never reachable through `Chan`: `send`/`recv` are
 * compiler intrinsics, not `prim` calls, so they never touch
 * rt_enter_blocking/rt_exit_blocking at all, which is also why corpus/
 * core's 1303/1304 (hundreds of green threads contending on a `Chan`) never
 * hit this, while real concurrent socket I/O did.
 *
 * THE FIX, entirely local to this function, not touching scheduler.c:
 * immediately undo the compiler's own rt_enter_blocking() on entry (this
 * call is not a real blocking syscall, so the carrier should never have
 * been marked stuck for it), and re-arm it immediately before returning,
 * so the compiler's own trailing rt_exit_blocking() has a freshly-set,
 * correctly-paired, near-instantaneous window to clear -- on whichever
 * carrier this green thread actually resumes on, which is exactly where
 * that bracket belongs. rt_enter_blocking/rt_exit_blocking are plain,
 * idempotent, counter-free timestamp sets/clears (see their own
 * definitions above this function's net-primitives section) -- calling
 * them an extra, nested time each is explicitly safe by their own
 * contract, not a hack. */
int64_t rt_wait_io(int64_t fd, int64_t events) {
    /* See this function's own doc comment above for the full account of
     * why this exit/enter pair is here: it is not optional and not a
     * stylistic choice. In short: the compiler already wrapped this call
     * in rt_enter_blocking()/rt_exit_blocking() (it is a `prim`), which is
     * wrong for THIS primitive specifically -- it parks the green thread
     * rather than genuinely blocking the carrier, so the carrier must not
     * be reported stuck for the park's duration, or the blocking-FFI
     * monitor can race the carrier's own unsynchronized local-buffer
     * access (a real, reproduced bug: see the comment above). */
    rt_exit_blocking();
    rt_reactor_wait(rt_global_reactor(), (int)fd, (uint32_t)events);
    rt_enter_blocking();
    return 0;
}
/* ---- end net primitives ------------------------------------------------ */


/* ---- terminal primitives: lib/term.m31 ---------------------------------
 *
 * Each of the first four is one sys-layer call with its value or -errno
 * passed straight through. What raw mode IS -- which flags to clear, what
 * VMIN and VTIME should be, what to do when the terminal is not one -- is
 * lib/term.m31, in language source, over the layer's own flag constants.
 *
 * Only six of a SysTermios's fields cross the seam, because a prim deals in
 * scalars and in what it pushes onto a collection it was handed: the four
 * flag words, and the two control characters a non-canonical read is steered
 * by. The other seventeen control characters -- the interrupt, quit, erase
 * and kill keys -- never reach the language, so rt_tcset READS the current
 * settings and patches those six into them. Without that, entering raw mode
 * would quietly set ^C, ^Z and ^H to NUL for the program that turned ISIG
 * back on later.
 *
 * sys_tcset reads first as well, for the one or two fields SysTermios itself
 * does not model (sys.h). The two reads are not the same read and neither
 * covers the other: that one keeps the line discipline and the speeds, this
 * one keeps everything the SEAM drops. Two ioctls per change of mode, which
 * happens twice in a program's life.
 *
 * `cflag` crosses as an opaque number. Nothing in lib/term.m31 looks at it;
 * it is carried out and back so that a restore puts back the control-mode
 * word it found, and so that a set does not have to invent one. */

int64_t rt_isatty(int64_t fd) {
    return sys_isatty(fd);
}

int64_t rt_tcget(int64_t fd, Obj *out) {
    SysTermios t;
    memset(&t, 0, sizeof t);
    int64_t r = sys_tcget(fd, &t);
    if (r < 0) return r;
    rt_list_push(out, t.iflag);
    rt_list_push(out, t.oflag);
    rt_list_push(out, t.cflag);
    rt_list_push(out, t.lflag);
    rt_list_push(out, t.cc[SYS_VMIN]);
    rt_list_push(out, t.cc[SYS_VTIME]);
    return 0;
}

int64_t rt_tcset(int64_t fd, int64_t iflag, int64_t oflag, int64_t cflag, int64_t lflag,
                 int64_t vmin, int64_t vtime) {
    /* A control character is one octet. The library computes these, so a
     * value outside the range is a bug in it, not in the world. */
    if (vmin < 0 || vmin > 255 || vtime < 0 || vtime > 255) {
        rt_trap("__tcset: VMIN and VTIME are single bytes");
    }
    SysTermios t;
    memset(&t, 0, sizeof t);
    int64_t r = sys_tcget(fd, &t);
    if (r < 0) return r;
    t.iflag = iflag;
    t.oflag = oflag;
    t.cflag = cflag;
    t.lflag = lflag;
    t.cc[SYS_VMIN] = (unsigned char)vmin;
    t.cc[SYS_VTIME] = (unsigned char)vtime;
    return sys_tcset(fd, &t);
}

int64_t rt_winsize(int64_t fd, Obj *out) {
    int64_t rows = 0, cols = 0;
    int64_t r = sys_winsize(fd, &rows, &cols);
    if (r < 0) return r;
    rt_list_push(out, rows);
    rt_list_push(out, cols);
    return 0;
}

/* ---- putting the terminal back when nothing else will ------------------
 *
 * `term.Session`'s destructor restores the terminal on every path the
 * language can see: the end of a scope, a `return`, a `?`, an assignment
 * over the last reference. Two paths it cannot see are the ones that matter
 * most to somebody sitting at a keyboard:
 *
 *   - a TRAP -- an index out of range, an overflow, `trap(msg)` -- which
 *     aborts and runs no destructor (reference §4.4);
 *   - `os.exit(code)`, which abandons every live object by design.
 *
 * Both would leave a terminal with no echo and no line editing, and the only
 * way out of that is to type `reset` blind. So the runtime keeps ONE
 * snapshot, taken when the session is armed, and puts it back on both paths.
 *
 * This is deliberately not a signal facility and does not need one. It is a
 * plain function called from rt_trap, rt_panic and an atexit handler -- code
 * running normally on the thread that is ending the process, free to make a
 * system call and to take a lock. A signal handler could cover more (a
 * SIGTERM or a SIGHUP from outside, a SIGSEGV) and can have none of those
 * freedoms; docs/sys-layer.md §2 says what it would take and why it is not
 * here. What is covered is what the language itself can cause, which is the
 * part a program should not be able to break.
 *
 * One snapshot, not a stack: a process has one terminal, and two sessions on
 * one descriptor is a bug in the program rather than a case to support.
 * Arming twice keeps the FIRST snapshot, which is the one that describes the
 * terminal as the program found it. */

static pthread_mutex_t term_lock = PTHREAD_MUTEX_INITIALIZER;
static int64_t term_fd = -1;
static SysTermios term_saved;

int64_t rt_term_arm(int64_t fd) {
    SysTermios t;
    memset(&t, 0, sizeof t);
    int64_t r = sys_tcget(fd, &t);
    if (r < 0) return r;
    pthread_mutex_lock(&term_lock);
    if (term_fd < 0) {
        term_fd = fd;
        term_saved = t;
    }
    pthread_mutex_unlock(&term_lock);
    return 0;
}

int64_t rt_term_disarm(void) {
    pthread_mutex_lock(&term_lock);
    term_fd = -1;
    pthread_mutex_unlock(&term_lock);
    return 0;
}

/* Idempotent, and silent about failure: every caller is already on its way
 * out and has nowhere to report to. A descriptor closed since it was armed
 * gives -EBADF, which is exactly the case where there is nothing to put
 * back. */
void rt_term_restore(void) {
    pthread_mutex_lock(&term_lock);
    int64_t fd = term_fd;
    SysTermios t = term_saved;
    term_fd = -1;
    pthread_mutex_unlock(&term_lock);
    if (fd >= 0) sys_tcset(fd, &t);
}
/* ---- end terminal primitives ------------------------------------------- */
