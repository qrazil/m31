/* Runtime interface.
 *
 * This header is included by emitted code. The implementations live in
 * rt.c and are compiled as a SEPARATE translation unit — see docs/ir-v0.md
 * §7.1. That separation is load-bearing, not stylistic.
 */
#ifndef RT_H
#define RT_H

#include <stdint.h>

#define RC_IMMORTAL (-1)

typedef struct Obj {
    long        rc;
    int64_t     len;
    const char *data;
} Obj;

void    rc_inc(Obj *o);
void    rc_dec(Obj *o);
int64_t rt_len(Obj *o);
void    rt_print(int64_t v);

/* Aborts with "trap: <msg>" on stderr and exit status 134 (SIGABRT).
 * Out-of-line and _Noreturn so the overflow checks below stay cheap: the
 * compiler treats the trap edge as cold and keeps the hot path straight. */
_Noreturn void rt_trap(const char *msg);

/* Checked arithmetic. Int is i64 and overflow TRAPS -- see docs/ir-v0.md §3.
 *
 * These are inline on purpose, unlike rc_dec: arithmetic is the hottest
 * thing in the language and a call per add would be indefensible. The
 * separate-TU rule exists only to keep free() away from static literal
 * objects; it does not apply here.
 *
 * __builtin_*_overflow is gcc 5+ and clang 3.8+. Verified 2026-09-18:
 * identical results and identical trap behaviour under gcc and clang at
 * -O0 and -O2, no warnings. */
static inline int64_t rt_iadd(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_add_overflow(a, b, &r)) rt_trap("integer overflow in +");
    return r;
}

static inline int64_t rt_isub(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_sub_overflow(a, b, &r)) rt_trap("integer overflow in -");
    return r;
}

static inline int64_t rt_imul(int64_t a, int64_t b) {
    int64_t r;
    if (__builtin_mul_overflow(a, b, &r)) rt_trap("integer overflow in *");
    return r;
}

#endif /* RT_H */
