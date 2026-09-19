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
 *
 * If LTO ever becomes worth having, the alternative is to drop immortality
 * and heap-allocate literals once at startup, so no static Obj exists.
 */
#include "rt.h"
#include "rc_debug.h"

#include <inttypes.h>
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
        RC_TRACK_FREE();
        free(o);
    }
}

int64_t rt_len(Obj *o) {
    return o->len;
}

Obj *rt_concat(Obj *a, Obj *b) {
    int64_t n;
    if (__builtin_add_overflow(a->len, b->len, &n)) rt_trap("string too long");

    /* One block: header, then bytes, then a NUL so the data is also a valid
     * C string for printing. */
    Obj *o = malloc(sizeof(Obj) + (size_t)n + 1);
    if (o == NULL) rt_trap("out of memory");

    char *buf = (char *)(o + 1);
    memcpy(buf, a->data, (size_t)a->len);
    memcpy(buf + a->len, b->data, (size_t)b->len);
    buf[n] = '\0';

    o->rc = 1;
    o->len = n;
    o->data = buf;
    RC_TRACK_ALLOC();
    return o;
}

void rt_print(int64_t v) {
    printf("%" PRId64 "\n", v);
}

void rt_print_bool(bool v) {
    puts(v ? "true" : "false");
}

void rt_print_str(Obj *o) {
    /* fwrite rather than puts: the string may contain NUL bytes, and len is
     * authoritative. */
    fwrite(o->data, 1, (size_t)o->len, stdout);
    putchar('\n');
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
