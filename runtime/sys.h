/* The sys layer: every operating-system operation the runtime performs.
 *
 * This is the line docs/stdlib-seam.md §5 draws -- "the line is the
 * operating system" -- written down as a C interface. Everything above it
 * is the runtime and, increasingly, the language itself; everything below
 * it is one of two implementations, chosen when the runtime is compiled:
 *
 *   sys_libc.c   the default. Calls the C library's POSIX functions, so it
 *                builds wherever the runtime already builds.
 *   sys_linux.c  -DRT_SYS_RAW. Enters the Linux kernel directly with
 *                inline assembly (x86-64, aarch64, riscv64); these
 *                operations make no C library call at all.
 *
 * Both are #included by rt.c rather than compiled on their own, so the
 * runtime stays ONE translation unit and every build line in the
 * repository keeps naming runtime/rt.c alone. See docs/sys-layer.md.
 *
 * One convention for every function, in both implementations: the result
 * is a non-negative value on success and -errno on failure, exactly as the
 * Linux kernel returns it. No global errno is read or written. That makes a
 * result a single int64_t that a `prim` can hand to the language unchanged,
 * and it is why the raw implementation needs no translation at all.
 *
 * The errno NUMBERS are Linux's (SYS_E* below), on every host. On Linux the
 * libc implementation's numbers are the same by construction; a port to a
 * host whose errno values differ translates them in its implementation, so
 * code above this line -- lib/io.src's from_errno -- is written once.
 *
 * Flags and clock ids are this layer's own constants, not the host's:
 * O_DIRECTORY is a different bit on aarch64 than on x86-64, and
 * CLOCK_MONOTONIC is 1 on Linux and 6 on macOS. Each implementation maps
 * them, so a number that crosses into the language means the same thing on
 * every target.
 *
 * Paths are NUL-terminated C strings. Refusing an embedded NUL is the
 * caller's job (rt.c's path_ok), because only the caller knows the length.
 */
#ifndef RT_SYS_H
#define RT_SYS_H

#include <stdint.h>

/* ---- errno values: Linux numbering, on every host ----------------------- */

#define SYS_EPERM         1
#define SYS_ENOENT        2
#define SYS_EINTR         4
#define SYS_EIO           5
#define SYS_EBADF         9
#define SYS_EAGAIN       11
#define SYS_ENOMEM       12
#define SYS_EACCES       13
#define SYS_EEXIST       17
#define SYS_EXDEV        18
#define SYS_ENOTDIR      20
#define SYS_EISDIR       21
#define SYS_EINVAL       22
#define SYS_EMFILE       24
#define SYS_ENOTTY       25
#define SYS_ENOSPC       28
#define SYS_ESPIPE       29
#define SYS_EROFS        30
#define SYS_EPIPE        32
#define SYS_ENAMETOOLONG 36
#define SYS_ENOSYS       38
#define SYS_ENOTEMPTY    39
#define SYS_ELOOP        40

/* The socket errnos. Nothing above the layer can act sensibly on a socket
 * without telling "nobody is listening" from "the network is unreachable"
 * from "you are already connected", so they are named here rather than left
 * to pass through as host numbers. */
#define SYS_ENOTSOCK         88
#define SYS_EDESTADDRREQ     89
#define SYS_EMSGSIZE         90
#define SYS_EPROTONOSUPPORT  93
#define SYS_EOPNOTSUPP       95
#define SYS_EAFNOSUPPORT     97
#define SYS_EADDRINUSE       98
#define SYS_EADDRNOTAVAIL    99
#define SYS_ENETUNREACH     101
#define SYS_ECONNABORTED    103
#define SYS_ECONNRESET      104
#define SYS_EISCONN         106
#define SYS_ENOTCONN        107
#define SYS_ETIMEDOUT       110
#define SYS_ECONNREFUSED    111
#define SYS_EHOSTUNREACH    113
#define SYS_EINPROGRESS     115

/* ---- open flags: the layer's own bits ----------------------------------- */

/* The access mode is the low two bits, as it is on every POSIX system. The
 * other values happen to be x86-64 Linux's, so the raw implementation's
 * mapping is the identity there. Close-on-exec is not a flag: every
 * descriptor the runtime opens has it, because a descriptor leaking into a
 * child process is never what a program meant (Go makes the same choice). */
#define SYS_O_RDONLY    0x0000
#define SYS_O_WRONLY    0x0001
#define SYS_O_RDWR      0x0002
#define SYS_O_ACCMODE   0x0003
#define SYS_O_CREAT     0x0040
#define SYS_O_EXCL      0x0080
#define SYS_O_TRUNC     0x0200
#define SYS_O_APPEND    0x0400
#define SYS_O_DIRECTORY 0x10000

#define SYS_SEEK_SET 0
#define SYS_SEEK_CUR 1
#define SYS_SEEK_END 2

#define SYS_CLOCK_REALTIME  0
#define SYS_CLOCK_MONOTONIC 1

/* File type bits in SysStat.mode. These are the historical Unix values, the
 * same on Linux, macOS and the BSDs, so they need no mapping. */
#define SYS_S_IFMT  0170000
#define SYS_S_IFDIR 0040000
#define SYS_S_IFREG 0100000
#define SYS_S_IFLNK 0120000

/* What fstat reports, reduced to what a program asks. The kernel's struct
 * stat has a different layout on every architecture; this one does not. */
typedef struct {
    int64_t size;      /* bytes */
    int64_t mode;      /* type and permission bits, SYS_S_* above */
    int64_t mtime_ns;  /* last modification, nanoseconds since the epoch */
} SysStat;

/* ---- sockets ------------------------------------------------------------ */

/* Address families. Linux's numbers, so the raw mapping is the identity;
 * AF_INET6 is 10 here and 30 on macOS, which is why the libc backend maps. */
#define SYS_AF_UNIX  1
#define SYS_AF_INET  2
#define SYS_AF_INET6 10

/* Socket types. The low bits are the type; SYS_SOCK_NONBLOCK is OR'd in.
 *
 * There is deliberately no SYS_SOCK_CLOEXEC, for the same reason sys_open
 * has no O_CLOEXEC: close-on-exec is not a flag in this layer (§1). Every
 * descriptor it hands out has it, sockets included -- a listening socket
 * inherited by a child is how a port stays bound after a server exits, which
 * is never what a program meant. A caller that wanted the leak would have to
 * clear the bit itself, and nothing here offers to. */
#define SYS_SOCK_STREAM   1
#define SYS_SOCK_DGRAM    2
#define SYS_SOCK_TYPEMASK 0xff    /* the type, without the flags below */
#define SYS_SOCK_NONBLOCK 0x800   /* Linux's SOCK_NONBLOCK, so raw maps by identity */

/* Protocols, for sys_socket's third argument. 0 means "the usual one for
 * this type" -- TCP for a stream, UDP for a datagram -- and is what almost
 * every caller passes. These two are IANA protocol numbers, identical on
 * every operating system, so no backend maps them. */
#define SYS_IPPROTO_TCP  6
#define SYS_IPPROTO_UDP 17

/* sys_shutdown's direction. 0/1/2 on every POSIX system. */
#define SYS_SHUT_RD   0
#define SYS_SHUT_WR   1
#define SYS_SHUT_RDWR 2

/* Socket options, for sys_getsockopt and sys_setsockopt.
 *
 * These are the layer's own dense numbering, not the host's, and each one
 * names a LEVEL and an OPTION together: SYS_SO_REUSEADDR is (SOL_SOCKET,
 * SO_REUSEADDR) and SYS_TCP_NODELAY is (IPPROTO_TCP, TCP_NODELAY). Folding
 * the level in removes an argument that only ever had one right value per
 * option, and it is what lets every option's value be a single int64_t: the
 * layer converts, so nothing above it ever builds a struct timeval or passes
 * a socklen_t.
 *
 *   SYS_SO_REUSEADDR   0 or 1. Set before bind, so a listening port can be
 *                      re-bound while an old connection is in TIME_WAIT.
 *   SYS_SO_KEEPALIVE   0 or 1.
 *   SYS_TCP_NODELAY    0 or 1. Send small writes at once (no Nagle).
 *   SYS_SO_RCVTIMEO    milliseconds, 0 for "wait forever". A read that
 *   SYS_SO_SNDTIMEO    times out fails with -SYS_EAGAIN, as it would if the
 *                      socket were non-blocking.
 *   SYS_SO_NONBLOCK    0 or 1. Not a socket option at all: it is the
 *                      descriptor's O_NONBLOCK, which the backends reach
 *                      through fcntl. It is in this table because it is the
 *                      one remaining thing a caller changes about a socket
 *                      it already has, and because an ACCEPTED socket needs
 *                      it: accept does not pass the listener's flag on
 *                      (POSIX leaves that unspecified and Linux does not),
 *                      yet accepted sockets are exactly the ones a server
 *                      must not block on. A function of its own for one
 *                      boolean would be the worse trade. Being a descriptor
 *                      flag, it works on any descriptor, not only a socket.
 *   SYS_SO_ERROR       get only. The pending error on the socket, as a
 *                      POSITIVE Linux errno, 0 for none, and the kernel
 *                      clears it. This is how a non-blocking connect reports
 *                      its result once poll says the socket is writable. It
 *                      is positive, unlike everything else in this header,
 *                      because it is an option's VALUE and not a result: a
 *                      negative return still means the getsockopt itself
 *                      failed, and -SYS_EBADF could not otherwise be told
 *                      from "the connection was refused".
 *
 * Booleans read back as exactly 0 or 1 from both backends. The kernel is
 * happy to return 4 for a "true" SO_KEEPALIVE; the layer normalises. */
#define SYS_SO_REUSEADDR 1
#define SYS_SO_KEEPALIVE 2
#define SYS_SO_ERROR     3
#define SYS_SO_RCVTIMEO  4
#define SYS_SO_SNDTIMEO  5
#define SYS_TCP_NODELAY  6
#define SYS_SO_NONBLOCK  7

/* A socket address, in the layer's own shape: ONE record for every family.
 *
 * The kernel has a different struct per family -- sockaddr_in is 16 bytes,
 * sockaddr_in6 is 28 with a flow label and a scope id in the middle,
 * sockaddr_un is 110 -- and the raw backend cannot borrow libc's headers for
 * any of them. Rather than teach everything above the line to tell them
 * apart, each backend converts this record to and from the kernel's at the
 * boundary, which is four short functions in each file and nothing anywhere
 * else.
 *
 * Byte order, explicitly, because it is the classic place to get this wrong:
 *
 *   - `port` is a PLAIN INTEGER in the machine's own order, 0..65535. The
 *     layer does the htons, in the backend, at the boundary. Nothing above
 *     this line ever byte-swaps anything; a program says 8080 and means 8080.
 *   - `addr` is in WIRE order -- 127.0.0.1 is the bytes {127,0,0,1}, and an
 *     IPv6 address is its 16 bytes left to right. This is already network
 *     order, so the backends copy it straight through without swapping. It
 *     is also the order the text forms are written and parsed in, so the
 *     `net` module's address parser and printer need no conversion either.
 *
 * The address bytes and the Unix path share no space: 124 bytes of struct
 * against a union's 108 is not worth a tagged access rule in two backends.
 * `path` is 108 bytes because that is Linux's sun_path; a longer path is
 * refused with -SYS_ENAMETOOLONG rather than truncated, and the libc backend
 * checks against the host's own sun_path, which is 104 on macOS and the BSDs. */
typedef struct {
    int64_t       family;    /* SYS_AF_INET, SYS_AF_INET6 or SYS_AF_UNIX */
    int64_t       port;      /* host order, 0..65535; unused for AF_UNIX */
    unsigned char addr[16];  /* wire order: 4 bytes for INET, 16 for INET6 */
    char          path[108]; /* AF_UNIX only, NUL-terminated; "" if unbound */
} SysAddr;

/* One descriptor's worth of what sys_poll waits for and what it found.
 *
 * This is deliberately the kernel's own `struct pollfd` -- a 32-bit fd and
 * two 16-bit masks, in that order -- rather than the int64 triple the rest of
 * the layer would suggest. sys_poll takes an ARRAY, and converting an array
 * needs somewhere to put the converted copy: the layer has no allocator, and
 * the raw backend is built freestanding, so a fixed-size bounce buffer would
 * put an arbitrary ceiling on how many descriptors a server may wait on.
 * Matching the layout exactly means there is nothing to convert. The
 * assumption is not assumed: both backends _Static_assert the size and the
 * field offsets, and the libc backend checks them against the host's real
 * struct pollfd, so a host that disagreed would fail to build rather than
 * quietly scribble. */
typedef struct {
    int32_t fd;       /* negative: skipped, and revents is cleared */
    int16_t events;   /* what to wait for: SYS_POLL_IN | SYS_POLL_OUT */
    int16_t revents;  /* what happened; the call fills this in */
} SysPollFd;

/* poll bits. ERR, HUP and NVAL are reported whether or not they were asked
 * for. A peer that closed shows up as SYS_POLL_IN with a read of 0, not as
 * HUP alone, so a reader only ever needs IN. */
#define SYS_POLL_IN   0x001
#define SYS_POLL_OUT  0x004
#define SYS_POLL_ERR  0x008
#define SYS_POLL_HUP  0x010
#define SYS_POLL_NVAL 0x020

/* ---- operations --------------------------------------------------------- */

/* Descriptors. Each returns what its POSIX namesake returns on success. */
int64_t sys_open(const char *path, int64_t flags, int64_t mode);  /* fd */
int64_t sys_read(int64_t fd, void *buf, int64_t n);               /* bytes, 0 at end */
int64_t sys_write(int64_t fd, const void *buf, int64_t n);        /* bytes, may be short */
int64_t sys_close(int64_t fd);                                    /* 0 */
int64_t sys_lseek(int64_t fd, int64_t off, int64_t whence);       /* new offset */
int64_t sys_fstat(int64_t fd, SysStat *st);                       /* 0 */
int64_t sys_isatty(int64_t fd);                                   /* 1 or 0, never fails */

/* The file system, relative to the current directory. */
int64_t sys_mkdir(const char *path, int64_t mode);                /* 0 */
int64_t sys_unlink(const char *path);                             /* 0 */
int64_t sys_rmdir(const char *path);                              /* 0 */
int64_t sys_rename(const char *from, const char *to);             /* 0 */
int64_t sys_symlink(const char *target, const char *path);        /* 0 */

/* stat by name. `follow` nonzero follows a final symlink (stat); zero
 * reports the link itself (lstat), which is what lets a tree walk refuse to
 * descend through one. A path, not a descriptor, because opening a file to
 * fstat it needs read permission the question does not, and opening a FIFO
 * blocks. */
int64_t sys_stat(const char *path, int64_t follow, SysStat *st);  /* 0 */

/* The names in a directory, except "." and "..", in the file system's
 * order, each followed by one NUL byte, written into buf as far as `cap`
 * allows. Returns the number of bytes the whole listing needs: if that is
 * more than `cap`, buf holds an incomplete listing and the caller asks
 * again with a buffer at least that large (the directory may have grown in
 * between, so it loops).
 *
 * One stateless call rather than an open/next/close triple, because the
 * two backends keep directory state in incompatible places: the kernel's
 * getdents64 cursor lives in a descriptor, the C library's in a DIR * that
 * owns one. A whole listing per call has no handle for either to leak. */
int64_t sys_listdir(const char *path, char *buf, int64_t cap);    /* bytes needed */

/* ---- sockets ------------------------------------------------------------
 *
 * A socket is a descriptor like any other: sys_read, sys_write and sys_close
 * work on one, and that is the whole reason these return descriptors instead
 * of a handle of their own. Every socket is close-on-exec (above).
 *
 * Reading a stream socket returns 0 exactly when the peer has shut down its
 * writing end -- end of file, the same value a file gives at its end -- so a
 * reader written for files reads a connection unchanged.
 *
 * The one thing a socket does that a file does not is raise SIGPIPE: writing
 * to a connection whose peer has gone away kills the process by default.
 * This layer does not install signal handlers (docs/sys-layer.md §2 lists
 * rt_sigaction as not built yet), so a `net` module must set SIGPIPE to
 * ignore at startup and read the -SYS_EPIPE instead. Until it can, a program
 * that writes to a dead peer dies. This is written down rather than worked
 * around because there is no honest way to work around it from here. */

/* A new socket. `type` is SYS_SOCK_STREAM or SYS_SOCK_DGRAM, optionally
 * OR'd with SYS_SOCK_NONBLOCK; `protocol` is 0 for the type's usual one. */
int64_t sys_socket(int64_t domain, int64_t type, int64_t protocol);  /* fd */

/* Take a local address. Port 0 asks the kernel to choose one, which
 * sys_getsockname then reports -- that is how a test, or a server that
 * advertises its port, gets one without guessing. */
int64_t sys_bind(int64_t fd, const SysAddr *addr);                   /* 0 */

/* Start accepting. `backlog` is a hint at how many connections may wait. */
int64_t sys_listen(int64_t fd, int64_t backlog);                     /* 0 */

/* The next waiting connection, as a new close-on-exec descriptor. `peer`
 * may be NULL; otherwise it is filled with the address that connected.
 *
 * The new descriptor does NOT inherit the listening socket's non-blocking
 * flag -- POSIX leaves that unspecified and Linux does not inherit it -- so
 * a server that wants one sets SYS_SO_NONBLOCK on what it gets back. On a
 * non-blocking LISTENER, accept itself fails with -SYS_EAGAIN when nothing
 * is waiting; the two flags are independent. */
int64_t sys_accept(int64_t fd, SysAddr *peer);                       /* fd */

/* Connect to a peer. On a non-blocking socket this returns -SYS_EINPROGRESS
 * at once; the caller then waits for SYS_POLL_OUT and reads SYS_SO_ERROR to
 * learn whether it worked. */
int64_t sys_connect(int64_t fd, const SysAddr *addr);                /* 0 */

/* End one or both directions: SYS_SHUT_WR sends the peer an end of file
 * while this side can still read the answer, which is how a client says "no
 * more requests" without losing the last response. */
int64_t sys_shutdown(int64_t fd, int64_t how);                       /* 0 */

/* One option, one int64 value. `opt` is a SYS_SO_* or SYS_TCP_* above, which
 * names the level too. sys_getsockopt returns the value, so a negative
 * result is always a failed call -- see SYS_SO_ERROR's note above. */
int64_t sys_setsockopt(int64_t fd, int64_t opt, int64_t value);      /* 0 */
int64_t sys_getsockopt(int64_t fd, int64_t opt);                     /* the value */

/* This end's address and the far end's. An unbound or unconnected socket
 * gives a zeroed address of its family, or -SYS_ENOTCONN for the peer. */
int64_t sys_getsockname(int64_t fd, SysAddr *addr);                  /* 0 */
int64_t sys_getpeername(int64_t fd, SysAddr *addr);                  /* 0 */

/* Wait until one of `n` descriptors is ready, or `timeout_ms` passes (0
 * returns at once, negative waits forever). Returns how many entries came
 * back with a non-zero `revents`, or 0 if the timeout ran out.
 *
 * poll and not epoll, on purpose. epoll is faster above a few hundred
 * descriptors -- it reports only the ready ones instead of rescanning the
 * set -- but it is three calls and a descriptor whose lifetime something has
 * to own, it is Linux-only so the libc backend would need a kqueue twin to
 * keep the same shape on macOS, and its advantage only appears once a
 * long-lived registration set exists to amortise. The scheduler in
 * docs/concurrency-decision.md is what will have such a set; when it lands,
 * epoll belongs BESIDE this call as a Linux fast path, not instead of it.
 * One stateless call with nothing to leak is the right primitive first.
 *
 * Interruption is handled here: a signal that arrives mid-wait restarts the
 * call rather than surfacing -SYS_EINTR, because a caller that must reason
 * about EINTR is a caller that will get it wrong, and the layer already
 * hides it in sys_getrandom. The restart offers the FULL timeout again
 * rather than what was left of it, in both backends, so a heavily signalled
 * process can wait longer than it asked. Matching the two backends matters
 * more than the accuracy: a deadline a caller cares about is one it should
 * be checking against sys_clock_ns anyway. */
int64_t sys_poll(SysPollFd *fds, int64_t n, int64_t timeout_ms);     /* ready count */

/* Turn a host name into addresses. Writes up to `cap` of them into `out`
 * with `port` filled in, and returns how many the name HAS -- so a caller
 * whose array was too small asks again with a larger one, exactly as
 * sys_listdir does. `family` narrows the answer to SYS_AF_INET or
 * SYS_AF_INET6; 0 accepts either.
 *
 * THIS IS THE ONE CALL THE RAW BACKEND REFUSES. It returns -SYS_ENOSYS
 * there, always, and a program can handle that: it is a named errno in the
 * layer's numbering like any other, so `from_errno` turns it into an error
 * value rather than a crash.
 *
 * The reason is not laziness. Resolving a name is not a system call; it is
 * getaddrinfo, and on glibc getaddrinfo is the NSS machinery, which dlopens
 * libnss_* at run time to read /etc/hosts, /etc/resolv.conf, mDNS and
 * whatever else nsswitch.conf names. A statically linked binary with no C
 * library cannot dlopen anything, so this can never work on the raw backend
 * -- not "is not written yet", cannot. docs/sys-layer.md §9 says what
 * replaces it: a DNS client in language source over UDP sockets, which is
 * the same choice Go made (a pure-Go resolver, with cgo's getaddrinfo only
 * as a fallback) and the reason a static Go binary resolves names at all. */
int64_t sys_resolve(const char *host, int64_t port, int64_t family,
                    SysAddr *out, int64_t cap);                      /* how many exist */

/* Nanoseconds on SYS_CLOCK_REALTIME (since the epoch) or
 * SYS_CLOCK_MONOTONIC (since an arbitrary start). */
int64_t sys_clock_ns(int64_t clock);

/* Fills all n bytes from the kernel's random source, retrying short reads
 * and interruptions; returns n. Blocks only until the pool is initialised
 * at boot. */
int64_t sys_getrandom(void *buf, int64_t n);

/* Ends the process at once: no atexit handlers, no stdio flush. The runtime
 * flushes its own output before calling this. */
_Noreturn void sys_exit(int64_t code);

#endif /* RT_SYS_H */
