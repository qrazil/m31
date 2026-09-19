/* Runtime implementation.
 *
 * Compiled separately from emitted code and linked. Do NOT make these
 * static inline in rt.h, and do NOT build with -flto: either lets the C
 * compiler see a free() whose argument is a static string literal object,
 * and it warns (correctly, as far as it can tell) with
 * -Wfree-nonheap-object. The immortality guard makes that path unreachable
 * at runtime, but the compiler cannot prove it. Keeping the runtime in its
 * own translation unit is what hides the static objects from this analysis.
 *
 * Measured 2026-09-18: separate TU is clean under gcc -O0 and -O2 with
 * -Wall -Wextra; -flto reintroduces the warning.
 *
 * If LTO ever becomes worth having, the alternative is to drop immortality
 * and heap-allocate literals once at startup, so no static Obj exists.
 */
#include "rt.h"
#include "rc_debug.h"

#include <stdio.h>
#include <stdlib.h>

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

void rt_print(int64_t v) {
    printf("%lld\n", (long long)v);
}
