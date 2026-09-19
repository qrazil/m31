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

#endif /* RT_H */
