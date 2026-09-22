/* The sys layer over the C library -- the default implementation.
 *
 * #included by rt.c, never compiled on its own; the contract is in sys.h.
 * Every function here is one POSIX call plus the two translations sys.h
 * promises: the layer's flag and clock constants into the host's, and the
 * host's errno into a negative Linux errno. That keeps this file portable to
 * Linux, macOS and the BSDs without a line of per-OS code beyond the two
 * spots marked below. Windows is not POSIX here: see docs/sys-layer.md.
 */
#include "sys.h"

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>          /* rename is ISO C, so it lives here, not in unistd.h */
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#if defined(__APPLE__)
/* Per-OS spot one of two: getentropy is declared here on macOS and in
 * unistd.h everywhere else. */
#include <sys/random.h>
#endif

/* The host's errno as the layer's: negative, and in Linux numbering. On
 * Linux every case is the identity, and the switch is still compiled, so
 * the code a macOS or BSD build depends on is the code the corpus runs.
 * A value with no case passes through unchanged -- it is still a real
 * error, just one whose number is the host's. */
static int64_t neg_errno(int e) {
    switch (e) {
    case EPERM:        return -SYS_EPERM;
    case ENOENT:       return -SYS_ENOENT;
    case EINTR:        return -SYS_EINTR;
    case EIO:          return -SYS_EIO;
    case EBADF:        return -SYS_EBADF;
    case EAGAIN:       return -SYS_EAGAIN;
    case ENOMEM:       return -SYS_ENOMEM;
    case EACCES:       return -SYS_EACCES;
    case EEXIST:       return -SYS_EEXIST;
    case EXDEV:        return -SYS_EXDEV;
    case ENOTDIR:      return -SYS_ENOTDIR;
    case EISDIR:       return -SYS_EISDIR;
    case EINVAL:       return -SYS_EINVAL;
    case EMFILE:       return -SYS_EMFILE;
    case ENOTTY:       return -SYS_ENOTTY;
    case ENOSPC:       return -SYS_ENOSPC;
    case ESPIPE:       return -SYS_ESPIPE;
    case EROFS:        return -SYS_EROFS;
    case EPIPE:        return -SYS_EPIPE;
    case ENAMETOOLONG: return -SYS_ENAMETOOLONG;
    case ENOSYS:       return -SYS_ENOSYS;
    case ENOTEMPTY:    return -SYS_ENOTEMPTY;
    case ELOOP:        return -SYS_ELOOP;
    default:           return e > 0 ? -(int64_t)e : -SYS_EIO;
    }
}

/* A POSIX call's -1-and-errno as the layer's single result. */
static int64_t ret(int64_t r) {
    return r < 0 ? neg_errno(errno) : r;
}

int64_t sys_open(const char *path, int64_t flags, int64_t mode) {
    int h;
    switch (flags & SYS_O_ACCMODE) {
    case SYS_O_WRONLY: h = O_WRONLY; break;
    case SYS_O_RDWR:   h = O_RDWR;   break;
    default:           h = O_RDONLY; break;
    }
    if (flags & SYS_O_CREAT)     h |= O_CREAT;
    if (flags & SYS_O_EXCL)      h |= O_EXCL;
    if (flags & SYS_O_TRUNC)     h |= O_TRUNC;
    if (flags & SYS_O_APPEND)    h |= O_APPEND;
    if (flags & SYS_O_DIRECTORY) h |= O_DIRECTORY;
    return ret(open(path, h | O_CLOEXEC, (mode_t)mode));
}

int64_t sys_read(int64_t fd, void *buf, int64_t n) {
    return ret(read((int)fd, buf, (size_t)n));
}

int64_t sys_write(int64_t fd, const void *buf, int64_t n) {
    return ret(write((int)fd, buf, (size_t)n));
}

int64_t sys_close(int64_t fd) {
    return ret(close((int)fd));
}

int64_t sys_lseek(int64_t fd, int64_t off, int64_t whence) {
    int w = whence == SYS_SEEK_CUR ? SEEK_CUR : whence == SYS_SEEK_END ? SEEK_END : SEEK_SET;
    return ret((int64_t)lseek((int)fd, (off_t)off, w));
}

static void from_stat(const struct stat *s, SysStat *st) {
    st->size = (int64_t)s->st_size;
    st->mode = (int64_t)s->st_mode;
    /* Per-OS spot two of two: POSIX.1-2008 names the field st_mtim, and
     * macOS still calls it st_mtimespec. */
#if defined(__APPLE__)
    st->mtime_ns = (int64_t)s->st_mtimespec.tv_sec * 1000000000 + s->st_mtimespec.tv_nsec;
#else
    st->mtime_ns = (int64_t)s->st_mtim.tv_sec * 1000000000 + s->st_mtim.tv_nsec;
#endif
}

int64_t sys_fstat(int64_t fd, SysStat *st) {
    struct stat s;
    if (fstat((int)fd, &s) != 0) return neg_errno(errno);
    from_stat(&s, st);
    return 0;
}

int64_t sys_stat(const char *path, int64_t follow, SysStat *st) {
    struct stat s;
    if ((follow ? stat(path, &s) : lstat(path, &s)) != 0) return neg_errno(errno);
    from_stat(&s, st);
    return 0;
}

/* opendir is not asked to open the directory: open with O_CLOEXEC first and
 * hand the descriptor to fdopendir, so this descriptor is close-on-exec like
 * every other one the layer opens (sys.h) whatever the C library's opendir
 * does. */
int64_t sys_listdir(const char *path, char *buf, int64_t cap) {
    int fd = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) return neg_errno(errno);
    DIR *d = fdopendir(fd);
    if (d == NULL) {
        int64_t e = neg_errno(errno);
        close(fd);
        return e;
    }
    int64_t need = 0;
    for (;;) {
        /* readdir reports an error only through errno, and reports the end
         * by returning NULL with errno untouched -- so it is cleared first. */
        errno = 0;
        struct dirent *e = readdir(d);
        if (e == NULL) {
            if (errno != 0) {
                int64_t r = neg_errno(errno);
                closedir(d);
                return r;
            }
            break;
        }
        const char *n = e->d_name;
        if (n[0] == '.' && (n[1] == 0 || (n[1] == '.' && n[2] == 0))) continue;
        for (size_t i = 0;; i++) {
            if (need < cap) buf[need] = n[i];
            need++;
            if (n[i] == 0) break;
        }
    }
    closedir(d);  /* closes fd too */
    return need;
}

int64_t sys_isatty(int64_t fd) {
    return isatty((int)fd) ? 1 : 0;
}

int64_t sys_mkdir(const char *path, int64_t mode) {
    return ret(mkdir(path, (mode_t)mode));
}

int64_t sys_unlink(const char *path) {
    return ret(unlink(path));
}

int64_t sys_rmdir(const char *path) {
    return ret(rmdir(path));
}

int64_t sys_rename(const char *from, const char *to) {
    return ret(rename(from, to));
}

int64_t sys_symlink(const char *target, const char *path) {
    return ret(symlink(target, path));
}

int64_t sys_clock_ns(int64_t clock) {
    if (clock != SYS_CLOCK_MONOTONIC && clock != SYS_CLOCK_REALTIME) return -SYS_EINVAL;
    clockid_t id = clock == SYS_CLOCK_MONOTONIC ? CLOCK_MONOTONIC : CLOCK_REALTIME;
    struct timespec ts;
    if (clock_gettime(id, &ts) != 0) return neg_errno(errno);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

/* getentropy rather than getrandom, because it is the one spelling glibc,
 * musl, macOS and the BSDs all have. Its limit of 256 bytes per call is why
 * this loops. */
int64_t sys_getrandom(void *buf, int64_t n) {
    unsigned char *p = buf;
    int64_t left = n;
    while (left > 0) {
        size_t k = left > 256 ? 256 : (size_t)left;
        if (getentropy(p, k) != 0) return neg_errno(errno);
        p += k;
        left -= (int64_t)k;
    }
    return n;
}

_Noreturn void sys_exit(int64_t code) {
    _exit((int)code);
}
