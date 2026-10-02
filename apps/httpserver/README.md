# `httpserver` -- a "Hello, World!" HTTP/1.1 server

The first real network *server* application written in this language: built
on `lib/http.src`'s real `Handler`/`serve_conn` machinery (request/response
parsing and writing, framing, keep-alive -- nothing hand-rolled), to exercise
and measure what tonight's green-thread concurrency fix actually bought.
Every accepted connection gets its own green thread and every request gets
the same fixed reply: `200 OK`, `Content-Type: text/plain; charset=utf-8`,
a correct `Content-Length`, body `Hello, World!\n`.

    cargo build                        # the compiler
    bash apps/httpserver/build.sh      # ./apps/httpserver/httpserver
    bash apps/httpserver/test.sh       # the tests

    httpserver                         listen on 0.0.0.0:8080
    httpserver --port 9000             listen on 0.0.0.0:9000
    httpserver --host 127.0.0.1        listen on a specific address only
    httpserver --help

Port and host are command-line flags, not environment variables: this
language's standard library has no `getenv` exposed to a `.src` program
today (`prim`s cover sockets, files and the clock, not the process
environment), and `lib/args` already gives a `--port`/`--host` pair the usual
shell idioms (`httpserver --port "${PORT:-8080}"`) work with anyway.

## Why this is not just `http.serve(ln, hello)`

`http.serve`'s own doc comment ("One connection at a time, and why") says
plainly that it serves one connection to completion before accepting the
next -- not a bug, a consequence of `spawn` moving its arguments: `serve`'s
`Handler` parameter is *borrowed*, and a borrowed value may not be moved into
a spawned green thread, so `serve` cannot hand each connection its own
thread without first giving every handler either module-constant status or a
worker-pool shape neither of which `serve` picks for its caller.

A **plain top-level function** sidesteps this. Per
`docs/closures-decision.md` §3.4 ("No captures"), a function's name used
where a one-method interface is expected is one static, immortal instance
per (function, interface) pair -- free across threads, nothing to move or
clone, by construction. `hello` here is exactly that, so this program writes
its own accept loop (the same shape as
`runtime/net_concurrency_demo/main.src`): `ln.accept()`, `spawn
handle_conn(c)`, repeat, with `handle_conn` calling the public
`http.serve_conn(c, hello)` on its own green thread. This is precisely the
pattern `http.serve`'s doc comment describes as "a program a caller can
write today on top of `serve_conn`."

## A note on read/write timeouts

This server does **not** call `set_read_timeout`/`set_write_timeout` on its
connections, unlike `http.serve`'s own default (`deadline()`, 30s both
ways). That is a deliberate choice, not an oversight -- see
`BENCHMARK.md`'s "A second finding" for the full account: a *bounded*
wait on a non-blocking socket is implemented in `lib/net.src` with a real
blocking `poll(2)` syscall on the calling green thread's own carrier OS
thread, because a non-blocking socket has no kernel-level receive timeout to
fall back on any more. That syscall does not free its carrier the way the
reactor-parked, unbounded wait does, so once concurrently open idle
keep-alive connections outnumber the carrier count (`nproc`, by default),
every carrier can end up parked inside somebody else's `poll()`, with none
left to service the rest -- a severe, reproducible stall, not a crash or a
wrong answer. Leaving the timeout at its default (wait forever,
cooperatively, via the reactor) is what makes this server actually scale
with concurrency, at the cost of a silent, never-sending peer holding a
green thread open indefinitely -- cheap (one small stack), but not free, and
a production server would want a real answer to this before shipping, which
is exactly what `BENCHMARK.md` recommends as follow-up work.

## Testing

`bash apps/httpserver/test.sh` builds the server, starts it on a scratch
port, and checks, against the real built binary over a real socket (no
mocking):

1. A single request: status 200, the exact body, a correct `Content-Length`.
2. A second request on the **same** kept-alive connection gets the same
   answer (`serve_conn`'s keep-alive loop is exercised, not just one
   request-then-close).
3. Several different paths/methods all get the same reply (this handler
   ignores the request entirely, on purpose).
4. **Concurrent load**: `N` simultaneous persistent connections, each
   issuing several sequential requests, all of which must succeed -- the
   actual property this whole app exists to demonstrate (every accepted
   connection runs on its own green thread; concurrent real socket I/O does
   not serialize on one carrier).
5. `--help`, a bad flag, and a bad `--port` are refused the way `lib/args`
   refuses them.

`BENCHMARK.md` is the throughput/latency comparison against an equivalent Go
`net/http` server, with methodology, raw numbers and honest caveats; it is
exploratory measurement, not part of this test suite, and not part of the
ordinary corpus (`apps/httpserver` is a long-running server, not a short
pass/fail program, so `gates.sh` does not build or run it).
