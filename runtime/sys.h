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
