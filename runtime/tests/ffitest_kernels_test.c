/* The `prim "c"` test kernels (runtime/ffitest_<arch>.S), called directly, with
 * no C library and no m31: the same program is built for x86-64 and, with
 * clang's cross target, for aarch64 and run under qemu-user. What it proves is
 * the callee half of docs/ffi-decision.md's contract -- the calling convention
 * is honoured, and each kernel reads and writes exactly the range it was given
 * (canary octets either side of every output) -- which the compiler's own
 * range checks cannot see from outside. The compiler half is the corpus.
 *
 * Built by runtime/tests/ffi_test.sh. Exit status 0 = every check passed. */
#include <stdint.h>

extern void rt_k_ffitest_xor(uint8_t *out, const uint8_t *a, const uint8_t *b, uint64_t n);
extern int64_t rt_k_ffitest_sum(const int64_t *p, uint64_t n);
extern void rt_k_ffitest_fill(uint8_t *p, uint64_t n, uint64_t v);
extern int64_t rt_k_ffitest_mix(int64_t a, int64_t b, int64_t c, int64_t d, int64_t e, int64_t f);
extern void rt_k_ffitest_double(int64_t *p, uint64_t n);
extern int64_t rt_k_ffitest_scale(int64_t k, const uint8_t *p, uint64_t n);

/* The compilers emit calls to these for an array initialiser even with no C
 * library; the empty asm in the loop keeps them from turning it into a call
 * to itself. */
void *memcpy(void *d, const void *s, unsigned long n) {
    uint8_t *dd = d;
    const uint8_t *ss = s;
    while (n--) {
        *dd++ = *ss++;
        __asm__ volatile("" ::: "memory");
    }
    return d;
}
void *memset(void *d, int c, unsigned long n) {
    uint8_t *dd = d;
    while (n--) {
        *dd++ = (uint8_t)c;
        __asm__ volatile("" ::: "memory");
    }
    return d;
}

extern int64_t rt_k_platform(void);
extern int64_t rt_k_cache_load(void);
extern void rt_k_cache_store(int64_t value);
extern void rt_k_cpuid(uint32_t leaf, uint32_t sub, uint32_t *out);

static long sys3(long n, long a, long b, long c) {
#if defined(__x86_64__)
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(n), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
#elif defined(__aarch64__)
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    return x0;
#else
#error "x86-64 and aarch64 only"
#endif
}

#if defined(__x86_64__)
#define SYS_WRITE 1
#define SYS_EXIT_GROUP 231
#else
#define SYS_WRITE 64
#define SYS_EXIT_GROUP 94
#endif

static int failures;
static void check(int ok, const char *what) {
    if (ok) return;
    failures++;
    const char *p = what;
    long n = 0;
    while (p[n]) n++;
    sys3(SYS_WRITE, 2, (long)"FAIL ", 5);
    sys3(SYS_WRITE, 2, (long)what, n);
    sys3(SYS_WRITE, 2, (long)"\n", 1);
}

#define CANARY 0xA5

/* A window of `n` octets with 16 canary octets either side. */
static uint8_t arena[3][256];

static int canaries_intact(const uint8_t *buf, int n) {
    for (int i = 0; i < 16; i++)
        if (buf[i] != CANARY || buf[16 + n + i] != CANARY) return 0;
    return 1;
}

static void arm(uint8_t *buf, int n, uint8_t fill) {
    for (int i = 0; i < 16; i++) buf[i] = buf[16 + n + i] = CANARY;
    for (int i = 0; i < n; i++) buf[16 + i] = fill;
}

static void run(void) {
    /* xor, every length around the loop's edges, odd offsets */
    for (int n = 0; n <= 33; n++) {
        for (int off = 0; off < 3; off++) {
            uint8_t *a = arena[0] + off, *b = arena[1] + off, *o = arena[2] + off;
            arm(a, n, 0);
            arm(b, n, 0);
            arm(o, n, 0x11);
            for (int i = 0; i < n; i++) {
                a[16 + i] = (uint8_t)(i * 7 + 1);
                b[16 + i] = (uint8_t)(i * 13 + 5);
            }
            rt_k_ffitest_xor(o + 16, a + 16, b + 16, (uint64_t)n);
            int ok = canaries_intact(o, n);
            for (int i = 0; i < n; i++) ok &= o[16 + i] == (uint8_t)(a[16 + i] ^ b[16 + i]);
            check(ok, "xor");
            ok = canaries_intact(a, n) && canaries_intact(b, n);
            check(ok, "xor leaves its inputs' surroundings alone");
        }
    }
    /* in place */
    {
        uint8_t *a = arena[0], *b = arena[1];
        arm(a, 20, 0x0f);
        arm(b, 20, 0xf0);
        rt_k_ffitest_xor(a + 16, a + 16, b + 16, 20);
        int ok = canaries_intact(a, 20);
        for (int i = 0; i < 20; i++) ok &= a[16 + i] == 0xff;
        check(ok, "xor in place");
    }
    /* sum and double over int64 */
    {
        int64_t v[8] = {1, -2, 3, -4, 5, -6, 7, -8};
        check(rt_k_ffitest_sum(v, 8) == -4, "sum");
        check(rt_k_ffitest_sum(v + 3, 0) == 0, "sum of nothing");
        check(rt_k_ffitest_sum(v + 1, 2) == 1, "sum of a middle range");
        int64_t w[6] = {-1, 1, 2, 3, 4, -1};
        rt_k_ffitest_double(w + 1, 4);
        check(w[0] == -1 && w[1] == 2 && w[2] == 4 && w[3] == 6 && w[4] == 8 && w[5] == -1, "double");
        int64_t big[1] = {INT64_C(1) << 62};
        rt_k_ffitest_double(big, 1);
        check(big[0] == INT64_MIN, "double wraps like the instruction does");
    }
    /* fill */
    for (int n = 0; n <= 20; n++) {
        uint8_t *p = arena[0];
        arm(p, n, 0);
        rt_k_ffitest_fill(p + 16, (uint64_t)n, 0x1234567890abcdefULL);
        int ok = canaries_intact(p, n);
        for (int i = 0; i < n; i++) ok &= p[16 + i] == 0xef;
        check(ok, "fill");
    }
    /* all six argument registers, 64-bit values */
    check(rt_k_ffitest_mix(1, 2, 3, 4, 5, 6) == 91, "mix");
    check(rt_k_ffitest_mix(-1, 0, 0, 0, 0, 1) == 5, "mix negative");
    check(rt_k_ffitest_mix(INT64_C(1) << 40, 0, 0, 0, 0, 0) == INT64_C(1) << 40, "mix keeps 64 bits");
    check(rt_k_ffitest_mix(0, 0, 0, 0, 0, INT64_C(1) << 40) == INT64_C(6) << 40, "mix keeps 64 bits in the last argument");
    /* platform, the feature cache word and cpuid (runtime/accel_<arch>.S) */
    {
#if defined(__x86_64__)
        check(rt_k_platform() == 1, "platform is x86-64");
#else
        check(rt_k_platform() == 2, "platform is aarch64 Linux");
#endif
        check(rt_k_cache_load() == 0, "the cache word starts empty");
        rt_k_cache_store(0x85);
        check(rt_k_cache_load() == 0x85, "the cache word keeps what was stored");
        rt_k_cache_store(INT64_C(1) << 40);
        check(rt_k_cache_load() == INT64_C(1) << 40, "the cache word is 64 bits");
        rt_k_cache_store(0);
        check(rt_k_cache_load() == 0, "the cache word can be emptied");
        uint32_t regs[4] = {0xAAAAAAAAu, 0xAAAAAAAAu, 0xAAAAAAAAu, 0xAAAAAAAAu};
        rt_k_cpuid(0, 0, regs);
#if defined(__x86_64__)
        check(regs[0] >= 1, "cpuid leaf 0 reports at least leaf 1");
        check((regs[1] | regs[2] | regs[3]) != 0, "cpuid leaf 0 reports a vendor");
#else
        check((regs[0] | regs[1] | regs[2] | regs[3]) == 0, "the aarch64 cpuid stub reports an all-zero CPU");
#endif
    }
    /* scale */
    {
        uint8_t t[8] = {1, 2, 3, 4, 5, 6, 7, 8};
        check(rt_k_ffitest_scale(1, t, 8) == 36, "scale 1");
        check(rt_k_ffitest_scale(4, t, 3) == 24, "scale 4");
        check(rt_k_ffitest_scale(2, t, 0) == 0, "scale of nothing");
    }
}

#if defined(__x86_64__)
__attribute__((force_align_arg_pointer))
#endif
void _start(void) {
    run();
    if (failures == 0) sys3(SYS_WRITE, 1, (long)"ok\n", 3);
    sys3(SYS_EXIT_GROUP, failures != 0, 0, 0);
    for (;;) {
    }
}
