#!/usr/bin/env python3
"""Checks `lib/crypto/rsasign.m31` (through the `t_rsasign` binary, argv[1])
against OpenSSL via `cryptography`: RSASSA-PKCS1-v1_5 is deterministic, so
every signature must be byte-identical to OpenSSL's, for 2048-, 3072- and
4096-bit keys, SHA-256 and SHA-512, assorted digests (including all-zero and
all-0xFF ones that exercise the carry paths), and unusual public exponents.
Inconsistent keys (wrong q, swapped CRT coefficient, a 1024-bit key, an even
factor) must be refused.
"""
import hashlib
import os
import subprocess
import sys

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa

BIN = sys.argv[1]


def be(n):
    return n.to_bytes((n.bit_length() + 7) // 8 or 1, "big").hex()


def key_fields(priv):
    nums = priv.private_numbers()
    pub = nums.public_numbers
    return [be(pub.n), be(pub.e), be(nums.d), be(nums.iqmp), be(nums.p), be(nums.q)]


vectors, wants, labels = [], [], []
sizes = [2048, 2048, 3072, 4096]
for bits in sizes:
    for exponent in ([65537, 3] if bits == 2048 else [65537]):
        priv = rsa.generate_private_key(public_exponent=exponent, key_size=bits)
        fields = key_fields(priv)
        digests = [
            ("sha256", hashlib.sha256(os.urandom(40)).digest()),
            ("sha256", bytes(32)),
            ("sha256", b"\xff" * 32),
            ("sha512", hashlib.sha512(os.urandom(40)).digest()),
            ("sha512", b"\xff" * 64),
        ]
        for name, digest in digests:
            h = hashes.SHA256() if name == "sha256" else hashes.SHA512()
            from cryptography.hazmat.primitives.asymmetric.utils import Prehashed
            want = priv.sign(digest, padding.PKCS1v15(), Prehashed(h)).hex()
            vectors.append("sign %s %s %s" % (name, " ".join(fields), digest.hex()))
            wants.append("signature " + want)
            labels.append("%d-bit e=%d %s" % (bits, exponent, name))

# refusals
priv = rsa.generate_private_key(public_exponent=65537, key_size=2048)
f = key_fields(priv)
d = hashlib.sha256(b"x").hexdigest()
bad = [
    ("p and q swapped against iqmp", [f[0], f[1], f[2], f[3], f[5], f[4]]),
    ("wrong iqmp", [f[0], f[1], f[2], be(int(f[3], 16) + 2), f[4], f[5]]),
    ("q replaced", [f[0], f[1], f[2], f[3], f[4], be(int(f[5], 16) + 2)]),
]
for label, fields in bad:
    vectors.append("sign sha256 %s %s" % (" ".join(fields), d))
    wants.append(None)
    labels.append("refused: " + label)
small = rsa.generate_private_key(public_exponent=65537, key_size=1024)
vectors.append("sign sha256 %s %s" % (" ".join(key_fields(small)), d))
wants.append(None)
labels.append("refused: 1024-bit key")

p = subprocess.run([BIN], input="\n".join(vectors) + "\n", capture_output=True, text=True, timeout=900)
if p.returncode != 0:
    print("FAIL t_rsasign exited %d: %s" % (p.returncode, p.stderr[-300:]))
    sys.exit(1)
got = p.stdout.strip().split("\n")
failures = []
if len(got) != len(wants):
    failures.append("answer count %d != %d" % (len(got), len(wants)))
for label, g, w in zip(labels, got, wants):
    if w is None:
        if not g.startswith("refused"):
            failures.append("%s: expected refusal, got %s" % (label, g[:60]))
    elif g != w:
        failures.append("%s: signature differs from OpenSSL's" % label)
if failures:
    for x in failures[:15]:
        print("FAIL " + x)
    sys.exit(1)
print("%d checks, 0 failures" % len(wants))
