# The sys layer — everything below the language, in one place

`docs/stdlib-seam.md` §5 set the rule: the standard library is written in
the language all the way down to the operating system, and `prim` exists
only where no language can reach. This document is what sits on the other
side of that line. It is Go's `syscall.Syscall` in our terms: one small,
explicit C interface, `runtime/sys.h`, with two implementations chosen at
build time.

| | file | selected by | reaches the kernel through |
|---|---|---|---|
| **libc** | `runtime/sys_libc.c` | default | the C library's POSIX functions |
| **raw** | `runtime/sys_linux.c` | `-DRT_SYS_RAW` | `syscall` / `svc #0` / `ecall`, inline assembly |

Decided **2026-09-21**. The recommendation (§7) is libc everywhere by
default and raw system calls as an opt-in Linux backend, both behind the
same functions.

---

## 1. The shape

### One convention: a value, or -errno

Every function returns an `int64_t`: non-negative on success, **-errno** on
failure — exactly what the Linux kernel returns from a system call. No
global `errno` is read or written anywhere above the layer.

Two reasons, and the second is the one that decides it:

  - **It is what the raw backend gets for free.** The kernel already
    returns -errno in a register; any other convention would be a
    translation in the one file whose whole point is having none.
  - **It is what a `prim` can carry.** A primitive returns a scalar, a
    `str`, or pushes onto a collection it was handed — never an `Option` or
    a `Result` (`stdlib-seam.md` §2). One `int` that is either the answer
    or a negative errno is the whole result in one scalar; the library
    builds its `Result` in source from the sign.

The libc backend turns C's `-1`-and-`errno` into the same thing, in one
helper (`ret`).

### One numbering: Linux's, on every host

The errno *values* are Linux's (`SYS_ENOENT` = 2, `SYS_EAGAIN` = 11, …) on
every host, not whatever the host's C library uses. On Linux both backends
agree by construction. On macOS and the BSDs the common values — the three
`lib/io.src` names, `ENOENT` 2, `EACCES` 13, `EISDIR` 21 — happen to match,
but others do not (`EAGAIN` is 35 on macOS, `ENOSYS` 78), so the libc
backend translates through a `switch` on the host's symbolic names. On
Linux every case of that switch is the identity; it is compiled and run by
the corpus anyway, so the code a macOS build depends on is not dead code
here. A host errno with no case passes through unchanged — still an error,
just in the host's numbering.

This is what lets `from_errno` in `lib/io.src` be written once, with
numbers, for every target.

### Flags and clocks are the layer's own constants

`SYS_O_*`, `SYS_SEEK_*`, `SYS_CLOCK_*` and `SysStat` belong to the layer,
not to the host. They have to: `O_DIRECTORY` is `0200000` on x86-64 and
`040000` on arm64; `CLOCK_MONOTONIC` is 1 on Linux and 6 on macOS; `struct
stat` has a different layout on every architecture. A number that crosses
into the language through a `prim` must mean the same thing on every
target, so each backend maps. The `SYS_O_*` values are x86-64 Linux's, which
makes the raw mapping the identity there and a single bit on arm64.

Close-on-exec is not a flag. Every descriptor the runtime opens has it,
because a descriptor leaking into a child process is never what a program
meant — Go makes the same choice in `os.OpenFile`.

### Why the implementation is `#include`d by `rt.c`

`run.sh`, `gates.sh` and anyone building by hand compile one runtime file,
`runtime/rt.c`, and `docs/ir-v0.md` §7.1 requires it to stay a translation
unit separate from the emitted program. Including the chosen backend from
`rt.c` keeps both true: the runtime is still one TU, still separate, and
the backend is a `-D` flag rather than a different list of files to
remember on every build line. The functions are extern, not `static`, so
the ones the runtime does not call yet do not warn, and a test can call
them all.

---

## 2. The operations

### Implemented now

| function | returns | libc | raw x86-64 | raw aarch64 / riscv64 |
|---|---|---|---|---|
| `sys_open(path, flags, mode)` | fd | `open` | `openat` 257 | `openat` 56 |
| `sys_read(fd, buf, n)` | bytes, 0 at end | `read` | `read` 0 | `read` 63 |
| `sys_write(fd, buf, n)` | bytes (may be short) | `write` | `write` 1 | `write` 64 |
| `sys_close(fd)` | 0 | `close` | `close` 3 | `close` 57 |
| `sys_lseek(fd, off, whence)` | new offset | `lseek` | `lseek` 8 | `lseek` 62 |
| `sys_fstat(fd, &SysStat)` | 0 | `fstat` | `statx` 332 | `statx` 291 |
| `sys_isatty(fd)` | 1 or 0 | `isatty` | `ioctl(TCGETS)` 16 | `ioctl(TCGETS)` 29 |
| `sys_mkdir(path, mode)` | 0 | `mkdir` | `mkdirat` 258 | `mkdirat` 34 |
| `sys_unlink(path)` | 0 | `unlink` | `unlinkat` 263 | `unlinkat` 35 |
| `sys_rmdir(path)` | 0 | `rmdir` | `unlinkat(AT_REMOVEDIR)` | same |
| `sys_rename(from, to)` | 0 | `rename` | `renameat2` 316 | `renameat2` 276 |
| `sys_symlink(target, path)` | 0 | `symlink` | `symlinkat` 266 | `symlinkat` 36 |
| `sys_stat(path, follow, &SysStat)` | 0 | `stat` / `lstat` | `statx` 332 | `statx` 291 |
| `sys_listdir(path, buf, cap)` | bytes needed | `open` + `fdopendir` + `readdir` | `openat` + `getdents64` 217 | `getdents64` 61 |
| `sys_clock_ns(clock)` | nanoseconds | `clock_gettime` | `clock_gettime` 228 | `clock_gettime` 113 |
| `sys_getrandom(buf, n)` | n (all of it) | `getentropy`, 256 at a time | `getrandom` 318 | `getrandom` 278 |
| `sys_exit(code)` | — | `_exit` | `exit_group` 231 | `exit_group` 94 |

Sockets, added **2026-09-23** as the foundation for a `net`/`http` module:

| function | returns | libc | raw x86-64 | raw aarch64 / riscv64 |
|---|---|---|---|---|
| `sys_socket(domain, type, proto)` | fd | `socket` | `socket` 41 | `socket` 198 |
| `sys_bind(fd, &SysAddr)` | 0 | `bind` | `bind` 49 | `bind` 200 |
| `sys_listen(fd, backlog)` | 0 | `listen` | `listen` 50 | `listen` 201 |
| `sys_accept(fd, &SysAddr)` | fd | `accept` + `fcntl` | `accept4` 288 | `accept4` 242 |
| `sys_connect(fd, &SysAddr)` | 0 | `connect` | `connect` 42 | `connect` 203 |
| `sys_shutdown(fd, how)` | 0 | `shutdown` | `shutdown` 48 | `shutdown` 210 |
| `sys_setsockopt(fd, opt, value)` | 0 | `setsockopt`, or `fcntl` | `setsockopt` 54 / `fcntl` 72 | `setsockopt` 208 / `fcntl` 25 |
| `sys_getsockopt(fd, opt)` | the value | `getsockopt`, or `fcntl` | `getsockopt` 55 / `fcntl` 72 | `getsockopt` 209 / `fcntl` 25 |
| `sys_getsockname(fd, &SysAddr)` | 0 | `getsockname` | `getsockname` 51 | `getsockname` 204 |
| `sys_getpeername(fd, &SysAddr)` | 0 | `getpeername` | `getpeername` 52 | `getpeername` 205 |
| `sys_poll(&SysPollFd, n, ms)` | how many ready | `poll` | `ppoll` 271 | `ppoll` 73 |
| `sys_resolve(host, port, af, out, cap)` | how many exist | `getaddrinfo` | **refused**, `-ENOSYS` | same |

A socket is a descriptor, so `sys_read`, `sys_write` and `sys_close` are the
rest of the interface; there is no `sys_send` or `sys_recv`. Reading a stream
socket gives 0 exactly when the peer has half-closed, which is the same value
a file gives at its end, so a reader written for files reads a connection
unchanged. Every socket is close-on-exec, like every other descriptor here.

Numbers are from `/usr/include/asm/unistd_64.h` (x86-64) and
`/usr/include/asm-generic/unistd.h` (the generic table arm64 and riscv64
share), checked on this machine.

The raw column uses only the `*at` forms and `statx`, on x86-64 too. The
generic syscall table has no `open`, `mkdir`, `unlink` or `rename` — they were
left out when it was designed, in favour of the `*at` calls — and riscv64
does not even have `renameat`, only `renameat2`. Using the forms every
architecture has means one code path instead of three. `statx` has one
struct layout on every architecture; `struct stat` has a different one on
each.

The sockets follow the same rule twice more. `accept4` rather than `accept`,
because the layer's descriptors are close-on-exec and `accept4` is the only
form that can say so without a second call. `ppoll` rather than `poll`,
because **the generic table has no `poll` at all** — it went out with the
other pre-`*at` calls — so x86-64 uses `ppoll` too and there is one code path
again. `ppoll`'s extra arguments are a NULL signal mask, which the kernel
short-circuits before it looks at the mask size.

`ppoll` writes the *remaining* time back through its `timespec`, which is why
glibc's `ppoll` copies the caller's; the raw backend rebuilds its own on every
retry, so that a signal restarts the wait with the full timeout in both
backends rather than the remainder in one and the full value in the other.

`sys_isatty` exists for the runtime's stdout buffer (§3), and `sys_rmdir`
because the layer's own test cannot clean up after `sys_mkdir` without it.

`sys_stat`, `sys_symlink` and `sys_listdir` arrived with `lib/fs.src`.
`sys_stat` takes a path rather than opening the file and calling
`sys_fstat`, because opening needs read permission the question does not,
and opening a FIFO blocks; `follow` is zero for `lstat`, which is what lets
`fs.walk` and `fs.rmtree` refuse to descend through a link. `sys_symlink`
exists so that the tests of that refusal can make the links they refuse.

`sys_listdir` is one stateless call per listing: every name except `.` and
`..`, each followed by a NUL, written into the caller's buffer as far as
`cap` allows, and the return value is the size the whole listing needs, so
a caller whose buffer was too small asks again with a larger one. The
alternative, an open/next/close triple, fails on the two backends keeping
directory state in incompatible places -- the kernel's `getdents64` cursor
in a descriptor, the C library's in a `DIR *` that owns one -- and would
hand the language a handle to leak. The libc backend opens with
`O_CLOEXEC` itself and hands the descriptor to `fdopendir`, so it is
close-on-exec whatever the C library's `opendir` does. Order is the file
system's; `fs.listdir` sorts.

### Addresses: one record, and who does the `htons`

This is the part that needed a decision rather than a syscall number. The
kernel has a different address struct per family — `sockaddr_in` is 16 bytes,
`sockaddr_in6` is 28 with a flow label and a scope id wedged into the middle,
`sockaddr_un` is 110 — and the raw backend cannot borrow libc's headers for
any of them. So the layer defines **one record for every family** and each
backend converts at its own boundary:

```c
typedef struct {
    int64_t       family;    /* SYS_AF_INET, SYS_AF_INET6 or SYS_AF_UNIX */
    int64_t       port;      /* host order, 0..65535; unused for AF_UNIX */
    unsigned char addr[16];  /* wire order: 4 bytes for INET, 16 for INET6 */
    char          path[108]; /* AF_UNIX only, NUL-terminated; "" if unbound */
} SysAddr;
```

Four choices in it, each with a reason:

  - **One record, not a union or a family-per-call.** A union needs a tagged
    access rule obeyed in two backends and in the language above; a
    `sys_bind_unix` beside `sys_bind` doubles four functions. 124 bytes of
    struct is cheaper than either, and nothing above the line ever has to tell
    `sockaddr_in` from `sockaddr_in6` — the conversion is four short functions
    per backend and nothing anywhere else.

  - **`port` is a plain integer, in the machine's own order.** *The layer does
    the `htons`*, in the backend, at the boundary, and nothing above this line
    ever byte-swaps anything: a program says 8080 and means 8080. The raw
    backend has no `htons` to call, so its kernel structs declare the port as
    **two bytes** rather than a `uint16_t` and write the high one first — which
    is the wire order on a host of either endianness, with no swap at all.

  - **`addr` is in wire order**, which is big-endian, which is also the order
    the text forms are written in: `127.0.0.1` is the bytes `{127,0,0,1}` and
    an IPv6 address is its 16 bytes left to right. So the backends copy it
    straight through, and the `net` module's address parser and printer need
    no conversion either. Storing it host-ordered would buy nothing and cost a
    swap in four places.

  - **`path` is 108 bytes and too-long paths are refused, not truncated**,
    because a truncated path names a *different socket*. 108 is Linux's
    `sun_path`; the libc backend checks against the host's own, which is 104
    on macOS and the BSDs. Linux's abstract namespace (a path whose first byte
    is NUL) has no representation here and reads back as `""`: the layer's
    path is a C string, and it does not offer what it cannot round-trip.

Socket options work the same way — the layer's own dense numbering, where each
id names a *level and an option together* (`SYS_SO_REUSEADDR` is
`(SOL_SOCKET, SO_REUSEADDR)`, `SYS_TCP_NODELAY` is `(IPPROTO_TCP,
TCP_NODELAY)`). Folding the level in removes an argument that only ever had
one right value per option, and it is what lets every option's value be a
single `int64_t`: timeouts are **milliseconds**, the same unit `sys_poll`
takes, so nothing above the line builds a `struct timeval` or passes a
`socklen_t`. Booleans read back as exactly 0 or 1 from both backends — the
kernel is happy to return 4 for a "true" `SO_KEEPALIVE`, and two backends that
disagreed on that would be exactly the drift this layer exists to prevent.

`SYS_SO_ERROR` is the one value that is **positive**: it is an option's value
and not a result, so `111` means the connection was refused while a negative
return still means the `getsockopt` itself failed. Without that split,
`-SYS_EBADF` could not be told from "the peer refused".

There is deliberately **no `SYS_SOCK_CLOEXEC`**, for the same reason `sys_open`
has no `O_CLOEXEC` (§1): close-on-exec is not a flag in this layer, it is what
every descriptor gets. A listening socket inherited by a child is how a port
stays bound after a server exits, which is never what a program meant.
`SYS_SOCK_NONBLOCK` is a flag, because that one really is a choice.

The one option that is not a socket option is **`SYS_SO_NONBLOCK`**, which is
the descriptor's `O_NONBLOCK` reached through `fcntl`. It is folded into the
option table anyway, because `accept` does not pass the listener's flag on to
the connection it returns — POSIX leaves that unspecified and Linux does not —
and accepted sockets are precisely the ones a server must not block on, so
without it the layer can create a non-blocking socket and never obtain one.
A function of its own for a single boolean would be the worse trade. Both
backends read the flag word and put one bit back rather than assigning it,
because `F_SETFL` takes the whole word and clobbering the access mode with it
would leave a different file behind.

### Readiness: `sys_poll`, and why not `epoll`

One call that waits on a set of descriptors with a timeout, and nothing else.

`epoll` is faster above a few hundred descriptors — it reports the ready ones
instead of rescanning the set — but it is three calls and a descriptor whose
lifetime something has to own and can leak, it is Linux-only so the libc
backend would need a `kqueue` twin to keep the same shape on macOS, and its
advantage only appears once there is a long-lived registration set to amortise
it over. The green-thread scheduler in `docs/concurrency-decision.md` is what
will have such a set. When it lands, **`epoll` belongs beside this call as a
Linux fast path, not instead of it** — the same relationship the raw backend
has to libc. One stateless call with nothing to leak is the right primitive to
have first.

`SysPollFd` is deliberately the kernel's own `struct pollfd` — a 32-bit fd and
two 16-bit masks — rather than the `int64` triple the rest of the layer would
suggest. `sys_poll` takes an *array*, and converting an array needs somewhere
to put the converted copy: the layer has no allocator, and the raw backend is
built freestanding, so a fixed-size bounce buffer would put an arbitrary
ceiling on how many descriptors a server may wait on. Matching the layout
means there is nothing to convert. The assumption is not *assumed*: both
backends `_Static_assert` the size and the field offsets, and the libc backend
checks them against the host's real `struct pollfd`, so a host that disagreed
would fail to build rather than quietly scribble.

`EINTR` is hidden: a signal mid-wait restarts the call. A caller that has to
reason about `EINTR` is a caller that will get it wrong, and `sys_getrandom`
already hides it.

### Name resolution: `sys_resolve`, refused on raw

**Decided: the layer exposes `sys_resolve` over libc's `getaddrinfo` and
refuses it on the raw backend with `-SYS_ENOSYS`. That is what is built. The
recommendation for the long run is the other option — a DNS client in language
source over UDP sockets — and the two are not in conflict: the small one is
what exists now, and it is the one that has to be replaced, not extended.**

The refusal is not laziness and it is not temporary. Resolving a name is not a
system call. It is `getaddrinfo`, and on glibc `getaddrinfo` is the NSS
machinery, which **`dlopen`s `libnss_*` at run time** to obey
`/etc/nsswitch.conf` — files, DNS, mDNS, whatever else is configured. A
statically linked binary with no C library cannot `dlopen` anything, so there
is nothing here to port: not "is not written yet", *cannot*. glibc says the
same thing about its own static builds.

Go reached this exact fork and took the other branch: a pure-Go resolver that
reads `/etc/resolv.conf` and speaks DNS over UDP and TCP itself, with cgo's
`getaddrinfo` kept only as a fallback (the `netgo`/`netcgo` build tags). That
is the *only* reason a static Go binary can resolve a name at all. It is also
several thousand lines — a message encoder, a response parser, search-domain
and `/etc/hosts` handling, timeouts and retries across multiple servers — and
it is language-level work, not kernel work: once UDP sockets exist, which they
now do, none of it needs C. So it belongs in `lib/`, written in the language,
and this layer's job there is done.

Until then, the refusal has to be something a program can *act on*, which is
why it is `-SYS_ENOSYS` and not a crash: 38 is already the layer's number for
"this operation does not exist here", `from_errno` turns it into an ordinary
error value, and a `net` module can fall back to a literal address. Parsing a
literal is pure string work and deliberately **not** in the layer — the sys
layer makes system calls; turning `"127.0.0.1"` into four bytes is the
language's job, and it is the same job on both backends.

### Listed, not implemented yet

| operation | for | notes |
|---|---|---|
| argv, environment | `main`'s arguments | Not a system call: the kernel leaves them on the initial stack. Today the emitted `main(void)` drops them. Needs `main(int, char **)` to hand them to the runtime (libc) or `_start` to read them off the stack (no libc, §5). |
| `mmap` 9/222, `munmap` 11/215 | a runtime allocator | For §5's malloc replacement. |
| `clone` 56/220 (or `clone3` 435), `futex` 202/98 | carrier threads for green threads | `docs/concurrency-decision.md`. `clone` is the easy part; see §5. |
| `rt_sigaction` 13/134 **with a handler**, `sigaltstack` | stack-probe traps, preemption | When preemption lands. The SIGPIPE half is done — see below — and it is the half that needs no trampoline. |
| `getpid`, `kill`/`tgkill` | `abort` without libc | `rt_trap` still calls `abort()`. |
| `pipe2`, `dup3`, `execve`, `wait4` | spawning the C compiler | Self-hosting needs it (`stdlib-seam.md` §5). |
| `sendto` 44/206, `recvfrom` 45/207 | unconnected UDP | A datagram socket works today by `connect`ing it and using `sys_read`/`sys_write`, which is what a UDP client does. A UDP *server*, which must answer whoever wrote to it, needs the peer address per message. The DNS client above is the first caller that will. |
| `epoll_create1`, `epoll_ctl`, `epoll_wait` | a scheduler's readiness set | Beside `sys_poll`, not instead of it — see above. |
| `socketpair` 53/199 | a connected pair with no name | `sys_test.c` builds one over a filesystem path instead, which also tests `bind` and `connect`. Wanted once the runtime needs to wake itself. |

**SIGPIPE is the one sharp edge sockets add, and it is covered by exactly one
call.** Writing to a connection whose peer has gone away kills the process by
default. `sys_ignore_sigpipe()` sets that signal's disposition to ignore, so
the write returns `-SYS_EPIPE` instead; `lib/net.src` calls it before its
first write, and `sys_test.c` now checks the other half of `shutdown`'s
contract instead of explaining why it cannot.

*Done 2026-09-23, both backends.* It is **one disposition, not a general
`sigaction`**, and that narrowing is the whole reason it was affordable now
rather than with preemption. A general `sigaction` has to carry a function
pointer into the kernel, which on x86-64 means the **restorer trampoline**:
a few instructions whose only job is to invoke `rt_sigreturn` when a handler
returns, which libc normally supplies and a raw backend must write per
architecture. `SIG_IGN` is never *delivered* — the kernel drops the signal in
`sig_task_ignored` before any signal frame is built — so no restorer can ever
be reached, and `rt_sigaction` itself never checks `SA_RESTORER` (only
delivery does, in `setup_rt_frame`). The raw backend is therefore a kernel
`struct sigaction` laid out by hand (x86-64 has a `sa_restorer` field between
the flags and the mask; aarch64 and riscv64 do not, and both layouts are
`_Static_assert`ed) and one `sc()` call. It is checked on every backend the
gate can build, the freestanding aarch64 and riscv64 ones under qemu
included.

What is still owed is the *handler* half — a real `sigaction` with a
trampoline — and it is owed to preemption and stack-probe traps, not to
sockets.

---

## 3. What the runtime routes through it today

`lib/io.src` and `lib/fs.src` reach it through one primitive per
function (§8): `rt_open` is `sys_open`, `rt_read` is `sys_read`, and so on,
with the -errno passed through. The loops, the buffers and the errno policy
that used to be C here (`rt_file_read` and its four siblings) are language
source now.

`print` goes through it too. The runtime keeps its own 64 KiB stdout buffer
over `sys_write(1, …)` with stdio's rules, because program output ordering
has always depended on them:

  - fully buffered to a pipe or a file, flushed when full and at exit;
  - flushed after every line when stdout is a terminal (`sys_isatty`);
  - flushed before any write to stderr (`eprint`) and before a trap aborts,
    so the two streams interleave in program order;
  - one `print` appended under a lock as one line, so spawned threads
    interleave by line, the guarantee glibc's locked stdio gave.

It is faster than what it replaced, not slower: 10 million `print`s (half
integers, half strings) to a pipe took 0.51 s through `printf`/`puts` and
0.19 s through the new buffer, with either backend — integers are
formatted by hand, and there is no stdio lock-and-format per call. Output
is byte-identical.

`corpus/modules/sys-streams` checks the buffer: 14,000 lines (73 KB, more
than one buffer) with an `eprint` in the middle that must land exactly
between lines 6999 and 7000, and a file larger than the first read.

---

## 4. What is still not us

Selecting `-DRT_SYS_RAW` means the *operating-system operations* make no C
library call. The process still links the C library, for:

| still libc | why | how it goes away |
|---|---|---|
| `malloc`/`realloc`/`free` | every allocation | our allocator over `mmap` (§5) |
| `pthread_create`/`join`, mutexes | `spawn`, the stdout lock | green threads on `clone` + `futex` (§5) |
| `snprintf`, `strtod` | float printing and parsing | moves into the language (`stdlib-seam.md` §5: Ryu, Eisel–Lemire) |
| `abort`, `atexit`, constructors | traps, exit flush | `tgkill(SIGABRT)`; our own exit path once we own `_start` |
| `memcpy`, `memcmp`, `strlen`, `memchr` | everywhere | a dozen lines each; compilers emit calls to `memcpy`/`memset` even with `-ffreestanding`, so they must exist regardless |
| `_start`, `__libc_start_main` | process startup | §5 |

`runtime/sys_test.sh` proves the layer itself is free of the C library by
building it with `-nostdlib -static` — no C library linked at all — and
running it natively and for aarch64 and riscv64 under qemu. A raw backend
that quietly called libc would not link.

One *operation*, not one implementation detail, is missing from the raw
backend rather than merely routed differently: **`sys_resolve`**. It is the
only entry in §2's tables whose raw column says "refused", and §2 explains
why that is permanent. Everything else in this section is a dependency that
goes away with enough work; that one goes away only by being rewritten
somewhere else, in the language.

---

## 5. What stands between us and "no libc", honestly

System call stubs are the easy tenth. The rest is what Go's runtime is
made of, and each part is a project:

**An allocator.** `malloc` becomes our own: size classes, free lists, and
`mmap`/`munmap` for the arenas, as Go's `runtime/malloc.go` over
`runtime/mem_linux.go`. With non-atomic refcounting and no sharing
(`concurrency-decision.md`), a per-carrier-thread cache with no locking on
the fast path is the natural design. Hardest part: returning memory to the
OS without fragmenting (`madvise(MADV_DONTNEED)`).

**Threads.** `pthread_create` does more than `clone`: it allocates the
stack and guard, sets up thread-local storage, and arranges the join.
Without libc, TLS is ours: on x86-64 an `arch_prctl(ARCH_SET_FS)` per
thread pointing at our own thread block, on arm64 `tpidr_el0`. Go keeps its
current-goroutine pointer in exactly such a register slot. Blocking and
waking is `futex`. The green-thread scheduler needs all of this anyway, so
this is less extra work than it looks — but every libc function that
touches TLS (`errno`, the stack protector's canary at `%fs:0x28`) stops
working the moment we set `%fs` ourselves, which is why this is all or
nothing per process.

**Process startup.** `_start` receives `argc`, `argv`, `envp` and the
auxiliary vector on the initial stack, with the stack pointer 16-byte
aligned and *no* return address pushed — so on x86-64 a C function entered
directly is misaligned by 8, which is why `sys_test.c`'s `_start` carries
`force_align_arg_pointer`. The auxiliary vector is where the vDSO lives
(`AT_SYSINFO_EHDR`); without parsing it, `clock_gettime` traps into the
kernel (~hundreds of ns) instead of reading the clock from user space
(~20 ns) as glibc and Go (`runtime/vdso_linux_amd64.go`) do. The raw backend
traps today, deliberately.

**Standard I/O.** Done for what the runtime prints (§3). What remains is
float formatting, which is scheduled to become language source anyway.

**Signals.** Stack probes and preemption will want a signal *handler*;
`rt_sigaction` with one needs a restorer trampoline (`SA_RESTORER`) that libc
normally supplies — a few instructions of assembly per architecture. Setting
a *disposition* needs none of that and is done: `sys_ignore_sigpipe` (§2).

None of this is needed for the raw backend to be useful. All of it is
needed before "no libc" is true, and it is the same list whether the goal
is fully static binaries, the self-hosted compiler's own runtime, or a
native backend without a C compiler.

---

## 6. Platforms: where raw system calls are allowed at all

This is the part that decides the shape, and every claim here is checked
against a source rather than remembered.

**Linux: a stable system call ABI, promised to programs.** The kernel's own
documentation: *"The kernel to userspace interface is the one that
application programs use, the syscall interface. That interface is **very**
stable over time, and will not break."*
([stable-api-nonsense](https://www.kernel.org/doc/html/latest/process/stable-api-nonsense.html)).
Go's standard library relies on it — `syscall` on Linux is assembly that
executes `SYSCALL` directly
([src/syscall/asm_linux_amd64.s](https://github.com/golang/go/blob/master/src/syscall/asm_linux_amd64.s)),
which is what lets a pure-Go Linux binary be fully static.

**macOS: no stable system call interface; libSystem is the ABI.** Apple:
*"Apple does not support statically linked binaries on Mac OS X. A
statically linked binary assumes binary compatibility at the kernel system
call interface, and we do not make any guarantees on that front."*
([Technical Q&A QA1118](https://developer.apple.com/library/archive/qa/qa1118/_index.html)).
Go gave in, in two steps. [Go 1.11](https://go.dev/doc/go1.11): *"On macOS
and iOS, the runtime now uses `libSystem.dylib` instead of calling the
kernel directly … The syscall package still makes direct system calls;
fixing this is planned for a future release."* [Go 1.12](https://go.dev/doc/go1.12):
*"`libSystem` is now used when making syscalls on Darwin, ensuring
forward-compatibility with future versions of macOS and iOS."*

**Windows: no public system call interface at all.** The documented ABI is
`kernel32.dll` and friends; `ntdll.dll`'s system call numbers are an
implementation detail and change between releases. From j00ru's table of
the actual numbers
([windows-syscalls](https://github.com/j00ru/windows-syscalls), x64
`nt-per-system.json`): `NtReadFile` is 3 on XP through 7, 5 on 8.1, 6 on 10
and 11; `NtCreateUserProcess` is 170 on 7 SP1, 183 on 8.1, 187 on 10 1511,
201 on 10 22H2, 206 on 11 21H2 and 209 on 11 24H2 — a different number in
almost every feature release. Go's Windows runtime calls `kernel32.dll`
functions (`//go:cgo_import_dynamic runtime._CloseHandle CloseHandle%1
"kernel32.dll"`,
[src/runtime/os_windows.go](https://github.com/golang/go/blob/master/src/runtime/os_windows.go)).

**OpenBSD: system calls pinned to libc by the kernel.** [Go 1.16](https://go.dev/doc/go1.16):
*"On the 64-bit x86 and 64-bit ARM architectures on OpenBSD … system calls
are now made through `libc`, instead of directly using the `SYSCALL`/`SVC`
instruction … OpenBSD 6.9 onwards will require system calls to be made
through `libc` for non-static Go binaries."* OpenBSD then went further:
[`pinsyscalls(2)`](https://man.openbsd.org/pinsyscalls.2), *"first appeared
in OpenBSD 7.5"*, registers where libc's system call instructions are, and
*"any attempt to invoke a mismatched system call entry instruction will
result in a SIGABRT."* A raw `syscall` from our code would kill the process.

So Linux is the only mainstream kernel where the raw backend is legitimate,
and the only one where Go still does it.

---

## 7. The recommendation

**libc everywhere by default; raw system calls as an opt-in Linux backend;
both behind the same functions.** That is what is built.

  - The default must be the one that works on every platform the language
    will run on, and on three of the four above that is only the C
    library. The libc backend is POSIX, so it covers Linux, macOS and the
    BSDs as written.
  - The raw backend earns its place on Linux, where it is guaranteed to
    keep working: static binaries with no libc, the self-hosted runtime,
    and eventually a native backend with no C toolchain in the loop. It is
    tested on every commit, with the whole corpus on x86-64 and the layer's
    own test on aarch64 and riscv64 under qemu.
  - Both behind one header means nothing above the line knows which it
    got. The corpus passes identically with either.

**Windows** is the gap. MinGW provides `open`/`read`/`write` over the MSVC
runtime, but not `getentropy`, and `isatty`/`O_CLOEXEC`/`mkdir(path, mode)`
differ. A Windows port is a third implementation file behind the same
header — `sys_win32.c` over `kernel32` (`CreateFileW`, `ReadFile`,
`BCryptGenRandom`, `QueryPerformanceCounter`) — and it is where the errno
translation earns its keep, since Windows reports `GetLastError` codes. It
is not attempted until a Windows target is.

---

## 8. Where `lib/io.src` goes next

> **Done 2026-09-21.** The list that was built, and why it differs from the
> proposal below, is `docs/stdlib-seam.md` §7. In short: `__read`,
> `__write`, `__write_str`, `__open`, `__close`, `__seek`, `__fstat`,
> `__mkdir`, `__unlink`, `__rmdir` and `__rename` as proposed; `__stat`,
> `__symlink` and `__listdir` added for `fs`; `__out_flush` added because
> `print`'s buffer is the runtime's and must be emptied before `io` writes
> to the same descriptor; `__isatty`, `__clock_ns`, `__random` and `__exit`
> not added, because `io` does not use them and `date`, `random` and `os`
> keep their own primitives until they move. The prim-rule amendment for
> writing into a caller's `bytes` is stdlib-seam.md §2.

When the mutable `bytes` type lands, `io` moves from whole-file primitives
down to descriptors, and the reading loop, buffering and line splitting move
into language source. The primitives it will declare — each a thin wrapper
over one `sys_*` function, keeping the layer's convention of a value or
**-errno**:

```c
prim int  __open(str path, int flags, int mode);           // fd, or -errno
prim int  __read(int fd, bytes buf, int off, int n);        // bytes read, 0 at end, or -errno
prim int  __write(int fd, bytes buf, int off, int n);       // bytes written (may be short), or -errno
prim int  __write_str(int fd, str s, int off, int n);       // the same, from an immutable str
prim int  __close(int fd);                                  // 0, or -errno
prim int  __seek(int fd, int off, int whence);              // new offset, or -errno
prim int  __fstat(int fd, List<int> out);                   // pushes size, mode, mtime_ns; 0 or -errno
prim int  __isatty(int fd);                                 // 1 or 0
prim int  __mkdir(str path, int mode);                      // 0, or -errno
prim int  __unlink(str path);                               // 0, or -errno
prim int  __rmdir(str path);                                // 0, or -errno
prim int  __rename(str from, str to);                       // 0, or -errno
prim int  __clock_ns(int clock);                            // nanoseconds, or -errno
prim int  __random(bytes buf, int off, int n);              // n, or -errno
prim void __exit(int code);
```

Points the `bytes` design has to settle for these to exist:

  - **`__read` and `__random` write into a buffer the caller owns.** Today
    a `prim` may only push onto a collection it was handed
    (`stdlib-seam.md` §2); writing into `bytes` in place is a new kind of
    mutation across the seam and needs that rule amended, with the bounds
    check (`off + n <= buf.size()`) done in the runtime wrapper, never
    trusted from the caller.
  - **`__write_str` exists because writing never needs mutation.** `str`
    is already an immutable byte sequence; converting it to `bytes` just to
    write it would copy every string a program prints to a file.
  - **`__fstat` pushes onto a `List<int>`** because a prim cannot return a
    struct; `io` builds its own `Stat` type from the three values.
  - **The flag and constant values** (`SYS_O_*`, `SYS_SEEK_*`,
    `SYS_CLOCK_*`) are written in `lib/io.src` as numbers, which is safe
    precisely because they are the layer's constants and not the host's.
  - **`from_errno` takes `-r`.** The three named errnos stay 2, 13 and 21,
    now guaranteed on every host by §1 rather than by coincidence.

---

## 9. Where a `net` module starts

The layer side of sockets is done (§2). What is *not* written here, on
purpose, is a line of the module above it: no `lib/net.src`, and no `prim`s
for one. This is the list that module will want, in the same shape as §8's
file primitives — each a thin wrapper over one `sys_*` function, keeping the
layer's convention of a value or **-errno**:

```c
prim int __socket(int domain, int type, int protocol);       // fd, or -errno
prim int __listen(int fd, int backlog);                      // 0, or -errno
prim int __shutdown(int fd, int how);                        // 0, or -errno

// An IP address crosses as (family, port, 16 bytes). The port is a plain
// integer: the sys layer does the htons (§2), and nothing in lib/ swaps.
prim int __bind(int fd, int family, int port, bytes addr);   // 0, or -errno
prim int __connect(int fd, int family, int port, bytes addr);

// AF_UNIX takes a path instead, so it gets its own pair rather than four
// arguments that are unused three quarters of the time.
prim int __bind_path(int fd, str path);                      // 0, or -errno
prim int __connect_path(int fd, str path);

// fd, or -errno; pushes the peer's family and port onto `out` and writes its
// 16 address bytes into `addr`.
prim int __accept(int fd, List<int> out, bytes addr);
prim int __sockname(int fd, int peer, List<int> out, bytes addr);  // 0, or -errno
prim str __sockpath(int fd, int peer);                       // AF_UNIX path, "" if unnamed

prim int __setsockopt(int fd, int opt, int value);           // 0, or -errno
prim int __getsockopt(int fd, int opt);                      // the value, or -errno

// How many are ready, or -errno. `fds` and `events` are read; `revents` is
// cleared and one value per descriptor is pushed onto it.
prim int __poll(List<int> fds, List<int> events, List<int> revents, int timeout_ms);

// How many addresses the name HAS, or -errno -- and -38 (ENOSYS) on the raw
// backend, always. Pushes family and port per address onto `out` and writes
// 16 bytes per address into `addrs`, which must be 16 x cap bytes.
prim int __resolve(str host, int port, int family, List<int> out, bytes addrs);
```

`sys_read`, `sys_write` and `sys_close` need no new primitives: a socket is a
descriptor, so `io`'s `__read`, `__write`, `__write_str` and `__close` (§8)
already work on one. That is most of the reason the layer returns descriptors
rather than a socket handle of its own.

Points the module has to settle, and the reasons they look like this:

  - **An address crosses as scalars plus `bytes`, never as a struct.** A prim
    returns a scalar or a `str`, or pushes onto a collection it was handed
    (`stdlib-seam.md` §2) — the same rule that made `__fstat` push three
    values onto a `List<int>`. `SysAddr` therefore comes apart at the seam:
    family and port push, the 16 address bytes are written in place under the
    §2 amendment, with `addr.size() >= 16` checked in the runtime wrapper and
    never trusted from the caller. `net` reassembles its own `Addr` type in
    source, and that type — not the layer — is what a program passes around.

  - **`__sockpath` is a second call, and only AF_UNIX pays for it.** One prim
    cannot both push the numbers and return the path, so the path is fetched
    separately by the one constructor that wants it. Charging every TCP
    accept for a `str` it will not read would be the worse trade.

  - **`__poll` takes three parallel `List<int>`s, not an array of records.**
    The runtime wrapper builds the `SysPollFd` array — it may allocate, the
    layer may not — so the language never sees a 32-bit fd or a 16-bit mask.
    `revents` is pushed rather than written, because its length is the
    answer's, not the caller's.

  - **`__resolve` is the one primitive whose result depends on the backend.**
    On raw it is always `-38`. `net` must therefore treat "cannot resolve" as
    an ordinary error on every path that uses a name, not as an impossibility
    — which is the right shape anyway, since DNS fails on real networks too.
    A program that must work on both backends connects to a literal address,
    parsed in language source (§2).

  - **The constant values** (`SYS_AF_*`, `SYS_SOCK_*`, `SYS_SO_*`,
    `SYS_POLL_*`, `SYS_SHUT_*`) are written in `lib/net.src` as numbers, which
    is safe for exactly the reason §8 gives for `SYS_O_*`: they are the
    layer's constants and not the host's, so 10 means IPv6 on every target.

  - **`net` must ignore SIGPIPE before its first write**, and now can (§2).
    That is a prerequisite, not a detail: without it a server dies the first
    time a client hangs up mid-response. `sys_ignore_sigpipe()` is the one
    signal call the layer offers, on both backends. `lib/net.src` makes it
    every time it opens a socket rather than once at startup, because a
    module cannot hold the "already done" flag
    (docs/module-state-decision.md); it is one idempotent system call per
    socket, which is nothing next to the connect or the accept beside it.
