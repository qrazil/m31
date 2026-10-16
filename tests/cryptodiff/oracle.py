#!/usr/bin/env python3
"""Cases for t_diff.m31 and the answers hashlib / cryptography compute for them.

    oracle.py cases   > cases.txt      # the stdin for t_diff
    oracle.py want    < cases.txt      # expected stdout
"""
import hashlib, hmac, sys

M = (1 << 64) - 1


def gen(count, seed):
    out, state = bytearray(), seed & M
    for _ in range(count):
        state = (state * 6364136223846793005 + 1442695040888963407) & M
        out.append((state >> 33) & 0xFF)
    return bytes(out)


EDGE = [0, 1, 2, 15, 16, 17, 31, 32, 33, 55, 56, 57, 63, 64, 65, 111, 112, 113, 119, 120, 121,
        127, 128, 129, 191, 192, 193, 255, 256, 257, 1000, 1023, 1024, 1025, 4095, 4096, 4097]


def cases():
    out, seed = [], 100
    for n in EDGE:
        for op in ("sha1", "sha256", "sha384", "sha512", "sha1x", "sha256x", "sha512x", "poly", "chacha", "hkdf256_"):
            seed += 1
            if op == "hkdf256_":
                if n <= 255 * 32:
                    out.append("hkdf256 %d %d %d" % (n, 1 + n % 97, seed))
            elif op == "chacha":
                out.append("chacha %d %d %d" % (n, n % 5 + (0xFFFFFFFE if n == 65 else 0), seed))
            else:
                out.append("%s %d 0 %d" % (op, n, seed))
        seed += 1
        out.append("hmac256 %d %d %d" % (n if n < 140 else 33, n, seed))
        seed += 1
        out.append("hmac256 %d %d %d" % (64, n, seed))
    for n in EDGE:
        for a in (0, 1, 13, 16, 20, 64, 65):
            seed += 1
            out.append("ccp %d %d %d" % (n, a, seed))
            if n <= 1100 and a in (0, 13, 20, 65):
                out.append("gcm %d %d %d" % (n, a, seed))
                out.append("gcm128 %d %d %d" % (n, a, seed))
    for _ in range(40):
        seed += 1
        out.append("aesblock 0 0 %d" % seed)
        out.append("aesblock128 0 0 %d" % seed)
    return out


def want(lines):
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM, ChaCha20Poly1305
    from cryptography.hazmat.primitives import poly1305
    for line in lines:
        f = line.split()
        if not f:
            continue
        op, a, b, seed = f[0], int(f[1]), int(f[2]), int(f[3])
        key, nonce = gen(32, seed + 1), gen(12, seed + 2)
        if op in ("sha1", "sha256", "sha384", "sha512", "sha1x", "sha256x", "sha512x"):
            r = hashlib.new(op.rstrip("x"), gen(a, seed)).hexdigest()
        elif op == "hmac256":
            r = hmac.new(gen(a, seed + 3), gen(b, seed), "sha256").hexdigest()
        elif op == "hkdf256":
            prk = hmac.new(gen(16, seed + 4), gen(a, seed), "sha256").digest()
            # RFC 5869 expand, spelled out (extract is the HMAC above)
            okm, t, i = b"", b"", 1
            info = gen(7, seed + 5)
            while len(okm) < b:
                t = hmac.new(prk, t + info + bytes([i]), "sha256").digest()
                okm += t
                i += 1
            r = okm[:b].hex()
        elif op == "chacha":
            c = Cipher(algorithms.ChaCha20(key, b.to_bytes(4, "little") + nonce), mode=None).encryptor()
            r = c.update(gen(a, seed)).hex()
        elif op == "poly":
            r = poly1305.Poly1305.generate_tag(key, gen(a, seed)).hex()
        elif op == "ccp":
            r = ChaCha20Poly1305(key).encrypt(nonce, gen(a, seed), gen(b, seed + 6)).hex()
        elif op in ("gcm", "gcm128"):
            k = key if op == "gcm" else key[:16]
            r = AESGCM(k).encrypt(nonce, gen(a, seed), gen(b, seed + 6)).hex()
        elif op in ("aesblock", "aesblock128"):
            k = key if op == "aesblock" else key[:16]
            r = Cipher(algorithms.AES(k), modes.ECB()).encryptor().update(gen(16, seed)).hex()
        else:
            raise SystemExit("unknown op " + op)
        print(r)


if __name__ == "__main__":
    if sys.argv[1] == "cases":
        print("\n".join(cases()))
    else:
        want(sys.stdin.read().splitlines())
