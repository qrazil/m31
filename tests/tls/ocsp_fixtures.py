#!/usr/bin/env python3
"""Fixtures for `test_ocsp.sh`: a CA, leaves, delegated responders, an OpenSSL `ca`
index, and a corpus of OCSP responses -- good ones and every kind of wrong one --
built by hand from RFC 6960's ASN.1 (a tiny DER writer below) and signed with the
`cryptography` package. Nothing here comes from this repository, so what each
response is *supposed* to do, written in `cases.txt`, is an independent verdict.

  ocsp_fixtures.py OUTDIR

OUTDIR gets ca.pem/ca.key, other_ca.pem, leaf*.pem/.key, responder certificates,
index.txt (for `openssl ocsp -index`), c_<name>.der, and cases.txt with one line per
response:  name|leaf file|issuer file|seconds from now to check at|expected

`expected` is `ok` or the name of the ocsp.Error variant.
"""
import datetime
import hashlib
import sys

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, padding, rsa
from cryptography.x509 import ocsp as cocsp
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

out = sys.argv[1]
now = datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0)
hour = datetime.timedelta(hours=1)
day = datetime.timedelta(days=1)
PEM = serialization.Encoding.PEM


# --- a DER writer -----------------------------------------------------------------

def tlv(tag, content):
    n = len(content)
    if n < 128:
        head = bytes([tag, n])
    else:
        octets = n.to_bytes((n.bit_length() + 7) // 8, "big")
        head = bytes([tag, 0x80 | len(octets)]) + octets
    return head + content


def seq(*parts): return tlv(0x30, b"".join(parts))
def octets(data): return tlv(0x04, data)
def explicit(number, content): return tlv(0xA0 | number, content)
def null(): return b"\x05\x00"
def boolean(value): return tlv(0x01, b"\xff" if value else b"\x00")
def enumerated(value): return tlv(0x0A, bytes([value]))
def integer(value):
    raw = value.to_bytes((value.bit_length() + 8) // 8, "big")
    return tlv(0x02, raw)
def bitstring(data, unused=0): return tlv(0x03, bytes([unused]) + data)
def gtime(moment): return tlv(0x18, moment.strftime("%Y%m%d%H%M%SZ").encode())
def utctime(moment): return tlv(0x17, moment.strftime("%y%m%d%H%M%SZ").encode())

def oid(dotted):
    parts = [int(p) for p in dotted.split(".")]
    body = bytes([40 * parts[0] + parts[1]])
    for number in parts[2:]:
        chunk = [number & 0x7F]
        number >>= 7
        while number:
            chunk.append(0x80 | (number & 0x7F))
            number >>= 7
        body += bytes(reversed(chunk))
    return tlv(0x06, body)

OID_BASIC = "1.3.6.1.5.5.7.48.1.1"
OID_NONCE = "1.3.6.1.5.5.7.48.1.2"
SHA1 = "1.3.14.3.2.26"
SHA256 = "2.16.840.1.101.3.4.2.1"
MD5 = "1.2.840.113549.2.5"
ECDSA_SHA1, ECDSA_SHA256, ECDSA_SHA384 = "1.2.840.10045.4.1", "1.2.840.10045.4.3.2", "1.2.840.10045.4.3.3"
RSA_SHA256 = "1.2.840.113549.1.1.11"


def algid(dotted, with_null=False):
    return seq(oid(dotted), null() if with_null else b"")


# --- keys and certificates ----------------------------------------------------------------

def name(common_name):
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])

def issue(subject, subject_key, issuer_name, issuer_key, serial, ca=False, eku=None, key_usage=None,
          before=now - day, after=now + 30 * day, san=None, extra=(), digest=hashes.SHA256()):
    builder = (x509.CertificateBuilder().subject_name(name(subject)).issuer_name(issuer_name)
               .public_key(subject_key.public_key()).serial_number(serial)
               .not_valid_before(before).not_valid_after(after))
    if ca:
        builder = builder.add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        builder = builder.add_extension(x509.KeyUsage(False, False, False, False, False, True, True, False, False), critical=True)
    if san:
        builder = builder.add_extension(x509.SubjectAlternativeName([x509.DNSName(san)]), critical=False)
    if eku is not None:
        builder = builder.add_extension(x509.ExtendedKeyUsage(eku), critical=False)
    if key_usage is not None:
        builder = builder.add_extension(key_usage, critical=True)
    for extension, critical in extra:
        builder = builder.add_extension(extension, critical=critical)
    return builder.sign(issuer_key, digest)

def write(file_name, data):
    open(out + "/" + file_name, "wb").write(data)

def pem(file_name, certificate):
    write(file_name, certificate.public_bytes(PEM))

def key_pem(file_name, key):
    write(file_name, key.private_bytes(PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))

def ec_key():
    return ec.generate_private_key(ec.SECP256R1())

ca_key = ec_key()
ca_name = name("ocsp test CA")
ca = issue("ocsp test CA", ca_key, ca_name, ca_key, 1, ca=True)
pem("ca.pem", ca); key_pem("ca.key", ca_key)
other_key = ec_key()
other_name = name("other CA")
other_ca = issue("other CA", other_key, other_name, other_key, 2, ca=True)
pem("other_ca.pem", other_ca); key_pem("other_ca.key", other_key)

def leaf(file_stem, common_name, serial, **options):
    key = ec_key()
    certificate = issue(common_name, key, ca_name, ca_key, serial, san="localhost", **options)
    pem(file_stem + ".pem", certificate); key_pem(file_stem + ".key", key)
    return certificate

leaf_certificate = leaf("leaf", "localhost", 0x1000)
leaf_two = leaf("leaf2", "second", 0x1001)
revoked_certificate = leaf("revoked", "revoked", 0x1002)
staple_certificate = leaf("mustStaple", "mustStaple", 0x1003, extra=[(x509.TLSFeature([x509.TLSFeatureType.status_request]), False)])
leaf("stranger", "stranger", 0x1004)
leaf("expired", "expired", 0x1005, before=now - 10 * day, after=now - day)

# An OpenSSL `ca` index: good (V), revoked (R); 0x1004 is deliberately absent (status unknown).
future = (now + 365 * day).strftime("%y%m%d%H%M%SZ")
past = (now - day).strftime("%y%m%d%H%M%SZ")
rows = [("V", future, "", "1000", "/CN=localhost"), ("V", future, "", "1001", "/CN=second"),
        ("R", future, past + ",keyCompromise", "1002", "/CN=revoked"), ("V", future, "", "1003", "/CN=mustStaple")]
write("index.txt", "".join("\t".join((s, e, r, n, "unknown", d)) + "\n" for s, e, r, n, d in rows).encode())

def responder(file_stem, serial, key=None, signed_by=(ca_name, ca_key), **options):
    key = key or ec_key()
    certificate = issue(file_stem, key, signed_by[0], signed_by[1], serial, **options)
    pem(file_stem + ".pem", certificate); key_pem(file_stem + ".key", key)
    return key, certificate

KEY_USAGE_SIGN = x509.KeyUsage(True, False, False, False, False, False, False, False, False)
KEY_USAGE_ENCIPHER = x509.KeyUsage(False, False, True, False, False, False, False, False, False)
OCSP_SIGNING = [ExtendedKeyUsageOID.OCSP_SIGNING]
resp_key, resp = responder("responder", 0x2000, eku=OCSP_SIGNING, key_usage=KEY_USAGE_SIGN)
responder("responder_no_eku", 0x2001, eku=[ExtendedKeyUsageOID.SERVER_AUTH])
responder("responder_any_eku", 0x2002, eku=[x509.ObjectIdentifier("2.5.29.37.0")])
responder("responder_plain", 0x2003)
responder("responder_expired", 0x2004, eku=OCSP_SIGNING, before=now - 10 * day, after=now - day)
responder("responder_encipher_only", 0x2005, eku=OCSP_SIGNING, key_usage=KEY_USAGE_ENCIPHER)
responder("responder_rogue_issuer", 0x2006, eku=OCSP_SIGNING, signed_by=(other_name, other_key))
responder("responder_critical", 0x2007, eku=OCSP_SIGNING,
          extra=[(x509.UnrecognizedExtension(x509.ObjectIdentifier("1.2.3.4.5.6.7"), b"\x05\x00"), True)])
rsa_key = rsa.generate_private_key(65537, 2048)
responder("responder_rsa", 0x2008, key=rsa_key, eku=OCSP_SIGNING)
responders = {n: x509.load_pem_x509_certificate(open(out + "/" + n + ".pem", "rb").read()) for n in (
    "responder", "responder_no_eku", "responder_any_eku", "responder_plain", "responder_expired",
    "responder_encipher_only", "responder_rogue_issuer", "responder_critical", "responder_rsa")}
responder_keys = {n: serialization.load_pem_private_key(open(out + "/" + n + ".key", "rb").read(), None) for n in responders}


# --- responses --------------------------------------------------------------------------------

def cert_id(certificate, issuer, algorithm=hashes.SHA1(), oid_override=None, serial=None):
    request = cocsp.OCSPRequestBuilder().add_certificate(certificate, issuer, algorithm).build()
    dotted = oid_override or {"sha1": SHA1, "sha256": SHA256}[algorithm.name]
    return seq(algid(dotted, with_null=True), octets(request.issuer_name_hash), octets(request.issuer_key_hash),
               integer(serial if serial is not None else request.serial_number))

GOOD = b"\x80\x00"
UNKNOWN = b"\x82\x00"
def revoked(when=now - day, reason=1): return tlv(0xA1, gtime(when) + explicit(0, enumerated(reason)))

def single(certid, status=GOOD, this_update=now - hour, next_update=now + 7 * day, extensions=None, this_der=None):
    parts = [certid, status, this_der or gtime(this_update)]
    if next_update is not None:
        parts.append(explicit(0, gtime(next_update)))
    if extensions is not None:
        parts.append(explicit(1, extensions))
    return seq(*parts)

def extension(dotted, value, critical=False):
    return seq(oid(dotted), *( [boolean(True)] if critical else [] ), octets(value))

def sign(key, algorithm, data):
    if isinstance(key, rsa.RSAPrivateKey):
        return key.sign(data, padding.PKCS1v15(), hashes.SHA256())
    return key.sign(data, ec.ECDSA({ECDSA_SHA1: hashes.SHA1(), ECDSA_SHA256: hashes.SHA256(), ECDSA_SHA384: hashes.SHA384()}[algorithm]))

def build(singles, signer_key=None, signer_certificate=None, by_key=False, carried=(), algorithm=ECDSA_SHA256,
          produced=now - hour, response_extensions=None, version=False, flip_signature=False, unused_bits=0,
          outer_extra=False, algorithm_der=None, responder_der=None, status=0, with_bytes=True, type_oid=OID_BASIC):
    signer_key = signer_key or ca_key
    signer_certificate = signer_certificate or ca
    if responder_der is None:
        if by_key:
            point = signer_certificate.public_key().public_bytes(serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint) \
                if isinstance(signer_certificate.public_key(), ec.EllipticCurvePublicKey) else \
                signer_certificate.public_key().public_bytes(serialization.Encoding.DER, serialization.PublicFormat.PKCS1)
            responder_der = tlv(0xA2, octets(hashlib.sha1(point).digest()))
        else:
            responder_der = tlv(0xA1, signer_certificate.subject.public_bytes())
    parts = ([explicit(0, integer(0))] if version else []) + [responder_der, gtime(produced), seq(*singles)]
    if response_extensions is not None:
        parts.append(explicit(1, response_extensions))
    tbs = seq(*parts)
    signature = bytearray(sign(signer_key, algorithm, tbs))
    if flip_signature:
        signature[-1] ^= 1
    basic = [tbs, algorithm_der or algid(algorithm, with_null=(algorithm == RSA_SHA256)), bitstring(bytes(signature), unused_bits)]
    if carried:
        basic.append(explicit(0, seq(*[c.public_bytes(serialization.Encoding.DER) for c in carried])))
    basic_der = seq(*basic)
    if status != 0:
        return seq(enumerated(status))
    body = [enumerated(0)]
    if with_bytes:
        body.append(explicit(0, seq(oid(type_oid), octets(basic_der))))
    if outer_extra:
        body.append(null())
    return seq(*body)


cases = []
def case(file_stem, response, leaf_name, expected, issuer="ca.pem", offset=0):
    write("c_" + file_stem + ".der", response)
    cases.append("|".join((file_stem, leaf_name, issuer, str(offset), expected)))

L = cert_id(leaf_certificate, ca)
nonce = extension(OID_NONCE, octets(b"\x01" * 16))
crit = extension("1.2.3.4.5.6.7", b"\x05\x00", critical=True)

case("good_sha1_certid", build([single(L)]), "leaf.pem", "ok")
case("good_sha256_certid", build([single(cert_id(leaf_certificate, ca, hashes.SHA256()))]), "leaf.pem", "ok")
case("good_by_key_hash", build([single(L)], by_key=True), "leaf.pem", "ok")
case("good_ecdsa_sha384", build([single(L)], algorithm=ECDSA_SHA384), "leaf.pem", "ok")
case("good_noncritical_response_extension", build([single(L)], response_extensions=seq(nonce)), "leaf.pem", "ok")
case("good_noncritical_single_extension", build([single(L, extensions=seq(nonce))]), "leaf.pem", "ok")
case("good_other_certificate_listed_first", build([single(cert_id(leaf_two, ca)), single(L)]), "leaf.pem", "ok")
case("good_skew_within_five_minutes", build([single(L, this_update=now + 200 * datetime.timedelta(seconds=1))]), "leaf.pem", "ok")
case("good_delegated", build([single(L)], signer_key=resp_key, signer_certificate=resp, carried=[resp]), "leaf.pem", "ok")
case("good_delegated_by_key_hash", build([single(L)], signer_key=resp_key, signer_certificate=resp, carried=[resp], by_key=True), "leaf.pem", "ok")
case("good_delegated_rsa", build([single(L)], signer_key=rsa_key, signer_certificate=responders["responder_rsa"],
                                 carried=[responders["responder_rsa"]], algorithm=RSA_SHA256), "leaf.pem", "ok")
case("good_extra_certificate_carried", build([single(L)], carried=[other_ca, ca]), "leaf.pem", "ok")

case("revoked", build([single(L, status=revoked())]), "leaf.pem", "Revoked")
case("revoked_without_reason", build([single(L, status=tlv(0xA1, gtime(now - day)))]), "leaf.pem", "Revoked")
case("revoked_beats_good", build([single(L), single(L, status=revoked())]), "leaf.pem", "Revoked")
case("unknown_status", build([single(L, status=UNKNOWN)]), "leaf.pem", "UnknownCertificate")
case("expired_by_clock", build([single(L)]), "leaf.pem", "Stale", offset=8 * 86400)
case("not_yet_valid", build([single(L, this_update=now + hour)]), "leaf.pem", "NotYetValid")
case("clock_before_by_a_day", build([single(L)]), "leaf.pem", "NotYetValid", offset=-86400)
case("no_next_update", build([single(L, next_update=None)]), "leaf.pem", "Stale")
case("next_before_this", build([single(L, this_update=now - hour, next_update=now - 2 * hour)]), "leaf.pem", "Stale")
case("next_already_past", build([single(L, this_update=now - 3 * hour, next_update=now - hour)]), "leaf.pem", "Stale")
case("wrong_certificate", build([single(cert_id(leaf_two, ca))]), "leaf.pem", "WrongCertificate")
case("wrong_serial", build([single(cert_id(leaf_certificate, ca, serial=0x1234))]), "leaf.pem", "WrongCertificate")
case("wrong_issuer_hashes", build([single(cert_id(leaf_certificate, other_ca))]), "leaf.pem", "WrongCertificate")
case("unknown_hash_algorithm_in_certid", build([single(cert_id(leaf_certificate, ca, oid_override=MD5))]), "leaf.pem", "WrongCertificate")
case("checked_against_the_wrong_issuer", build([single(L)]), "leaf.pem", "UnauthorizedResponder", issuer="other_ca.pem")

case("sha1_signature", build([single(L)], algorithm=ECDSA_SHA1), "leaf.pem", "UnsupportedAlgorithm")
case("unknown_signature_algorithm", build([single(L)], algorithm_der=algid("1.2.3.4.5")), "leaf.pem", "UnsupportedAlgorithm")
case("bad_signature", build([single(L)], flip_signature=True), "leaf.pem", "BadSignature")
case("signed_by_other_key", build([single(L)], signer_key=other_key), "leaf.pem", "BadSignature")
case("signature_unused_bits", build([single(L)], unused_bits=3), "leaf.pem", "Malformed")

case("delegated_not_carried", build([single(L)], signer_key=resp_key, signer_certificate=resp), "leaf.pem", "UnauthorizedResponder")
for stem, expected in (("responder_no_eku", "UnauthorizedResponder"), ("responder_any_eku", "UnauthorizedResponder"),
                       ("responder_plain", "UnauthorizedResponder"), ("responder_expired", "UnauthorizedResponder"),
                       ("responder_encipher_only", "UnauthorizedResponder"), ("responder_rogue_issuer", "UnauthorizedResponder"),
                       ("responder_critical", "Malformed")):
    case("delegated_" + stem, build([single(L)], signer_key=responder_keys[stem], signer_certificate=responders[stem],
                                    carried=[responders[stem]]), "leaf.pem", expected)
case("delegated_named_but_signed_by_another_key",
     build([single(L)], signer_key=ec_key(), signer_certificate=resp, carried=[resp]), "leaf.pem", "BadSignature")
case("responder_id_names_nobody", build([single(L)], responder_der=tlv(0xA1, name("nobody").public_bytes())), "leaf.pem", "UnauthorizedResponder")
case("responder_id_key_hash_wrong", build([single(L)], responder_der=tlv(0xA2, octets(b"\x00" * 20))), "leaf.pem", "UnauthorizedResponder")
case("carried_certificate_unreadable", build([single(L)], signer_key=resp_key, signer_certificate=resp, carried=[]) , "leaf.pem", "UnauthorizedResponder")

case("critical_response_extension", build([single(L)], response_extensions=seq(crit)), "leaf.pem", "UnknownCriticalExtension")
case("critical_single_extension", build([single(L, extensions=seq(crit))]), "leaf.pem", "UnknownCriticalExtension")
case("critical_extension_on_another_certificates_single", build([single(cert_id(leaf_two, ca), extensions=seq(crit)), single(L)]), "leaf.pem", "UnknownCriticalExtension")
case("extension_with_explicit_false", build([single(L)], response_extensions=seq(seq(oid(OID_NONCE), boolean(False), octets(b"\x04\x00")))), "leaf.pem", "Malformed")
case("empty_extension_list", build([single(L)], response_extensions=seq()), "leaf.pem", "Malformed")

case("explicit_default_version", build([single(L)], version=True), "leaf.pem", "Malformed")
case("utc_time_not_generalized", build([single(L, this_der=utctime(now - hour))]), "leaf.pem", "Malformed")
case("fractional_seconds", build([single(L, this_der=tlv(0x18, (now - hour).strftime("%Y%m%d%H%M%S").encode() + b".5Z"))]), "leaf.pem", "Malformed")
case("no_single_responses", build([]), "leaf.pem", "Malformed")
case("unknown_status_tag", build([single(L, status=b"\x83\x00")]), "leaf.pem", "Malformed")
case("good_with_content", build([single(L, status=b"\x80\x01\x00")]), "leaf.pem", "Malformed")
case("revoked_with_bad_time", build([single(L, status=tlv(0xA1, utctime(now - day)))]), "leaf.pem", "Malformed")
case("revoked_with_bad_reason", build([single(L, status=tlv(0xA1, gtime(now - day) + explicit(0, null())))]), "leaf.pem", "Malformed")
case("trailing_element_in_response", build([single(L)], outer_extra=True), "leaf.pem", "Malformed")
case("wrong_response_type", build([single(L)], type_oid="1.3.6.1.5.5.7.48.1.2"), "leaf.pem", "Malformed")

for code, expected in ((1, "Unsuccessful"), (2, "Unsuccessful"), (3, "Unsuccessful"), (5, "Unsuccessful"), (6, "Unsuccessful"),
                       (4, "Malformed"), (7, "Malformed")):
    case("status_%d_no_body" % code, build([], status=code), "leaf.pem", expected)
case("successful_without_body", build([single(L)], with_bytes=False), "leaf.pem", "Malformed")

write("cases.txt", ("\n".join(cases) + "\n").encode())
