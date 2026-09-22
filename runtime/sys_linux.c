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
 * stat's layout differs per architecture, struct statx does not. */
#if defined(__x86_64__)
#define NR_read          0
#define NR_write         1
#define NR_close         3
#define NR_lseek         8
#define NR_ioctl        16
#define NR_clock_gettime 228
#define NR_exit_group   231
#define NR_openat       257
#define NR_mkdirat      258
#define NR_unlinkat     263
#define NR_renameat2    316
#define NR_getrandom    318
#define NR_statx        332
#elif defined(__aarch64__) || (defined(__riscv) && __riscv_xlen == 64)
#define NR_ioctl         29
#define NR_mkdirat       34
#define NR_unlinkat      35
#define NR_openat        56
#define NR_close         57
#define NR_lseek         62
#define NR_read          63
#define NR_write         64
#define NR_exit_group    94
#define NR_clock_gettime 113
#define NR_renameat2    276
#define NR_getrandom    278
#define NR_statx        291
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
#define O_CLOEXEC_     02000000   /* the same on all three architectures */
#define TCGETS         0x5401     /* likewise */

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

int64_t sys_fstat(int64_t fd, SysStat *st) {
    Statx sx;
    int64_t r = sc(NR_statx, fd, P(""), AT_EMPTY_PATH, STATX_BASIC_STATS, P(&sx), 0);
    if (r < 0) return r;
    st->size = (int64_t)sx.size;
    st->mode = sx.mode;
    st->mtime_ns = sx.mtime.tv_sec * 1000000000 + sx.mtime.tv_nsec;
    return 0;
}

/* isatty is "does the terminal ioctl succeed", which is also how every C
 * library implements it. The buffer is larger than the kernel's 36-byte
 * struct termios so no architecture's layout can overrun it. */
int64_t sys_isatty(int64_t fd) {
    unsigned char termios[64];
    return sc(NR_ioctl, fd, TCGETS, P(termios), 0, 0, 0) == 0 ? 1 : 0;
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

int64_t sys_rename(const char *from, const char *to) {
    return sc(NR_renameat2, AT_FDCWD, P(from), AT_FDCWD, P(to), 0, 0);
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
