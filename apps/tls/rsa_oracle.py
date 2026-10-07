#!/usr/bin/env python3
"""Vectors and expected verdicts for `lib/rsa.m31`'s verify, from the
`cryptography` package (OpenSSL underneath), Python integers and the system CA
bundle -- not from this project's code.

    python3 rsa_oracle.py VECTORS_OUT EXPECTED_OUT

writes two files. VECTORS_OUT is read by `t_rsa_verify.m31`, one case per line:

    key|name|modulus_hex|exponent_hex                                  -> does parse_public_key accept it?
    sig|name|scheme|hash|modulus_hex|exponent_hex|digest_hex|sig_hex     -> does verify_pkcs1_v15 / verify_pss say yes?

(`scheme` is `pkcs1` or `pss`; `hash` is `sha256`, `sha384` or `sha512`.)
EXPECTED_OUT holds the `name verdict` line the m31 program must print for each,
in order.

How the expectations are made, strongest first:

  * Positive controls. Every signature the oracle expects to be GOOD is either
    made by OpenSSL (`cryptography`) or built by hand here and then checked
    against OpenSSL's own verifier, so a mistake in this script's PKCS#1 /
    PSS encoder cannot quietly define "correct". The hand-built PKCS#1
    encoding must also reproduce OpenSSL's deterministic signature byte for
    byte.
  * Plain mutations (a bit flipped in the digest, signature or modulus; the
    wrong hash; the wrong scheme) are expected to be rejected, and the script
    asserts OpenSSL rejects each too.
  * Hand-built malformed encodings (the Bleichenbacher e=3 forgery, a wrong
    DigestInfo, a missing NULL, short padding, garbage after the digest, a
    wrong PSS trailer or salt length, ...) are expected to be rejected because
    the RFC says so. For those, OpenSSL's verdict is printed to stderr when it
    differs (it is lenient in places by design) but does not define the answer.
  * Real certificates: every self-signed RSA root in the system CA bundle,
    verified over its own TBSCertificate.
"""
import base64
import hashlib
import random
import sys

from cryptography import x509
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from cryptography.hazmat.primitives.asymmetric.utils import Prehashed

rng = random.Random(0x25A)

HASHES = {
    "sha256": dict(py=hashlib.sha256, crypto=hashes.SHA256(), size=32,
                   prefix=bytes.fromhex("3031300d060960864801650304020105000420"),
                   no_null=bytes.fromhex("302f300b060960864801650304020104" + "20")),
    "sha384": dict(py=hashlib.sha384, crypto=hashes.SHA384(), size=48,
                   prefix=bytes.fromhex("3041300d060960864801650304020205000430"),
                   no_null=bytes.fromhex("303f300b060960864801650304020204" + "30")),
    "sha512": dict(py=hashlib.sha512, crypto=hashes.SHA512(), size=64,
                   prefix=bytes.fromhex("3051300d060960864801650304020305000440"),
                   no_null=bytes.fromhex("304f300b060960864801650304020304" + "40")),
}

cases = []   # (line, expected_verdict_int)
counter = {"n": 0}
notes = []


def add(kind_prefix, fields, expected):
    counter["n"] += 1
    name = "%s%04d" % (kind_prefix, counter["n"])
    cases.append(("|".join([fields[0], name] + fields[1:]), expected))
    return name


# --- tiny RSA key generator (public data only; a test fixture, not a library) --

SMALL_PRIMES = [p for p in range(3, 2000) if all(p % q for q in range(2, int(p ** 0.5) + 1))]


def probably_prime(candidate):
    if candidate < 2000:
        return candidate in SMALL_PRIMES or candidate == 2
    for p in SMALL_PRIMES:
        if candidate % p == 0:
            return False
    odd, twos = candidate - 1, 0
    while odd % 2 == 0:
        odd //= 2
        twos += 1
    for _ in range(24):
        base = rng.randrange(2, candidate - 1)
        x = pow(base, odd, candidate)
        if x in (1, candidate - 1):
            continue
        for _ in range(twos - 1):
            x = x * x % candidate
            if x == candidate - 1:
                break
        else:
            return False
    return True


def random_prime(bits, public_exponent):
    while True:
        candidate = rng.getrandbits(bits) | (1 << (bits - 1)) | (1 << (bits - 2)) | 1
        if (candidate - 1) % public_exponent != 0 and probably_prime(candidate):
            return candidate


class Key:
    def __init__(self, bits, public_exponent):
        while True:
            p = random_prime(bits // 2, public_exponent)
            q = random_prime(bits - bits // 2, public_exponent)
            if p != q and (p * q).bit_length() == bits:
                break
        self.p, self.q, self.e = p, q, public_exponent
        self.n = p * q
        self.bits = bits
        self.size = (bits + 7) // 8
        phi = (p - 1) * (q - 1)
        self.d = pow(public_exponent, -1, phi)
        numbers = rsa.RSAPrivateNumbers(
            p, q, self.d, self.d % (p - 1), self.d % (q - 1), pow(q, -1, p),
            rsa.RSAPublicNumbers(public_exponent, self.n))
        self.private = numbers.private_key()
        self.public = self.private.public_key()

    def modulus_hex(self):
        return self.n.to_bytes(self.size, "big").hex()

    def exponent_hex(self):
        return self.e.to_bytes((self.e.bit_length() + 7) // 8, "big").hex()

    def raw_sign(self, em):
        value = int.from_bytes(em, "big")
        assert value < self.n
        return pow(value, self.d, self.n).to_bytes(self.size, "big")


# --- encoders, written from RFC 8017 and checked against OpenSSL --------------

def pkcs1_em(key, hash_name, digest, padding_length=None, prefix=None, tail=b"", lead=b"\x00\x01", sep=b"\x00"):
    info = HASHES[hash_name]["prefix"] if prefix is None else prefix
    body = sep + info + digest + tail
    if padding_length is None:
        padding_length = key.size - len(lead) - len(body)
    return lead + b"\xff" * padding_length + body


def mgf1(hash_name, seed, length):
    out = b""
    counter_value = 0
    while len(out) < length:
        out += HASHES[hash_name]["py"](seed + counter_value.to_bytes(4, "big")).digest()
        counter_value += 1
    return out[:length]


def pss_em(key, hash_name, digest, salt, trailer=0xBC, flip_hash=False, flip_masked=None,
           padding_noise=None, drop_one=False, keep_top_bits=False):
    info = HASHES[hash_name]
    h_len = info["size"]
    em_bits = key.bits - 1
    em_len = (em_bits + 7) // 8
    h = info["py"](b"\x00" * 8 + digest + salt).digest()
    padding_zeros = bytearray(em_len - len(salt) - h_len - 2)
    if padding_noise is not None:
        padding_zeros[padding_noise] = 1
    one = b"" if drop_one else b"\x01"
    db = bytes(padding_zeros) + one + salt
    if drop_one:
        db = b"\x00" + db
    mask = mgf1(hash_name, h, em_len - h_len - 1)
    masked = bytearray(a ^ b for a, b in zip(db, mask))
    if not keep_top_bits:
        masked[0] &= 0xFF >> (8 * em_len - em_bits)
    elif 8 * em_len - em_bits:
        masked[0] |= 0x80
    if flip_masked is not None:
        masked[flip_masked] ^= 0x01
    if flip_hash:
        h = bytes([h[0] ^ 0x01]) + h[1:]
    return bytes(masked) + h + bytes([trailer])


def flip(data, bit):
    out = bytearray(data)
    out[bit // 8] ^= 1 << (bit % 8)
    return bytes(out)


def openssl_accepts(key_public, scheme, hash_name, digest, signature):
    info = HASHES[hash_name]
    try:
        if scheme == "pkcs1":
            key_public.verify(signature, digest, padding.PKCS1v15(), Prehashed(info["crypto"]))
        else:
            key_public.verify(signature, digest,
                              padding.PSS(padding.MGF1(info["crypto"]), info["size"]), Prehashed(info["crypto"]))
        return True
    except (InvalidSignature, ValueError):
        return False


def sig_case(prefix, key, scheme, hash_name, digest, signature, expected, modulus_hex=None, exponent_hex=None):
    return add(prefix, ["sig", scheme, hash_name,
                        key.modulus_hex() if modulus_hex is None else modulus_hex,
                        key.exponent_hex() if exponent_hex is None else exponent_hex,
                        digest.hex(), signature.hex()], expected)


# --- keys -----------------------------------------------------------------------

sys.stderr.write("generating keys...\n")
standard = {}
for bits in (2048, 3072, 4096):
    private = rsa.generate_private_key(public_exponent=65537, key_size=bits)
    numbers = private.private_numbers()
    key = Key.__new__(Key)
    key.p, key.q, key.e, key.n, key.d = numbers.p, numbers.q, 65537, numbers.public_numbers.n, numbers.d
    key.bits, key.size = bits, bits // 8
    key.private, key.public = private, private.public_key()
    standard[bits] = key
e3_key = Key(2048, 3)
odd_keys = {bits: Key(bits, 65537) for bits in (2049, 2050, 2056)}
all_keys = list(standard.values()) + [e3_key] + list(odd_keys.values())

# --- key parsing ------------------------------------------------------------------

e65537 = "010001"
for key in all_keys:
    add("key", ["key", key.modulus_hex(), key.exponent_hex()], 1)
big = standard[2048]
add("key", ["key", "00" + big.modulus_hex(), e65537], 1)            # DER sign byte
add("key", ["key", big.modulus_hex(), "00010001"], 1)                # padded exponent
add("key", ["key", big.modulus_hex(), "03"], 1)
add("key", ["key", big.modulus_hex(), "11"], 1)
add("key", ["key", big.modulus_hex(), "ffffffff"], 1)                 # 2^32 - 1, odd
add("key", ["key", big.modulus_hex(), "0100000001"], 0)               # 2^32 + 1
add("key", ["key", big.modulus_hex(), "01"], 0)
add("key", ["key", big.modulus_hex(), "00"], 0)
add("key", ["key", big.modulus_hex(), ""], 0)
add("key", ["key", big.modulus_hex(), "02"], 0)
add("key", ["key", big.modulus_hex(), "010000"], 0)                   # 65536, even
add("key", ["key", "", e65537], 0)
add("key", ["key", "00", e65537], 0)
add("key", ["key", format(big.n ^ 1, "0%dx" % (big.size * 2)), e65537], 0)   # even modulus
add("key", ["key", format((1 << 2047) | 1, "0512x"), e65537], 1)      # exactly 2048 bits, odd
add("key", ["key", format((1 << 2046) | 1, "0512x"), e65537], 0)      # 2047 bits
add("key", ["key", format((1 << 1023) | 1, "0256x"), e65537], 0)      # 1024 bits
add("key", ["key", format((1 << 4095) | 1, "01024x"), e65537], 1)     # exactly 4096 bits
add("key", ["key", format((1 << 4096) | 1, "01026x"), e65537], 0)     # 4097 bits
add("key", ["key", "0000" + format((1 << 4095) | 1, "01024x"), e65537], 1)
add("key", ["key", format((1 << 8191) | 1, "02048x"), e65537], 0)     # 8192 bits

# --- good signatures from OpenSSL, and their mutations --------------------------------

def message_digest(hash_name):
    message = rng.randbytes(rng.randrange(0, 200))
    return HASHES[hash_name]["py"](message).digest()


def openssl_sign(key, scheme, hash_name, digest):
    info = HASHES[hash_name]
    if scheme == "pkcs1":
        return key.private.sign(digest, padding.PKCS1v15(), Prehashed(info["crypto"]))
    return key.private.sign(digest, padding.PSS(padding.MGF1(info["crypto"]), info["size"]), Prehashed(info["crypto"]))


for key in (standard[2048], standard[3072], standard[4096], e3_key):
    for scheme in ("pkcs1", "pss"):
        for hash_name in ("sha256", "sha384", "sha512"):
            rounds = 3 if key.bits == 2048 else 1
            for _ in range(rounds):
                digest = message_digest(hash_name)
                signature = openssl_sign(key, scheme, hash_name, digest)
                assert openssl_accepts(key.public, scheme, hash_name, digest, signature)
                sig_case("good", key, scheme, hash_name, digest, signature, 1)
            # mutations of the last good signature
            for bit in rng.sample(range(8 * key.size), 3):
                bad = flip(signature, bit)
                assert not openssl_accepts(key.public, scheme, hash_name, digest, bad)
                sig_case("flipsig", key, scheme, hash_name, digest, bad, 0)
            for bit in (0, 8 * len(digest) - 1, rng.randrange(8 * len(digest))):
                bad_digest = flip(digest, bit)
                assert not openssl_accepts(key.public, scheme, hash_name, bad_digest, signature)
                sig_case("flipdigest", key, scheme, hash_name, bad_digest, signature, 0)
            # a one-bit-different modulus (parse may reject it; verify never accepts)
            low = key.n ^ (1 << rng.randrange(1, 8 * key.size - 8))
            sig_case("flipkey", key, scheme, hash_name, digest, signature, 0,
                     modulus_hex=low.to_bytes(key.size, "big").hex())
            # wrong exponent
            wrong_e = "010001" if key.e == 3 else "03"
            sig_case("wronge", key, scheme, hash_name, digest, signature, 0, exponent_hex=wrong_e)
            # digest of the wrong length
            sig_case("shortdigest", key, scheme, hash_name, digest[:-1], signature, 0)
            sig_case("longdigest", key, scheme, hash_name, digest + b"\x00", signature, 0)
            sig_case("emptydigest", key, scheme, hash_name, b"", signature, 0)
            # the other scheme, and the other hashes
            other = "pss" if scheme == "pkcs1" else "pkcs1"
            assert not openssl_accepts(key.public, other, hash_name, digest, signature)
            sig_case("wrongscheme", key, other, hash_name, digest, signature, 0)
            for other_hash in HASHES:
                if other_hash != hash_name:
                    other_digest = HASHES[other_hash]["py"](b"unrelated").digest()
                    sig_case("wronghash", key, scheme, other_hash, other_digest, signature, 0)
            # signature shape
            sig_case("shortsig", key, scheme, hash_name, digest, signature[1:], 0)
            sig_case("longsig", key, scheme, hash_name, digest, b"\x00" + signature, 0)
            sig_case("nosig", key, scheme, hash_name, digest, b"", 0)

# every value of the signature from 0 and 1 up: zero, one, n-1, n, n+1, all ones
for key in (standard[2048], standard[4096]):
    digest = HASHES["sha256"]["py"](b"edge").digest()
    for label, value in (("zero", 0), ("one", 1), ("nminus1", key.n - 1), ("n", key.n), ("nplus1", key.n + 1),
                         ("allones", (1 << (8 * key.size)) - 1)):
        signature = value.to_bytes(key.size, "big")
        assert not openssl_accepts(key.public, "pkcs1", "sha256", digest, signature)
        sig_case("edge" + label, key, "pkcs1", "sha256", digest, signature, 0)
        sig_case("edge" + label, key, "pss", "sha256", digest, signature, 0)

# s >= n: a valid signature plus the modulus, which a verifier that reduces mod n would accept
for key in (standard[2048], e3_key):
    for scheme in ("pkcs1", "pss"):
        done = 0
        while done < 2:
            digest = HASHES["sha256"]["py"](rng.randbytes(8)).digest()
            signature = openssl_sign(key, scheme, "sha256", digest)
            shifted = int.from_bytes(signature, "big") + key.n
            if shifted < (1 << (8 * key.size)):
                assert openssl_accepts(key.public, scheme, "sha256", digest, signature)
                sig_case("plusn", key, scheme, "sha256", digest, shifted.to_bytes(key.size, "big"), 0)
                done += 1

# --- hand-built PKCS#1 v1.5: the control and the malformed shapes ------------------------

for key in (standard[2048], standard[3072], e3_key) + tuple(odd_keys.values()):
    for hash_name in HASHES:
        digest = HASHES[hash_name]["py"](rng.randbytes(20)).digest()
        good = key.raw_sign(pkcs1_em(key, hash_name, digest))
        assert openssl_accepts(key.public, "pkcs1", hash_name, digest, good), (key.bits, hash_name)
        if key.bits in (2048, 3072) and key.e == 65537:
            assert good == openssl_sign(key, "pkcs1", hash_name, digest), "hand-built PKCS#1 differs from OpenSSL"
        sig_case("p1control", key, "pkcs1", hash_name, digest, good, 1)
        if key.size != 256:
            continue
        malformed = {
            "nonull": dict(prefix=HASHES[hash_name]["no_null"]),
            "trailing": dict(tail=b"\x00"),
            "trailing_ff": dict(tail=b"\xff"),
            "pad7": dict(padding_length=None, lead=b"\x00\x01", tail=b""),
            "blocktype2": dict(lead=b"\x00\x02"),
            "noleadzero": dict(lead=b"\x01"),
            "nosep": dict(sep=b""),
            "wrongprefix": dict(prefix=HASHES["sha256" if hash_name != "sha256" else "sha384"]["prefix"]),
        }
        for label, kwargs in malformed.items():
            if label == "pad7":
                # 7 bytes of 0xFF plus a tail that makes up the length: fewer than eight FF
                em = b"\x00\x01" + b"\xff" * 7 + b"\x00" + HASHES[hash_name]["prefix"] + digest
                em = em + b"\x00" * (key.size - len(em))
            else:
                em = pkcs1_em(key, hash_name, digest, **kwargs)
                em = em[-key.size:] if len(em) > key.size else em
                if len(em) < key.size:
                    em = em + b"\x00" * (key.size - len(em))
            if int.from_bytes(em, "big") >= key.n:
                continue
            bad = key.raw_sign(em)
            if openssl_accepts(key.public, "pkcs1", hash_name, digest, bad):
                notes.append("OpenSSL accepts the %s encoding (%s)" % (label, hash_name))
            sig_case("p1" + label, key, "pkcs1", hash_name, digest, bad, 0)
        # padding with a zero in the middle, and a too-long block (k + 1 bytes, hand-encoded)
        em = b"\x00\x01" + b"\xff" * 10 + b"\x00" + b"\xff" * (key.size - 2 - 10 - 1 - len(HASHES[hash_name]["prefix"]) - len(digest) - 0)
        em = em + HASHES[hash_name]["prefix"] + digest
        em = (b"\x00" * (key.size - len(em))) + em if len(em) < key.size else em[-key.size:]
        if int.from_bytes(em, "big") < key.n:
            sig_case("p1midzero", key, "pkcs1", hash_name, digest, key.raw_sign(em), 0)

# Bleichenbacher's e=3 forgery: a cube root of an encoding with garbage after the digest.
def icbrt_ceil(value):
    low, high = 0, 1 << (value.bit_length() // 3 + 2)
    while low < high:
        mid = (low + high) // 2
        if mid ** 3 >= value:
            high = mid
        else:
            low = mid + 1
    return low


for hash_name in HASHES:
    key = e3_key
    digest = HASHES[hash_name]["py"](b"forge me " + hash_name.encode()).digest()
    head = b"\x00\x01\xff\x00" + HASHES[hash_name]["prefix"] + digest
    target = int.from_bytes(head + b"\x00" * (key.size - len(head)), "big")
    forged_root = icbrt_ceil(target)
    forged_power = forged_root ** 3
    assert forged_power < key.n
    block = forged_power.to_bytes(key.size, "big")
    if not block.startswith(head):
        notes.append("no e=3 forgery fits a 2048-bit modulus for %s (the digest is too long)" % hash_name)
        continue
    # a lax verifier that stops parsing after the digest accepts it
    assert block[:4] == b"\x00\x01\xff\x00" and block[4:4 + len(head) - 4] == head[4:]
    forged_signature = forged_root.to_bytes(key.size, "big")
    assert not openssl_accepts(key.public, "pkcs1", hash_name, digest, forged_signature)
    sig_case("bleichenbacher", key, "pkcs1", hash_name, digest, forged_signature, 0)

# --- hand-built PSS: the control and the malformed shapes ---------------------------------

for key in (standard[2048], standard[3072], standard[4096], e3_key) + tuple(odd_keys.values()):
    for hash_name in HASHES:
        info = HASHES[hash_name]
        digest = info["py"](rng.randbytes(20)).digest()
        salt = rng.randbytes(info["size"])
        em = pss_em(key, hash_name, digest, salt)
        good = key.raw_sign(em)
        assert openssl_accepts(key.public, "pss", hash_name, digest, good), (key.bits, hash_name)
        sig_case("psscontrol", key, "pss", hash_name, digest, good, 1)
        if key.bits not in (2048, 2049, 2056):
            continue
        variants = {
            "trailer": dict(trailer=0xBB),
            "trailer00": dict(trailer=0x00),
            "flipH": dict(flip_hash=True),
            "flipmasked": dict(flip_masked=5),
            "flipmasked_last": dict(flip_masked=(key.bits - 1 + 7) // 8 - info["size"] - 2),
            "noise": dict(padding_noise=3),
            "noone": dict(drop_one=True),
        }
        for label, kwargs in variants.items():
            variant_em = pss_em(key, hash_name, digest, salt, **kwargs)
            if int.from_bytes(variant_em, "big") >= key.n:
                continue
            bad = key.raw_sign(variant_em)
            if openssl_accepts(key.public, "pss", hash_name, digest, bad):
                notes.append("OpenSSL accepts the PSS %s encoding (%s)" % (label, hash_name))
            sig_case("pss" + label, key, "pss", hash_name, digest, bad, 0)
        # a salt of the wrong length: well-formed PSS, but not the one TLS 1.3 specifies
        for salt_length in (0, 16, info["size"] - 1, info["size"] + 1):
            wrong_salt = rng.randbytes(salt_length)
            variant_em = pss_em(key, hash_name, digest, wrong_salt)
            if int.from_bytes(variant_em, "big") >= key.n:
                continue
            bad = key.raw_sign(variant_em)
            sig_case("psssalt%d" % salt_length, key, "pss", hash_name, digest, bad, 0)
        # the top bits of the encoded message left set (the leftmost 8*emLen-emBits bits must be zero)
        if 8 * ((key.bits - 1 + 7) // 8) - (key.bits - 1) > 0:
            variant_em = pss_em(key, hash_name, digest, salt, keep_top_bits=True)
            if int.from_bytes(variant_em, "big") < key.n:
                sig_case("psstopbits", key, "pss", hash_name, digest, key.raw_sign(variant_em), 0)
        # the PSS encoding of a different digest
        other = info["py"](b"another message").digest()
        sig_case("pssotherdigest", key, "pss", hash_name, other, good, 0)

# --- real certificates: the self-signed RSA roots in the system CA bundle ------------------

BUNDLES = ["/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/certs/ca-certificates.crt", "/etc/ssl/cert.pem"]
real_count = {"accepted": 0, "refused": 0}
for path in BUNDLES:
    try:
        with open(path, "rb") as handle:
            certificates = x509.load_pem_x509_certificates(handle.read())
    except (OSError, ValueError):
        continue
    for certificate in certificates:
        public = certificate.public_key()
        if not isinstance(public, rsa.RSAPublicKey) or certificate.issuer != certificate.subject:
            continue
        try:
            hash_algorithm = certificate.signature_hash_algorithm
        except Exception:
            continue
        if hash_algorithm is None or hash_algorithm.name not in HASHES:
            continue
        hash_name = hash_algorithm.name
        numbers = public.public_numbers()
        size = (numbers.n.bit_length() + 7) // 8
        digest = HASHES[hash_name]["py"](certificate.tbs_certificate_bytes).digest()
        oid = certificate.signature_algorithm_oid.dotted_string
        if oid == "1.2.840.113549.1.1.10":
            scheme = "pss"
            parameters = certificate.signature_algorithm_parameters
            salt_ok = parameters.salt_length == HASHES[hash_name]["size"]
            try:
                public.verify(certificate.signature, certificate.tbs_certificate_bytes, parameters, hash_algorithm)
                verified = True
            except InvalidSignature:
                verified = False
        else:
            scheme = "pkcs1"
            salt_ok = True
            try:
                public.verify(certificate.signature, certificate.tbs_certificate_bytes, padding.PKCS1v15(), hash_algorithm)
                verified = True
            except InvalidSignature:
                verified = False
        supported_key = 2048 <= numbers.n.bit_length() <= 4096 and numbers.n % 2 == 1 and numbers.e % 2 == 1 \
            and 3 <= numbers.e < (1 << 32)
        expected = 1 if (verified and salt_ok and supported_key) else 0
        real_count["accepted" if expected else "refused"] += 1
        signature = certificate.signature
        modulus_hex = numbers.n.to_bytes(size, "big").hex()
        exponent_hex = numbers.e.to_bytes((numbers.e.bit_length() + 7) // 8, "big").hex()
        add("root", ["sig", scheme, hash_name, modulus_hex, exponent_hex, digest.hex(), signature.hex()], expected)
        add("rootflip", ["sig", scheme, hash_name, modulus_hex, exponent_hex, flip(digest, 3).hex(), signature.hex()], 0)
    break
sys.stderr.write("real roots: %d expected accepted, %d expected refused\n" % (real_count["accepted"], real_count["refused"]))
for note in sorted(set(notes)):
    sys.stderr.write("note: %s\n" % note)

with open(sys.argv[1], "w") as vectors, open(sys.argv[2], "w") as expected_file:
    for line, expected in cases:
        vectors.write(line + "\n")
        expected_file.write("%s %d\n" % (line.split("|")[1], expected))
sys.stderr.write("%d cases (%d expected accepted)\n" % (len(cases), sum(e for _, e in cases)))
