#!/usr/bin/env python3
"""Tests the assembly crypto kernels directly, against independent oracles.

    python3 runtime/tests/crypto_kernels_test.py x86_64     # native; SHA-NI kernels need an emulated or real CPU with SHA
    python3 runtime/tests/crypto_kernels_test.py aarch64    # cross-assembled, run under qemu-aarch64

x86_64 loads runtime/crypto_x86_64.S as a shared object and calls the kernels
through ctypes; with `--sha-via-qemu` (or when the CPU has no SHA extensions)
the SHA-1/SHA-256 cases run in a second pass under `qemu-x86_64 -cpu max`.
aarch64 builds a freestanding test program (clang --target=aarch64-linux-gnu,
rust-lld, no libc) containing the vectors, and runs it under qemu-aarch64 with
`-cpu max` (every ARMv8 crypto extension). Oracles: `cryptography` (OpenSSL)
for AES, ChaCha20 and Poly1305; hashlib for the hashes; a straight Python
GHASH for GCM's multiplication. Random inputs from a fixed seed, odd offsets
(unaligned buffers), sizes 0, 1, around every block boundary and several blocks.
"""
import ctypes, hashlib, os, random, shutil, struct, subprocess, sys, tempfile

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
rnd = random.Random(20261016)


def rb(n):
    return bytes(rnd.getrandbits(8) for _ in range(n))


def aes_schedule(key):
    sb = [0] * 256
    p = q = 1
    while True:
        p = p ^ ((p << 1) & 0xFF) ^ (0x1B if p & 0x80 else 0)
        q ^= q << 1
        q ^= q << 2
        q ^= q << 4
        q &= 0xFF
        if q & 0x80:
            q ^= 0x09
        x = q ^ ((q << 1 | q >> 7) & 0xFF) ^ ((q << 2 | q >> 6) & 0xFF) ^ ((q << 3 | q >> 5) & 0xFF) ^ ((q << 4 | q >> 4) & 0xFF)
        sb[p] = (x ^ 0x63) & 0xFF
        if p == 1:
            break
    sb[0] = 0x63
    nk = len(key) // 4
    rounds = nk + 6
    w = [list(key[4 * i:4 * i + 4]) for i in range(nk)]
    rc = 1
    for i in range(nk, 4 * (rounds + 1)):
        t = list(w[i - 1])
        if i % nk == 0:
            t = t[1:] + t[:1]
            t = [sb[x] for x in t]
            t[0] ^= rc
            rc = ((rc << 1) ^ 0x11B) & 0xFF if rc & 0x80 else rc << 1
        elif nk > 6 and i % nk == 4:
            t = [sb[x] for x in t]
        w.append([a ^ b for a, b in zip(w[i - nk], t)])
    return bytes(sum(w, [])), rounds


def gmul(x, y):
    R = 0xE1 << 120
    z, v = 0, y
    for i in range(128):
        if (x >> (127 - i)) & 1:
            z ^= v
        v = (v >> 1) ^ R if v & 1 else v >> 1
    return z


def clamp(r):
    r = bytearray(r)
    for i in (3, 7, 11, 15):
        r[i] &= 15
    for i in (4, 8, 12):
        r[i] &= 252
    return bytes(r)


def pad_md(msg, blk):
    lenb = 16 if blk == 128 else 8
    m = msg + b"\x80"
    while (len(m) + lenb) % blk:
        m += b"\0"
    return m + (len(msg) * 8).to_bytes(lenb, "big")


H1 = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0]
H256 = [0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19]
H512 = [0x6A09E667F3BCC908, 0xBB67AE8584CAA73B, 0x3C6EF372FE94F82B, 0xA54FF53A5F1D36F1,
        0x510E527FADE682D1, 0x9B05688C2B3E6C1F, 0x1F83D9ABFB41BD6B, 0x5BE0CD19137E2179]
SIZES = list(range(0, 20)) + [31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 257, 511, 512, 513, 1000, 4096]


def cases():
    """Every kernel call as (name, inputs dict, expected output bytes). Offsets are odd on purpose."""
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
    from cryptography.hazmat.primitives import poly1305
    out = []
    ecb = lambda k: Cipher(algorithms.AES(k), modes.ECB()).encryptor()
    for klen in (16, 32):
        key = rb(klen)
        rk, rounds = aes_schedule(key)
        for n in list(range(0, 18)) + [31, 64]:
            data = rb(16 * n)
            out.append(("aes_ecb", dict(rk=rk, rounds=rounds, data=data), ecb(key).update(data) if n else b""))
            for ctr0 in (rb(16), b"\xff" * 16, b"\0" * 8 + b"\xff" * 8, b"\x01" * 8 + b"\xff" * 7 + b"\xfe"):
                c = int.from_bytes(ctr0, "big")
                ks = b"".join(ecb(key).update(((c + i) % (1 << 128)).to_bytes(16, "big")) for i in range(n))
                exp = bytes(a ^ b for a, b in zip(data, ks)) + ((c + n) % (1 << 128)).to_bytes(16, "big")
                out.append(("aes_ctr", dict(rk=rk, rounds=rounds, ctr=ctr0, data=data), exp))
    for n in list(range(0, 10)) + [33]:
        h, acc0, data = rb(16), rb(16), rb(16 * n)
        a, H = int.from_bytes(acc0, "big"), int.from_bytes(h, "big")
        for i in range(n):
            a = gmul(a ^ int.from_bytes(data[16 * i:16 * i + 16], "big"), H)
        out.append(("ghash", dict(h=h, acc=acc0, data=data), a.to_bytes(16, "big")))
    for ln in SIZES:
        key, iv, data = rb(32), rb(16), rb(ln)
        exp = Cipher(algorithms.ChaCha20(key, iv), mode=None).encryptor().update(data) if ln else b""
        out.append(("chacha20", dict(key=key, iv=iv, data=data), exp))
    for ln in SIZES:
        for shape in ("random", "ff"):
            key = rb(32) if shape == "random" else b"\xff" * 16 + rb(16)
            msg = rb(ln) if shape == "random" else b"\xff" * ln
            st = b"\0" * 24 + clamp(key[:16])
            full = ln // 16
            rest = msg[full * 16:]
            last = (rest + b"\x01").ljust(16, b"\0") if rest else b""
            out.append(("poly1305", dict(st=st, pad=key[16:], data=msg[:full * 16], last=last),
                        poly1305.Poly1305.generate_tag(key, msg)))
    for ln in list(range(0, 200)) + [1000, 4096]:
        msg = rb(ln)
        out.append(("sha1", dict(state=struct.pack("<5I", *H1), data=pad_md(msg, 64)), hashlib.sha1(msg).digest()))
        out.append(("sha256", dict(state=struct.pack("<8I", *H256), data=pad_md(msg, 64)), hashlib.sha256(msg).digest()))
        out.append(("sha512", dict(state=struct.pack("<8Q", *H512), data=pad_md(msg, 128)), hashlib.sha512(msg).digest()))
    return out


P25519 = 2**255 - 19
E10 = [0, 26, 51, 77, 102, 128, 153, 179, 204, 230]


def fe_value(limbs):
    return sum(l << E10[i] for i, l in enumerate(limbs))


def fe_carry(h):
    h = list(h)
    for i, w, nxt in ((0, 26, 1), (4, 26, 5), (1, 25, 2), (5, 25, 6), (2, 26, 3), (6, 26, 7), (3, 25, 4), (7, 25, 8),
                      (4, 26, 5), (8, 26, 9), (9, 25, 0), (0, 26, 1)):
        c = (h[i] + (1 << (w - 1))) >> w
        h[nxt] += c * (19 if nxt == 0 else 1)
        h[i] -= c << w
    return h


def fe_mul_ref(f, g):
    h = [0] * 10
    for i in range(10):
        for j in range(10):
            k, coef = i + j, 1
            if i % 2 and j % 2:
                coef *= 2
            if k >= 10:
                k -= 10
                coef *= 19
            h[k] += coef * f[i] * g[j]
    return fe_carry(h)


def fe_limbs():
    """Random carried-range limbs: even limbs up to 2^26, odd up to 2^25 in magnitude."""
    return [rnd.randint(-(1 << (26 - i % 2)), 1 << (26 - i % 2)) for i in range(10)]


def fe_cases():
    out = []
    for _ in range(300):
        f, g = fe_limbs(), fe_limbs()
        for name, exp_val, exp in (("fe_mul", f and fe_value(f) * fe_value(g), fe_mul_ref(f, g)),
                                   ("fe_add", fe_value(f) + fe_value(g), fe_carry([a + b for a, b in zip(f, g)])),
                                   ("fe_sub", fe_value(f) - fe_value(g), fe_carry([a - b for a, b in zip(f, g)]))):
            assert (fe_value(exp) - exp_val) % P25519 == 0, name       # the reference itself is right
            assert all(abs(x) <= 1 << 26 for x in exp), name
            out.append((name, dict(f=f, g=g), exp))
    for f in ([1 << 26] * 10, [-(1 << 26)] * 10, [0] * 10):             # the extremes
        out.append(("fe_mul", dict(f=f, g=f), fe_mul_ref(f, f)))
    return out


def digest_of(name, state):
    if name == "sha1":
        return b"".join(struct.pack(">I", w) for w in struct.unpack("<5I", state))
    if name == "sha256":
        return b"".join(struct.pack(">I", w) for w in struct.unpack("<8I", state))
    return b"".join(struct.pack(">Q", w) for w in struct.unpack("<8Q", state))


def run_x86(only=None):
    work = tempfile.mkdtemp(prefix="m31kern-")
    try:
        so = os.path.join(work, "libk.so")
        subprocess.run(["cc", "-shared", "-fPIC", "-o", so, os.path.join(ROOT, "runtime", "crypto_x86_64.S")], check=True)
        L = ctypes.CDLL(so)
        buf = lambda b: ctypes.create_string_buffer(bytes(b) + b"\0", len(b) + 1)
        u = ctypes.c_uint64
        failures = 0
        for name, a, exp in cases():
            if only and name not in only:
                continue
            pre = rb(3)
            if name == "aes_ecb":
                n = len(a["data"]) // 16
                din, o = buf(pre + a["data"]), buf(bytes(3 + len(a["data"])))
                L.rt_k_aes_ecb(buf(a["rk"]), u(a["rounds"]), ctypes.byref(din, 3), ctypes.byref(o, 3), u(n))
                got = o.raw[3:3 + len(exp)]
            elif name == "aes_ctr":
                n = len(a["data"]) // 16
                din, o, c = buf(pre + a["data"]), buf(bytes(3 + len(a["data"]))), buf(a["ctr"])
                L.rt_k_aes_ctr(buf(a["rk"]), u(a["rounds"]), c, ctypes.byref(din, 3), ctypes.byref(o, 3), u(n))
                got = o.raw[3:3 + len(a["data"])] + c.raw[:16]
            elif name == "ghash":
                n = len(a["data"]) // 16
                din, acc = buf(pre + a["data"]), buf(a["acc"])
                L.rt_k_ghash(buf(a["h"]), acc, ctypes.byref(din, 3), u(n))
                got = acc.raw[:16]
            elif name == "chacha20":
                ln = len(a["data"])
                din, o = buf(pre + a["data"]), buf(bytes(3 + ln))
                L.rt_k_chacha20(buf(a["key"]), buf(a["iv"]), ctypes.byref(din, 3), ctypes.byref(o, 3), u(ln))
                got = o.raw[3:3 + ln]
                if o.raw[:3] != bytes(3) or o.raw[3 + ln:3 + ln + 1] != b"\0":
                    got = b"overrun"
            elif name == "poly1305":
                st = buf(a["st"])
                nfull = len(a["data"]) // 16
                if nfull:
                    din = buf(pre + a["data"])
                    L.rt_k_poly1305_blocks(st, ctypes.byref(din, 3), u(nfull), u(1))
                if a["last"]:
                    L.rt_k_poly1305_blocks(st, buf(a["last"]), u(1), u(0))
                o = buf(bytes(16))
                L.rt_k_poly1305_finish(st, buf(a["pad"]), o)
                got = o.raw[:16]
            else:
                continue
            if got != exp:
                failures += 1
                print("FAIL", name, len(a.get("data", b"")))
        for name, a, exp in fe_cases():
            if only and "fe" not in only:
                continue
            arr = lambda v: (ctypes.c_int64 * 10)(*v)
            o, f, g = arr([0] * 10), arr(a["f"]), arr(a["g"])
            getattr(L, "rt_k_" + name)(o, f, g)
            if list(o) != exp:
                failures += 1
                print("FAIL", name)
            # aliasing: out == f
            getattr(L, "rt_k_" + name)(f, f, g)
            if list(f) != exp:
                failures += 1
                print("FAIL (alias)", name)
        print("x86_64 native kernels: %s" % ("OK" if not failures else "%d FAILED" % failures))
        return failures
    finally:
        shutil.rmtree(work, ignore_errors=True)


def run_x86_hashes():
    """SHA-1/SHA-256/SHA-512: native when the CPU has SHA, else under qemu -cpu max."""
    work = tempfile.mkdtemp(prefix="m31kern-")
    try:
        so = os.path.join(work, "libk.so")
        subprocess.run(["cc", "-shared", "-fPIC", "-o", so, os.path.join(ROOT, "runtime", "crypto_x86_64.S")], check=True)
        script = os.path.join(work, "hash_check.py")
        open(script, "w").write(open(__file__).read().replace("if __name__ == \"__main__\":", "if False:") + HASH_DRIVER.replace("@SO@", so))
        have_sha = " sha_ni" in open("/proc/cpuinfo").read() if os.path.exists("/proc/cpuinfo") else False
        cmd = [sys.executable, script]
        if not have_sha:
            q = shutil.which("qemu-x86_64-static") or shutil.which("qemu-x86_64")
            if not q:
                print("x86_64 SHA-NI kernels: SKIPPED (no SHA extensions here and no qemu-x86_64)")
                return 0
            cmd = [q, "-cpu", "max", "-L", "/"] + cmd
        r = subprocess.run(cmd, capture_output=True, text=True)
        print(r.stdout.strip() or r.stderr.strip()[-500:])
        return 0 if "OK" in r.stdout and "FAILED" not in r.stdout else 1
    finally:
        shutil.rmtree(work, ignore_errors=True)


HASH_DRIVER = '''
def _main():
    L = ctypes.CDLL("@SO@")
    buf = lambda b: ctypes.create_string_buffer(bytes(b) + b"\\0", len(b) + 1)
    bad = 0
    for name, a, exp in cases():
        if not name.startswith("sha"):
            continue
        n = len(a["data"]) // (128 if name == "sha512" else 64)
        din, st = buf(b"\\0\\0\\0" + a["data"]), buf(a["state"])
        getattr(L, "rt_k_" + name)(st, ctypes.byref(din, 3), ctypes.c_uint64(n))
        if digest_of(name, st.raw[:len(a["state"])]) != exp:
            bad += 1
            print("FAIL", name, len(a["data"]))
    print("x86_64 hash kernels (SHA-1, SHA-256 via SHA-NI, SHA-512): %s" % ("OK" if not bad else "%d FAILED" % bad))
_main()
'''


A64_C = r'''
typedef unsigned long u64;
typedef unsigned char u8;
extern void rt_k_aes_ecb(const u8 *rk, u64 rounds, const u8 *in, u8 *out, u64 n);
extern void rt_k_aes_ctr(const u8 *rk, u64 rounds, u8 *ctr, const u8 *in, u8 *out, u64 n);
extern void rt_k_ghash(const u8 *h, u8 *acc, const u8 *data, u64 n);
extern void rt_k_chacha20(const u8 *key, const u8 *iv, const u8 *in, u8 *out, u64 len);
extern void rt_k_poly1305_blocks(u8 *st, const u8 *data, u64 n, u64 hibit);
extern void rt_k_poly1305_finish(const u8 *st, const u8 *pad, u8 *out);
extern void rt_k_sha256(u8 *state, const u8 *data, u64 n);
extern void rt_k_sha512(u8 *state, const u8 *data, u64 n);
extern void rt_k_sha1(u8 *state, const u8 *data, u64 n);
extern void rt_k_fe_mul(long *out, const long *f, const long *g);
extern void rt_k_fe_add(long *out, const long *f, const long *g);
extern void rt_k_fe_sub(long *out, const long *f, const long *g);
void *memcpy(void *d, const void *s, u64 n) { u8 *a = d; const u8 *b = s; while (n--) *a++ = *b++; return d; }
void *memset(void *d, int c, u64 n) { u8 *a = d; while (n--) *a++ = (u8)c; return d; }
static int same(const u8 *a, const u8 *b, u64 n) { while (n--) if (*a++ != *b++) return 0; return 1; }
static void say(const char *s) { u64 n = 0; while (s[n]) n++; register long x8 __asm__("x8") = 64; register long x0 __asm__("x0") = 1; register const char *x1 __asm__("x1") = s; register long x2 __asm__("x2") = n; __asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory"); }
static u8 scratch[8192], scratch2[8192], tmp16[16], tmp40[40];
static u64 total, failed;
static void say_num(u64 v) { char b[24]; int i = 23; b[i] = 0; do { b[--i] = (char)(48 + v % 10); v /= 10; } while (v); say(b + i); }
#define CHECK(ok, name) do { total++; if (!(ok)) { failed++; say("FAIL "); say(name); say("\n"); } } while (0)
'''


def c_array(name, b):
    return "static const u8 %s[] = {%s};" % (name, ",".join(str(x) for x in (b or b"\0")))


def run_aarch64():
    clang = shutil.which("clang")
    qemu = shutil.which("qemu-aarch64-static") or shutil.which("qemu-aarch64")
    lld = None
    for cand in (shutil.which("ld.lld"), shutil.which("lld")):
        if cand:
            lld = [cand]
    if not lld:
        home = os.path.expanduser("~/.rustup/toolchains")
        if os.path.isdir(home):
            for t in sorted(os.listdir(home)):
                c = os.path.join(home, t, "lib", "rustlib", "x86_64-unknown-linux-gnu", "bin", "rust-lld")
                if os.path.exists(c):
                    lld = [c, "-flavor", "gnu"]
                    break
    if not (clang and qemu and lld):
        print("aarch64 kernels: SKIPPED (need clang, qemu-aarch64 and an aarch64-capable lld)")
        return 0
    work = tempfile.mkdtemp(prefix="m31kern-")
    try:
        src = [A64_C]
        calls = []
        for i, (name, a, exp) in enumerate(cases()):
            pre = b"\0\0\0"
            for k, v in a.items():
                if isinstance(v, (bytes, bytearray)):
                    src.append(c_array("c%d_%s" % (i, k), v))
            src.append(c_array("c%d_exp" % i, exp))
            nm = "%s#%d" % (name, i)
            g = lambda k: "c%d_%s" % (i, k)
            if name == "aes_ecb":
                n = len(a["data"]) // 16
                body = ("memcpy(scratch + 3, %s, %d); memset(scratch2, 0, %d); rt_k_aes_ecb(%s, %d, scratch + 3, scratch2 + 3, %d); CHECK(same(scratch2 + 3, %s, %d) && scratch2[0] == 0 && scratch2[%d] == 0, \"%s\");"
                        % (g("data"), len(a["data"]), len(a["data"]) + 8, g("rk"), a["rounds"], n, g("exp"), len(exp), 3 + len(exp), nm))
            elif name == "aes_ctr":
                n = len(a["data"]) // 16
                body = ("memcpy(scratch + 3, %s, %d); memcpy(tmp16, %s, 16); rt_k_aes_ctr(%s, %d, tmp16, scratch + 3, scratch2 + 3, %d); CHECK(same(scratch2 + 3, %s, %d) && same(tmp16, %s + %d, 16), \"%s\");"
                        % (g("data"), len(a["data"]), g("ctr"), g("rk"), a["rounds"], n, g("exp"), len(a["data"]), g("exp"), len(a["data"]), nm))
            elif name == "ghash":
                n = len(a["data"]) // 16
                body = ("memcpy(scratch + 3, %s, %d); memcpy(tmp16, %s, 16); rt_k_ghash(%s, tmp16, scratch + 3, %d); CHECK(same(tmp16, %s, 16), \"%s\");"
                        % (g("data"), len(a["data"]), g("acc"), g("h"), n, g("exp"), nm))
            elif name == "chacha20":
                ln = len(a["data"])
                body = ("memcpy(scratch + 3, %s, %d); memset(scratch2, 0, %d); rt_k_chacha20(%s, %s, scratch + 3, scratch2 + 3, %d); CHECK(same(scratch2 + 3, %s, %d) && scratch2[0] == 0 && scratch2[%d] == 0, \"%s\");"
                        % (g("data"), ln, ln + 8, g("key"), g("iv"), ln, g("exp"), ln, 3 + ln, nm))
            elif name == "poly1305":
                nfull = len(a["data"]) // 16
                body = "memcpy(tmp40, %s, 40); " % g("st")
                if nfull:
                    body += "memcpy(scratch + 3, %s, %d); rt_k_poly1305_blocks(tmp40, scratch + 3, %d, 1); " % (g("data"), len(a["data"]), nfull)
                if a["last"]:
                    body += "rt_k_poly1305_blocks(tmp40, %s, 1, 0); " % g("last")
                body += "rt_k_poly1305_finish(tmp40, %s, tmp16); CHECK(same(tmp16, %s, 16), \"%s\");" % (g("pad"), g("exp"), nm)
            else:
                blk = 128 if name == "sha512" else 64
                n = len(a["data"]) // blk
                sl = len(a["state"])
                words = {"sha1": ("u32", 5), "sha256": ("u32", 8), "sha512": ("u64", 8)}[name]
                body = ("memcpy(scratch + 3, %s, %d); memcpy(tmp40, %s, %d); rt_k_%s(tmp40, scratch + 3, %d); {u8 d[64]; %s CHECK(same(d, %s, %d), \"%s\");}"
                        % (g("data"), len(a["data"]), g("state"), sl, name, n,
                           "for (int w = 0; w < %d; w++) for (int j = 0; j < %d; j++) d[w * %d + j] = tmp40[w * %d + %d - 1 - j];" % (words[1], 4 if words[0] == "u32" else 8, 4 if words[0] == "u32" else 8, 4 if words[0] == "u32" else 8, 4 if words[0] == "u32" else 8),
                           g("exp"), len(exp), nm))
            calls.append("    " + body)
        for i, (name, a, exp) in enumerate(fe_cases()):
            lit = lambda v: "{" + ",".join("%dL" % x for x in v) + "}"
            src.append("static const long fe%d_f[10] = %s, fe%d_g[10] = %s, fe%d_e[10] = %s;" % (i, lit(a["f"]), i, lit(a["g"]), i, lit(exp)))
            calls.append("    { long o[10], h[10]; for (int k = 0; k < 10; k++) h[k] = fe%d_f[k]; rt_k_%s(o, fe%d_f, fe%d_g); rt_k_%s(h, h, fe%d_g); int ok = 1; for (int k = 0; k < 10; k++) ok &= o[k] == fe%d_e[k] && h[k] == fe%d_e[k]; CHECK(ok, \"%s#%d\"); }" % (i, name, i, i, name, i, i, i, name, i))
        # tmp40 is only 40 octets; the SHA states are up to 64
        csrc = "\n".join(src).replace("tmp16[16], tmp40[40]", "tmp16[16], tmp40[64]") + "\nint cmain(void) {\n" + "\n".join(calls) + \
            '\n    say(failed ? "aarch64 kernels: FAILED (" : "aarch64 kernels: OK ("); say_num(total); say(" checks)\\n");\n    return failed ? 1 : 0;\n}\n'
        cpath = os.path.join(work, "t.c")
        open(cpath, "w").write(csrc)
        open(os.path.join(work, "start.S"), "w").write(".globl _start\n_start:\n    bl cmain\n    mov x8, #93\n    svc #0\n")
        tgt = ["--target=aarch64-linux-gnu"]
        subprocess.run([clang] + tgt + ["-O1", "-ffreestanding", "-fno-builtin", "-c", cpath, "-o", os.path.join(work, "t.o")], check=True)
        subprocess.run([clang] + tgt + ["-c", os.path.join(work, "start.S"), "-o", os.path.join(work, "start.o")], check=True)
        subprocess.run([clang] + tgt + ["-c", os.path.join(ROOT, "runtime", "crypto_aarch64.S"), "-o", os.path.join(work, "k.o")], check=True)
        subprocess.run(lld + ["-static", "-nostdlib", "-e", "_start", "-o", os.path.join(work, "t"),
                              os.path.join(work, "start.o"), os.path.join(work, "t.o"), os.path.join(work, "k.o")], check=True)
        r = subprocess.run([qemu, "-cpu", "max", os.path.join(work, "t")], capture_output=True, text=True)
        print(r.stdout.strip() + (" " + r.stderr.strip() if r.stderr.strip() else ""))
        return r.returncode
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    arch = sys.argv[1] if len(sys.argv) > 1 else "x86_64"
    if arch == "x86_64":
        rc = run_x86({"aes_ecb", "aes_ctr", "ghash", "chacha20", "poly1305", "fe"}) + run_x86_hashes()
    else:
        rc = run_aarch64()
    sys.exit(1 if rc else 0)
