#!/usr/bin/env python3
"""Cases and expected output for `lib/crypto/ecdsasign.m31` (`t_tlsserver_sign.m31`).

    python3 tlsserver_sign_oracle.py CASES_OUT EXPECTED_OUT [COUNT]

CASES_OUT holds `name|secret_hex|digest_hex` lines. EXPECTED_OUT holds the
line the m31 program must print for each: `name public raw_signature der_signature`.

Two independent references:
  * the RFC 6979 A.2.5 published vectors (P-256 / SHA-256), asserted below;
  * a pure-Python RFC 6979 + textbook ECDSA written here from the RFC text, and
    the `cryptography` package (OpenSSL), which must VERIFY every signature and
    derive every public key.
"""
import hashlib
import hmac
import random
import sys

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import Prehashed, encode_dss_signature

P = 0xFFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF
N = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551


def rfc6979_nonce(secret, digest):
    x = secret.to_bytes(32, "big")
    h1 = (int.from_bytes(digest, "big") % N).to_bytes(32, "big")
    v = b"\x01" * 32
    k = b"\x00" * 32
    k = hmac.new(k, v + b"\x00" + x + h1, hashlib.sha256).digest()
    v = hmac.new(k, v, hashlib.sha256).digest()
    k = hmac.new(k, v + b"\x01" + x + h1, hashlib.sha256).digest()
    v = hmac.new(k, v, hashlib.sha256).digest()
    while True:
        v = hmac.new(k, v, hashlib.sha256).digest()
        candidate = int.from_bytes(v, "big")
        if 1 <= candidate < N:
            yield candidate
        k = hmac.new(k, v + b"\x00", hashlib.sha256).digest()
        v = hmac.new(k, v, hashlib.sha256).digest()


def sign(secret, digest):
    e = int.from_bytes(digest, "big") % N
    for k in rfc6979_nonce(secret, digest):
        point = ec.derive_private_key(k, ec.SECP256R1()).public_key().public_numbers()
        r = point.x % N
        if r == 0:
            continue
        s = pow(k, -1, N) * (e + r * secret) % N
        if s == 0:
            continue
        return r, s


def der(r, s):
    return encode_dss_signature(r, s)


def public(secret):
    n = ec.derive_private_key(secret, ec.SECP256R1()).public_key().public_numbers()
    return b"\x04" + n.x.to_bytes(32, "big") + n.y.to_bytes(32, "big")


def check(name, secret, digest):
    r, s = sign(secret, digest)
    pub = public(secret)
    key = ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), pub)
    key.verify(der(r, s), digest, ec.ECDSA(Prehashed(hashes.SHA256())))
    raw = r.to_bytes(32, "big") + s.to_bytes(32, "big")
    return "%s %s %s %s" % (name, pub.hex(), raw.hex(), der(r, s).hex())


def main():
    cases_path, expected_path = sys.argv[1], sys.argv[2]
    count = int(sys.argv[3]) if len(sys.argv) > 3 else 200
    x = 0xC9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721
    vectors = {
        b"sample": (0xEFD48B2AACB6A8FD1140DD9CD45E81D69D2C877B56AAF991C34D0EA84EAF3716,
                    0xF7CB1C942D657C41D436C7A1B6E29F65F3E900DBB9AFF4064DC4AB2F843ACDA8),
        b"test": (0xF1ABB023518351CD71D881567B1EA663ED3EFCF6C5132B354F28D3B0B7D38367,
                  0x019F4113742A2B14BD25926B49C649155F267E60D3814B4C0CC84250E46F0083),
    }
    cases = []
    for message, (r, s) in vectors.items():
        digest = hashlib.sha256(message).digest()
        assert sign(x, digest) == (r, s), "pure-Python RFC 6979 disagrees with the RFC vector"
        cases.append(("rfc6979-" + message.decode(), x, digest))
    cases.append(("min-secret", 1, hashlib.sha256(b"one").digest()))
    cases.append(("max-secret", N - 1, hashlib.sha256(b"max").digest()))
    cases.append(("digest-zero", 0x1234567, b"\x00" * 32))
    cases.append(("digest-ones", 0x7654321, b"\xff" * 32))
    cases.append(("digest-n", 0x7654321, N.to_bytes(32, "big")))
    cases.append(("digest-n-plus-1", 0x7654321, (N + 1).to_bytes(32, "big")))
    rng = random.Random(6979)
    for index in range(count):
        secret = rng.randrange(1, N)
        digest = hashlib.sha256(rng.randbytes(rng.randrange(0, 80))).digest()
        cases.append(("random-%d" % index, secret, digest))
    with open(cases_path, "w") as out, open(expected_path, "w") as expected:
        for name, secret, digest in cases:
            out.write("%s|%064x|%s\n" % (name, secret, digest.hex()))
            expected.write(check(name, secret, digest) + "\n")


main()
