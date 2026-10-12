#!/usr/bin/env python3
"""Vectors and expected verdicts for `lib/crypto/ecdsa.m31`'s verify, from the
`cryptography` package (OpenSSL underneath) -- not from this project's code.

    python3 ecdsa_oracle.py VECTORS_OUT EXPECTED_OUT

writes two files. VECTORS_OUT is read by `t_ecdsa_verify.m31`, one case per line:

    key|name|curve|pub_hex                           -> does parse_public_key accept it?
    sig|name|curve|form|pub_hex|digest_hex|sig_hex    -> does verify_der / verify_raw say yes?

(`form` is `der` or `raw`.) EXPECTED_OUT holds the `name verdict` line the m31
program must print for each, in order.

Where a case is a plain mutation of something OpenSSL can judge, the verdict
written here is OpenSSL's own and this script asserts it equals the verdict
the construction implies. Cases OpenSSL is lenient about by design (BER
lengths, padded integers) are hand-built strict-DER rejections: the expected
verdict is the DER rule, not OpenSSL's tolerance.
"""
import hashlib
import random
import sys

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import Prehashed, decode_dss_signature, encode_dss_signature

CURVES = {
    "p256": dict(
        curve=ec.SECP256R1(), size=32, hash=hashlib.sha256, prehash=hashes.SHA256(),
        p=0xFFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF,
        n=0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551,
        b=0x5AC635D8AA3A93E7B3EBBD55769886BC651D06B0CC53B0F63BCE3C3E27D2604B,
        gx=0x6B17D1F2E12C4247F8BCE6E563A440F277037D812DEB33A0F4A13945D898C296,
        gy=0x4FE342E2FE1A7F9B8EE7EB4A7C0F9E162BCE33576B315ECECBB6406837BF51F5),
    "p384": dict(
        curve=ec.SECP384R1(), size=48, hash=hashlib.sha384, prehash=hashes.SHA384(),
        p=0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFFFF0000000000000000FFFFFFFF,
        n=0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFC7634D81F4372DDF581A0DB248B0A77AECEC196ACCC52973,
        b=0xB3312FA7E23EE7E4988E056BE3F82D19181D9C6EFE8141120314088F5013875AC656398D8A2ED19D2A85C8EDD3EC2AEF,
        gx=0xAA87CA22BE8B05378EB1C71EF320AD746E1D3B628BA79B9859F741E082542A385502F25DBF55296C3A545E3872760AB7,
        gy=0x3617DE4A96262C6F5D9E98BF9292DC29F8F41DBD289A147CE9DA3113B5F0B8C00A60B1CE1D7E819D7A431D7C90EA0E5F),
}

rng = random.Random(0x7E57)
cases = []   # (line, expected_verdict_int)


# --- helpers ---------------------------------------------------------------

def be(value, size):
    return value.to_bytes(size, "big")


def pub_bytes(c, x, y):
    return b"\x04" + be(x, c["size"]) + be(y, c["size"])


def der_int(value):
    raw = value.to_bytes(max(1, (value.bit_length() + 7) // 8), "big")
    if raw[0] & 0x80:
        raw = b"\x00" + raw
    return b"\x02" + bytes([len(raw)]) + raw


def der_sig(r, s):
    body = der_int(r) + der_int(s)
    return b"\x30" + bytes([len(body)]) + body


def openssl_verifies(c, pub, digest, r, s):
    try:
        key = ec.EllipticCurvePublicKey.from_encoded_point(c["curve"], pub)
    except ValueError:
        return False
    try:
        key.verify(encode_dss_signature(r, s), digest, ec.ECDSA(Prehashed(c["prehash"])))
        return True
    except (InvalidSignature, ValueError):
        return False


def add_sig(name, cname, form, pub, digest, sig, expected, openssl=None):
    """Record a case. `openssl` is (r, s) when OpenSSL can judge it."""
    if openssl is not None:
        got = openssl_verifies(CURVES[cname], pub, digest, *openssl)
        assert got == bool(expected), f"{name}: oracle disagrees with construction ({got} vs {expected})"
    cases.append((f"sig|{name}|{cname}|{form}|{pub.hex()}|{digest.hex()}|{sig.hex()}", int(expected)))


def flip(data, bit):
    out = bytearray(data)
    out[bit // 8] ^= 1 << (bit % 8)
    return bytes(out)


# --- minimal affine EC arithmetic (only to craft keys for special signatures) ----

def ec_add(c, a, b):
    p = c["p"]
    if a is None:
        return b
    if b is None:
        return a
    if a[0] == b[0] and (a[1] + b[1]) % p == 0:
        return None
    if a == b:
        slope = (3 * a[0] * a[0] - 3) * pow(2 * a[1], -1, p) % p
    else:
        slope = (b[1] - a[1]) * pow(b[0] - a[0], -1, p) % p
    x = (slope * slope - a[0] - b[0]) % p
    return (x, (slope * (a[0] - x) - a[1]) % p)


def ec_mul(c, k, point):
    out = None
    while k:
        if k & 1:
            out = ec_add(c, out, point)
        point = ec_add(c, point, point)
        k >>= 1
    return out


def ec_neg(c, point):
    return (point[0], (-point[1]) % c["p"])


# --- per-curve cases -----------------------------------------------------------

def sign(c, key, digest):
    r, s = decode_dss_signature(key.sign(digest, ec.ECDSA(Prehashed(c["prehash"]))))
    return r, s


def new_key(c, d=None):
    key = ec.derive_private_key(d, c["curve"]) if d else ec.generate_private_key(c["curve"])
    nums = key.public_key().public_numbers()
    return key, pub_bytes(c, nums.x, nums.y)


def public_key_cases(cname):
    c = CURVES[cname]
    p, n, size = c["p"], c["n"], c["size"]
    _, good = new_key(c)
    x, y = int.from_bytes(good[1:1 + size], "big"), int.from_bytes(good[1 + size:], "big")

    def key_case(name, pub, expected):
        cases.append((f"key|{cname}_{name}|{cname}|{pub.hex()}", int(expected)))

    key_case("valid", good, True)
    key_case("valid_negated", pub_bytes(c, x, p - y), True)
    key_case("generator", pub_bytes(c, c["gx"], c["gy"]), True)
    key_case("infinity_byte", b"\x00", False)
    key_case("infinity_padded", b"\x00" * (1 + 2 * size), False)
    key_case("empty", b"", False)
    key_case("compressed_even", b"\x02" + good[1:1 + size], False)
    key_case("compressed_odd", b"\x03" + good[1:1 + size], False)
    key_case("hybrid", b"\x06" + good[1:], False)
    key_case("tag_zero", b"\x00" + good[1:], False)
    key_case("tag_five", b"\x05" + good[1:], False)
    key_case("truncated", good[:-1], False)
    key_case("extended", good + b"\x00", False)
    key_case("off_curve_y_plus_one", pub_bytes(c, x, (y + 1) % p), False)
    key_case("off_curve_x_plus_one", pub_bytes(c, (x + 1) % p, y), False)
    key_case("origin", pub_bytes(c, 0, 0), False)
    key_case("swapped", pub_bytes(c, y, x), False)
    key_case("x_equals_p", pub_bytes(c, p, y), False)
    key_case("y_equals_p", pub_bytes(c, x, p), False)
    key_case("x_all_ones", pub_bytes(c, (1 << (8 * size)) - 1, y), False)
    key_case("y_all_ones", pub_bytes(c, x, (1 << (8 * size)) - 1), False)
    # x + p still fits in the field width only for tiny x; generator's x does not,
    # but y + p for the origin-adjacent y=0 case would, so use p + 1 explicitly.
    key_case("x_p_plus_one", pub_bytes(c, p + 1, y) if p + 1 < (1 << (8 * size)) else pub_bytes(c, (1 << (8 * size)) - 2, y), False)
    for bit in rng.sample(range(8 * 2 * size), 12):
        bad = flip(good[1:], bit)
        key_case(f"bitflip_{bit}", b"\x04" + bad, False)
    for k in range(8):
        key_case(f"random_xy_{k}", b"\x04" + rng.randbytes(2 * size), False)


def valid_and_mutated_cases(cname, count):
    c = CURVES[cname]
    n, size = c["n"], c["size"]
    for index in range(count):
        key, pub = new_key(c)
        message = rng.randbytes(rng.randrange(0, 200))
        digest = c["hash"](message).digest()
        r, s = sign(c, key, digest)
        tag = f"{cname}_k{index}"
        add_sig(f"{tag}_der", cname, "der", pub, digest, der_sig(r, s), True, (r, s))
        add_sig(f"{tag}_raw", cname, "raw", pub, digest, be(r, size) + be(s, size), True, (r, s))
        add_sig(f"{tag}_highs", cname, "raw", pub, digest, be(r, size) + be(n - s, size), True, (r, n - s))
        if index >= 2:
            continue
        # single-bit flips: digest, r, s, key (always rejected)
        for bit in rng.sample(range(8 * size), 5):
            add_sig(f"{tag}_digest_flip{bit}", cname, "raw", pub, flip(digest, bit), be(r, size) + be(s, size), False, (r, s))
        for bit in rng.sample(range(8 * size), 5):
            r2 = int.from_bytes(flip(be(r, size), bit), "big")
            add_sig(f"{tag}_r_flip{bit}", cname, "raw", pub, digest, be(r2, size) + be(s, size), False, (r2, s) if 0 < r2 < n else None)
        der = der_sig(r, s)
        for bit in rng.sample(range(8 * len(der)), 8):
            add_sig(f"{tag}_der_flip{bit}", cname, "der", pub, digest, flip(der, bit), False)
        for bit in rng.sample(range(8 * size), 5):
            s2 = int.from_bytes(flip(be(s, size), bit), "big")
            add_sig(f"{tag}_s_flip{bit}", cname, "raw", pub, digest, be(r, size) + be(s2, size), False, (r, s2) if 0 < s2 < n else None)
        for bit in rng.sample(range(8 * 2 * size), 6):
            add_sig(f"{tag}_key_flip{bit}", cname, "raw", b"\x04" + flip(pub[1:], bit), digest, be(r, size) + be(s, size), False)
        # signature of another message, and under another key
        other = c["hash"](message + b"x").digest()
        add_sig(f"{tag}_other_digest", cname, "raw", pub, other, be(r, size) + be(s, size), False, (r, s))
        _, other_pub = new_key(c)
        add_sig(f"{tag}_other_key", cname, "raw", other_pub, digest, be(r, size) + be(s, size), False, (r, s))
        # edge scalars
        raw_r, raw_s = be(r, size), be(s, size)
        for label, er, es in [("r0", 0, s), ("s0", r, 0), ("rn", n, s), ("sn", r, n), ("rn1", n - 1, s),
                              ("sn1", r, n - 1), ("rn_plus1", n + 1, s), ("sn_plus1", r, n + 1),
                              ("r1", 1, s), ("s1", r, 1), ("both0", 0, 0), ("bothn", n, n),
                              ("rmax", (1 << 8 * size) - 1, s), ("smax", r, (1 << 8 * size) - 1)]:
            add_sig(f"{tag}_edge_{label}", cname, "raw", pub, digest, be(er, size) + be(es, size), False,
                    (er, es) if 0 < er < n and 0 < es < n else None)
        # DER edge scalars (r=0, s=0, r=n, s=n are well-formed DER of out-of-range values)
        for label, er, es in [("r0", 0, s), ("s0", r, 0), ("rn", n, s), ("sn", r, n)]:
            add_sig(f"{tag}_der_edge_{label}", cname, "der", pub, digest, der_sig(er, es), False)
        # length handling: wrong total size for raw
        add_sig(f"{tag}_raw_short", cname, "raw", pub, digest, (raw_r + raw_s)[:-1], False)
        add_sig(f"{tag}_raw_long", cname, "raw", pub, digest, raw_r + raw_s + b"\x00", False)
        add_sig(f"{tag}_raw_empty", cname, "raw", pub, digest, b"", False)
        add_sig(f"{tag}_der_as_raw", cname, "raw", pub, digest, der_sig(r, s), False)
        add_sig(f"{tag}_raw_as_der", cname, "der", pub, digest, raw_r + raw_s, False)
        malformed_der_cases(cname, tag, pub, digest, r, s)


def malformed_der_cases(cname, tag, pub, digest, r, s):
    c = CURVES[cname]
    n, size = c["n"], c["size"]
    good = der_sig(r, s)
    ri, si = der_int(r), der_int(s)

    def bad(label, sig):
        add_sig(f"{tag}_der_{label}", cname, "der", pub, digest, sig, False)

    def seq(body, tag_byte=0x30):
        return bytes([tag_byte, len(body)]) + body

    bad("trailing_byte", good + b"\x00")
    bad("trailing_garbage_in_seq", seq(ri + si + b"\x00"))
    bad("truncated", good[:-1])
    bad("empty", b"")
    bad("only_tag", b"\x30")
    bad("seq_len_plus1", bytes([good[0], good[1] + 1]) + good[2:])
    bad("seq_len_minus1", bytes([good[0], good[1] - 1]) + good[2:])
    bad("seq_tag_31", seq(ri + si, 0x31))
    bad("seq_tag_10", seq(ri + si, 0x10))
    bad("long_form_len", b"\x30\x81" + bytes([len(ri + si)]) + ri + si)
    bad("long_form_len2", b"\x30\x82\x00" + bytes([len(ri + si)]) + ri + si)
    bad("indefinite_len", b"\x30\x80" + ri + si + b"\x00\x00")
    bad("int_tag_03", seq(b"\x03" + ri[1:] + si))
    bad("second_int_tag_03", seq(ri + b"\x03" + si[1:]))
    bad("r_len_plus1", seq(bytes([ri[0], ri[1] + 1]) + ri[2:] + si))
    bad("r_len_minus1", seq(bytes([ri[0], ri[1] - 1]) + ri[2:] + si))
    bad("s_len_plus1", seq(ri + bytes([si[0], si[1] + 1]) + si[2:]))
    bad("s_len_minus1", seq(ri + bytes([si[0], si[1] - 1]) + si[2:]))
    bad("r_empty", seq(b"\x02\x00" + si))
    bad("s_empty", seq(ri + b"\x02\x00"))
    bad("r_missing", seq(si))
    bad("s_missing", seq(ri))
    # non-minimal integers: a superfluous leading 0x00 (value unchanged)
    raw_r = r.to_bytes((r.bit_length() + 7) // 8, "big")
    raw_s = s.to_bytes((s.bit_length() + 7) // 8, "big")
    bad("r_padded_zero", seq(b"\x02" + bytes([len(raw_r) + 1]) + b"\x00" + raw_r + si) if not raw_r[0] & 0x80 else seq(b"\x02" + bytes([len(raw_r) + 2]) + b"\x00\x00" + raw_r + si))
    bad("s_padded_zero", seq(ri + (b"\x02" + bytes([len(raw_s) + 1]) + b"\x00" + raw_s if not raw_s[0] & 0x80 else b"\x02" + bytes([len(raw_s) + 2]) + b"\x00\x00" + raw_s)))
    # negative integers: high bit set with no 0x00 pad (the same bytes as r / s)
    if raw_r[0] & 0x80:
        bad("r_negative", seq(b"\x02" + bytes([len(raw_r)]) + raw_r + si))
    if raw_s[0] & 0x80:
        bad("s_negative", seq(ri + b"\x02" + bytes([len(raw_s)]) + raw_s))
    bad("r_negative_forced", seq(b"\x02" + bytes([size]) + bytes([0x80 | (r >> (8 * size - 8))]) + be(r, size)[1:] + si))
    # too wide for the curve
    bad("r_too_wide", seq(b"\x02" + bytes([size + 2]) + b"\x00\x01" + be(r, size) + si))
    bad("s_too_wide", seq(ri + b"\x02" + bytes([size + 2]) + b"\x00\x01" + be(s, size)))
    bad("r_all_ff_pad", seq(b"\x02" + bytes([size + 1]) + b"\x00" + b"\xff" * size + si))   # >= n
    # a one-byte-per-integer degenerate signature
    bad("tiny", b"\x30\x06\x02\x01\x01\x02\x01\x01")
    bad("zero_zero", b"\x30\x06\x02\x01\x00\x02\x01\x00")
    bad("seq_of_one_int", seq(ri))
    bad("three_ints", seq(ri + si + der_int(1)))
    bad("nested_seq", seq(seq(ri + si)))
    bad("null_instead_of_s", seq(ri + b"\x05\x00"))


def special_signatures(cname):
    c = CURVES[cname]
    n, p, size = c["n"], c["p"], c["size"]
    g = (c["gx"], c["gy"])
    # keys whose table entry G+Q is a doubling (Q = G), an infinity (Q = -G), or small multiples
    for d in (1, 2, 3, n - 1, n - 2):
        key, pub = new_key(c, d)
        digest = c["hash"](b"special %d" % (d % 1000)).digest()
        r, s = sign(c, key, digest)
        add_sig(f"{cname}_d{'_neg' if d > 3 else ''}{d if d <= 3 else n - d}_raw", cname, "raw", pub, digest, be(r, size) + be(s, size), True, (r, s))
        add_sig(f"{cname}_d{'_neg' if d > 3 else ''}{d if d <= 3 else n - d}_der", cname, "der", pub, digest, der_sig(r, s), True, (r, s))
    # digests: all-zero, all-ones (>= n), a value just under n, n itself, n + small
    key, pub = new_key(c)
    for label, value in [("zero", 0), ("ones", (1 << 8 * size) - 1), ("n_minus_1", n - 1), ("n", n),
                         ("n_plus_1", n + 1), ("one", 1)]:
        digest = be(value, size)
        r, s = sign(c, key, digest)
        add_sig(f"{cname}_digest_{label}", cname, "raw", pub, digest, be(r, size) + be(s, size), True, (r, s))
    # other digest lengths: longer is truncated, shorter is zero-extended
    for label, algo, hasher in [("sha224", hashes.SHA224(), hashlib.sha224), ("sha384", hashes.SHA384(), hashlib.sha384),
                                ("sha512", hashes.SHA512(), hashlib.sha512), ("sha256", hashes.SHA256(), hashlib.sha256)]:
        digest = hasher(b"digest length").digest()
        if label == "sha256" and cname == "p256":
            continue
        sig = key.sign(digest, ec.ECDSA(Prehashed(algo)))
        r, s = decode_dss_signature(sig)
        pk = key.public_key()
        pk.verify(sig, digest, ec.ECDSA(Prehashed(algo)))
        add_sig(f"{cname}_digestlen_{label}", cname, "raw", pub, digest, be(r, size) + be(s, size), True)
    # r + n wrap: x(R) = n + t < p, so r = t. Craft Q so that u1*G + u2*Q = R.
    for t in range(1, 50):
        x = n + t
        rhs = (x ** 3 - 3 * x + c["b"]) % p
        y = pow(rhs, (p + 1) // 4, p)
        if y * y % p != rhs:
            continue
        point = (x, y)
        digest = c["hash"](b"wrap").digest()
        e = int.from_bytes(digest[:size], "big") % n
        s = rng.randrange(1, n)
        w = pow(s, -1, n)
        u1, u2 = e * w % n, t * w % n
        diff = ec_add(c, point, ec_neg(c, ec_mul(c, u1, g)))
        q = ec_mul(c, pow(u2, -1, n), diff)
        pub = pub_bytes(c, *q)
        add_sig(f"{cname}_wrap_r_plus_n", cname, "raw", pub, digest, be(t, size) + be(s, size), True, (t, s))
        # and the same R with r = x mod n is not what a wrong r accepts
        add_sig(f"{cname}_wrap_wrong_r", cname, "raw", pub, digest, be(t + 1, size) + be(s, size), False, (t + 1, s))
        break


def short_integer_cases(cname):
    c = CURVES[cname]
    size = c["size"]
    key, pub = new_key(c)
    for which in ("r", "s"):
        for attempt in range(60000):
            digest = c["hash"](b"short %d" % attempt).digest()
            r, s = sign(c, key, digest)
            if (r if which == "r" else s) < (1 << (8 * size - 8)):
                add_sig(f"{cname}_short_{which}_der", cname, "der", pub, digest, der_sig(r, s), True, (r, s))
                add_sig(f"{cname}_short_{which}_raw", cname, "raw", pub, digest, be(r, size) + be(s, size), True, (r, s))
                break


def known_vectors():
    # RFC 6979 appendix A.2.5 / A.2.6, message "sample".
    rfc = {
        "p256": dict(
            x=0xC9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721,
            ux=0x60FED4BA255A9D31C961EB74C6356D68C049B8923B61FA6CE669622E60F29FB6,
            uy=0x7903FE1008B8BC99A41AE9E95628BC64F2F1B20C2D7E9F5177A3C294D4462299,
            r=0xEFD48B2AACB6A8FD1140DD9CD45E81D69D2C877B56AAF991C34D0EA84EAF3716,
            s=0xF7CB1C942D657C41D436C7A1B6E29F65F3E900DBB9AFF4064DC4AB2F843ACDA8),
        "p384": dict(
            x=0x6B9D3DAD2E1B8C1C05B19875B6659F4DE23C3B667BF297BA9AA47740787137D896D5724E4C70A825F872C9EA60D2EDF5,
            ux=0xEC3A4E415B4E19A4568618029F427FA5DA9A8BC4AE92E02E06AAE5286B300C64DEF8F0EA9055866064A254515480BC13,
            uy=0x8015D9B72D7D57244EA8EF9AC0C621896708A59367F9DFB9F54CA84B3F1C9DB1288B231C3AE0D4FE7344FD2533264720,
            r=0x94EDBB92A5ECB8AAD4736E56C691916B3F88140666CE9FA73D64C4EA95AD133C81A648152E44ACF96E36DD1E80FABE46,
            s=0x99EF4AEB15F178CEA1FE40DB2603138F130E740A19624526203B6351D0A3A94FA329C145786E679E7B82C71A38628AC8),
    }
    for cname, v in rfc.items():
        c = CURVES[cname]
        digest = c["hash"](b"sample").digest()
        pub = pub_bytes(c, v["ux"], v["uy"])
        derived = ec.derive_private_key(v["x"], c["curve"]).public_key().public_numbers()
        assert (derived.x, derived.y) == (v["ux"], v["uy"]), "RFC 6979 public key typo"
        assert openssl_verifies(c, pub, digest, v["r"], v["s"]), "RFC 6979 signature typo"
        size = c["size"]
        add_sig(f"rfc6979_{cname}_der", cname, "der", pub, digest, der_sig(v["r"], v["s"]), True)
        add_sig(f"rfc6979_{cname}_raw", cname, "raw", pub, digest, be(v["r"], size) + be(v["s"], size), True)
        wrong = c["hash"](b"test").digest()
        add_sig(f"rfc6979_{cname}_wrong_msg", cname, "raw", pub, wrong, be(v["r"], size) + be(v["s"], size), False)


def main():
    vectors_path, expected_path = sys.argv[1], sys.argv[2]
    known_vectors()
    for cname in CURVES:
        public_key_cases(cname)
        valid_and_mutated_cases(cname, 6)
        special_signatures(cname)
        short_integer_cases(cname)
    with open(vectors_path, "w") as vectors, open(expected_path, "w") as expected:
        for line, verdict in cases:
            name = line.split("|")[1]
            vectors.write(line + "\n")
            expected.write(f"{name} {verdict}\n")
    print(f"{len(cases)} cases", file=sys.stderr)


main()
