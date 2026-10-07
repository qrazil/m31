#!/usr/bin/env python3
"""The other half of `t_hkdf.m31`: the same lines, from Python's hmac/hashlib.

HKDF here is written straight from RFC 5869 over `hmac`, and the TLS label
structure straight from RFC 8446 §7.1 over `struct` -- neither shares code
with the language's `hkdf.m31`. The published RFC 5869 and RFC 8448 values are
asserted against this implementation first, so the oracle is itself checked
against the standards. The RFC 5869 expand step is additionally cross-checked
against the `cryptography` package (OpenSSL underneath).
"""
import hashlib
import hmac
import struct

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.kdf.hkdf import HKDFExpand


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


def extract(salt, ikm):
    if len(salt) == 0:
        salt = b"\x00" * 32
    return hmac.new(salt, ikm, "sha256").digest()


def expand(prk, info, length):
    out = b""
    previous = b""
    counter = 1
    while len(out) < length:
        previous = hmac.new(prk, previous + info + bytes([counter]), "sha256").digest()
        out += previous
        counter += 1
    return out[:length]


def expand_cross_check(prk, info, length):
    if length == 0:
        return b""
    return HKDFExpand(hashes.SHA256(), length, info).derive(prk)


def expand_label(secret, label, context, length):
    full = b"tls13 " + label.encode()
    info = struct.pack(">H", length) + bytes([len(full)]) + full + bytes([len(context)]) + context
    return expand(secret, info, length)


def derive_secret(secret, label, transcript_hash):
    return expand_label(secret, label, transcript_hash, 32)


out = []

# RFC 5869 Appendix A.1 to A.3, published values.
RFC5869 = {
    1: (
        b"\x0b" * 22, pattern(13, 1, 0), pattern(10, 1, 0xF0), 42,
        "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
    ),
    2: (
        pattern(80, 1, 0), pattern(80, 1, 0x60), pattern(80, 1, 0xB0), 82,
        "06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244",
        "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71cc30c58179ec3e87c14c01d5c1f3434f1d87",
    ),
    3: (
        b"\x0b" * 22, b"", b"", 42,
        "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
        "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8",
    ),
}
for number, (ikm, salt, info, length, want_prk, want_okm) in RFC5869.items():
    prk = extract(salt, ikm)
    assert prk.hex() == want_prk, "RFC 5869 case %d PRK disagrees" % number
    okm = expand(prk, info, length)
    assert okm.hex() == want_okm, "RFC 5869 case %d OKM disagrees" % number
    out.append("rfc5869-%d prk %s" % (number, prk.hex()))
    out.append("rfc5869-%d okm %s" % (number, okm.hex()))

# RFC 8448 §3: published start of the key schedule.
empty_hash = hashlib.sha256(b"").digest()
early = extract(b"", b"\x00" * 32)
assert early.hex() == "33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a"
derived = derive_secret(early, "derived", empty_hash)
assert derived.hex() == "6f2615a108c702c5678f54fc9dbab69716c076189c48250cebeac3576c3611ba"
shared = bytes.fromhex("8bd4054fb55b9d63fdfbacf9f04b9f0d35e6d63f537563efd46272900f89492d")
handshake = extract(derived, shared)
assert handshake.hex() == "1dc826e93606aa6fdc0aadc12f741b01046aa6b99f691ed221a9f0ca043fbeac"
out.append("rfc8448 early " + early.hex())
out.append("rfc8448 derived " + derived.hex())
out.append("rfc8448 handshake " + handshake.hex())

SALT_SIZES = [0, 1, 32, 63, 64, 65]
IKM_SIZES = [0, 1, 22, 32, 64, 100]
for salt_size in SALT_SIZES:
    for ikm_size in IKM_SIZES:
        out.append("extract salt%d ikm%d %s" % (
            salt_size, ikm_size, extract(pattern(salt_size, 5, 3), pattern(ikm_size, 9, 1)).hex()))

OUTPUT_SIZES = [0, 1, 31, 32, 33, 63, 64, 65, 100, 255, 1000, 8160]
INFO_SIZES = [0, 1, 10, 80]
expand_key = pattern(32, 3, 7)
for output_size in OUTPUT_SIZES:
    for info_size in INFO_SIZES:
        info = pattern(info_size, 13, 2)
        got = expand(expand_key, info, output_size)
        assert got == expand_cross_check(expand_key, info, output_size), "cryptography HKDFExpand disagrees"
        out.append("expand len%d info%d %s" % (output_size, info_size, got.hex()))

LABELS = ["derived", "c hs traffic", "s hs traffic", "c ap traffic", "s ap traffic",
          "finished", "key", "iv", "res binder", "exp master"]
LABEL_OUTPUT_SIZES = [12, 16, 32, 100]
CONTEXT_SIZES = [0, 1, 32, 255]
label_secret = pattern(32, 17, 29)
for label in LABELS:
    for output_size in LABEL_OUTPUT_SIZES:
        for context_size in CONTEXT_SIZES:
            context = pattern(context_size, 19, 31)
            out.append("label %s len%d ctx%d %s" % (
                label, output_size, context_size,
                expand_label(label_secret, label, context, output_size).hex()))
    transcript = hashlib.sha256(label.encode()).digest()
    out.append("derive %s %s" % (label, derive_secret(label_secret, label, transcript).hex()))

out.append("longest-label " + expand_label(label_secret, "k" * 249, b"", 16).hex())

print("\n".join(out))
