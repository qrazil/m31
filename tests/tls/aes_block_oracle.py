#!/usr/bin/env python3
"""The other half of `aes_block.m31`: the same lines, from the `cryptography`
package (OpenSSL's AES). FIPS 197's published ciphertexts are asserted against
it first, so the oracle is checked against the standard and not only against
the harness. Nothing here may read the language's answer."""
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


def encrypt(key, block):
    encryptor = Cipher(algorithms.AES(key), modes.ECB()).encryptor()
    return (encryptor.update(block) + encryptor.finalize()).hex()


PLAIN = pattern(16, 0x11, 0)
FIPS = {
    "b": (bytes.fromhex("2b7e151628aed2a6abf7158809cf4f3c"),
          bytes.fromhex("3243f6a8885a308d313198a2e0370734"),
          "3925841d02dc09fbdc118597196a0b32"),
    "c128": (pattern(16, 1, 0), PLAIN, "69c4e0d86a7b0430d8cdb78070b4c55a"),
    "c256": (pattern(32, 1, 0), PLAIN, "8ea2b7ca516745bfeafc49904b496089"),
}
for name, (key, block, want) in FIPS.items():
    got = encrypt(key, block)
    assert got == want, (name, got, want)
    print("fips197-%s %s" % (name, got))

for key_size in (16, 32):
    for rnd in range(40):
        print("sweep-%d %d %s" % (key_size * 8, rnd,
              encrypt(pattern(key_size, 7 + rnd, rnd), pattern(16, 11 + rnd, 3 * rnd + 1))))
    print("zero-%d %s" % (key_size * 8, encrypt(bytes(key_size), bytes(16))))
    print("ones-%d %s" % (key_size * 8, encrypt(b"\xff" * key_size, b"\xff" * 16)))

key = pattern(16, 3, 5)
for block in range(16):
    print("values %d %s" % (block, encrypt(key, pattern(16, 1, block * 16))))
