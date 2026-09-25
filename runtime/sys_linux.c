/* The sys layer as raw Linux system calls -- selected with -DRT_SYS_RAW.
 *
 * #included by rt.c, never compiled on its own; the contract is in sys.h.
 * This file includes no C library header and calls no C library function:
 * each operation is a trap instruction with its arguments in registers.
 * That is possible on Linux and nowhere else, because Linux is the one
 * kernel that promises its system call numbers and calling convention to
 * programs rather than to its own C library (docs/sys-layer.md, Platforms).
 *
 * The kernel already returns -errno in Linux numbering, which is sys.h's
 * convention, so no result is translated. Only the layer's own flag bits
 * are, and only where an architecture disagrees with x86-64.
 *
 * Every call goes through one six-argument stub per architecture. Passing
 * zeros for unused arguments costs a register move each, which is noise
 * next to the trap, and it means there is exactly one piece of inline
 * assembly per architecture to get right.
 *
 * Deliberately NOT done here: the vDSO. glibc's clock_gettime reads the
 * clock in user space through a page the kernel maps into every process,
 * without trapping; this file traps. That is roughly 20 ns against a few
 * hundred, it matters only for a program that reads the clock in a hot
 * loop, and finding the vDSO means parsing the auxiliary vector, which is
 * process-startup work this backend does not own yet.
 */
#include "sys.h"

#if !defined(__linux__)
#error "RT_SYS_RAW is the raw Linux system call backend; this target is not Linux"
#endif

/* Syscall numbers. x86-64 has its own table; aarch64 and riscv64 share the
 * kernel's generic one (include/uapi/asm-generic/unistd.h). The generic
 * table has only the *at forms -- no open, mkdir, unlink or rename -- so
 * those are used on x86-64 too, with AT_FDCWD, and every architecture runs
 * the same code. renameat2 rather than renameat because riscv64 has only
 * the former. fstat is statx for the same reason one step further: struct
 * stat's layout differs per architecture, struct statx does not.
 *
 * Sockets follow the same rule twice more. accept4 rather than accept,
 * because the layer's descriptors are close-on-exec and accept4 is the only
 * form that can say so without a second call. ppoll rather than poll,
 * because the generic table has no poll at all -- it was left out with the
 * other pre-*at calls -- so x86-64 uses ppoll too and there is one code
 * path. ppoll's extra arguments are a NULL signal mask, which the kernel
 * short-circuits before it looks at the mask size. */
#if defined(__x86_64__)
#define NR_read          0
#define NR_write         1
#define NR_close         3
#define NR_lseek         8
#define NR_ioctl        16
#define NR_socket       41
#define NR_connect      42
#define NR_shutdown     48
#define NR_bind         49
#define NR_listen       50
#define NR_getsockname  51
#define NR_getpeername  52
#define NR_setsockopt   54
#define NR_getsockopt   55
#define NR_fcntl        72
#define NR_getdents64  217
#define NR_clock_gettime 228
#define NR_exit_group   231
#define NR_openat       257
#define NR_mkdirat      258
#define NR_unlinkat     263
#define NR_symlinkat    266
#define NR_ppoll        271
#define NR_accept4      288
#define NR_renameat2    316
#define NR_rt_sigaction  13
#define NR_getrandom    318
#define NR_statx        332
#elif defined(__aarch64__) || (defined(__riscv) && __riscv_xlen == 64)
#define NR_fcntl         25
#define NR_ioctl         29
#define NR_mkdirat       34
#define NR_unlinkat      35
#define NR_symlinkat     36
#define NR_openat        56
#define NR_close         57
#define NR_getdents64    61
#define NR_lseek         62
#define NR_read          63
#define NR_write         64
#define NR_ppoll         73
#define NR_exit_group    94
#define NR_clock_gettime 113
#define NR_socket       198
#define NR_bind         200
#define NR_listen       201
#define NR_connect      203
#define NR_getsockname  204
#define NR_getpeername  205
#define NR_setsockopt   208
#define NR_getsockopt   209
#define NR_shutdown     210
#define NR_accept4      242
#define NR_renameat2    276
#define NR_getrandom    278
#define NR_statx        291
#define NR_rt_sigaction 134
#else
#error "RT_SYS_RAW supports x86-64, aarch64 and riscv64 Linux; use the default libc backend here"
#endif

/* ---- the trap, once per architecture ----------------------------------- */

/* Arguments are bound to the registers the kernel reads them from with GNU
 * local register variables, the documented way to pin an asm operand to a
 * specific register in gcc and clang.
 *
 * The "memory" clobber is not optional: read, statx and friends write
 * through a pointer the compiler cannot see the kernel use, and without it
 * the compiler may keep a buffer's old contents in registers across the
 * call, or sink a store to a buffer we write() past it. */
static inline int64_t sc(int64_t n, int64_t a, int64_t b, int64_t c,
                         int64_t d, int64_t e, int64_t f) {
#if defined(__x86_64__)
    /* The syscall instruction itself overwrites rcx (return address) and
     * r11 (saved flags); the kernel preserves every other register. The
     * fourth argument goes in r10, not the C convention's rcx, for exactly
     * that reason. */
    register int64_t rax __asm__("rax") = n;
    register int64_t rdi __asm__("rdi") = a;
    register int64_t rsi __asm__("rsi") = b;
    register int64_t rdx __asm__("rdx") = c;
    register int64_t r10 __asm__("r10") = d;
    register int64_t r8  __asm__("r8")  = e;
    register int64_t r9  __asm__("r9")  = f;
    __asm__ volatile("syscall"
                     : "+r"(rax)
                     : "r"(rdi), "r"(rsi), "r"(rdx), "r"(r10), "r"(r8), "r"(r9)
                     : "rcx", "r11", "memory");
    return rax;
#elif defined(__aarch64__)
    /* Number in x8, arguments in x0-x5, result in x0. svc preserves every
     * other general register, so nothing but memory is clobbered. */
    register int64_t x8 __asm__("x8") = n;
    register int64_t x0 __asm__("x0") = a;
    register int64_t x1 __asm__("x1") = b;
    register int64_t x2 __asm__("x2") = c;
    register int64_t x3 __asm__("x3") = d;
    register int64_t x4 __asm__("x4") = e;
    register int64_t x5 __asm__("x5") = f;
    __asm__ volatile("svc #0"
                     : "+r"(x0)
                     : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5)
                     : "memory");
    return x0;
#else
    /* riscv64: number in a7, arguments in a0-a5, result in a0. */
    register int64_t a7 __asm__("a7") = n;
    register int64_t a0 __asm__("a0") = a;
    register int64_t a1 __asm__("a1") = b;
    register int64_t a2 __asm__("a2") = c;
    register int64_t a3 __asm__("a3") = d;
    register int64_t a4 __asm__("a4") = e;
    register int64_t a5 __asm__("a5") = f;
    __asm__ volatile("ecall"
                     : "+r"(a0)
                     : "r"(a7), "r"(a1), "r"(a2), "r"(a3), "r"(a4), "r"(a5)
                     : "memory");
    return a0;
#endif
}

#define P(x) ((int64_t)(intptr_t)(x))

#define AT_FDCWD       (-100)
#define AT_REMOVEDIR   0x200
#define AT_EMPTY_PATH  0x1000
#define AT_SYMLINK_NOFOLLOW 0x100
#define O_CLOEXEC_     02000000   /* the same on all three architectures */
#define TCGETS         0x5401     /* likewise */

/* No memcpy or memset by name: this file has no C library to take them from.
 * The compiler may still synthesise a call for a struct assignment, which is
 * why the freestanding test defines both (runtime/sys_test.c). */
static void zero_bytes(unsigned char *p, int64_t n) {
    for (int64_t i = 0; i < n; i++) p[i] = 0;
}

/* ---- descriptors ------------------------------------------------------- */

int64_t sys_open(const char *path, int64_t flags, int64_t mode) {
    int64_t k = flags & (SYS_O_ACCMODE | SYS_O_CREAT | SYS_O_EXCL | SYS_O_TRUNC | SYS_O_APPEND);
    if (flags & SYS_O_DIRECTORY) {
        /* The one open flag whose bit differs: arm64 kept the 32-bit ARM
         * value (arch/arm64/include/uapi/asm/fcntl.h). */
#if defined(__aarch64__)
        k |= 040000;
#else
        k |= 0200000;
#endif
    }
    return sc(NR_openat, AT_FDCWD, P(path), k | O_CLOEXEC_, mode, 0, 0);
}

int64_t sys_read(int64_t fd, void *buf, int64_t n) {
    return sc(NR_read, fd, P(buf), n, 0, 0, 0);
}

int64_t sys_write(int64_t fd, const void *buf, int64_t n) {
    return sc(NR_write, fd, P(buf), n, 0, 0, 0);
}

int64_t sys_close(int64_t fd) {
    return sc(NR_close, fd, 0, 0, 0, 0, 0);
}

int64_t sys_lseek(int64_t fd, int64_t off, int64_t whence) {
    return sc(NR_lseek, fd, off, whence, 0, 0, 0);
}

/* The kernel's struct statx (include/uapi/linux/stat.h), which has one
 * layout on every architecture. Only the fields read are named; the rest is
 * padding to the full 256 bytes the kernel may write. */
typedef struct {
    int64_t  tv_sec;
    uint32_t tv_nsec;
    int32_t  reserved;
} StatxTime;

typedef struct {
    uint32_t  mask, blksize;
    uint64_t  attributes;
    uint32_t  nlink, uid, gid;
    uint16_t  mode, pad0;
    uint64_t  ino, size, blocks, attributes_mask;
    StatxTime atime, btime, ctime, mtime;
    uint64_t  spare[16];
} Statx;

_Static_assert(sizeof(Statx) == 256, "struct statx is 256 bytes");
_Static_assert(__builtin_offsetof(Statx, size) == 40, "statx size offset");
_Static_assert(__builtin_offsetof(Statx, mtime) == 112, "statx mtime offset");

#define STATX_BASIC_STATS 0x7ff

static int64_t statx_into(int64_t dirfd, const char *path, int64_t flags, SysStat *st) {
    Statx sx;
    int64_t r = sc(NR_statx, dirfd, P(path), flags, STATX_BASIC_STATS, P(&sx), 0);
    if (r < 0) return r;
    st->size = (int64_t)sx.size;
    st->mode = sx.mode;
    st->mtime_ns = sx.mtime.tv_sec * 1000000000 + sx.mtime.tv_nsec;
    return 0;
}

int64_t sys_fstat(int64_t fd, SysStat *st) {
    return statx_into(fd, "", AT_EMPTY_PATH, st);
}

int64_t sys_stat(const char *path, int64_t follow, SysStat *st) {
    return statx_into(AT_FDCWD, path, follow ? 0 : AT_SYMLINK_NOFOLLOW, st);
}

/* isatty is "does the terminal ioctl succeed", which is also how every C
 * library implements it. The buffer is larger than the kernel's 36-byte
 * struct termios so no architecture's layout can overrun it. */
int64_t sys_isatty(int64_t fd) {
    unsigned char termios[64];
    return sc(NR_ioctl, fd, TCGETS, P(termios), 0, 0, 0) == 0 ? 1 : 0;
}

/* ---- the terminal -------------------------------------------------------
 *
 * Three ioctls. Their numbers are the same on all three architectures this
 * backend supports, and that is not luck: TCGETS, TCSETS and TIOCGWINSZ are
 * defined in include/uapi/asm-generic/ioctls.h, which x86-64, aarch64 and
 * riscv64 all include unchanged (arch/x86, arch/arm64 and arch/riscv have no
 * ioctls.h of their own). The architectures that DO renumber them -- mips,
 * powerpc, alpha, sparc -- are the same ones whose termios BITS differ, and
 * this backend supports none of them; the #error at the top of the file is
 * what keeps that honest.
 *
 * TCSETS and not TCSETSW or TCSETSF: TCSANOW, for the reason sys.h gives. */
#define TCSETS     0x5402
#define TIOCGWINSZ 0x5413

/* The kernel's struct termios (include/uapi/asm-generic/termbits.h), which
 * is what TCGETS fills and TCSETS reads -- NOT the C library's, which on
 * glibc is 60 bytes with the speeds appended and a 32-entry c_cc. The layout
 * is asserted rather than assumed, because getting it wrong would compile
 * and then scribble on a terminal. */
typedef struct {
    uint32_t      c_iflag, c_oflag, c_cflag, c_lflag;
    unsigned char c_line;
    unsigned char c_cc[SYS_NCCS];
} KTermios;

_Static_assert(sizeof(KTermios) == 36, "the kernel's struct termios is 36 bytes");
_Static_assert(__builtin_offsetof(KTermios, c_oflag) == 4, "termios c_oflag");
_Static_assert(__builtin_offsetof(KTermios, c_cflag) == 8, "termios c_cflag");
_Static_assert(__builtin_offsetof(KTermios, c_lflag) == 12, "termios c_lflag");
_Static_assert(__builtin_offsetof(KTermios, c_line) == 16, "termios c_line");
_Static_assert(__builtin_offsetof(KTermios, c_cc) == 17, "termios c_cc");
_Static_assert(SYS_NCCS == 19, "the kernel's NCCS is 19");

/* The kernel's struct winsize (include/uapi/asm-generic/termios.h): four
 * 16-bit fields, the same on every architecture. */
typedef struct {
    uint16_t ws_row, ws_col, ws_xpixel, ws_ypixel;
} KWinsize;

_Static_assert(sizeof(KWinsize) == 8, "struct winsize is 8 bytes");

/* No translation in either direction: the SYS_TC_* values in sys.h ARE the
 * kernel's, on every architecture this file builds for. The copy is still
 * written field by field rather than as a cast, because SysTermios is
 * 64-bit words and the kernel's is 32-bit ones. */
int64_t sys_tcget(int64_t fd, SysTermios *t) {
    KTermios k;
    zero_bytes((unsigned char *)&k, (int64_t)sizeof k);
    int64_t r = sc(NR_ioctl, fd, TCGETS, P(&k), 0, 0, 0);
    if (r < 0) return r;
    t->iflag = k.c_iflag;
    t->oflag = k.c_oflag;
    t->cflag = k.c_cflag;
    t->lflag = k.c_lflag;
    for (int i = 0; i < SYS_NCCS; i++) t->cc[i] = k.c_cc[i];
    return 0;
}

int64_t sys_tcset(int64_t fd, const SysTermios *t) {
    /* Read first, for the line discipline byte: it is the one field of the
     * kernel's struct that SysTermios does not model, and writing a zero
     * there would switch a terminal to N_TTY when it was something else.
     * The speeds need no such care here -- on Linux they live in c_cflag,
     * which this record carries through. */
    KTermios k;
    zero_bytes((unsigned char *)&k, (int64_t)sizeof k);
    int64_t r = sc(NR_ioctl, fd, TCGETS, P(&k), 0, 0, 0);
    if (r < 0) return r;
    k.c_iflag = (uint32_t)t->iflag;
    k.c_oflag = (uint32_t)t->oflag;
    k.c_cflag = (uint32_t)t->cflag;
    k.c_lflag = (uint32_t)t->lflag;
    for (int i = 0; i < SYS_NCCS; i++) k.c_cc[i] = t->cc[i];
    return sc(NR_ioctl, fd, TCSETS, P(&k), 0, 0, 0);
}

int64_t sys_winsize(int64_t fd, int64_t *rows, int64_t *cols) {
    KWinsize w;
    zero_bytes((unsigned char *)&w, (int64_t)sizeof w);
    int64_t r = sc(NR_ioctl, fd, TIOCGWINSZ, P(&w), 0, 0, 0);
    if (r < 0) return r;
    *rows = w.ws_row;
    *cols = w.ws_col;
    return 0;
}

/* ---- the file system --------------------------------------------------- */

int64_t sys_mkdir(const char *path, int64_t mode) {
    return sc(NR_mkdirat, AT_FDCWD, P(path), mode, 0, 0, 0);
}

int64_t sys_unlink(const char *path) {
    return sc(NR_unlinkat, AT_FDCWD, P(path), 0, 0, 0, 0);
}

int64_t sys_rmdir(const char *path) {
    return sc(NR_unlinkat, AT_FDCWD, P(path), AT_REMOVEDIR, 0, 0, 0);
}

int64_t sys_symlink(const char *target, const char *path) {
    return sc(NR_symlinkat, P(target), AT_FDCWD, P(path), 0, 0, 0);
}

int64_t sys_rename(const char *from, const char *to) {
    return sc(NR_renameat2, AT_FDCWD, P(from), AT_FDCWD, P(to), 0, 0);
}

/* struct linux_dirent64 (include/linux/dirent.h): an 8-byte inode, an
 * 8-byte cookie, a 2-byte record length at offset 16, a type byte, and the
 * NUL-terminated name from offset 19. Only the length and the name are
 * read. The length is assembled from its bytes in the machine's order
 * rather than read through a cast pointer, which would be an aliasing
 * violation on a char buffer. */
static int64_t reclen_at(const unsigned char *p) {
#if __BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__
    return (int64_t)p[16] | (int64_t)p[17] << 8;
#else
    return (int64_t)p[16] << 8 | (int64_t)p[17];
#endif
}

int64_t sys_listdir(const char *path, char *buf, int64_t cap) {
    int64_t fd = sys_open(path, SYS_O_RDONLY | SYS_O_DIRECTORY, 0);
    if (fd < 0) return fd;
    /* 4 KiB is what glibc's readdir asks for at a time. The largest record,
     * a 255-byte name, is 280 bytes, so every call makes progress. */
    unsigned char d[4096];
    int64_t need = 0;
    for (;;) {
        int64_t n = sc(NR_getdents64, fd, P(d), (int64_t)sizeof d, 0, 0, 0);
        if (n == -SYS_EINTR) continue;
        if (n < 0) {
            sys_close(fd);
            return n;
        }
        if (n == 0) break;
        for (int64_t at = 0; at < n; at += reclen_at(d + at)) {
            const char *name = (const char *)d + at + 19;
            if (name[0] == '.' && (name[1] == 0 || (name[1] == '.' && name[2] == 0))) continue;
            for (int64_t i = 0;; i++) {
                if (need < cap) buf[need] = name[i];
                need++;
                if (name[i] == 0) break;
            }
        }
    }
    sys_close(fd);
    return need;
}

/* ---- sockets ------------------------------------------------------------
 *
 * The kernel's socket constants, spelled out. Every one of them is the value
 * sys.h already chose for the layer, so all of this is the identity -- which
 * is the point of having chosen Linux's numbers there. They are written out
 * anyway rather than used implicitly, because the day a constant here stops
 * matching sys.h is the day someone needs to see both of them. */
#define K_SOCK_CLOEXEC  02000000  /* the same on all three architectures */
#define K_SOL_SOCKET    1
#define K_SO_REUSEADDR  2
#define K_SO_ERROR      4
#define K_SO_KEEPALIVE  9
/* SO_RCVTIMEO_OLD / SO_SNDTIMEO_OLD, which is what SO_RCVTIMEO and
 * SO_SNDTIMEO are defined to for 64-bit userspace in the kernel's
 * asm-generic/socket.h -- shared by all three architectures here. The value
 * they take is a struct __kernel_old_timeval: two 64-bit longs. */
#define K_SO_RCVTIMEO  20
#define K_SO_SNDTIMEO  21
#define K_IPPROTO_TCP   6
#define K_TCP_NODELAY   1
/* For SYS_SO_NONBLOCK, which is a descriptor flag and not a socket option
 * (sys.h). O_NONBLOCK is 04000 on all three architectures here. */
#define K_F_GETFL       3
#define K_F_SETFL       4
#define K_O_NONBLOCK    04000

/* The kernel's sockaddrs, one struct per family, laid out by hand.
 *
 * The port is TWO BYTES, not a uint16_t, so that writing the high byte first
 * is the network order the wire wants on any host, with no htons to call --
 * there is no C library here to call one from. The family is a native-order
 * uint16_t, as the kernel reads it. */
typedef struct {
    uint16_t      family;
    unsigned char port[2];
    unsigned char addr[4];
    unsigned char zero[8];
} KAddrIn;

typedef struct {
    uint16_t      family;
    unsigned char port[2];
    uint32_t      flowinfo;
    unsigned char addr[16];
    uint32_t      scope_id;
} KAddrIn6;

typedef struct {
    uint16_t family;
    char     path[108];
} KAddrUn;

/* Checked against include/uapi/linux/{in,in6,un}.h. A wrong offset here
 * would be a connection to the wrong port rather than a compile error, so
 * the compiler is made to check. */
_Static_assert(sizeof(KAddrIn) == 16, "struct sockaddr_in is 16 bytes");
_Static_assert(sizeof(KAddrIn6) == 28, "struct sockaddr_in6 is 28 bytes");
_Static_assert(sizeof(KAddrUn) == 110, "struct sockaddr_un is 110 bytes");
_Static_assert(__builtin_offsetof(KAddrIn6, addr) == 8, "sin6_addr offset");
_Static_assert(__builtin_offsetof(KAddrIn6, scope_id) == 24, "sin6_scope_id offset");
_Static_assert(__builtin_offsetof(KAddrUn, path) == 2, "sun_path offset");

/* All three begin with the same uint16_t family, so reading it back through
 * any member is a union's common initial sequence and not a type pun. */
typedef union {
    KAddrIn  v4;
    KAddrIn6 v6;
    KAddrUn  un;
} KAddr;

static void copy_bytes(unsigned char *d, const unsigned char *s, int64_t n) {
    for (int64_t i = 0; i < n; i++) d[i] = s[i];
}

/* A SysAddr as the kernel's sockaddr, with the length to pass. The only
 * place in this file that knows one family from another. */
static int64_t to_kaddr(const SysAddr *a, KAddr *k, int64_t *len) {
    zero_bytes((unsigned char *)k, (int64_t)sizeof *k);
    if (a->family != SYS_AF_UNIX && (a->port < 0 || a->port > 65535)) return -SYS_EINVAL;
    switch (a->family) {
    case SYS_AF_INET:
        k->v4.family = SYS_AF_INET;
        k->v4.port[0] = (unsigned char)((a->port >> 8) & 0xff);
        k->v4.port[1] = (unsigned char)(a->port & 0xff);
        copy_bytes(k->v4.addr, a->addr, 4);
        *len = (int64_t)sizeof k->v4;
        return 0;
    case SYS_AF_INET6:
        k->v6.family = SYS_AF_INET6;
        k->v6.port[0] = (unsigned char)((a->port >> 8) & 0xff);
        k->v6.port[1] = (unsigned char)(a->port & 0xff);
        copy_bytes(k->v6.addr, a->addr, 16);
        *len = (int64_t)sizeof k->v6;
        return 0;
    case SYS_AF_UNIX: {
        int64_t n = 0;
        while (n < (int64_t)sizeof a->path && a->path[n] != 0) n++;
        /* n == sizeof a->path means there was no NUL to find, so the path
         * cannot be terminated inside sun_path either. */
        if (n + 1 > (int64_t)sizeof k->un.path) return -SYS_ENAMETOOLONG;
        k->un.family = SYS_AF_UNIX;
        copy_bytes((unsigned char *)k->un.path, (const unsigned char *)a->path, n + 1);
        /* Exactly the bytes that matter, so the kernel takes the path as
         * ending where its NUL is rather than at the end of the struct. */
        *len = (int64_t)__builtin_offsetof(KAddrUn, path) + n + 1;
        return 0;
    }
    default:
        return -SYS_EAFNOSUPPORT;
    }
}

static int64_t from_kaddr(const KAddr *k, int64_t len, SysAddr *a) {
    zero_bytes((unsigned char *)a, (int64_t)sizeof *a);
    switch (k->v4.family) {
    case SYS_AF_INET:
        a->family = SYS_AF_INET;
        a->port = ((int64_t)k->v4.port[0] << 8) | k->v4.port[1];
        copy_bytes(a->addr, k->v4.addr, 4);
        return 0;
    case SYS_AF_INET6:
        a->family = SYS_AF_INET6;
        a->port = ((int64_t)k->v6.port[0] << 8) | k->v6.port[1];
        copy_bytes(a->addr, k->v6.addr, 16);
        return 0;
    case SYS_AF_UNIX: {
        /* An unbound socket comes back as the family alone: len stops before
         * sun_path and the path is left "". See sys_libc.c for why the
         * abstract namespace has no representation here. */
        int64_t head = (int64_t)__builtin_offsetof(KAddrUn, path);
        int64_t n = len > head ? len - head : 0;
        if (n > (int64_t)sizeof a->path - 1) n = (int64_t)sizeof a->path - 1;
        a->family = SYS_AF_UNIX;
        copy_bytes((unsigned char *)a->path, (const unsigned char *)k->un.path, n);
        a->path[n] = 0;
        return 0;
    }
    default:
        return -SYS_EAFNOSUPPORT;
    }
}

/* One of the layer's option ids as the kernel's (level, name), and the shape
 * of its value. Kept in the same order and the same shape as sys_libc.c's
 * opt_lookup, because the two must accept and refuse exactly the same set. */
static int opt_lookup(int64_t opt, int64_t *level, int64_t *name, int *is_ms) {
    *is_ms = 0;
    switch (opt) {
    case SYS_SO_REUSEADDR: *level = K_SOL_SOCKET;  *name = K_SO_REUSEADDR; return 0;
    case SYS_SO_KEEPALIVE: *level = K_SOL_SOCKET;  *name = K_SO_KEEPALIVE; return 0;
    case SYS_SO_ERROR:     *level = K_SOL_SOCKET;  *name = K_SO_ERROR;     return 0;
    case SYS_SO_RCVTIMEO:  *level = K_SOL_SOCKET;  *name = K_SO_RCVTIMEO;  *is_ms = 1; return 0;
    case SYS_SO_SNDTIMEO:  *level = K_SOL_SOCKET;  *name = K_SO_SNDTIMEO;  *is_ms = 1; return 0;
    case SYS_TCP_NODELAY:  *level = K_IPPROTO_TCP; *name = K_TCP_NODELAY;  return 0;
    default: return -1;
    }
}

int64_t sys_socket(int64_t domain, int64_t type, int64_t protocol) {
    /* The same validation, in the same order, as the libc backend: the two
     * must refuse the same arguments with the same errno, and leaving that
     * to the kernel here and to a switch there would not guarantee it. */
    if (domain != SYS_AF_UNIX && domain != SYS_AF_INET && domain != SYS_AF_INET6) {
        return -SYS_EAFNOSUPPORT;
    }
    if (type & ~(int64_t)(SYS_SOCK_TYPEMASK | SYS_SOCK_NONBLOCK)) return -SYS_EINVAL;
    int64_t t = type & SYS_SOCK_TYPEMASK;
    if (t != SYS_SOCK_STREAM && t != SYS_SOCK_DGRAM) return -SYS_EINVAL;
    /* SYS_SOCK_NONBLOCK is Linux's own SOCK_NONBLOCK bit (sys.h), so it is
     * passed through where it already sits. */
    t |= (type & SYS_SOCK_NONBLOCK) | K_SOCK_CLOEXEC;
    return sc(NR_socket, domain, t, protocol, 0, 0, 0);
}

int64_t sys_bind(int64_t fd, const SysAddr *addr) {
    KAddr k;
    int64_t len;
    int64_t r = to_kaddr(addr, &k, &len);
    if (r < 0) return r;
    return sc(NR_bind, fd, P(&k), len, 0, 0, 0);
}

int64_t sys_listen(int64_t fd, int64_t backlog) {
    if (backlog < 0) return -SYS_EINVAL;
    return sc(NR_listen, fd, backlog > 65535 ? 65535 : backlog, 0, 0, 0, 0);
}

/* accept4 with SOCK_CLOEXEC: the flag is set by the kernel as the descriptor
 * is created, so unlike the libc backend's accept-then-fcntl there is no
 * instant in which a concurrent exec could inherit the connection. */
int64_t sys_accept(int64_t fd, SysAddr *peer) {
    KAddr k;
    /* socklen_t is 32 bits and the kernel writes exactly 32 bits back
     * through this pointer; an int64_t here would leave four bytes of the
     * stack untouched and read as garbage. */
    uint32_t klen = (uint32_t)sizeof k;
    zero_bytes((unsigned char *)&k, (int64_t)sizeof k);
    int64_t c = sc(NR_accept4, fd, P(&k), P(&klen), K_SOCK_CLOEXEC, 0, 0);
    if (c < 0) return c;
    if (peer != 0) {  /* not NULL: that is stddef.h's, and this file has no headers */
        int64_t r = from_kaddr(&k, (int64_t)klen, peer);
        if (r < 0) {
            sys_close(c);
            return r;
        }
    }
    return c;
}

int64_t sys_connect(int64_t fd, const SysAddr *addr) {
    KAddr k;
    int64_t len;
    int64_t r = to_kaddr(addr, &k, &len);
    if (r < 0) return r;
    return sc(NR_connect, fd, P(&k), len, 0, 0, 0);
}

int64_t sys_shutdown(int64_t fd, int64_t how) {
    if (how != SYS_SHUT_RD && how != SYS_SHUT_WR && how != SYS_SHUT_RDWR) return -SYS_EINVAL;
    return sc(NR_shutdown, fd, how, 0, 0, 0, 0);
}

/* struct __kernel_old_timeval: two 64-bit longs, which is what SO_RCVTIMEO
 * and SO_SNDTIMEO take for 64-bit userspace. */
typedef struct { int64_t sec, usec; } KTimeval;

int64_t sys_setsockopt(int64_t fd, int64_t opt, int64_t value) {
    /* Answered before the option table, exactly as in sys_libc.c: read the
     * flags and put one bit back, never assign the whole word. */
    if (opt == SYS_SO_NONBLOCK) {
        int64_t fl = sc(NR_fcntl, fd, K_F_GETFL, 0, 0, 0, 0);
        if (fl < 0) return fl;
        fl = value != 0 ? (fl | K_O_NONBLOCK) : (fl & ~(int64_t)K_O_NONBLOCK);
        return sc(NR_fcntl, fd, K_F_SETFL, fl, 0, 0, 0);
    }
    int64_t level, name;
    int is_ms;
    if (opt_lookup(opt, &level, &name, &is_ms) != 0) return -SYS_EINVAL;
    if (opt == SYS_SO_ERROR) return -SYS_EINVAL;  /* read-only, as in sys_libc.c */
    if (is_ms) {
        if (value < 0) return -SYS_EINVAL;
        KTimeval tv;
        tv.sec = value / 1000;
        tv.usec = (value % 1000) * 1000;
        return sc(NR_setsockopt, fd, level, name, P(&tv), (int64_t)sizeof tv, 0);
    }
    int32_t v = value != 0;
    return sc(NR_setsockopt, fd, level, name, P(&v), (int64_t)sizeof v, 0);
}

int64_t sys_getsockopt(int64_t fd, int64_t opt) {
    if (opt == SYS_SO_NONBLOCK) {
        int64_t fl = sc(NR_fcntl, fd, K_F_GETFL, 0, 0, 0, 0);
        if (fl < 0) return fl;
        return (fl & K_O_NONBLOCK) != 0;
    }
    int64_t level, name;
    int is_ms;
    if (opt_lookup(opt, &level, &name, &is_ms) != 0) return -SYS_EINVAL;
    if (is_ms) {
        KTimeval tv = {0, 0};
        uint32_t n = (uint32_t)sizeof tv;
        int64_t r = sc(NR_getsockopt, fd, level, name, P(&tv), P(&n), 0);
        if (r < 0) return r;
        return tv.sec * 1000 + tv.usec / 1000;
    }
    int32_t v = 0;
    uint32_t n = (uint32_t)sizeof v;
    int64_t r = sc(NR_getsockopt, fd, level, name, P(&v), P(&n), 0);
    if (r < 0) return r;
    /* SO_ERROR is already a Linux errno, and already positive, which is what
     * sys.h promises -- the libc backend has to translate to reach the same
     * number. Everything else is normalised to 0 or 1 so the two backends
     * compare equal. */
    if (opt == SYS_SO_ERROR) return v;
    return v != 0;
}

int64_t sys_getsockname(int64_t fd, SysAddr *addr) {
    KAddr k;
    uint32_t klen = (uint32_t)sizeof k;
    zero_bytes((unsigned char *)&k, (int64_t)sizeof k);
    int64_t r = sc(NR_getsockname, fd, P(&k), P(&klen), 0, 0, 0);
    if (r < 0) return r;
    return from_kaddr(&k, (int64_t)klen, addr);
}

int64_t sys_getpeername(int64_t fd, SysAddr *addr) {
    KAddr k;
    uint32_t klen = (uint32_t)sizeof k;
    zero_bytes((unsigned char *)&k, (int64_t)sizeof k);
    int64_t r = sc(NR_getpeername, fd, P(&k), P(&klen), 0, 0, 0);
    if (r < 0) return r;
    return from_kaddr(&k, (int64_t)klen, addr);
}

/* sys.h passes the caller's array to the kernel untouched, so its layout has
 * to BE struct pollfd's: a 32-bit fd and two 16-bit masks. There is no libc
 * header here to compare against, so the absolute layout is asserted. */
_Static_assert(sizeof(SysPollFd) == 8, "SysPollFd is struct pollfd's 8 bytes");
_Static_assert(__builtin_offsetof(SysPollFd, fd) == 0, "pollfd.fd offset");
_Static_assert(__builtin_offsetof(SysPollFd, events) == 4, "pollfd.events offset");
_Static_assert(__builtin_offsetof(SysPollFd, revents) == 6, "pollfd.revents offset");

int64_t sys_poll(SysPollFd *fds, int64_t n, int64_t timeout_ms) {
    if (n < 0) return -SYS_EINVAL;
    for (;;) {
        /* Rebuilt every time round, for two reasons. The kernel writes the
         * REMAINING time back through this pointer -- glibc's ppoll copies
         * the caller's timespec for exactly that reason -- and a restart
         * after a signal must offer the full timeout again, which is what
         * the libc backend's poll() does, so that the two behave alike. */
        struct { int64_t sec, nsec; } ts;
        int64_t tsp = 0;
        if (timeout_ms >= 0) {
            ts.sec = timeout_ms / 1000;
            ts.nsec = (timeout_ms % 1000) * 1000000;
            tsp = P(&ts);
        }
        /* A NULL signal mask, which the kernel checks before it looks at the
         * mask size, so the 8 is only what glibc passes. */
        int64_t r = sc(NR_ppoll, P(fds), n, tsp, 0, 8, 0);
        if (r == -SYS_EINTR) continue;  /* sys.h: EINTR is hidden here */
        return r;
    }
}

/* Refused, and it always will be. Resolving a name is getaddrinfo, and
 * getaddrinfo on glibc is the NSS machinery, which dlopens libnss_* at run
 * time to obey /etc/nsswitch.conf. A static binary with no C library cannot
 * dlopen anything, so there is nothing to port here -- see sys.h, and
 * docs/sys-layer.md §9 for what takes its place: a DNS client in language
 * source over these UDP sockets.
 *
 * -SYS_ENOSYS and not a crash, because a program can act on it: it is the
 * errno the layer already means by "this operation does not exist here", so
 * from_errno turns it into an ordinary error value and a `net` module can
 * fall back to a literal address or its own resolver. */
int64_t sys_resolve(const char *host, int64_t port, int64_t family,
                    SysAddr *out, int64_t cap) {
    (void)host;
    (void)port;
    (void)family;
    (void)out;
    (void)cap;
    return -SYS_ENOSYS;
}

/* ---- the one signal call ------------------------------------------------
 *
 * SIGPIPE is 13 on x86-64, aarch64 and riscv64 -- the generic numbering,
 * which only alpha, mips and parisc depart from, and none of those is a
 * target here. SIG_IGN is the constant 1 cast to a handler pointer, which is
 * the kernel's own convention and not the C library's invention.
 *
 * The struct is the KERNEL's `struct sigaction`, which is not the C
 * library's: sa_mask is a bare 8-byte word rather than glibc's 128-byte
 * sigset_t, the fields are in a different order, and x86-64 has a
 * sa_restorer between the flags and the mask that the generic architectures
 * do not (the kernel spells this __ARCH_HAS_SA_RESTORER). Getting the layout
 * wrong would not fail to compile, so each field is placed against the
 * kernel header it comes from and the size is asserted below.
 *
 * sa_restorer is left NULL and SA_RESTORER is left out of the flags, which
 * looks like the classic raw-syscall bug and is not one here. The restorer
 * is the address the kernel makes a HANDLER return to, so that the handler's
 * return runs rt_sigreturn; x86-64 refuses to deliver a signal without one
 * (arch/x86/kernel/signal_64.c checks SA_RESTORER in setup_rt_frame). A
 * SIG_IGN disposition is never delivered -- the kernel drops the signal in
 * sig_task_ignored before any frame is built -- so no restorer can ever be
 * reached. rt_sigaction itself does not check the flag: do_sigaction only
 * validates the signal number. That is the whole reason this file can ignore
 * SIGPIPE without a line of new assembly, and the reason the layer offers
 * only this disposition rather than sigaction in general (sys.h).
 *
 * The fourth argument is sigsetsize, and the kernel refuses anything but the
 * size of ITS sigset_t -- 8 bytes on all three architectures. */
#define K_SIGPIPE 13
#define K_SIG_IGN 1

typedef struct {
    int64_t  handler;
    uint64_t flags;
#if defined(__x86_64__)
    int64_t  restorer;
#endif
    uint64_t mask;
} KSigaction;

#if defined(__x86_64__)
_Static_assert(sizeof(KSigaction) == 32, "x86-64 kernel sigaction is 32 bytes");
#else
_Static_assert(sizeof(KSigaction) == 24, "generic kernel sigaction is 24 bytes");
#endif

int64_t sys_ignore_sigpipe(void) {
    KSigaction act;
    zero_bytes((unsigned char *)&act, (int64_t)sizeof act);
    act.handler = K_SIG_IGN;
    return sc(NR_rt_sigaction, K_SIGPIPE, P(&act), 0, (int64_t)sizeof act.mask, 0, 0);
}

/* ---- time, randomness, exit -------------------------------------------- */

int64_t sys_clock_ns(int64_t clock) {
    /* Linux's CLOCK_REALTIME and CLOCK_MONOTONIC are 0 and 1, the layer's
     * values; anything else is refused rather than passed through, so the
     * two backends accept the same set. */
    if (clock != SYS_CLOCK_MONOTONIC && clock != SYS_CLOCK_REALTIME) return -SYS_EINVAL;
    struct { int64_t sec, nsec; } ts;
    int64_t r = sc(NR_clock_gettime, clock, P(&ts), 0, 0, 0, 0);
    if (r < 0) return r;
    return ts.sec * 1000000000 + ts.nsec;
}

/* getrandom may return fewer bytes than asked for a large request, and may
 * be interrupted by a signal; sys.h promises all n bytes, so both are
 * retried here. */
int64_t sys_getrandom(void *buf, int64_t n) {
    unsigned char *p = buf;
    int64_t left = n;
    while (left > 0) {
        int64_t r = sc(NR_getrandom, P(p), left, 0, 0, 0, 0);
        if (r == -SYS_EINTR) continue;
        if (r < 0) return r;
        p += r;
        left -= r;
    }
    return n;
}

/* exit_group, not exit: plain exit ends only the calling thread, and a
 * spawned thread calling it would leave the process running. */
_Noreturn void sys_exit(int64_t code) {
    for (;;) sc(NR_exit_group, code, 0, 0, 0, 0, 0);
}
