#!/usr/bin/env python3
"""m31 crypto micro-benchmarks against the references on this host.

    python3 bench/crypto/run.py                       # everything, 5 reps
    python3 bench/crypto/run.py --ops sha256,aes-gcm-seal --reps 3
    python3 bench/crypto/run.py --tag after --baseline before.json

Builds bench/crypto/bench.m31 with the repo's ./build.sh into a fresh
temporary directory (no fixed /tmp paths), runs each (op, size) several
times and reports min and median.  References: `openssl speed` (also with
the SIMD/AES-NI capability bits masked, a "plain integer code" baseline),
Python's hashlib / `cryptography` / PyNaCl where installed.  Works on
Linux and macOS (taskset is used only where it exists).
"""
import argparse, hashlib, hmac as pyhmac, json, os, platform, re, shutil
import statistics, subprocess, sys, tempfile, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SIZES = [64, 1024, 16384, 1048576]
STREAM = ["sha1", "sha256", "sha512", "hmac-sha256", "chacha20", "poly1305",
          "chacha20-poly1305-seal", "chacha20-poly1305-open",
          "aes-gcm-seal", "aes-gcm-open"]
# (op, size) with a size of 0 means "not size dependent"
FIXED = [("hkdf", 32), ("aes-ecb-block", 16), ("x25519", 0), ("ed25519-keygen", 0),
         ("ed25519-sign", 0), ("ed25519-verify", 0), ("ecdsa-p256-sign", 0),
         ("ecdsa-p256-verify", 0), ("rsa2048-sign", 0), ("rsa2048-verify", 0),
         ("bcrypt-pbkdf", 16)]
MASK = "~0xfffffffffbffffff:~0xffffffffffffffff:~0xffffffffffffffff:~0xffffffffffffffff"


def cpu_model():
    try:
        if platform.system() == "Darwin":
            return subprocess.check_output(["sysctl", "-n", "machdep.cpu.brand_string"], text=True).strip()
        for line in open("/proc/cpuinfo"):
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except Exception:
        pass
    return platform.processor() or "unknown"


def prefix(pin):
    p = ["nice", "-n", "5"]
    if pin is not None and shutil.which("taskset"):
        p = ["taskset", "-c", str(pin)] + p
    return p


def build(workdir):
    out = os.path.join(workdir, "bench")
    r = subprocess.run(["./build.sh", "bench/crypto/bench.m31", "-o", out], cwd=ROOT,
                       capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit("build failed:\n" + r.stdout + r.stderr)
    return out


def key_args():
    """RSA-2048 and P-256 key material from `cryptography`, as hex argv."""
    from cryptography.hazmat.primitives.asymmetric import rsa, ec
    nums = rsa.generate_private_key(65537, 2048).private_numbers()
    pub = nums.public_numbers
    h = lambda v: ("%x" % v).rjust(((v.bit_length() + 7) // 8) * 2, "0")
    rsa_args = [h(pub.n), h(pub.e), h(nums.d), h(nums.iqmp), h(nums.p), h(nums.q)]
    ec_secret = ec.generate_private_key(ec.SECP256R1()).private_numbers().private_value
    return {"rsa": rsa_args, "ec": ["%064x" % ec_secret]}


def run_once(binary, op, size, iters, extra, pin):
    cmd = prefix(pin) + [binary, op, str(size), str(iters)] + extra
    r = subprocess.run(cmd, capture_output=True, text=True)
    for line in r.stdout.splitlines():
        if line.startswith("RESULT "):
            return int(line.split()[4])
    sys.exit("bench failed: %s\n%s%s" % (" ".join(cmd[:8]), r.stdout, r.stderr[-400:]))


def measure(binary, op, size, extra, reps, pin, budget_ns=300_000_000):
    first = run_once(binary, op, size, 1, extra, pin)
    iters = max(1, min(100000, int(budget_ns / max(first, 1))))
    samples = [first]
    if iters > 1:
        samples = []
    n_runs = reps if iters > 1 else max(reps, 3)
    for _ in range(n_runs):
        total = run_once(binary, op, size, iters, extra, pin)
        samples.append(total / iters)
    return {"min_ns": min(samples), "median_ns": statistics.median(samples), "iters": iters}


# ---- references -----------------------------------------------------------

def openssl_speed(args, mask=False, seconds=1):
    env = dict(os.environ)
    if mask:
        env["OPENSSL_ia32cap"] = MASK
    r = subprocess.run(["openssl", "speed", "-seconds", str(seconds)] + args, capture_output=True,
                       text=True, env=env)
    return r.stdout


def openssl_bulk(alg_args, size, mask):
    """MiB/s of an `openssl speed -evp` line at one size."""
    out = openssl_speed(["-bytes", str(size)] + alg_args, mask)
    for line in out.splitlines():
        m = re.match(r"^\S+.*?\s([0-9.]+)k\s*$", line)
        if m and "bytes" not in line and not line.startswith("The"):
            return float(m.group(1)) * 1000 / 1048576
    return None


def openssl_hmac(size, mask):
    out = openssl_speed(["-bytes", str(size), "-hmac", "sha256"], mask)
    for line in out.splitlines():
        m = re.match(r"^hmac.*?\s([0-9.]+)k\s*$", line)
        if m:
            return float(m.group(1)) * 1000 / 1048576
    return None


def openssl_asym(mask=False):
    """ops/s for the asymmetric rows: {name: ops_per_s}."""
    res = {}
    out = openssl_speed(["rsa2048", "ecdsap256", "ed25519", "x25519"], mask, seconds=2)
    for line in out.splitlines():
        f = line.split()
        if line.startswith("rsa 2048"):
            res["rsa2048-sign"], res["rsa2048-verify"] = float(f[-2]), float(f[-1])
        elif "256 bits ecdsa" in line:
            res["ecdsa-p256-sign"], res["ecdsa-p256-verify"] = float(f[-2]), float(f[-1])
        elif "Ed25519" in line:
            res["ed25519-sign"], res["ed25519-verify"] = float(f[-2]), float(f[-1])
        elif "X25519" in line:
            res["x25519"] = float(f[-1])
    return res


def timeit_mibs(fn, size, min_time=0.3):
    fn()
    n, t0 = 0, time.perf_counter()
    while time.perf_counter() - t0 < min_time:
        fn()
        n += 1
    el = time.perf_counter() - t0
    return n * size / el / 1048576, el / n * 1e9


def python_refs(size):
    """MiB/s from hashlib / cryptography for ops openssl speed has no row for."""
    res = {}
    data = bytes(range(256)) * (size // 256 + 1)
    data = data[:size]
    key, nonce = bytes(32), bytes(12)
    try:
        from cryptography.hazmat.primitives.ciphers import Cipher, algorithms
        from cryptography.hazmat.primitives import poly1305
        c = lambda: Cipher(algorithms.ChaCha20(key, bytes(16)), mode=None).encryptor().update(data)
        res["chacha20"] = timeit_mibs(c, size)[0]
        res["poly1305"] = timeit_mibs(lambda: poly1305.Poly1305.generate_tag(key, data), size)[0]
    except Exception:
        pass
    return res


def python_fixed():
    """ns per op for rows with no `openssl speed` equivalent."""
    res = {}
    try:
        from cryptography.hazmat.primitives.kdf.hkdf import HKDF
        from cryptography.hazmat.primitives import hashes
        f = lambda: HKDF(hashes.SHA256(), 64, bytes(16), bytes(16)).derive(bytes(32))
        res["hkdf"] = timeit_mibs(f, 1)[1]
    except Exception:
        pass
    try:
        from nacl.bindings import crypto_sign_seed_keypair
        res["ed25519-keygen"] = timeit_mibs(lambda: crypto_sign_seed_keypair(bytes(32)), 1)[1]
    except Exception:
        pass
    try:
        from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
        blk = bytes(16)
        e = lambda: Cipher(algorithms.AES(bytes(32)), modes.ECB()).encryptor().update(blk)
        res["aes-ecb-block"] = timeit_mibs(e, 1)[1]
    except Exception:
        pass
    return res


def bcrypt_ref(rounds):
    """ns for bcrypt_pbkdf at `rounds`, via ssh-keygen decrypting a key made with -a rounds."""
    if not (shutil.which("ssh-keygen")):
        return None
    d = tempfile.mkdtemp(prefix="m31bench-")
    try:
        k = os.path.join(d, "k")
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-a", str(rounds), "-N", "pw", "-f", k],
                       check=True, capture_output=True)
        best = None
        for _ in range(3):
            t0 = time.perf_counter()
            subprocess.run(["ssh-keygen", "-y", "-P", "pw", "-f", k], check=True, capture_output=True)
            dt = time.perf_counter() - t0
            best = dt if best is None else min(best, dt)
        return best * 1e9
    finally:
        shutil.rmtree(d, ignore_errors=True)


OSSL = {"sha1": ["-evp", "sha1"], "sha256": ["-evp", "sha256"], "sha512": ["-evp", "sha512"],
        "chacha20-poly1305-seal": ["-evp", "chacha20-poly1305"],
        "chacha20-poly1305-open": ["-evp", "chacha20-poly1305"],
        "aes-gcm-seal": ["-evp", "aes-256-gcm"], "aes-gcm-open": ["-evp", "aes-256-gcm"]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ops", help="comma list; default all")
    ap.add_argument("--sizes", default=",".join(map(str, SIZES)))
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--pin", type=int, help="cpu to pin to (taskset, Linux)")
    ap.add_argument("--tag", default="run")
    ap.add_argument("--out", help="write results JSON here")
    ap.add_argument("--baseline", help="JSON from an earlier run, for the before column")
    ap.add_argument("--no-ref", action="store_true", help="skip the reference measurements")
    a = ap.parse_args()
    want = set(a.ops.split(",")) if a.ops else None
    sizes = [int(s) for s in a.sizes.split(",")]
    work = tempfile.mkdtemp(prefix="m31bench-")
    try:
        binary = build(work)
        keys = key_args()
        results, refs = {}, {}
        jobs = [(op, s) for op in STREAM for s in sizes] + FIXED
        for op, s in jobs:
            if want and op not in want:
                continue
            extra = []
            if op.startswith("rsa2048"):
                extra = keys["rsa"]
            elif op.startswith("ecdsa"):
                extra = keys["ec"]
            r = measure(binary, op, s, extra, a.reps, a.pin)
            results["%s/%d" % (op, s)] = r
            print("%-26s %8d  min %12.0f ns  median %12.0f ns" % (op, s, r["min_ns"], r["median_ns"]),
                  flush=True)
        if not a.no_ref:
            for op, s in jobs:
                if want and op not in want:
                    continue
                k = "%s/%d" % (op, s)
                if op in OSSL:
                    refs[k] = {"openssl": openssl_bulk(OSSL[op], s, False),
                               "openssl_nosimd": openssl_bulk(OSSL[op], s, True)}
                elif op == "hmac-sha256":
                    refs[k] = {"openssl": openssl_hmac(s, False), "openssl_nosimd": openssl_hmac(s, True)}
                elif op in ("chacha20", "poly1305"):
                    refs[k] = {"python-cryptography": python_refs(s).get(op)}
            asym = openssl_asym(False)
            asym_ns = openssl_asym(True)
            pyf = python_fixed()
            for op, s in FIXED:
                if want and op not in want:
                    continue
                k = "%s/%d" % (op, s)
                ent = {}
                if op in asym:
                    ent["openssl_ns"] = 1e9 / asym[op]
                if op in pyf:
                    ent["python_ns"] = pyf[op]
                if op == "bcrypt-pbkdf":
                    b = bcrypt_ref(16)
                    if b:
                        ent["ssh-keygen_ns"] = b
                refs[k] = ent
        meta = {"tag": a.tag, "cpu": cpu_model(), "os": platform.platform(),
                "date": time.strftime("%Y-%m-%d %H:%M:%S"), "reps": a.reps,
                "load": os.getloadavg(), "openssl": subprocess.check_output(["openssl", "version"], text=True).strip()}
        doc = {"meta": meta, "results": results, "refs": refs}
        if a.out:
            json.dump(doc, open(a.out, "w"), indent=1)
        report(doc, json.load(open(a.baseline)) if a.baseline else None)
    finally:
        shutil.rmtree(work, ignore_errors=True)


def mibs(ns, size):
    return size / (ns / 1e9) / 1048576


def report(doc, base):
    print("\n# %s  (%s, load %.1f)" % (doc["meta"]["tag"], doc["meta"]["cpu"], doc["meta"]["load"][0]))
    print("| op | size | min ns/op | MiB/s | median MiB/s | before MiB/s | openssl MiB/s | openssl ratio |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for k, r in doc["results"].items():
        op, s = k.rsplit("/", 1)
        s = int(s)
        ref = doc["refs"].get(k, {})
        if s and op in STREAM or op in ("hkdf", "aes-ecb-block"):
            m = mibs(r["min_ns"], s)
            md = mibs(r["median_ns"], s)
            bm = ""
            if base and k in base["results"]:
                bm = "%.2f" % mibs(base["results"][k]["min_ns"], s)
            o = ref.get("openssl") or ref.get("python-cryptography")
            ratio = "%.1fx" % (o / m) if o else ""
            print("| %s | %d | %.0f | %.2f | %.2f | %s | %s | %s |" %
                  (op, s, r["min_ns"], m, md, bm, "%.1f" % o if o else "", ratio))
        else:
            ops = 1e9 / r["min_ns"]
            bm = ""
            if base and k in base["results"]:
                bm = "%.1f" % (1e9 / base["results"][k]["min_ns"])
            on = ref.get("openssl_ns") or ref.get("python_ns") or ref.get("ssh-keygen_ns")
            print("| %s | %d | %.0f | %.1f ops/s | %.1f ops/s | %s | %s | %s |" %
                  (op, s, r["min_ns"], ops, 1e9 / r["median_ns"], bm,
                   "%.1f ops/s" % (1e9 / on) if on else "", "%.1fx" % (r["min_ns"] / on) if on else ""))


if __name__ == "__main__":
    main()
