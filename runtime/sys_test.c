/* Tests for the sys layer, run against each backend by runtime/sys_test.sh.
 *
 * The corpus exercises the layer only as far as the runtime uses it today:
 * open, read, write, close and fstat, through lib/io.src. This checks every
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

    /* ---- rename, unlink, rmdir ---- */
    expect("rename", sys_rename(a, b), 0);
    expect("old name is gone", sys_open(a, SYS_O_RDONLY, 0), -SYS_ENOENT);
    expect("rename a missing file", sys_rename(a, b), -SYS_ENOENT);
    expect("rmdir a non-empty directory", sys_rmdir(dir), -SYS_ENOTEMPTY);
    expect("unlink a directory is refused", sys_unlink(dir) < 0, 1);
    expect("unlink", sys_unlink(b), 0);
    expect("unlink again", sys_unlink(b), -SYS_ENOENT);
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
