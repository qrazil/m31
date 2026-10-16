#!/usr/bin/env python3
"""Before/after/OpenSSL table from two run.py JSON files.

    python3 bench/crypto/table.py before.json after.json [after_noaccel.json]

Per primitive and size: MiB/s (or ops/s) before, after, after with the
hardware kernels switched off (`M31_CRYPTO_ACCEL=off`, optional third file),
OpenSSL's number from the "after" run's references, and how many times slower
than OpenSSL the "after" is. Min of the repetitions throughout.
"""
import json, sys

STREAM = ("sha1", "sha256", "sha512", "hmac-sha256", "chacha20", "poly1305", "chacha20-poly1305-seal",
          "chacha20-poly1305-open", "aes-gcm-seal", "aes-gcm-open")


def rate(key, r):
    op, size = key.rsplit("/", 1)
    size = int(size)
    if op in STREAM or op in ("hkdf", "aes-ecb-block"):
        return r["min_ns"] and size / (r["min_ns"] / 1e9) / 1048576, "MiB/s"
    return 1e9 / r["min_ns"], "ops/s"


def fmt(v):
    return "%.0f" % v if v >= 100 else ("%.1f" % v if v >= 10 else "%.2f" % v)


def main():
    before = json.load(open(sys.argv[1]))
    after = json.load(open(sys.argv[2]))
    noacc = json.load(open(sys.argv[3])) if len(sys.argv) > 3 else None
    meta = after["meta"]
    print("Host: %s; %s; load average at the end %.1f; min of %d runs each.\n" % (meta["cpu"], meta["openssl"], meta["load"][0], meta["reps"]))
    print("| primitive | size | before | after | after, kernels off | OpenSSL | after vs before | OpenSSL vs after |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for key, r in after["results"].items():
        if key not in before["results"]:
            continue
        op, size = key.rsplit("/", 1)
        if op in STREAM and size == "64":
            continue
        b, unit = rate(key, before["results"][key])
        a, _ = rate(key, r)
        n = rate(key, noacc["results"][key])[0] if noacc and key in noacc["results"] else None
        ref = after["refs"].get(key, {})
        o = ref.get("openssl")
        if o is None and "openssl_ns" in ref:
            o = 1e9 / ref["openssl_ns"]
        if o is None and op in ("chacha20", "poly1305") and int(size) >= 16384:
            o = ref.get("python-cryptography")
        label = op if size in ("0",) else "%s" % op
        sz = "" if size == "0" else ("%d B" % int(size) if int(size) < 1024 else ("%d KiB" % (int(size) // 1024) if int(size) < 1 << 20 else "%d MiB" % (int(size) >> 20)))
        print("| %s | %s | %s %s | %s %s | %s | %s | %.1fx | %s |" % (
            label, sz, fmt(b), unit, fmt(a), unit, fmt(n) if n else "", fmt(o) if o else "", a / b,
            ("%.1fx" % (o / a) if o and o > a else ("%.1fx faster" % (a / o) if o else "")) if o else ""))


if __name__ == "__main__":
    main()
