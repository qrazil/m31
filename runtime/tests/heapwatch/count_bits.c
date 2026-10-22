/* Heap watch's fields in the count word (runtime/rt.h): a retain and a
 * release change the count and nothing beside it -- not RC_HW_NOTE, the
 * EPOCH field or RC_HW_SAMPLED, nor a flag -- and so does what a
 * destructor's code sets it with. Built with -DRT_HEAPWATCH and run by
 * runtime/tests/heapwatch_test.sh; prints "ok", or what broke. */
#include "rt.h"

#include <stdio.h>
#include <stdlib.h>

int main(void) {
    Obj o;
    long high = RC_HW_NOTE | (40L << RC_EPOCH_SHIFT) | RC_HW_SAMPLED | RC_FROZEN;
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
        printf("FAILED: the widest count\n");
        return 1;
    }
    printf("ok\n");
    return 0;
}
