/* Heap watch's split of the count word (runtime/rt.h): a retain and a
 * release change the count and nothing beside it, and a retain past the
 * 33-bit count traps instead of carrying into the birth minute. Built and
 * run by runtime/tests/heapwatch_test.sh; exits 0 or prints what broke,
 * then traps on purpose at the end, which the script expects. */
#include "rt.h"

#include <stdio.h>
#include <stdlib.h>

int main(void) {
    Obj o;
    long high = (12345L << RC_BIRTH_SHIFT) | (40L << RC_EPOCH_SHIFT) | RC_FROZEN;
    o.rc = high | 7;
    o.ty = &rt_str_type;
    rc_inc(&o);
    rc_dec(&o);
    rc_dec(&o);
    if ((o.rc & ~RC_COUNT) != high || rt_rc_count(&o) != 6) {
        printf("FAILED: a retain and two releases left %lx\n", (unsigned long)o.rc);
        return 1;
    }
    rt_rc_set_count(&o, 1);
    if ((o.rc & ~RC_COUNT) != high || rt_rc_count(&o) != 1) {
        printf("FAILED: rt_rc_set_count changed the fields beside the count\n");
        return 1;
    }
    o.rc = high | (RC_COUNT - 1);
    rc_inc(&o);
    if ((o.rc & ~RC_COUNT) != high || rt_rc_count(&o) != RC_COUNT) {
        printf("FAILED: the last count before the limit\n");
        return 1;
    }
    printf("ok\n");
    fflush(stdout);
    rc_inc(&o); /* traps */
    printf("FAILED: a retain past the count's width did not trap\n");
    return 1;
}
