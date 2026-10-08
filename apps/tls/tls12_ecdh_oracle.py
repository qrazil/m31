#!/usr/bin/env python3
"""The other half of `tls12_ecdh.m31`: the same lines from the `cryptography`
package (OpenSSL). Public keys come from `derive_private_key`, shared secrets
from `exchange`, and the refusals from OpenSSL's own point validation. Nothing
here may read the language's answer."""
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives import serialization

ORDER = int("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551", 16)
PRIME = int("ffffffff00000001000000000000000000000000ffffffffffffffffffffffff", 16)
CURVE = ec.SECP256R1()


def pattern(count, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(count))


def private(secret):
    return ec.derive_private_key(int.from_bytes(secret, "big"), CURVE)


def public_bytes(key):
    return key.public_key().public_bytes(
        serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint)


def shared_line(peer_encoded, secret):
    try:
        peer = ec.EllipticCurvePublicKey.from_encoded_point(CURVE, peer_encoded)
    except ValueError:
        return "refused"
    # the language accepts only the uncompressed form; so does this
    if len(peer_encoded) != 65 or peer_encoded[0] != 4:
        return "refused"
    return private(secret).exchange(ec.ECDH(), peer).hex()


def exchange(label, secret_a, secret_b):
    public_a = public_bytes(private(secret_a))
    public_b = public_bytes(private(secret_b))
    print("%s-public-a %s" % (label, public_a.hex()))
    print("%s-public-b %s" % (label, public_b.hex()))
    print("%s-shared-ab %s" % (label, shared_line(public_b, secret_a)))
    print("%s-shared-ba %s" % (label, shared_line(public_a, secret_b)))


def hexb(text):
    return bytes.fromhex(text)


def verdict(flag):
    return "yes" if flag else "no"


def is_scalar(secret):
    return len(secret) == 32 and 1 <= int.from_bytes(secret, "big") <= ORDER - 1


one = hexb("00" * 31 + "01")
two = hexb("00" * 31 + "02")
order_less_one = hexb("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632550")
order_less_two = hexb("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc63254f")
half = hexb("7fffffff800000007fffffffffffffffde737d56d38bcf4279dce5617e3192a8")
exchange("small", one, two)
exchange("top", order_less_one, order_less_two)
exchange("half", half, one)
exchange("wide",
         hexb("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721"),
         hexb("7d7dc5f71eb29ddaf80d6214632eeae03d9058af1fb6d22ed80badb62bc1a534"))
for rnd in range(12):
    first = bytearray(pattern(32, 7 + rnd, 1 + rnd))
    second = bytearray(pattern(32, 11 + 2 * rnd, 3 + rnd))
    first[0] &= 0x7F
    second[0] &= 0x7F
    exchange("pattern%d" % rnd, bytes(first), bytes(second))

zero = bytes(32)
order = hexb("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551")
above = hexb("ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632552")
all_ones = b"\xff" * 32
print("scalar-zero " + verdict(is_scalar(zero)))
print("scalar-one " + verdict(is_scalar(one)))
print("scalar-order-less-one " + verdict(is_scalar(order_less_one)))
print("scalar-order " + verdict(is_scalar(order)))
print("scalar-order-plus-one " + verdict(is_scalar(above)))
print("scalar-all-ones " + verdict(is_scalar(all_ones)))
print("scalar-short " + verdict(is_scalar(pattern(31, 1, 1))))
print("scalar-long " + verdict(is_scalar(pattern(33, 1, 1))))

good = public_bytes(private(half))
bad_y = bytearray(good); bad_y[64] ^= 1
bad_x = bytearray(good); bad_x[1] ^= 1
compressed = bytes([2 + (good[64] & 1)]) + good[1:33]
infinity = b"\x00"
wide_x = b"\x04" + PRIME.to_bytes(32, "big") + good[33:65]
zeros_point = b"\x04" + bytes(64)
short_point = good[:64]
hybrid = bytearray(good); hybrid[0] = 6
print("peer-good " + shared_line(good, one))
print("peer-bad-y " + shared_line(bytes(bad_y), one))
print("peer-bad-x " + shared_line(bytes(bad_x), one))
print("peer-compressed " + shared_line(compressed, one))
print("peer-infinity " + shared_line(infinity, one))
print("peer-x-at-prime " + shared_line(wide_x, one))
print("peer-zeros " + shared_line(zeros_point, one))
print("peer-short " + shared_line(short_point, one))
print("peer-hybrid " + shared_line(bytes(hybrid), one))
