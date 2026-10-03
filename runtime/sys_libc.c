/* The sys layer over the C library -- the default implementation.
 *
 * #included by rt.c, never compiled on its own; the contract is in sys.h.
 * Every function here is one POSIX call plus the two translations sys.h
 * promises: the layer's flag and clock constants into the host's, and the
 * host's errno into a negative Linux errno. That keeps this file portable to
 * Linux, macOS and the BSDs without a line of per-OS code beyond the three
 * spots marked below. Windows is not POSIX here: see docs/sys-layer.md.
 */
#include "sys.h"

#include <arpa/inet.h>      /* htons and ntohs: POSIX declares them here */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>          /* getaddrinfo, for sys_resolve -- see the note there */
#include <netinet/in.h>
#include <netinet/tcp.h>    /* TCP_NODELAY */
#include <poll.h>
#include <signal.h>         /* sigaction, for sys_ignore_sigpipe alone */
#include <spawn.h>          /* posix_spawnp, for sys_proc_start -- does the $PATH search */
#include <stddef.h>         /* offsetof, for sockaddr_un's length */
#include <stdio.h>          /* rename is ISO C, so it lives here, not in unistd.h */
#include <string.h>         /* memcpy and memset, for the address conversions */
#include <sys/ioctl.h>      /* TIOCGWINSZ: the window size is an ioctl everywhere */
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>       /* waitpid and WIFEXITED/WIFSIGNALED, for sys_proc_wait */
#include <termios.h>        /* tcgetattr and tcsetattr, for sys_tcget/sys_tcset */
#include <time.h>
#include <unistd.h>
#if defined(__APPLE__)
/* Per-OS spot one of three: getentropy is declared here on macOS and in
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
    /* The socket errnos. These are where the host numbers diverge most --
     * ECONNREFUSED is 111 on Linux and 61 on macOS, EINPROGRESS 115 and 36 --
     * so a `net` module written against the numbers needs every one mapped.
     * EWOULDBLOCK and ENOTSUP have no case of their own: on Linux they are
     * EAGAIN and EOPNOTSUPP, and a duplicate case label would not compile. */
    case ENOTSOCK:        return -SYS_ENOTSOCK;
    case EDESTADDRREQ:    return -SYS_EDESTADDRREQ;
    case EMSGSIZE:        return -SYS_EMSGSIZE;
    case EPROTONOSUPPORT: return -SYS_EPROTONOSUPPORT;
    case EOPNOTSUPP:      return -SYS_EOPNOTSUPP;
    case EAFNOSUPPORT:    return -SYS_EAFNOSUPPORT;
    case EADDRINUSE:      return -SYS_EADDRINUSE;
    case EADDRNOTAVAIL:   return -SYS_EADDRNOTAVAIL;
    case ENETUNREACH:     return -SYS_ENETUNREACH;
    case ECONNABORTED:    return -SYS_ECONNABORTED;
    case ECONNRESET:      return -SYS_ECONNRESET;
    case EISCONN:         return -SYS_EISCONN;
    case ENOTCONN:        return -SYS_ENOTCONN;
    case ETIMEDOUT:       return -SYS_ETIMEDOUT;
    case ECONNREFUSED:    return -SYS_ECONNREFUSED;
    case EHOSTUNREACH:    return -SYS_EHOSTUNREACH;
    case EINPROGRESS:     return -SYS_EINPROGRESS;
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

/* ---- the terminal -------------------------------------------------------
 *
 * tcgetattr and tcsetattr, plus the one ioctl POSIX never standardised:
 * TIOCGWINSZ. Every operating system that has a terminal has it under that
 * name -- Linux, macOS, the BSDs, Solaris -- which is why the window size is
 * an ioctl here and a flag translation everywhere else in this file.
 *
 * The flag words are translated bit by bit, through one table per word. On
 * Linux every entry is the identity, and the assertions below say so rather
 * than leaving it to be believed; the loop runs anyway, so the code a macOS
 * or BSD build would depend on is the code the corpus exercises. */

typedef struct {
    int64_t  mine;
    tcflag_t host;
} FlagMap;

static const FlagMap in_flags[] = {
    {SYS_TC_IGNBRK, IGNBRK}, {SYS_TC_BRKINT, BRKINT}, {SYS_TC_PARMRK, PARMRK},
    {SYS_TC_INPCK, INPCK},   {SYS_TC_ISTRIP, ISTRIP}, {SYS_TC_INLCR, INLCR},
    {SYS_TC_IGNCR, IGNCR},   {SYS_TC_ICRNL, ICRNL},   {SYS_TC_IXON, IXON},
};
static const FlagMap out_flags[] = {
    {SYS_TC_OPOST, OPOST}, {SYS_TC_ONLCR, ONLCR},
};
static const FlagMap local_flags[] = {
    {SYS_TC_ISIG, ISIG},     {SYS_TC_ICANON, ICANON}, {SYS_TC_ECHO, ECHO},
    {SYS_TC_ECHONL, ECHONL}, {SYS_TC_IEXTEN, IEXTEN},
};

/* Every named bit is exactly one bit, so a set is a test and an OR. A bit
 * the table does not name is carried through unchanged -- exact on Linux,
 * where the two numbering schemes are the same one, and the documented
 * approximation anywhere else (sys.h, SysTermios). */
static tcflag_t to_host(int64_t v, const FlagMap *m, size_t n) {
    int64_t named = 0;
    tcflag_t h = 0;
    for (size_t i = 0; i < n; i++) {
        named |= m[i].mine;
        if (v & m[i].mine) h |= m[i].host;
    }
    return h | (tcflag_t)(v & ~named);
}

static int64_t from_host(tcflag_t h, const FlagMap *m, size_t n) {
    tcflag_t named = 0;
    int64_t v = 0;
    for (size_t i = 0; i < n; i++) {
        named |= m[i].host;
        if (h & m[i].host) v |= m[i].mine;
    }
    return v | (int64_t)(h & ~named);
}

#define NELEMS(a) (sizeof(a) / sizeof(a)[0])

#if defined(__linux__)
/* On Linux the layer's numbers ARE the host's, for every named bit and for
 * both control-character indices, so both directions above are provably the
 * identity here. A host that disagreed would still work -- the tables are
 * what make it work -- but a Linux one that disagreed would mean the numbers
 * in sys.h were copied wrong, and that must not compile. */
_Static_assert(SYS_TC_IGNBRK == IGNBRK, "IGNBRK");
_Static_assert(SYS_TC_BRKINT == BRKINT, "BRKINT");
_Static_assert(SYS_TC_PARMRK == PARMRK, "PARMRK");
_Static_assert(SYS_TC_INPCK == INPCK, "INPCK");
_Static_assert(SYS_TC_ISTRIP == ISTRIP, "ISTRIP");
_Static_assert(SYS_TC_INLCR == INLCR, "INLCR");
_Static_assert(SYS_TC_IGNCR == IGNCR, "IGNCR");
_Static_assert(SYS_TC_ICRNL == ICRNL, "ICRNL");
_Static_assert(SYS_TC_IXON == IXON, "IXON");
_Static_assert(SYS_TC_OPOST == OPOST, "OPOST");
_Static_assert(SYS_TC_ONLCR == ONLCR, "ONLCR");
_Static_assert(SYS_TC_ISIG == ISIG, "ISIG");
_Static_assert(SYS_TC_ICANON == ICANON, "ICANON");
_Static_assert(SYS_TC_ECHO == ECHO, "ECHO");
_Static_assert(SYS_TC_ECHONL == ECHONL, "ECHONL");
_Static_assert(SYS_TC_IEXTEN == IEXTEN, "IEXTEN");
_Static_assert(SYS_VMIN == VMIN, "VMIN index");
_Static_assert(SYS_VTIME == VTIME, "VTIME index");
#endif

/* The host's c_cc is at least as long as ours on every system this builds
 * on -- Linux's NCCS is 19 in the kernel and 32 in glibc, macOS's is 20 --
 * and the copy below walks ours, so a shorter one would run off the end. */
_Static_assert(NCCS >= SYS_NCCS, "the host's c_cc is shorter than the layer's");

int64_t sys_tcget(int64_t fd, SysTermios *t) {
    struct termios h;
    memset(&h, 0, sizeof h);
    if (tcgetattr((int)fd, &h) != 0) return neg_errno(errno);
    t->iflag = from_host(h.c_iflag, in_flags, NELEMS(in_flags));
    t->oflag = from_host(h.c_oflag, out_flags, NELEMS(out_flags));
    t->lflag = from_host(h.c_lflag, local_flags, NELEMS(local_flags));
    t->cflag = (int64_t)h.c_cflag;  /* opaque: the host's own bits (sys.h) */
    for (int i = 0; i < SYS_NCCS; i++) t->cc[i] = (unsigned char)h.c_cc[i];
    return 0;
}

int64_t sys_tcset(int64_t fd, const SysTermios *t) {
    /* Read first, so the fields SysTermios does not model -- the line
     * discipline, and c_ispeed/c_ospeed where the host keeps them apart from
     * c_cflag -- keep what the terminal already has instead of becoming
     * zero. A zero speed is B0, and B0 hangs the line up. */
    struct termios h;
    memset(&h, 0, sizeof h);
    if (tcgetattr((int)fd, &h) != 0) return neg_errno(errno);
    h.c_iflag = to_host(t->iflag, in_flags, NELEMS(in_flags));
    h.c_oflag = to_host(t->oflag, out_flags, NELEMS(out_flags));
    h.c_lflag = to_host(t->lflag, local_flags, NELEMS(local_flags));
    h.c_cflag = (tcflag_t)t->cflag;
    for (int i = 0; i < SYS_NCCS; i++) h.c_cc[i] = (cc_t)t->cc[i];
    return ret(tcsetattr((int)fd, TCSANOW, &h));
}

int64_t sys_winsize(int64_t fd, int64_t *rows, int64_t *cols) {
    struct winsize w;
    memset(&w, 0, sizeof w);
    if (ioctl((int)fd, TIOCGWINSZ, &w) != 0) return neg_errno(errno);
    *rows = w.ws_row;
    *cols = w.ws_col;
    return 0;
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

/* ---- sockets ------------------------------------------------------------ */

/* The layer's address family as the host's, and back. AF_INET6 is 10 here
 * and 30 on macOS; AF_INET and AF_UNIX happen to agree everywhere. */
static int host_family(int64_t f) {
    switch (f) {
    case SYS_AF_UNIX:  return AF_UNIX;
    case SYS_AF_INET:  return AF_INET;
    case SYS_AF_INET6: return AF_INET6;
    default:           return -1;
    }
}

static int64_t layer_family(int f) {
    switch (f) {
    case AF_UNIX:  return SYS_AF_UNIX;
    case AF_INET:  return SYS_AF_INET;
    case AF_INET6: return SYS_AF_INET6;
    default:       return -1;
    }
}

/* A SysAddr as the kernel's sockaddr for its family, with the length to pass
 * alongside it. Returns 0, or -errno.
 *
 * This and from_sockaddr are the only two places in this file that know
 * sockaddr_in from sockaddr_in6, and the only two that byte-swap: sys.h
 * promises that SysAddr.port is a plain host-order integer, so the htons
 * happens here. The address bytes are already in wire order and are copied
 * straight through. */
static int64_t to_sockaddr(const SysAddr *a, struct sockaddr_storage *ss, socklen_t *len) {
    memset(ss, 0, sizeof *ss);
    switch (a->family) {
    case SYS_AF_INET: {
        struct sockaddr_in *v4 = (struct sockaddr_in *)ss;
        if (a->port < 0 || a->port > 65535) return -SYS_EINVAL;
        v4->sin_family = AF_INET;
        v4->sin_port = htons((uint16_t)a->port);
        memcpy(&v4->sin_addr, a->addr, 4);
        *len = sizeof *v4;
        return 0;
    }
    case SYS_AF_INET6: {
        struct sockaddr_in6 *v6 = (struct sockaddr_in6 *)ss;
        if (a->port < 0 || a->port > 65535) return -SYS_EINVAL;
        v6->sin6_family = AF_INET6;
        v6->sin6_port = htons((uint16_t)a->port);
        memcpy(&v6->sin6_addr, a->addr, 16);
        *len = sizeof *v6;
        return 0;
    }
    case SYS_AF_UNIX: {
        struct sockaddr_un *un = (struct sockaddr_un *)ss;
        /* Not strlen: `path` is the last member of SysAddr, so a caller who
         * filled all 108 bytes without a NUL would send strlen off the end
         * of the struct. The scan is bounded and the unterminated case then
         * falls out as too long, which it is. */
        size_t n = 0;
        while (n < sizeof a->path && a->path[n] != 0) n++;
        /* Against the HOST's sun_path, which is 104 on macOS and the BSDs
         * and 108 on Linux -- refused, never truncated, because a truncated
         * path names a different socket. */
        if (n + 1 > sizeof un->sun_path) return -SYS_ENAMETOOLONG;
        un->sun_family = AF_UNIX;
        memcpy(un->sun_path, a->path, n + 1);
        *len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + n + 1);
        return 0;
    }
    default:
        return -SYS_EAFNOSUPPORT;
    }
}

/* The other direction, for accept, getsockname and getpeername. */
static int64_t from_sockaddr(const struct sockaddr_storage *ss, socklen_t len, SysAddr *a) {
    memset(a, 0, sizeof *a);
    int64_t f = layer_family((int)ss->ss_family);
    if (f < 0) return -SYS_EAFNOSUPPORT;
    a->family = f;
    switch (f) {
    case SYS_AF_INET: {
        const struct sockaddr_in *v4 = (const struct sockaddr_in *)ss;
        a->port = ntohs(v4->sin_port);
        memcpy(a->addr, &v4->sin_addr, 4);
        return 0;
    }
    case SYS_AF_INET6: {
        const struct sockaddr_in6 *v6 = (const struct sockaddr_in6 *)ss;
        a->port = ntohs(v6->sin6_port);
        memcpy(a->addr, &v6->sin6_addr, 16);
        return 0;
    }
    default: {
        /* AF_UNIX. An unbound socket comes back as the family alone, with
         * `len` stopping before sun_path, and its path is left "". Linux's
         * abstract namespace (a path whose first byte is NUL) has no
         * representation here and also arrives as "": sys.h's path is a C
         * string, and the layer does not offer what it cannot round-trip. */
        const struct sockaddr_un *un = (const struct sockaddr_un *)ss;
        size_t head = offsetof(struct sockaddr_un, sun_path);
        size_t n = (size_t)len > head ? (size_t)len - head : 0;
        /* Capped against BOTH ends: the layer's path, and the host's
         * sun_path, which is 104 on macOS where the layer's is 108. */
        if (n > sizeof un->sun_path) n = sizeof un->sun_path;
        if (n > sizeof a->path - 1) n = sizeof a->path - 1;
        memcpy(a->path, un->sun_path, n);
        a->path[n] = 0;  /* n usually counts the kernel's own NUL; harmless if so */
        return 0;
    }
    }
}

/* One of the layer's option ids as the host's (level, name) pair, plus the
 * shape of its value -- which is what lets every option be one int64. */
typedef enum { OPT_BOOL, OPT_MS, OPT_ERR } OptKind;

static int opt_lookup(int64_t opt, int *level, int *name, OptKind *kind) {
    switch (opt) {
    case SYS_SO_REUSEADDR: *level = SOL_SOCKET;  *name = SO_REUSEADDR; *kind = OPT_BOOL; return 0;
    case SYS_SO_KEEPALIVE: *level = SOL_SOCKET;  *name = SO_KEEPALIVE; *kind = OPT_BOOL; return 0;
    case SYS_SO_ERROR:     *level = SOL_SOCKET;  *name = SO_ERROR;     *kind = OPT_ERR;  return 0;
    case SYS_SO_RCVTIMEO:  *level = SOL_SOCKET;  *name = SO_RCVTIMEO;  *kind = OPT_MS;   return 0;
    case SYS_SO_SNDTIMEO:  *level = SOL_SOCKET;  *name = SO_SNDTIMEO;  *kind = OPT_MS;   return 0;
    case SYS_TCP_NODELAY:  *level = IPPROTO_TCP; *name = TCP_NODELAY;  *kind = OPT_BOOL; return 0;
    default: return -1;
    }
}

int64_t sys_socket(int64_t domain, int64_t type, int64_t protocol) {
    int d = host_family(domain);
    if (d < 0) return -SYS_EAFNOSUPPORT;
    /* The type is validated here rather than left to the kernel, so that
     * both backends refuse exactly the same arguments with exactly the same
     * errno -- which is the property runtime/sys_test.c exists to check. */
    if (type & ~(int64_t)(SYS_SOCK_TYPEMASK | SYS_SOCK_NONBLOCK)) return -SYS_EINVAL;
    int t;
    switch (type & SYS_SOCK_TYPEMASK) {
    case SYS_SOCK_STREAM: t = SOCK_STREAM; break;
    case SYS_SOCK_DGRAM:  t = SOCK_DGRAM;  break;
    default: return -SYS_EINVAL;
    }
#if defined(SOCK_CLOEXEC) && defined(SOCK_NONBLOCK)
    /* Per-OS spot two of three: Linux and the modern BSDs take both
     * descriptor flags in socket()'s type argument, which needs no second
     * call and cannot race a fork in another thread. */
    t |= SOCK_CLOEXEC;
    if (type & SYS_SOCK_NONBLOCK) t |= SOCK_NONBLOCK;
    return ret(socket(d, t, (int)protocol));
#else
    /* macOS has neither flag, so the descriptor exists for a moment without
     * them. The window is real and unavoidable through POSIX alone. */
    int fd = socket(d, t, (int)protocol);
    if (fd < 0) return neg_errno(errno);
    if (fcntl(fd, F_SETFD, FD_CLOEXEC) != 0 ||
        ((type & SYS_SOCK_NONBLOCK) && fcntl(fd, F_SETFL, O_NONBLOCK) != 0)) {
        int64_t e = neg_errno(errno);
        close(fd);
        return e;
    }
    return fd;
#endif
}

int64_t sys_bind(int64_t fd, const SysAddr *addr) {
    struct sockaddr_storage ss;
    socklen_t len;
    int64_t r = to_sockaddr(addr, &ss, &len);
    if (r < 0) return r;
    return ret(bind((int)fd, (const struct sockaddr *)&ss, len));
}

int64_t sys_listen(int64_t fd, int64_t backlog) {
    if (backlog < 0) return -SYS_EINVAL;
    return ret(listen((int)fd, backlog > 65535 ? 65535 : (int)backlog));
}

/* Plain accept and a second call for close-on-exec, not accept4: glibc hides
 * accept4 behind __USE_GNU, and rt.c #includes this file after its own
 * headers, so _GNU_SOURCE could not be defined here in time. The raw backend
 * uses accept4 and has no such window (sys_linux.c). */
int64_t sys_accept(int64_t fd, SysAddr *peer) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    memset(&ss, 0, sizeof ss);
    int c = accept((int)fd, (struct sockaddr *)&ss, &len);
    if (c < 0) return neg_errno(errno);
    int64_t r = 0;
    /* Per-OS spot three of three, in spirit: every POSIX host needs this
     * fcntl, and Linux's raw backend does not. */
    if (fcntl(c, F_SETFD, FD_CLOEXEC) != 0) r = neg_errno(errno);
    if (r == 0 && peer != NULL) r = from_sockaddr(&ss, len, peer);
    if (r < 0) {
        close(c);
        return r;
    }
    return c;
}

int64_t sys_connect(int64_t fd, const SysAddr *addr) {
    struct sockaddr_storage ss;
    socklen_t len;
    int64_t r = to_sockaddr(addr, &ss, &len);
    if (r < 0) return r;
    return ret(connect((int)fd, (const struct sockaddr *)&ss, len));
}

int64_t sys_shutdown(int64_t fd, int64_t how) {
    int h;
    switch (how) {
    case SYS_SHUT_RD:   h = SHUT_RD;   break;
    case SYS_SHUT_WR:   h = SHUT_WR;   break;
    case SYS_SHUT_RDWR: h = SHUT_RDWR; break;
    default: return -SYS_EINVAL;
    }
    return ret(shutdown((int)fd, h));
}

/* SYS_SO_NONBLOCK is the descriptor's O_NONBLOCK and not a socket option at
 * all (sys.h), so it is answered before the option table is consulted. Read
 * and modify rather than assign: F_SETFL takes the whole word, and clobbering
 * the access mode with it would be a different file. */
int64_t sys_setsockopt(int64_t fd, int64_t opt, int64_t value) {
    if (opt == SYS_SO_NONBLOCK) {
        int fl = fcntl((int)fd, F_GETFL, 0);
        if (fl < 0) return neg_errno(errno);
        fl = value != 0 ? (fl | O_NONBLOCK) : (fl & ~O_NONBLOCK);
        return ret(fcntl((int)fd, F_SETFL, fl));
    }
    int level, name;
    OptKind kind;
    if (opt_lookup(opt, &level, &name, &kind) != 0) return -SYS_EINVAL;
    if (kind == OPT_ERR) return -SYS_EINVAL;  /* SYS_SO_ERROR is read-only */
    if (kind == OPT_MS) {
        if (value < 0) return -SYS_EINVAL;
        struct timeval tv;
        tv.tv_sec = (time_t)(value / 1000);
        tv.tv_usec = (suseconds_t)((value % 1000) * 1000);
        return ret(setsockopt((int)fd, level, name, &tv, (socklen_t)sizeof tv));
    }
    int v = value != 0;
    return ret(setsockopt((int)fd, level, name, &v, (socklen_t)sizeof v));
}

int64_t sys_getsockopt(int64_t fd, int64_t opt) {
    if (opt == SYS_SO_NONBLOCK) {
        int fl = fcntl((int)fd, F_GETFL, 0);
        if (fl < 0) return neg_errno(errno);
        return (fl & O_NONBLOCK) != 0;
    }
    int level, name;
    OptKind kind;
    if (opt_lookup(opt, &level, &name, &kind) != 0) return -SYS_EINVAL;
    if (kind == OPT_MS) {
        struct timeval tv;
        socklen_t n = sizeof tv;
        if (getsockopt((int)fd, level, name, &tv, &n) != 0) return neg_errno(errno);
        return (int64_t)tv.tv_sec * 1000 + tv.tv_usec / 1000;
    }
    int v = 0;
    socklen_t n = sizeof v;
    if (getsockopt((int)fd, level, name, &v, &n) != 0) return neg_errno(errno);
    /* The pending error is a HOST errno; it goes through the same translation
     * as every other, then back to positive, because it is a value and not a
     * result (sys.h). */
    if (kind == OPT_ERR) return v == 0 ? 0 : -neg_errno(v);
    /* The kernel may report a "true" boolean as any non-zero number; the
     * layer promises 0 or 1 so that the two backends compare equal. */
    return v != 0;
}

int64_t sys_getsockname(int64_t fd, SysAddr *addr) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    memset(&ss, 0, sizeof ss);
    if (getsockname((int)fd, (struct sockaddr *)&ss, &len) != 0) return neg_errno(errno);
    return from_sockaddr(&ss, len, addr);
}

int64_t sys_getpeername(int64_t fd, SysAddr *addr) {
    struct sockaddr_storage ss;
    socklen_t len = sizeof ss;
    memset(&ss, 0, sizeof ss);
    if (getpeername((int)fd, (struct sockaddr *)&ss, &len) != 0) return neg_errno(errno);
    return from_sockaddr(&ss, len, addr);
}

/* SysPollFd is struct pollfd, and this is where that is checked rather than
 * believed. If a host ever disagreed, this file would not compile -- which is
 * the whole reason sys.h may pass the caller's array straight through instead
 * of copying it into a buffer it has nowhere to allocate. */
_Static_assert(sizeof(SysPollFd) == sizeof(struct pollfd), "SysPollFd is struct pollfd");
_Static_assert(offsetof(SysPollFd, fd) == offsetof(struct pollfd, fd), "pollfd.fd");
_Static_assert(offsetof(SysPollFd, events) == offsetof(struct pollfd, events), "pollfd.events");
_Static_assert(offsetof(SysPollFd, revents) == offsetof(struct pollfd, revents), "pollfd.revents");
_Static_assert(sizeof(((struct pollfd *)0)->events) == 2, "pollfd.events is 16 bits");

int64_t sys_poll(SysPollFd *fds, int64_t n, int64_t timeout_ms) {
    if (n < 0) return -SYS_EINVAL;
    /* poll's timeout is an int of milliseconds; anything longer than about
     * 24 days is clamped rather than wrapped into the past. */
    int t = timeout_ms < 0 ? -1 : timeout_ms > 2147483647 ? 2147483647 : (int)timeout_ms;
    void *p = fds;  /* through void *, so the cast is a reinterpretation the
                     * compiler is told about rather than a type pun it may
                     * assume cannot happen */
    for (;;) {
        int r = poll((struct pollfd *)p, (nfds_t)n, t);
        if (r < 0 && errno == EINTR) continue;  /* sys.h: EINTR is hidden here */
        return ret(r);
    }
}

/* getaddrinfo's own error codes are not errnos, so they are mapped to the
 * nearest errno a program can act on. "No such host" is -SYS_ENOENT, which is
 * the same answer an unresolvable path gives.
 *
 * The default is ENOENT rather than EIO on purpose. glibc's other codes --
 * EAI_NODATA ("the name exists but has no address of this family") and
 * EAI_ADDRFAMILY -- sit behind __USE_GNU, which this file cannot turn on:
 * rt.c #includes it after its own headers, so a _GNU_SOURCE here would come
 * too late. They cannot be given cases of their own, and every one of them
 * means the same thing to a caller: there is no address to connect to under
 * that name. ENOENT says that. The codes that mean something else -- try
 * again, out of memory, the arguments were wrong, the name server failed --
 * are each matched above, so nothing informative falls through. */
static int64_t gai_errno(int rc) {
    switch (rc) {
    case EAI_NONAME:   return -SYS_ENOENT;
    case EAI_AGAIN:    return -SYS_EAGAIN;  /* the name server is busy; retrying may work */
    case EAI_FAIL:     return -SYS_EIO;     /* the name server answered, with a failure */
    case EAI_FAMILY:   return -SYS_EAFNOSUPPORT;
    case EAI_SOCKTYPE: return -SYS_EINVAL;
    case EAI_SERVICE:  return -SYS_EINVAL;
    case EAI_BADFLAGS: return -SYS_EINVAL;
    case EAI_MEMORY:   return -SYS_ENOMEM;
    case EAI_SYSTEM:   return neg_errno(errno);
    default:           return -SYS_ENOENT;
    }
}

int64_t sys_resolve(const char *host, int64_t port, int64_t family,
                    SysAddr *out, int64_t cap) {
    if (port < 0 || port > 65535 || cap < 0) return -SYS_EINVAL;
    struct addrinfo hints;
    memset(&hints, 0, sizeof hints);
    switch (family) {
    case 0:            hints.ai_family = AF_UNSPEC; break;
    case SYS_AF_INET:  hints.ai_family = AF_INET;   break;
    case SYS_AF_INET6: hints.ai_family = AF_INET6;  break;
    default:           return -SYS_EAFNOSUPPORT;
    }
    /* One socket type, or the same address comes back once per type. The
     * service is NULL and the port is filled in afterwards, because the port
     * is already a number here and asking getaddrinfo to parse it back from
     * text would only add a way to fail. */
    hints.ai_socktype = SOCK_STREAM;
    struct addrinfo *res = NULL;
    int rc = getaddrinfo(host, NULL, &hints, &res);
    if (rc != 0) return gai_errno(rc);
    int64_t found = 0;
    for (struct addrinfo *p = res; p != NULL; p = p->ai_next) {
        /* hints.ai_family already asked for this, but not every libc
         * refuses to answer anyway: asked for AF_INET6 with an IPv4
         * literal and no AI_V4MAPPED, glibc refuses outright (EAI_
         * ADDRFAMILY, which is where the -SYS_ENOENT below actually comes
         * from) -- macOS's resolver instead happily synthesizes an IPv4-
         * mapped IPv6 address and returns it, found for real on a macOS CI
         * run (this call returning 1 result instead of failing). Filtering
         * defensively here makes the contract this layer actually
         * documents -- a specific family, or none of them -- true
         * regardless of how strict the host's own getaddrinfo is about
         * honoring what hints.ai_family already asked for. */
        if (family != 0 && p->ai_family != hints.ai_family) continue;
        if (found < cap) {
            /* Copied into a storage first: ai_addr points at exactly
             * ai_addrlen bytes, and from_sockaddr reads a whole struct. */
            struct sockaddr_storage ss;
            size_t n = (size_t)p->ai_addrlen;
            if (n > sizeof ss) n = sizeof ss;
            memset(&ss, 0, sizeof ss);
            memcpy(&ss, p->ai_addr, n);
            if (from_sockaddr(&ss, (socklen_t)n, &out[found]) < 0) continue;
            out[found].port = port;
        }
        found++;
    }
    freeaddrinfo(res);
    if (found == 0 && family != 0) return -SYS_ENOENT;
    return found;
}

/* sigaction and not signal(): signal()'s semantics are the one corner of ISO
 * C that POSIX and the BSDs still disagree about -- whether the disposition
 * resets after a delivery, and whether an interrupted call restarts -- and
 * although neither can be observed through SIG_IGN, the spelling that has
 * one meaning everywhere costs three extra lines. sa_mask is zeroed rather
 * than left as it came off the stack: it is read by the kernel even for
 * SIG_IGN on some systems, and an uninitialised read is what the sanitized
 * builds of runtime/sys_test.sh exist to catch. */
int64_t sys_ignore_sigpipe(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = SIG_IGN;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;
    return ret(sigaction(SIGPIPE, &sa, NULL));
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

/* posix_spawnp, not fork+execvp, because it already IS the two things a
 * hand-written fork+exec would have to add: it searches $PATH itself (the
 * "p" in the name, POSIX's own execvp rule) and, on glibc since 2.24, it
 * reports a failed exec back to THIS call rather than only to the child's
 * exit status -- glibc's spawn helper runs the search and the exec in the
 * child and relays a failure through an internal pipe before this returns,
 * which is exactly the synchronisation sys_linux.c has to build by hand
 * with its own pipe2 (that file's comment says why). Checked on this
 * machine (glibc 2.39): posix_spawnp of a name on no $PATH returns ENOENT
 * directly, with no pid and no zombie left behind, not a pid whose wait
 * later reports 127 the way an older glibc or a naive fork+exec would.
 *
 * posix_spawn/posix_spawnp do not use errno: a failure is the return value
 * itself, a plain positive errno number, so neg_errno takes it directly
 * rather than through ret()'s -1-and-errno convention. */
int64_t sys_proc_start(char *const argv[], char *const envp[]) {
    pid_t pid;
    int rc = posix_spawnp(&pid, argv[0], NULL, NULL, argv, envp);
    if (rc != 0) return neg_errno(rc);
    return (int64_t)pid;
}

/* waitpid, then sys.h's own encoding rather than the raw status: see
 * sys.h's comment on sys_proc_wait for why this layer does not hand the
 * kernel's WIFEXITED/WIFSIGNALED bit pattern through unchanged. */
int64_t sys_proc_wait(int64_t pid) {
    int status;
    pid_t r;
    for (;;) {
        r = waitpid((pid_t)pid, &status, 0);
        if (r < 0 && errno == EINTR) continue;  /* sys.h: EINTR is hidden here */
        break;
    }
    if (r < 0) return neg_errno(errno);
    if (WIFSIGNALED(status)) return SYS_WAIT_SIGNAL_BASE + WTERMSIG(status);
    return WEXITSTATUS(status);
}
