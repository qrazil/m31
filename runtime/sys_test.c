/* Tests for the sys layer, run against each backend by runtime/sys_test.sh.
 *
 * The corpus exercises the layer through lib/io.src and lib/fs.src, but
 * only on one backend per run and only as far as those ask. This checks every
 * function sys.h declares, including the error results, because the point
 * of the layer is that both backends return the SAME value for the same
 * situation -- a -ENOENT from one and a -1 from the other would pass every
 * corpus program that only asks "did it fail".
 *
 * It uses nothing but the sys layer itself: output is sys_write, the result
 * is sys_exit's status, and the temporary directory's name comes from
 * sys_getrandom. That is what lets the raw backend be built with
 * -DSYS_TEST_FREESTANDING into a static binary with no C library at all --
 * natively, and for aarch64 and riscv64 under qemu -- which is the only
 * honest test that the raw backend does not lean on libc somewhere.
 */
#include "sys.h"

#ifdef RT_SYS_RAW
#include "sys_linux.c"
#else
#include "sys_libc.c"
#endif

#ifdef SYS_TEST_FREESTANDING
/* The compiler may turn a struct copy or a loop into a call to these even
 * with -ffreestanding, and there is no C library to provide them. */
void *memset(void *d, int c, __SIZE_TYPE__ n);
void *memset(void *d, int c, __SIZE_TYPE__ n) {
    unsigned char *p = d;
    while (n--) *p++ = (unsigned char)c;
    return d;
}
void *memcpy(void *d, const void *s, __SIZE_TYPE__ n);
void *memcpy(void *d, const void *s, __SIZE_TYPE__ n) {
    unsigned char *p = d;
    const unsigned char *q = s;
    while (n--) *p++ = *q++;
    return d;
}
#endif

static int failures;
static int checks;

static int64_t len_of(const char *s) {
    int64_t n = 0;
    while (s[n]) n++;
    return n;
}

static void put(const char *s) {
    sys_write(1, s, len_of(s));
}

static void put_int(int64_t v) {
    char buf[24];
    char *end = buf + sizeof buf, *p = end;
    uint64_t m = v < 0 ? (uint64_t)0 - (uint64_t)v : (uint64_t)v;
    do {
        *--p = (char)('0' + m % 10);
        m /= 10;
    } while (m);
    if (v < 0) *--p = '-';
    sys_write(1, p, end - p);
}

static void expect(const char *what, int64_t got, int64_t want) {
    checks++;
    if (got == want) return;
    failures++;
    put("FAIL ");
    put(what);
    put(": got ");
    put_int(got);
    put(", want ");
    put_int(want);
    put("\n");
}

static void expect_true(const char *what, int ok) {
    expect(what, ok ? 1 : 0, 1);
}

static int same(const char *a, const char *b, int64_t n) {
    for (int64_t i = 0; i < n; i++) {
        if (a[i] != b[i]) return 0;
    }
    return 1;
}

/* dst = a + b, NUL-terminated. */
static void join(char *dst, const char *a, const char *b) {
    while (*a) *dst++ = *a++;
    while (*b) *dst++ = *b++;
    *dst = 0;
}

static void zero(void *p, int64_t n) {
    unsigned char *q = p;
    for (int64_t i = 0; i < n; i++) q[i] = 0;
}

/* ---- sockets ------------------------------------------------------------
 *
 * Both ends of every connection below live in this one process, on this one
 * thread. That works because a connect to a LISTENING socket completes as
 * soon as the kernel puts it on the backlog queue -- accept hands the
 * connection over, it does not finish the handshake -- so the client half and
 * the server half can simply take turns. It has to work that way: the
 * freestanding build has no threads, and spawning one would mean pthreads,
 * which is the C library this build exists to prove it does without.
 */

/* 127.0.0.1:port, in the layer's shape. The address bytes go in wire order,
 * which for a dotted-quad is just the digits left to right; the port is a
 * plain integer and the layer byte-swaps it (sys.h). */
static void loopback4(SysAddr *a, int64_t port) {
    zero(a, (int64_t)sizeof *a);
    a->family = SYS_AF_INET;
    a->port = port;
    a->addr[0] = 127;
    a->addr[3] = 1;
}

/* ::1, the IPv6 loopback: fifteen zero bytes and a one. */
static void loopback6(SysAddr *a, int64_t port) {
    zero(a, (int64_t)sizeof *a);
    a->family = SYS_AF_INET6;
    a->port = port;
    a->addr[15] = 1;
}

static void unix_addr(SysAddr *a, const char *path) {
    zero(a, (int64_t)sizeof *a);
    a->family = SYS_AF_UNIX;
    int64_t i = 0;
    while (path[i] != 0 && i < (int64_t)sizeof a->path - 1) {
        a->path[i] = path[i];
        i++;
    }
    a->path[i] = 0;
}

static int addr_same(const SysAddr *x, const SysAddr *y, int64_t n) {
    for (int64_t i = 0; i < n; i++) {
        if (x->addr[i] != y->addr[i]) return 0;
    }
    return 1;
}

/* Bigger than any socket buffer a kernel will autotune to -- tcp_wmem and
 * tcp_rmem top out at a few megabytes each -- so one write of it is
 * GUARANTEED to come back short. The obvious alternative, writing 64 KiB at
 * a time until the connection fills, finds a short write only when the
 * capacity is not an exact multiple of the chunk; that is a gate which fails
 * once in 65536 runs for no reason, which is worse than no gate. Static, not
 * on the stack: this is .bss, and the freestanding build runs on whatever
 * stack the kernel handed _start. */
static unsigned char big_write[32 * 1024 * 1024];

static void net_tests(void) {
    SysAddr a1, a2, a3;
    char rb[64];

    /* ---- SIGPIPE first, or the rest of this function is a coin toss ----
     * Every write below is to a socket, and a write to a socket whose peer
     * has gone away raises SIGPIPE, whose default action kills the process.
     * That is checked for real further down; here it is a precondition.
     * Called twice on purpose: a disposition is idempotent, and `net` calls
     * this from more than one place rather than trusting a flag. */
    expect("ignore SIGPIPE", sys_ignore_sigpipe(), 0);
    expect("and again, harmlessly", sys_ignore_sigpipe(), 0);

    /* ---- arguments both backends must refuse identically ---- */
    /* Each of these is caught by the layer's own validation, before any
     * system call, so that the two backends cannot drift: leaving it to the
     * kernel on one side and a switch on the other would agree today and
     * differ on the next kernel. */
    expect("socket of an unknown family", sys_socket(9999, SYS_SOCK_STREAM, 0),
           -SYS_EAFNOSUPPORT);
    expect("socket of an unknown type", sys_socket(SYS_AF_INET, 999, 0), -SYS_EINVAL);
    expect("socket with an unknown flag",
           sys_socket(SYS_AF_INET, SYS_SOCK_STREAM | 0x4000, 0), -SYS_EINVAL);
    expect("shutdown in an unknown direction", sys_shutdown(0, 9), -SYS_EINVAL);
    expect("an unknown socket option", sys_getsockopt(0, 999), -SYS_EINVAL);
    expect("setting the read-only SO_ERROR", sys_setsockopt(0, SYS_SO_ERROR, 0), -SYS_EINVAL);
    expect("a negative listen backlog", sys_listen(0, -1), -SYS_EINVAL);

    /* ---- socket calls on something that is not a socket ---- */
    int64_t notsock = sys_open("/", SYS_O_RDONLY, 0);
    expect_true("a descriptor that is not a socket", notsock >= 0);
    loopback4(&a1, 0);
    expect("bind a directory", sys_bind(notsock, &a1), -SYS_ENOTSOCK);
    expect("listen on a directory", sys_listen(notsock, 4), -SYS_ENOTSOCK);
    expect("getsockname of a directory", sys_getsockname(notsock, &a2), -SYS_ENOTSOCK);
    expect("a socket option on a directory", sys_getsockopt(notsock, SYS_SO_ERROR),
           -SYS_ENOTSOCK);
    sys_close(notsock);

    /* ---- a listening TCP socket on a port the kernel picks ---- */
    int64_t ls = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM, 0);
    expect_true("a TCP socket", ls >= 0);
    expect("no pending error on a new socket", sys_getsockopt(ls, SYS_SO_ERROR), 0);
    expect("SO_REUSEADDR starts off", sys_getsockopt(ls, SYS_SO_REUSEADDR), 0);
    expect("set SO_REUSEADDR", sys_setsockopt(ls, SYS_SO_REUSEADDR, 1), 0);
    expect("SO_REUSEADDR reads back as exactly 1", sys_getsockopt(ls, SYS_SO_REUSEADDR), 1);
    expect("set SO_KEEPALIVE", sys_setsockopt(ls, SYS_SO_KEEPALIVE, 1), 0);
    expect("SO_KEEPALIVE reads back as exactly 1", sys_getsockopt(ls, SYS_SO_KEEPALIVE), 1);
    expect("getpeername of an unconnected socket", sys_getpeername(ls, &a2), -SYS_ENOTCONN);
    loopback4(&a1, 0);
    expect("bind to 127.0.0.1, port of the kernel's choosing", sys_bind(ls, &a1), 0);
    SysAddr srv;
    expect("getsockname", sys_getsockname(ls, &srv), 0);
    expect("the bound family", srv.family, SYS_AF_INET);
    expect_true("the kernel chose a port", srv.port > 0 && srv.port < 65536);
    expect_true("on loopback", addr_same(&srv, &a1, 4));
    expect("accept before listen", sys_accept(ls, 0), -SYS_EINVAL);
    expect("listen", sys_listen(ls, 8), 0);

    /* ---- a connection nobody will answer ----
     * The refusing port is BOUND here and never listened on, so no other
     * program can take it in between and the kernel answers with a reset.
     * Picking a number and hoping would be a race. */
    int64_t dead = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM, 0);
    loopback4(&a1, 0);
    expect("bind a socket that will never listen", sys_bind(dead, &a1), 0);
    SysAddr refusing;
    expect("its address", sys_getsockname(dead, &refusing), 0);
    int64_t cr = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM, 0);
    expect("connect where nothing listens", sys_connect(cr, &refusing), -SYS_ECONNREFUSED);
    sys_close(cr);

    /* A non-blocking connect to the same place: it cannot fail yet, so it
     * says so, and the answer arrives through poll and SO_ERROR. That triple
     * is the whole reason SYS_SO_ERROR is readable. */
    int64_t nb = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM | SYS_SOCK_NONBLOCK, 0);
    expect_true("a non-blocking socket", nb >= 0);
    expect("made non-blocking, and says so", sys_getsockopt(nb, SYS_SO_NONBLOCK), 1);
    int64_t pending = sys_connect(nb, &refusing);
    expect_true("a non-blocking connect is in progress, or already refused",
                pending == -SYS_EINPROGRESS || pending == -SYS_ECONNREFUSED);
    if (pending == -SYS_EINPROGRESS) {
        SysPollFd pf;
        pf.fd = (int32_t)nb;
        pf.events = SYS_POLL_OUT;
        pf.revents = 0;
        expect("poll says the connect finished", sys_poll(&pf, 1, 5000), 1);
        expect("SO_ERROR is the refusal, positive", sys_getsockopt(nb, SYS_SO_ERROR),
               SYS_ECONNREFUSED);
        expect("and reading it cleared it", sys_getsockopt(nb, SYS_SO_ERROR), 0);
    }
    sys_close(nb);
    sys_close(dead);

    /* ---- a non-blocking listener with nobody waiting ---- */
    int64_t nl = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM | SYS_SOCK_NONBLOCK, 0);
    loopback4(&a1, 0);
    sys_bind(nl, &a1);
    sys_listen(nl, 4);
    expect("accept with nobody waiting", sys_accept(nl, 0), -SYS_EAGAIN);
    sys_close(nl);

    /* ---- connect, accept, and who each side thinks the other is ---- */
    int64_t cl = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM, 0);
    expect("connect to the listener", sys_connect(cl, &srv), 0);
    SysPollFd waiting;
    waiting.fd = (int32_t)ls;
    waiting.events = SYS_POLL_IN;
    waiting.revents = 0;
    expect("poll sees a connection waiting", sys_poll(&waiting, 1, 5000), 1);
    expect("as readable", waiting.revents & SYS_POLL_IN, SYS_POLL_IN);
    SysAddr peer;
    int64_t sv = sys_accept(ls, &peer);
    expect_true("accept", sv >= 0);
    expect("the peer's family", peer.family, SYS_AF_INET);
    expect("getsockname on the client", sys_getsockname(cl, &a2), 0);
    expect("accept reported the client's own port", peer.port, a2.port);
    expect_true("and its address", addr_same(&peer, &a2, 4));
    expect("getpeername on the client", sys_getpeername(cl, &a3), 0);
    expect("is the listener's port", a3.port, srv.port);

    expect("TCP_NODELAY starts off", sys_getsockopt(sv, SYS_TCP_NODELAY), 0);
    expect("set TCP_NODELAY", sys_setsockopt(sv, SYS_TCP_NODELAY, 1), 0);
    expect("TCP_NODELAY reads back as exactly 1", sys_getsockopt(sv, SYS_TCP_NODELAY), 1);

    expect("write to the connection", sys_write(cl, "ping", 4), 4);
    expect("read it on the other side", sys_read(sv, rb, sizeof rb), 4);
    expect_true("what arrived", same(rb, "ping", 4));

    /* ---- a receive timeout ----
     * Nothing more is coming, so the read gives up with the same errno a
     * non-blocking socket would -- which is what makes a timeout usable by
     * code already written for non-blocking descriptors. */
    expect("set SO_RCVTIMEO", sys_setsockopt(sv, SYS_SO_RCVTIMEO, 150), 0);
    expect("SO_RCVTIMEO reads back in milliseconds", sys_getsockopt(sv, SYS_SO_RCVTIMEO), 150);
    int64_t t0 = sys_clock_ns(SYS_CLOCK_MONOTONIC);
    expect("a read that times out", sys_read(sv, rb, sizeof rb), -SYS_EAGAIN);
    expect_true("waited most of the 150 ms",
                sys_clock_ns(SYS_CLOCK_MONOTONIC) - t0 > 100 * 1000000);
    expect("set SO_SNDTIMEO too", sys_setsockopt(sv, SYS_SO_SNDTIMEO, 250), 0);
    expect("SO_SNDTIMEO reads back", sys_getsockopt(sv, SYS_SO_SNDTIMEO), 250);
    expect("clear SO_RCVTIMEO", sys_setsockopt(sv, SYS_SO_RCVTIMEO, 0), 0);
    expect("0 means wait forever again", sys_getsockopt(sv, SYS_SO_RCVTIMEO), 0);
    expect("a negative timeout", sys_setsockopt(sv, SYS_SO_RCVTIMEO, -1), -SYS_EINVAL);

    /* ---- non-blocking, after the fact ----
     * An accepted socket is blocking however its listener was made, so this
     * is the only way a server gets the one thing it actually needs:
     * accepted sockets are exactly the ones it must not block on. */
    expect("an accepted socket starts blocking", sys_getsockopt(sv, SYS_SO_NONBLOCK), 0);
    expect("make it non-blocking", sys_setsockopt(sv, SYS_SO_NONBLOCK, 1), 0);
    expect("it reads back as 1", sys_getsockopt(sv, SYS_SO_NONBLOCK), 1);
    expect("a read with nothing there gives up at once",
           sys_read(sv, rb, sizeof rb), -SYS_EAGAIN);
    /* The access mode has to survive: the flag word is read and one bit put
     * back, never assigned whole. */
    expect("and the socket is still readable and writable", sys_write(sv, "ok", 2), 2);
    expect("read that on the client", sys_read(cl, rb, sizeof rb), 2);
    expect("make it blocking again", sys_setsockopt(sv, SYS_SO_NONBLOCK, 0), 0);
    expect("it reads back as 0", sys_getsockopt(sv, SYS_SO_NONBLOCK), 0);

    /* ---- poll's own behaviour ---- */
    SysPollFd idle;
    idle.fd = (int32_t)sv;
    idle.events = SYS_POLL_IN;
    idle.revents = 0;
    t0 = sys_clock_ns(SYS_CLOCK_MONOTONIC);
    expect("poll times out with nothing ready", sys_poll(&idle, 1, 120), 0);
    expect_true("after about that long",
                sys_clock_ns(SYS_CLOCK_MONOTONIC) - t0 > 80 * 1000000);
    expect("nothing was reported", idle.revents, 0);
    expect("a zero timeout returns at once", sys_poll(&idle, 1, 0), 0);
    expect("polling no descriptors at all", sys_poll(&idle, 0, 0), 0);
    expect("a negative count", sys_poll(&idle, -1, 0), -SYS_EINVAL);

    int64_t closed = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM, 0);
    sys_close(closed);
    SysPollFd two[2];
    two[0].fd = -1;                  /* skipped, and revents cleared even so */
    two[0].events = SYS_POLL_IN;
    two[0].revents = 0x7f;
    two[1].fd = (int32_t)closed;
    two[1].events = SYS_POLL_IN;
    two[1].revents = 0;
    expect("poll reports the closed descriptor", sys_poll(two, 2, 0), 1);
    expect("a negative fd is skipped", two[0].revents, 0);
    expect("the closed one is invalid", two[1].revents, SYS_POLL_NVAL);

    /* ---- a short write ----
     * One write of more than any socket buffer can hold: the kernel takes
     * what fits and returns that count. Nothing on a file ever does this,
     * and a `net` module that assumes a write is all-or-nothing loses data
     * on its first busy connection. */
    int64_t fl = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM | SYS_SOCK_NONBLOCK, 0);
    int64_t started = sys_connect(fl, &srv);
    expect_true("a second, non-blocking connection", started == 0 || started == -SYS_EINPROGRESS);
    waiting.fd = (int32_t)ls;
    waiting.events = SYS_POLL_IN;
    waiting.revents = 0;
    expect("poll sees it arrive", sys_poll(&waiting, 1, 5000), 1);
    int64_t sv2 = sys_accept(ls, 0);
    expect_true("accept it", sv2 >= 0);
    int64_t part = sys_write(fl, big_write, (int64_t)sizeof big_write);
    expect_true("a write larger than the buffers is short",
                part > 0 && part < (int64_t)sizeof big_write);
    /* Nobody has read a byte of it, so the connection is now full. */
    expect("and the next write is refused outright",
           sys_write(fl, big_write, (int64_t)sizeof big_write), -SYS_EAGAIN);
    sys_close(fl);
    sys_close(sv2);

    /* ---- shutdown ----
     * SHUT_WR sends the peer an end of file while this side can still read:
     * the half-close a client uses to say "no more requests" without losing
     * the answer to the last one. A read past it gives 0, exactly as a file
     * does at its end, which is why a reader written for files works here.
     *
     * The other half of shutdown's contract -- that writing to a peer which
     * has gone away fails with -SYS_EPIPE rather than killing the process --
     * is checked in the Unix-socket section below, where a closed peer makes
     * the very next write fail rather than the one after it. It could not be
     * checked at all until sys_ignore_sigpipe existed, which is why this
     * comment used to say so. */
    expect("shut down the writing end", sys_shutdown(cl, SYS_SHUT_WR), 0);
    expect("the peer reads end of file", sys_read(sv, rb, sizeof rb), 0);
    expect("the answer still gets through", sys_write(sv, "pong", 4), 4);
    expect("and the half-closed side still reads it", sys_read(cl, rb, sizeof rb), 4);
    expect_true("what came back", same(rb, "pong", 4));
    expect("shut down the reading end too", sys_shutdown(cl, SYS_SHUT_RD), 0);
    expect("a read after SHUT_RD is end of file", sys_read(cl, rb, sizeof rb), 0);
    /* A socket that was never connected has nothing to shut down. A
     * LISTENING one is not that case and succeeds: Linux uses it to wake a
     * blocked accept, which is how a server is told to stop. Both backends
     * agree on both answers, which is what this is here to pin down. */
    int64_t fresh = sys_socket(SYS_AF_INET, SYS_SOCK_STREAM, 0);
    expect("shutdown of a socket that never connected", sys_shutdown(fresh, SYS_SHUT_RDWR),
           -SYS_ENOTCONN);
    sys_close(fresh);
    expect("shutdown of a listening socket is allowed", sys_shutdown(ls, SYS_SHUT_RDWR), 0);
    sys_close(cl);
    sys_close(sv);
    sys_close(ls);

    /* ---- datagrams ----
     * Connected at both ends, so plain read and write carry them: the layer
     * has no sendto yet (docs/sys-layer.md §2). */
    int64_t ua = sys_socket(SYS_AF_INET, SYS_SOCK_DGRAM, 0);
    int64_t ub = sys_socket(SYS_AF_INET, SYS_SOCK_DGRAM, 0);
    expect_true("two UDP sockets", ua >= 0 && ub >= 0);
    loopback4(&a1, 0);
    expect("bind the first", sys_bind(ua, &a1), 0);
    expect("bind the second", sys_bind(ub, &a1), 0);
    SysAddr ap, bp;
    expect("the first one's port", sys_getsockname(ua, &ap), 0);
    expect("the second one's port", sys_getsockname(ub, &bp), 0);
    expect_true("two different ports", ap.port != bp.port);
    expect("point each at the other", sys_connect(ua, &bp), 0);
    expect("and back", sys_connect(ub, &ap), 0);
    expect("send a datagram", sys_write(ua, "dgram", 5), 5);
    expect("receive it whole", sys_read(ub, rb, sizeof rb), 5);
    expect_true("what arrived", same(rb, "dgram", 5));
    sys_close(ua);
    sys_close(ub);

    /* ---- IPv6, where the host has any ----
     * This is the only path that carries all sixteen address bytes, so it is
     * worth running; but a container with the module unloaded refuses the
     * socket outright, and that is the host's business, not the layer's. */
    int64_t l6 = sys_socket(SYS_AF_INET6, SYS_SOCK_STREAM, 0);
    if (l6 >= 0) {
        loopback6(&a1, 0);
        if (sys_bind(l6, &a1) == 0) {
            SysAddr s6;
            expect("getsockname over IPv6", sys_getsockname(l6, &s6), 0);
            expect("the family survives", s6.family, SYS_AF_INET6);
            expect_true("and all sixteen address bytes", addr_same(&s6, &a1, 16));
            expect_true("with a port", s6.port > 0);
            expect("listen over IPv6", sys_listen(l6, 4), 0);
            int64_t c6 = sys_socket(SYS_AF_INET6, SYS_SOCK_STREAM, 0);
            expect("connect over IPv6", sys_connect(c6, &s6), 0);
            SysAddr p6;
            int64_t v6 = sys_accept(l6, &p6);
            expect_true("accept over IPv6", v6 >= 0);
            expect("the peer is IPv6", p6.family, SYS_AF_INET6);
            expect_true("from ::1", addr_same(&p6, &a1, 16));
            expect("write over IPv6", sys_write(c6, "six", 3), 3);
            expect("read over IPv6", sys_read(v6, rb, sizeof rb), 3);
            expect_true("what arrived", same(rb, "six", 3));
            sys_close(v6);
            sys_close(c6);
        }
        sys_close(l6);
    } else {
        expect_true("a host without IPv6 says so",
                    l6 == -SYS_EAFNOSUPPORT || l6 == -SYS_EPROTONOSUPPORT);
    }

    /* ---- Unix sockets ----
     * A directory of this test's own, named from the random source like the
     * file-system section's, so two runs at once cannot collide. */
    unsigned char rnd[8];
    expect("getrandom for the socket directory", sys_getrandom(rnd, sizeof rnd),
           (int64_t)sizeof rnd);
    char dir[64];
    join(dir, "/tmp/lang-sys-net-", "");
    char *p = dir + len_of(dir);
    for (int i = 0; i < 8; i++) {
        *p++ = "0123456789abcdef"[rnd[i] >> 4];
        *p++ = "0123456789abcdef"[rnd[i] & 15];
    }
    *p = 0;
    expect("mkdir for the socket", sys_mkdir(dir, 0700), 0);
    char sock[96];
    join(sock, dir, "/s");

    int64_t us = sys_socket(SYS_AF_UNIX, SYS_SOCK_STREAM, 0);
    expect_true("a Unix stream socket", us >= 0);
    unix_addr(&a1, sock);
    expect("bind it to a path", sys_bind(us, &a1), 0);
    int64_t us2 = sys_socket(SYS_AF_UNIX, SYS_SOCK_STREAM, 0);
    expect("bind a second socket to the same path", sys_bind(us2, &a1), -SYS_EADDRINUSE);
    sys_close(us2);
    expect("listen", sys_listen(us, 4), 0);
    SysAddr named;
    expect("getsockname", sys_getsockname(us, &named), 0);
    expect("the family", named.family, SYS_AF_UNIX);
    expect_true("and the path, NUL and all",
                same(named.path, a1.path, len_of(sock) + 1));

    int64_t uc = sys_socket(SYS_AF_UNIX, SYS_SOCK_STREAM, 0);
    expect("connect over a Unix socket", sys_connect(uc, &a1), 0);
    SysAddr upeer;
    int64_t usv = sys_accept(us, &upeer);
    expect_true("accept", usv >= 0);
    expect("the peer is a Unix address", upeer.family, SYS_AF_UNIX);
    /* The client never bound a path of its own, so it has no name: the
     * kernel returns the family and nothing after it. */
    expect("an unbound client has no path", upeer.path[0], 0);
    expect("its port field is unused", upeer.port, 0);
    expect("write over a Unix socket", sys_write(uc, "unix", 4), 4);
    expect("read it", sys_read(usv, rb, sizeof rb), 4);
    expect_true("what arrived", same(rb, "unix", 4));
    expect("and back the other way", sys_write(usv, "xinu", 4), 4);
    expect("read that", sys_read(uc, rb, sizeof rb), 4);
    expect_true("what came back", same(rb, "xinu", 4));

    /* ---- writing to a peer that has gone ----
     * A Unix socket and not a TCP one, because the answer is deterministic
     * here: the peer's descriptor is gone the moment it is closed, so the
     * NEXT write fails. Over TCP the first write after a close is handed to
     * the kernel and succeeds, the peer's stack answers with a reset, and
     * only the write after THAT reports it -- true, and not something to
     * hang a gate on.
     *
     * Without the sys_ignore_sigpipe at the top of this function, the write
     * below does not return a value at all: the process dies on signal 13
     * and the whole test reports nothing. That is exactly what a server did
     * before the layer had this call. */
    sys_close(usv);
    expect("a read after the peer closed is end of file", sys_read(uc, rb, sizeof rb), 0);
    expect("and a write to it is EPIPE, not death", sys_write(uc, "gone", 4), -SYS_EPIPE);
    sys_close(uc);
    sys_close(us);

    char missing[96];
    join(missing, dir, "/nothing");
    unix_addr(&a2, missing);
    int64_t u3 = sys_socket(SYS_AF_UNIX, SYS_SOCK_STREAM, 0);
    expect("connect to a path with no socket on it", sys_connect(u3, &a2), -SYS_ENOENT);
    /* Every byte of the path filled and not a NUL among them: there is no
     * room for a terminator, so it cannot fit sun_path on any host. Refused,
     * never truncated -- a truncated path names a different socket. */
    zero(&a3, (int64_t)sizeof a3);
    a3.family = SYS_AF_UNIX;
    for (int64_t i = 0; i < (int64_t)sizeof a3.path; i++) a3.path[i] = 'x';
    expect("a path with no room for its NUL", sys_bind(u3, &a3), -SYS_ENAMETOOLONG);
    sys_close(u3);

    expect("unlink the socket file", sys_unlink(sock), 0);
    expect("rmdir", sys_rmdir(dir), 0);

    /* ---- resolution: the one place the backends differ ---- */
    SysAddr res[4];
#ifdef RT_SYS_RAW
    /* Refused, and it always will be: getaddrinfo is glibc's NSS, which
     * dlopens libnss_* at run time, and a static binary with no C library
     * cannot dlopen anything (sys.h). The point of this check is that the
     * refusal is an errno a program can branch on rather than a crash. */
    expect("resolve is refused on the raw backend",
           sys_resolve("127.0.0.1", 80, SYS_AF_INET, res, 4), -SYS_ENOSYS);
    expect("and refused the same way for any argument",
           sys_resolve("localhost", 0, 0, res, 0), -SYS_ENOSYS);
#else
    /* A literal address only. Resolving a NAME would make this test depend
     * on the host's /etc/hosts, its resolver and possibly the network, none
     * of which the sys layer is responsible for. */
    expect("resolve a literal address", sys_resolve("127.0.0.1", 80, SYS_AF_INET, res, 4), 1);
    expect("its family", res[0].family, SYS_AF_INET);
    expect("the port the caller asked for", res[0].port, 80);
    loopback4(&a1, 80);
    expect_true("and the address", addr_same(&res[0], &a1, 4));
    expect("counting with nowhere to put them",
           sys_resolve("127.0.0.1", 80, SYS_AF_INET, res, 0), 1);
    expect("either family also finds it", sys_resolve("127.0.0.1", 1, 0, res, 4), 1);
    expect("a port out of range", sys_resolve("127.0.0.1", 99999, 0, res, 4), -SYS_EINVAL);
    expect("an unknown family", sys_resolve("127.0.0.1", 80, 1234, res, 4),
           -SYS_EAFNOSUPPORT);
    /* The name is fine and the family is fine; there is just no address of
     * that family behind it. glibc says EAI_ADDRFAMILY, which is a GNU code
     * this file cannot see -- and which means exactly this. */
    expect("an IPv4 literal asked for as IPv6",
           sys_resolve("127.0.0.1", 80, SYS_AF_INET6, res, 4), -SYS_ENOENT);
#endif
}

/* ---- the terminal -------------------------------------------------------
 *
 * A pty is the only honest way to test this: a real terminal is not there
 * under a gate, and every other descriptor answers -ENOTTY to all three
 * calls, which proves the error path and nothing else.
 *
 * So the test opens one, and it opens it TWICE OVER -- through POSIX where
 * there is a C library, and through /dev/ptmx and two ioctls where there is
 * not. That is not duplication for its own sake: the freestanding builds,
 * natively and for aarch64 and riscv64 under qemu, have no posix_openpt to
 * call, and they are the builds that matter most here, because they are the
 * ones where a wrong ioctl number or a wrong struct offset has nothing to
 * hide behind. Both paths reach the same kernel object and run the same
 * checks below.
 *
 * Neither open makes the pty a controlling terminal: this process is not a
 * session leader, so it cannot acquire one, and the POSIX path passes
 * O_NOCTTY as well.
 *
 * What is NOT tested here, and cannot be: that the flags mean to a real
 * terminal emulator what they mean to a pty's line discipline. They are the
 * same line discipline -- a pty slave and a serial line both run n_tty -- so
 * the kernel side is covered; what a particular terminal does with the
 * BYTES that come out is lib/term.src's business and a human's. */

#ifdef RT_SYS_RAW

/* ptmx's own ioctls. _IOW('T', 0x31, int) and _IOR('T', 0x30, unsigned int),
 * which encode to the same numbers on every architecture this backend
 * supports, since the encoding is (dir, size, 'T', nr) and the size is 4. */
#define T_TIOCGPTN   0x80045430
#define T_TIOCSPTLCK 0x40045431
#define T_TIOCSWINSZ 0x5414

typedef struct {
    uint16_t row, col, xpixel, ypixel;
} TWinsize;

static int64_t pty_open(int64_t *master, int64_t *slave) {
    int64_t m = sys_open("/dev/ptmx", SYS_O_RDWR, 0);
    if (m < 0) return m;
    int unlock = 0;
    if (sc(NR_ioctl, m, T_TIOCSPTLCK, P(&unlock), 0, 0, 0) < 0) {
        sys_close(m);
        return -SYS_EIO;
    }
    unsigned int n = 0;
    if (sc(NR_ioctl, m, T_TIOCGPTN, P(&n), 0, 0, 0) < 0) {
        sys_close(m);
        return -SYS_EIO;
    }
    char path[32] = {'/', 'd', 'e', 'v', '/', 'p', 't', 's', '/', 0};
    int at = 9, digits = 1;
    for (unsigned int t = n; t >= 10; t /= 10) digits++;
    for (int i = digits - 1; i >= 0; i--) {
        unsigned int d = n;
        for (int k = 0; k < i; k++) d /= 10;
        path[at++] = (char)('0' + d % 10);
    }
    path[at] = 0;
    int64_t s = sys_open(path, SYS_O_RDWR, 0);
    if (s < 0) {
        sys_close(m);
        return s;
    }
    *master = m;
    *slave = s;
    return 0;
}

static int64_t pty_resize(int64_t fd, int rows, int cols) {
    TWinsize w = {(uint16_t)rows, (uint16_t)cols, 0, 0};
    return sc(NR_ioctl, fd, T_TIOCSWINSZ, P(&w), 0, 0, 0);
}

#else /* the C library is here, so POSIX's own four calls are */

#include <sys/ioctl.h>  /* TIOCSWINSZ, to give the pty a size to report */

/* POSIX's four pseudo-terminal calls, declared here rather than taken from
 * <stdlib.h>, because glibc hides them behind __USE_XOPEN2KXSI -- which needs
 * _XOPEN_SOURCE >= 700 defined before the FIRST header this file pulls in.
 * Defining it would also change what sys_libc.c sees a few lines above (its
 * getaddrinfo error codes move behind and out from behind feature macros),
 * and this file exists to test that file as the runtime compiles it. The
 * signatures are POSIX's own, identical on Linux, macOS and the BSDs, so a
 * host that does declare them declares exactly this. */
int posix_openpt(int oflag);
int grantpt(int fd);
int unlockpt(int fd);
char *ptsname(int fd);

static int64_t pty_open(int64_t *master, int64_t *slave) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0) return neg_errno(errno);
    if (grantpt(m) != 0 || unlockpt(m) != 0) {
        int e = errno;
        close(m);
        return neg_errno(e);
    }
    const char *name = ptsname(m);
    if (name == NULL) {
        close(m);
        return -SYS_EIO;
    }
    int s = open(name, O_RDWR | O_NOCTTY);
    if (s < 0) {
        int e = errno;
        close(m);
        return neg_errno(e);
    }
    *master = m;
    *slave = s;
    return 0;
}

static int64_t pty_resize(int64_t fd, int rows, int cols) {
    struct winsize w;
    zero(&w, (int64_t)sizeof w);
    w.ws_row = (unsigned short)rows;
    w.ws_col = (unsigned short)cols;
    return ret(ioctl((int)fd, TIOCSWINSZ, &w));
}

#endif

/* Every named flag, so a backend that mapped one of them to the wrong bit
 * is caught by name rather than by a word comparison that says only "they
 * differ". */
static void expect_flag(const char *what, int64_t word, int64_t bit, int want) {
    expect(what, (word & bit) != 0 ? 1 : 0, want);
}

static void term_tests(void) {
    SysTermios t;
    int64_t rows = -1, cols = -1;

    /* Zeroed before it is passed anywhere, so a backend that read it on a
     * path that should have failed first reads zeros rather than the stack,
     * which is what the sanitized builds of runtime/sys_test.sh look for. */
    zero(&t, (int64_t)sizeof t);

    /* ---- not a terminal: the same answer from both backends ---- */
    int64_t f = sys_open("/", SYS_O_RDONLY, 0);
    expect_true("a descriptor that is not a terminal", f >= 0);
    expect("isatty of a directory", sys_isatty(f), 0);
    expect("tcget of a directory", sys_tcget(f, &t), -SYS_ENOTTY);
    expect("tcset of a directory", sys_tcset(f, &t), -SYS_ENOTTY);
    expect("winsize of a directory", sys_winsize(f, &rows, &cols), -SYS_ENOTTY);
    sys_close(f);
    expect("isatty of a bad fd", sys_isatty(-1), 0);
    expect("tcget of a bad fd", sys_tcget(-1, &t), -SYS_EBADF);
    expect("tcset of a bad fd", sys_tcset(-1, &t), -SYS_EBADF);
    expect("winsize of a bad fd", sys_winsize(-1, &rows, &cols), -SYS_EBADF);

    /* ---- and now a real one ---- */
    int64_t m = -1, s = -1;
    if (pty_open(&m, &s) != 0) {
        /* A machine with no /dev/ptmx -- a container built without one -- is
         * a real environment, not a failure. Say so, loudly enough to be
         * read, rather than passing a gate that did not run. */
        put("NOTE no pty here: the terminal calls were checked only on a "
            "descriptor that is not one\n");
        return;
    }

    expect("isatty of a pty master", sys_isatty(m), 1);
    expect("isatty of a pty slave", sys_isatty(s), 1);

    /* The size the master sets is the size the slave reports. A fresh pty
     * has no size at all, which is 0 by 0 and not an error -- sys.h says a
     * caller must expect that -- so it is checked before it is given one. */
    expect("a fresh pty has no size", sys_winsize(s, &rows, &cols), 0);
    expect("no rows", rows, 0);
    expect("no columns", cols, 0);
    expect("resize the pty", pty_resize(m, 40, 132), 0);
    expect("winsize", sys_winsize(s, &rows, &cols), 0);
    expect("rows", rows, 40);
    expect("columns", cols, 132);

    /* ---- what a terminal looks like before a program touches it ---- */
    SysTermios saved;
    zero(&saved, (int64_t)sizeof saved);
    expect("tcget", sys_tcget(s, &saved), 0);
    expect_flag("ECHO is on to start with", saved.lflag, SYS_TC_ECHO, 1);
    expect_flag("ICANON is on to start with", saved.lflag, SYS_TC_ICANON, 1);
    expect_flag("ISIG is on to start with", saved.lflag, SYS_TC_ISIG, 1);
    expect_flag("ICRNL is on to start with", saved.iflag, SYS_TC_ICRNL, 1);
    expect_flag("OPOST is on to start with", saved.oflag, SYS_TC_OPOST, 1);
    /* The interrupt character is ^C on every Unix, and this is the value the
     * round trip below has to bring back: it is in the part of the record
     * that neither the layer nor a caller ever names. */
    expect("VINTR is ^C", saved.cc[0], 3);

    /* ---- raw mode, the way lib/term.src builds it ---- */
    t = saved;
    t.iflag = t.iflag & ~(int64_t)(SYS_TC_IGNBRK | SYS_TC_BRKINT | SYS_TC_PARMRK |
                                   SYS_TC_ISTRIP | SYS_TC_INLCR | SYS_TC_IGNCR |
                                   SYS_TC_ICRNL | SYS_TC_IXON);
    t.oflag = t.oflag & ~(int64_t)SYS_TC_OPOST;
    t.lflag = t.lflag & ~(int64_t)(SYS_TC_ECHO | SYS_TC_ECHONL | SYS_TC_ICANON |
                                   SYS_TC_ISIG | SYS_TC_IEXTEN);
    t.cc[SYS_VMIN] = 1;
    t.cc[SYS_VTIME] = 0;
    expect("tcset raw", sys_tcset(s, &t), 0);

    SysTermios now;
    zero(&now, (int64_t)sizeof now);
    expect("tcget after tcset", sys_tcget(s, &now), 0);
    expect_flag("ECHO is off", now.lflag, SYS_TC_ECHO, 0);
    expect_flag("ICANON is off", now.lflag, SYS_TC_ICANON, 0);
    expect_flag("ISIG is off", now.lflag, SYS_TC_ISIG, 0);
    expect_flag("IEXTEN is off", now.lflag, SYS_TC_IEXTEN, 0);
    expect_flag("ICRNL is off", now.iflag, SYS_TC_ICRNL, 0);
    expect_flag("IXON is off", now.iflag, SYS_TC_IXON, 0);
    expect_flag("ISTRIP is off", now.iflag, SYS_TC_ISTRIP, 0);
    expect_flag("OPOST is off", now.oflag, SYS_TC_OPOST, 0);
    expect("VMIN came back", now.cc[SYS_VMIN], 1);
    expect("VTIME came back", now.cc[SYS_VTIME], 0);
    /* The two fields nothing above the layer names, and the reason sys_tcset
     * is a read-modify-write: a set that only knew about the flags it was
     * asked to change would have zeroed both. */
    expect("VINTR survived the round trip", now.cc[0], saved.cc[0]);
    expect("the control word survived the round trip", now.cflag, saved.cflag);

    /* ---- raw mode is not just bits: it changes what a read gives back ----
     *
     * With ICANON off a read hands over what has arrived rather than waiting
     * for a line, and with ICRNL off a carriage return arrives as 13 instead
     * of being turned into 10. Both are checked in one write, because with
     * the flags still set the read would block on the missing newline and
     * the CR would come back as an LF -- so a backend that failed to apply
     * either one would hang the gate or fail this line, not pass it. */
    char got[8];
    expect("write two bytes and a CR to the master", sys_write(m, "ab\r", 3), 3);
    expect("read them without waiting for a line", sys_read(s, got, sizeof got), 3);
    expect_true("the bytes are what was written", same(got, "ab\r", 3));

    /* And with OPOST off, a newline written by the program is the one byte
     * it wrote: no CR is inserted on the way out. */
    expect("write a line from the slave", sys_write(s, "x\n", 2), 2);
    expect("read it on the master", sys_read(m, got, sizeof got), 2);
    expect_true("no CR was added", same(got, "x\n", 2));

    /* ---- and back, which is the whole point ---- */
    expect("tcset the saved settings back", sys_tcset(s, &saved), 0);
    zero(&now, (int64_t)sizeof now);
    expect("tcget after restoring", sys_tcget(s, &now), 0);
    expect("iflag restored", now.iflag, saved.iflag);
    expect("oflag restored", now.oflag, saved.oflag);
    expect("cflag restored", now.cflag, saved.cflag);
    expect("lflag restored", now.lflag, saved.lflag);
    int cc_same = 1;
    for (int i = 0; i < SYS_NCCS; i++) {
        if (now.cc[i] != saved.cc[i]) cc_same = 0;
    }
    expect_true("every control character restored", cc_same);

    expect("close the slave", sys_close(s), 0);
    expect("close the master", sys_close(m), 0);
}

int test_main(void);
int test_main(void) {
    /* ---- errors every backend must spell the same way ---- */
    expect("open missing", sys_open("/no/such/path/at/all", SYS_O_RDONLY, 0), -SYS_ENOENT);
    int64_t root = sys_open("/", SYS_O_RDONLY, 0);
    expect_true("open / for reading", root >= 0);
    char buf[64];
    expect("read a directory", sys_read(root, buf, sizeof buf), -SYS_EISDIR);
    expect("close /", sys_close(root), 0);
    expect("read a bad fd", sys_read(-1, buf, 1), -SYS_EBADF);
    expect("write a bad fd", sys_write(-1, buf, 1), -SYS_EBADF);
    expect("close a closed fd", sys_close(root), -SYS_EBADF);
    expect("lseek a bad fd", sys_lseek(-1, 0, SYS_SEEK_SET), -SYS_EBADF);
    /* Zeroed so a failed fstat below is reported as a wrong value, not as
     * whatever the stack held. */
    SysStat st = {0, 0, 0};
    expect("fstat a bad fd", sys_fstat(-1, &st), -SYS_EBADF);

    /* ---- a private directory, named from the random source ---- */
    unsigned char rnd[8];
    expect("getrandom small", sys_getrandom(rnd, sizeof rnd), (int64_t)sizeof rnd);
    char dir[64];
    join(dir, "/tmp/lang-sys-test-", "");
    char *p = dir + len_of(dir);
    for (int i = 0; i < 8; i++) {
        *p++ = "0123456789abcdef"[rnd[i] >> 4];
        *p++ = "0123456789abcdef"[rnd[i] & 15];
    }
    *p = 0;
    expect("mkdir", sys_mkdir(dir, 0700), 0);
    expect("mkdir again", sys_mkdir(dir, 0700), -SYS_EEXIST);

    char a[96], b[96];
    join(a, dir, "/a");
    join(b, dir, "/b");

    /* ---- write, reopen, fstat, seek, read ---- */
    int64_t fd = sys_open(a, SYS_O_WRONLY | SYS_O_CREAT | SYS_O_TRUNC, 0644);
    expect_true("create", fd >= 0);
    expect("write", sys_write(fd, "hello", 5), 5);
    expect("isatty on a file", sys_isatty(fd), 0);
    expect("close", sys_close(fd), 0);

    expect("create exclusive over an existing file",
           sys_open(a, SYS_O_WRONLY | SYS_O_CREAT | SYS_O_EXCL, 0644), -SYS_EEXIST);
    expect("O_DIRECTORY on a file", sys_open(a, SYS_O_RDONLY | SYS_O_DIRECTORY, 0), -SYS_ENOTDIR);
    int64_t dfd = sys_open(dir, SYS_O_RDONLY | SYS_O_DIRECTORY, 0);
    expect_true("O_DIRECTORY on a directory", dfd >= 0);
    expect("fstat a directory", sys_fstat(dfd, &st), 0);
    expect("directory type bits", st.mode & SYS_S_IFMT, SYS_S_IFDIR);
    sys_close(dfd);

    fd = sys_open(a, SYS_O_WRONLY | SYS_O_APPEND, 0);
    expect_true("open for append", fd >= 0);
    expect("append", sys_write(fd, " world", 6), 6);
    sys_close(fd);

    fd = sys_open(a, SYS_O_RDONLY, 0);
    expect_true("reopen", fd >= 0);
    expect("fstat", sys_fstat(fd, &st), 0);
    expect("fstat size", st.size, 11);
    expect("file type bits", st.mode & SYS_S_IFMT, SYS_S_IFREG);
    /* The umask may take bits away from 0644, never add any. */
    expect("permission bits", st.mode & 0777 & ~0644, 0);
    expect_true("mtime is after 2020", st.mtime_ns > (int64_t)1577836800 * 1000000000);
    expect("write to a read-only fd", sys_write(fd, "x", 1), -SYS_EBADF);
    expect("lseek to the end", sys_lseek(fd, 0, SYS_SEEK_END), 11);
    expect("read at the end", sys_read(fd, buf, sizeof buf), 0);
    expect("lseek from the start", sys_lseek(fd, 6, SYS_SEEK_SET), 6);
    expect("lseek from here", sys_lseek(fd, -5, SYS_SEEK_CUR), 1);
    expect("read the rest", sys_read(fd, buf, sizeof buf), 10);
    expect_true("what was read", same(buf, "ello world", 10));
    expect("lseek before the start", sys_lseek(fd, -100, SYS_SEEK_SET), -SYS_EINVAL);
    sys_close(fd);

    /* ---- stat by name ---- */
    st.size = st.mode = 0;
    expect("stat", sys_stat(a, 1, &st), 0);
    expect("stat size", st.size, 11);
    expect("stat type bits", st.mode & SYS_S_IFMT, SYS_S_IFREG);
    st.size = st.mode = 0;
    expect("lstat of a plain file", sys_stat(a, 0, &st), 0);
    expect("lstat size", st.size, 11);
    expect("stat a directory", sys_stat(dir, 1, &st), 0);
    expect("stat directory type bits", st.mode & SYS_S_IFMT, SYS_S_IFDIR);
    expect("stat missing", sys_stat("/no/such/path/at/all", 1, &st), -SYS_ENOENT);
    char under[128];
    join(under, a, "/x");
    expect("stat through a file", sys_stat(under, 1, &st), -SYS_ENOTDIR);

    /* ---- listing: each name and a NUL, in no promised order ---- */
    char c[96];
    join(c, dir, "/cc");
    fd = sys_open(c, SYS_O_WRONLY | SYS_O_CREAT | SYS_O_EXCL, 0600);
    expect_true("create a second file", fd >= 0);
    sys_close(fd);
    char names[64];
    expect("listdir", sys_listdir(dir, names, sizeof names), 5);  /* "a\0cc\0" */
    expect_true("listdir names, either order",
                same(names, "a\0cc", 5) || same(names, "cc\0a", 5));
    expect("listdir with no room says how much", sys_listdir(dir, names, 0), 5);
    expect("listdir with some room", sys_listdir(dir, names, 3), 5);
    expect("listdir a file", sys_listdir(a, names, sizeof names), -SYS_ENOTDIR);
    expect("listdir missing", sys_listdir("/no/such/path/at/all", names, sizeof names),
           -SYS_ENOENT);
    expect("unlink the second file", sys_unlink(c), 0);

    /* ---- a link made here, dangling and not ---- */
    char ln[96];
    join(ln, dir, "/ln");
    expect("symlink", sys_symlink("a", ln), 0);
    expect("symlink over an existing name", sys_symlink("a", ln), -SYS_EEXIST);
    expect("lstat the new link", sys_stat(ln, 0, &st), 0);
    expect("it is a link", st.mode & SYS_S_IFMT, SYS_S_IFLNK);
    expect("stat through it", sys_stat(ln, 1, &st), 0);
    expect("to the file", st.size, 11);
    expect("listdir sees it", sys_listdir(dir, names, sizeof names), 5);  /* "a\0ln\0" */
    expect("unlink removes the link", sys_unlink(ln), 0);
    expect("and not the file", sys_stat(a, 1, &st), 0);
    expect("dangling symlink", sys_symlink("nowhere", ln), 0);
    expect("stat a dangling link", sys_stat(ln, 1, &st), -SYS_ENOENT);
    expect("lstat a dangling link", sys_stat(ln, 0, &st), 0);
    expect("unlink the dangling link", sys_unlink(ln), 0);

    /* ---- rename, unlink, rmdir ---- */
    expect("rename", sys_rename(a, b), 0);
    expect("old name is gone", sys_open(a, SYS_O_RDONLY, 0), -SYS_ENOENT);
    expect("rename a missing file", sys_rename(a, b), -SYS_ENOENT);
    expect("rmdir a non-empty directory", sys_rmdir(dir), -SYS_ENOTEMPTY);
    expect("unlink a directory is refused", sys_unlink(dir) < 0, 1);
    expect("unlink", sys_unlink(b), 0);
    expect("unlink again", sys_unlink(b), -SYS_ENOENT);
    expect("listdir an empty directory", sys_listdir(dir, names, sizeof names), 0);
    expect("rmdir", sys_rmdir(dir), 0);
    expect("rmdir again", sys_rmdir(dir), -SYS_ENOENT);

    /* ---- clocks ---- */
    int64_t wall = sys_clock_ns(SYS_CLOCK_REALTIME);
    expect_true("realtime is after 2020", wall > (int64_t)1577836800 * 1000000000);
    int64_t t0 = sys_clock_ns(SYS_CLOCK_MONOTONIC);
    int64_t t1 = sys_clock_ns(SYS_CLOCK_MONOTONIC);
    expect_true("monotonic is positive", t0 > 0);
    expect_true("monotonic does not go back", t1 >= t0);
    expect("unknown clock", sys_clock_ns(99), -SYS_EINVAL);

    /* ---- randomness: more than one getentropy call's worth ---- */
    unsigned char big[1000];
    for (int i = 0; i < 1000; i++) big[i] = 0;
    expect("getrandom 1000", sys_getrandom(big, sizeof big), 1000);
    int nonzero = 0;
    for (int i = 900; i < 1000; i++) nonzero += big[i] != 0;
    /* The last 100 bytes all zero by chance is 2^-800. */
    expect_true("getrandom filled the tail", nonzero > 0);

    term_tests();
    net_tests();

    put("sys layer: ");
    put_int(checks - failures);
    put(" of ");
    put_int(checks);
    put(" checks passed\n");
    return failures;
}

#ifdef SYS_TEST_FREESTANDING
/* No C runtime: the kernel jumps here with the stack pointer 16-byte
 * aligned and nothing pushed. On x86-64 a C function expects to be entered
 * by a call, 8 bytes below alignment, so the attribute realigns rather
 * than let the first aligned vector store fault. */
#if defined(__x86_64__)
__attribute__((force_align_arg_pointer))
#endif
void _start(void);
void _start(void) {
    sys_exit(test_main());
}
#else
int main(void) {
    return test_main();
}
#endif
