#!/usr/bin/env python3
"""Independent oracle for lib/x509.m31 and lib/der.m31.

  x509_oracle.py dump <file.pem>           the exact `key value` lines
                                           x509_dump.m31 prints, but computed
                                           by Python's `cryptography`
  x509_oracle.py openssl-check <file.pem>  cross-checks `cryptography`
                                           against `openssl x509 -text`

Neither shares a line of code with the Oro side. The only hand-written DER
walking here is two `read_tlv` calls to pull out the signature-algorithm
parameters, which `cryptography` does not expose.
"""
import hashlib
import subprocess
import sys
import warnings

warnings.simplefilter("ignore")

from cryptography import x509
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec, rsa

DER = serialization.Encoding.DER
CURVE_OIDS = {
    "secp256r1": "1.2.840.10045.3.1.7",
    "secp384r1": "1.3.132.0.34",
    "secp521r1": "1.3.132.0.35",
}
KEY_USAGE_BITS = [
    ("digital_signature", 0x001),
    ("content_commitment", 0x002),
    ("key_encipherment", 0x004),
    ("data_encipherment", 0x008),
    ("key_agreement", 0x010),
    ("key_cert_sign", 0x020),
    ("crl_sign", 0x040),
]


def read_tlv(data, at):
    tag = data[at]
    first = data[at + 1]
    if first < 0x80:
        size, body = first, at + 2
    else:
        count = first & 0x7F
        size = int.from_bytes(data[at + 2 : at + 2 + count], "big")
        body = at + 2 + count
    return tag, body, body + size


def children(data, body, stop):
    out = []
    while body < stop:
        _, start, end = read_tlv(data, body)
        out.append((body, end))
        body = end
    return out


def signed_bytes(number):
    length = 1
    while not (-(1 << (8 * length - 1)) <= number < (1 << (8 * length - 1))):
        length += 1
    return number.to_bytes(length, "big", signed=True)


def unsigned_bytes(number):
    return number.to_bytes((number.bit_length() + 7) // 8, "big")


def or_dash(text):
    return text if text else "-"


def flag(value):
    return "1" if value else "0"


def extension(cert, cls):
    try:
        return cert.extensions.get_extension_for_class(cls).value
    except x509.ExtensionNotFound:
        return None


def dump_certificate(number, cert):
    der = cert.public_bytes(DER)
    _, body, stop = read_tlv(der, 0)
    parts = children(der, body, stop)
    alg_start, alg_end = parts[1]
    _, alg_body, alg_stop = read_tlv(der, alg_start)
    alg_parts = children(der, alg_body, alg_stop)
    parameters = ""
    if len(alg_parts) == 2:
        parameters = der[alg_parts[1][0] : alg_parts[1][1]].hex()

    key = cert.public_key()
    spki = key.public_bytes(DER, serialization.PublicFormat.SubjectPublicKeyInfo).hex()
    curve = modulus = exponent = ""
    if isinstance(key, rsa.RSAPublicKey):
        algorithm = "1.2.840.113549.1.1.1"
        numbers = key.public_numbers()
        modulus = unsigned_bytes(numbers.n).hex()
        exponent = unsigned_bytes(numbers.e).hex()
        key_bytes = key.public_bytes(DER, serialization.PublicFormat.PKCS1).hex()
    elif isinstance(key, ec.EllipticCurvePublicKey):
        algorithm = "1.2.840.10045.2.1"
        curve = CURVE_OIDS[key.curve.name]
        key_bytes = key.public_bytes(
            serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint
        ).hex()
    else:
        raise SystemExit("oracle: unsupported key type %r" % type(key))

    san = extension(cert, x509.SubjectAlternativeName)
    dns = san.get_values_for_type(x509.DNSName) if san else []
    basic = extension(cert, x509.BasicConstraints)
    usage = extension(cert, x509.KeyUsage)
    mask = 0
    if usage:
        for name, bit in KEY_USAGE_BITS:
            if getattr(usage, name):
                mask |= bit
        if usage.key_agreement:
            mask |= 0x080 if usage.encipher_only else 0
            mask |= 0x100 if usage.decipher_only else 0
    eku = extension(cert, x509.ExtendedKeyUsage)
    ski = extension(cert, x509.SubjectKeyIdentifier)
    aki = extension(cert, x509.AuthorityKeyIdentifier)

    lines = [
        "cert %d" % number,
        "version %d" % (cert.version.value + 1),
        "serial " + signed_bytes(cert.serial_number).hex(),
        "tbs_sha256 " + hashlib.sha256(cert.tbs_certificate_bytes).hexdigest(),
        "signature_algorithm " + cert.signature_algorithm_oid.dotted_string,
        "signature_parameters " + or_dash(parameters),
        "signature " + cert.signature.hex(),
        "issuer " + cert.issuer.public_bytes().hex(),
        "subject " + cert.subject.public_bytes().hex(),
        "not_before %d" % int(cert.not_valid_before_utc.timestamp()),
        "not_after %d" % int(cert.not_valid_after_utc.timestamp()),
        "spki " + spki,
        "key_algorithm " + algorithm,
        "key_curve " + or_dash(curve),
        "key_modulus " + or_dash(modulus),
        "key_exponent " + or_dash(exponent),
        "key_bytes " + key_bytes,
        "san %s %s" % (flag(san is not None), or_dash(",".join(dns))),
        "basic_constraints %s %s %d"
        % (
            flag(basic is not None),
            flag(basic.ca if basic else False),
            basic.path_length if basic and basic.path_length is not None else -1,
        ),
        "key_usage %s %d" % (flag(usage is not None), mask),
        "extended_key_usage "
        + or_dash(",".join(o.dotted_string for o in eku) if eku else ""),
        "subject_key_id " + or_dash(ski.digest.hex() if ski else ""),
        "authority_key_id "
        + or_dash(aki.key_identifier.hex() if aki and aki.key_identifier else ""),
    ]
    return lines


def load(path):
    with open(path, "rb") as handle:
        return x509.load_pem_x509_certificates(handle.read())


def dump(path):
    for number, cert in enumerate(load(path), 1):
        print("\n".join(dump_certificate(number, cert)))


def epoch_from_openssl(text):
    out = subprocess.run(
        ["date", "-u", "-d", text, "+%s"], capture_output=True, text=True, check=True
    )
    return int(out.stdout)


def openssl_check(path):
    bad = 0
    certs = load(path)
    for number, cert in enumerate(certs, 1):
        text = subprocess.run(
            ["openssl", "x509", "-noout", "-serial", "-startdate", "-enddate",
             "-ext", "subjectAltName,basicConstraints"],
            input=cert.public_bytes(serialization.Encoding.PEM),
            capture_output=True,
        ).stdout.decode()
        fields = dict(
            line.split("=", 1) for line in text.splitlines() if "=" in line
        )
        problems = []
        openssl_serial = int(fields["serial"], 16)
        if openssl_serial != cert.serial_number and (
            openssl_serial != cert.serial_number % (1 << (8 * len(signed_bytes(cert.serial_number))))
        ):
            problems.append("serial %x vs %x" % (openssl_serial, cert.serial_number))
        if epoch_from_openssl(fields["notBefore"]) != int(cert.not_valid_before_utc.timestamp()):
            problems.append("notBefore")
        if epoch_from_openssl(fields["notAfter"]) != int(cert.not_valid_after_utc.timestamp()):
            problems.append("notAfter")
        san = extension(cert, x509.SubjectAlternativeName)
        for name in san.get_values_for_type(x509.DNSName) if san else []:
            if "DNS:" + name not in text:
                problems.append("san " + name)
        basic = extension(cert, x509.BasicConstraints)
        if basic is not None:
            if ("CA:TRUE" in text) != basic.ca:
                problems.append("cA")
            if basic.path_length is not None and "pathlen:%d" % basic.path_length not in text:
                problems.append("pathlen")
        if problems:
            bad += 1
            print("openssl disagrees on cert %d: %s" % (number, ", ".join(problems)))
    print("openssl cross-check: %d certs, %d disagreements" % (len(certs), bad))
    return 1 if bad else 0


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] not in ("dump", "openssl-check"):
        raise SystemExit(__doc__)
    if sys.argv[1] == "dump":
        dump(sys.argv[2])
    else:
        sys.exit(openssl_check(sys.argv[2]))
