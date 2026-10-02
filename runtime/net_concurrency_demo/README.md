# net concurrency demo -- NOT part of the ordinary corpus, on purpose

`main.src` spawns a server accept loop, 200 handler green threads and 200
client green threads, all doing real TCP I/O through `lib/net.src` at once,
to demonstrate that real concurrent socket I/O does not serialize on a
carrier (docs/concurrency-decision.md, "Phase 3.5"). It is correct, and when
it works it is a strong demonstration of the integration this task built.

**It is not in `corpus/modules/` because it is not reliable there yet.**
While building it, a real, TSan-confirmed data race was found in the
pre-existing Phase 1/2 fiber-switching machinery (`rt_stack_limit`, written
in `rt_fiber_switch` by one carrier OS thread and read by a green thread's
own compiler-emitted stack probe, racing across carriers) -- NOT something
introduced by this task's own new code (`rt_spawn`, `Chan`, `rt_wait_io`),
but a latent bug in code this task was scoped to reuse as-is, newly exposed
because this is the first time anything has driven genuinely concurrent
real socket I/O through multiple real carriers at once. See the task's own
report (and `runtime/greenthread.h`'s `RT_STACK_SIZE` comment) for the full
account, including why it was not fixed here.

**Practical effect**: this program is correct in design but intermittently
crashes (a spurious "stack overflow" trap) when run with more than one
carrier -- reproduced directly, well over half the time, with as few as 2
carriers and ~10 real concurrent connections. It passes reliably with
`LANG_NUM_CARRIERS=1`, which removes genuine parallelism between carriers
entirely (so it also stops proving the thing it exists to prove, until the
race above is fixed).

## How to run it

```
LANG_NUM_CARRIERS=1 ./build.sh runtime/net_concurrency_demo/main.src -o /tmp/demo && /tmp/demo
```

prints `200` (every connection succeeded) reliably. Running it without
`LANG_NUM_CARRIERS=1` is how `runtime/spawn_wiring_tsan.sh` reproduces and
documents the race under ThreadSanitizer -- see that script.
