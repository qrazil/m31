#!/usr/bin/env python3
"""The other half of `gcm_seal.m31`: the same lines, from the `cryptography`
package's AESGCM (OpenSSL). The GCM specification's published test-case tags
are asserted first, so the oracle is checked against the standard and not only
against the harness. Nothing here may read the language's answer."""
from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers.aead import AESGCM


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


def seal(key, nonce, plaintext, aad):
    return AESGCM(key).encrypt(nonce, plaintext, aad)


def open_result(key, nonce, sealed, aad):
    if len(sealed) < 16:
        return "Short"
    try:
        return "ok:" + AESGCM(key).decrypt(nonce, sealed, aad).hex()
    except InvalidTag:
        return "Tag"


def emit(label, key, nonce, plaintext, aad):
    sealed = seal(key, nonce, plaintext, aad)
    print("%s %s" % (label, sealed.hex()))
    print("%s-open %s" % (label, open_result(key, nonce, sealed, aad)))
    return sealed


def flipped(data, index):
    out = bytearray(data)
    out[index] ^= 1
    return bytes(out)


def tamper(label, key, nonce, plaintext, aad):
    sealed = seal(key, nonce, plaintext, aad)
    last = len(sealed) - 1
    tag_start = len(sealed) - 16
    print("%s-tag-first %s" % (label, open_result(key, nonce, flipped(sealed, tag_start), aad)))
    print("%s-tag-last %s" % (label, open_result(key, nonce, flipped(sealed, last), aad)))
    if len(plaintext) > 0:
        print("%s-ct-first %s" % (label, open_result(key, nonce, flipped(sealed, 0), aad)))
        print("%s-ct-last %s" % (label, open_result(key, nonce, flipped(sealed, tag_start - 1), aad)))
    print("%s-aad-extended %s" % (label, open_result(key, nonce, sealed, aad + b"\x00")))
    if len(aad) > 0:
        print("%s-aad-flipped %s" % (label, open_result(key, nonce, sealed, flipped(aad, 0))))
    print("%s-nonce %s" % (label, open_result(key, flipped(nonce, 11), sealed, aad)))
    print("%s-key %s" % (label, open_result(flipped(key, 0), nonce, sealed, aad)))
    print("%s-truncated %s" % (label, open_result(key, nonce, sealed[:last], aad)))
    print("%s-dropped-first %s" % (label, open_result(key, nonce, sealed[1:], aad)))


SPEC_PLAIN = bytes.fromhex(
    "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72"
    "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255")
SPEC_AAD = bytes.fromhex("feedfacedeadbeeffeedfacedeadbeefabaddad2")
SPEC_NONCE = bytes.fromhex("cafebabefacedbaddecaf888")
KEY128 = bytes.fromhex("feffe9928665731c6d6a8f9467308308")
KEY256 = KEY128 + KEY128
ZERO12 = bytes(12)

# McGrew & Viega, "The Galois/Counter Mode of Operation", appendix B: the
# published tags (and, for the short cases, ciphertexts).
SPEC = [
    ("spec-1", bytes(16), ZERO12, b"", b"", "58e2fccefa7e3061367f1d57a4e7455a"),
    ("spec-2", bytes(16), ZERO12, bytes(16), b"", "0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"),
    ("spec-3", KEY128, SPEC_NONCE, SPEC_PLAIN, b"", None),
    ("spec-4", KEY128, SPEC_NONCE, SPEC_PLAIN[:60], SPEC_AAD, None),
    ("spec-13", bytes(32), ZERO12, b"", b"", "530f8afbc74536b9a963b4f1c4cb738b"),
    ("spec-14", bytes(32), ZERO12, bytes(16), b"", "cea7403d4d606b6e074ec5d3baf39d18d0d1c8a799996bf0265b98b5d48ab919"),
    ("spec-15", KEY256, SPEC_NONCE, SPEC_PLAIN, b"", None),
    ("spec-16", KEY256, SPEC_NONCE, SPEC_PLAIN[:60], SPEC_AAD, None),
]
SPEC_TAGS = {
    "spec-3": "4d5c2af327cd64a62cf35abd2ba6fab4",
    "spec-4": "5bc94fbc3221a5db94fae95ae7121a47",
    "spec-15": "b094dac5d93471bdec1a502270e3cc6c",
    "spec-16": "76fc6ece0f4e1768cddf8853bb2d551b",
}
for label, key, nonce, plaintext, aad, want in SPEC:
    sealed = emit(label, key, nonce, plaintext, aad)
    if want is not None:
        assert sealed.hex() == want, (label, sealed.hex(), want)
    else:
        assert sealed[-16:].hex() == SPEC_TAGS[label], (label, sealed[-16:].hex())

for key_size in (16, 32):
    for plain_size in (0, 1, 15, 16, 17, 31, 32, 33, 48, 63, 64, 65, 100, 255):
        for aad_size in (0, 1, 16, 20, 40):
            label = "sweep-%d-%d-%d" % (key_size * 8, plain_size, aad_size)
            emit(label, pattern(key_size, 5, plain_size), pattern(12, 3, aad_size),
                 pattern(plain_size, 7, 1), pattern(aad_size, 11, 2))

for key_size in (16, 32):
    emit("record-%d" % (key_size * 8), pattern(key_size, 13, 4), pattern(12, 17, 6),
         pattern(16385, 31, 9), pattern(5, 1, 0x17))

for key_size in (16, 32):
    prefix = "tamper-%d" % (key_size * 8)
    key, nonce = pattern(key_size, 5, 1), pattern(12, 3, 2)
    tamper(prefix + "-empty", key, nonce, b"", pattern(5, 1, 0x17))
    tamper(prefix + "-short", key, nonce, pattern(1, 1, 1), b"")
    tamper(prefix + "-block", key, nonce, pattern(16, 1, 1), pattern(13, 2, 0))
    tamper(prefix + "-long", key, nonce, pattern(100, 1, 1), pattern(5, 1, 0x17))
    for size in (0, 1, 15):
        print("%s-input-%d %s" % (prefix, size, open_result(key, nonce, pattern(size, 1, 1), b"")))
