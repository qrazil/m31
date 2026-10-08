#!/usr/bin/env python3
"""Certificate-chain fixtures for `apps/tls/test_chain.sh`.

    python3 apps/tls/chain_fixtures.py <output directory>

Writes one directory per case, each holding

    anchors.pem   the roots the client trusts
    leaf.pem      the server's certificate
    rest.pem      the certificates the server sends after it, in the order sent
    chain.pem     leaf.pem and rest.pem together: what the server presents
    key.pem       the leaf's private key

and, in the output directory, `cases.tsv`: name, host name to verify, the
`x509chain` error the case must produce (`ok` for none), whether the case is
also run as a live handshake (`live` or `offline`). The host of a live
case is `localhost`, or a `*.localhost` name for the wildcard cases (which
resolve to the loopback on systems with systemd-resolved or nss-myhostname;
`test_chain.sh` skips those where they do not), because that is where
`openssl s_server` listens.

Time is fixed: every certificate is valid across 2020-2060 unless the case is
about validity, and the clock the tests hand the client is `NOW` below
(2030-01-01 12:00 UTC), so nothing here can expire under the tests.

Needs the `cryptography` package.
"""
import datetime
import os
import sys

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, padding, rsa
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

NOW = int(datetime.datetime(2030, 1, 1, 12, tzinfo=datetime.timezone.utc).timestamp())
BEGIN = datetime.datetime(2020, 1, 1)
END = datetime.datetime(2060, 1, 1)
LONG_PAST = datetime.datetime(2025, 1, 1)
LONG_FUTURE = datetime.datetime(2035, 1, 1)
UNKNOWN_CRITICAL_OID = x509.ObjectIdentifier("1.3.6.1.4.1.99999.1")

cases = []


def new_key(kind):
    if kind == "p256":
        return ec.generate_private_key(ec.SECP256R1())
    if kind == "p384":
        return ec.generate_private_key(ec.SECP384R1())
    if kind == "rsa":
        return rsa.generate_private_key(65537, 2048)
    if kind == "rsa1024":
        return rsa.generate_private_key(65537, 1024)
    raise ValueError(kind)


def default_hash(key):
    if isinstance(key, ec.EllipticCurvePrivateKey) and key.curve.name == "secp384r1":
        return hashes.SHA384()
    return hashes.SHA256()


class Node:
    def __init__(self, common_name, key_kind="p256"):
        self.common_name = common_name
        self.key = new_key(key_kind)
        self.name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])
        self.cert = None


def issue(subject, issuer, *, ca=False, pathlen=None, key_usage="auto", eku="auto",
          san=("localhost",), not_before=BEGIN, not_after=END, signature_hash=None,
          pss=False, basic_constraints=True, critical_unknown=False, subject_key=None,
          authority_key_id=True):
    """Certificate for `subject` signed by `issuer` (the same node for a root)."""
    public_key = (subject_key or subject.key).public_key()
    builder = (
        x509.CertificateBuilder()
        .subject_name(subject.name)
        .issuer_name(issuer.name)
        .public_key(public_key)
        .serial_number(x509.random_serial_number())
        .not_valid_before(not_before)
        .not_valid_after(not_after)
        .add_extension(x509.SubjectKeyIdentifier.from_public_key(public_key), False)
    )
    if authority_key_id:
        builder = builder.add_extension(
            x509.AuthorityKeyIdentifier.from_issuer_public_key(issuer.key.public_key()), False)
    if basic_constraints:
        builder = builder.add_extension(x509.BasicConstraints(ca=ca, path_length=pathlen), True)
    if key_usage == "auto":
        key_usage = "ca" if ca else "leaf"
    if key_usage == "ca":
        builder = builder.add_extension(usage(key_cert_sign=True, crl_sign=True), True)
    elif key_usage == "leaf":
        builder = builder.add_extension(usage(digital_signature=True), True)
    elif key_usage == "no_cert_sign":
        builder = builder.add_extension(usage(digital_signature=True), True)
    elif key_usage == "no_digital_signature":
        builder = builder.add_extension(usage(key_encipherment=True), True)
    if eku == "auto":
        eku = None if ca else [ExtendedKeyUsageOID.SERVER_AUTH]
    if eku is not None:
        builder = builder.add_extension(x509.ExtendedKeyUsage(eku), False)
    if san:
        builder = builder.add_extension(x509.SubjectAlternativeName([x509.DNSName(n) for n in san]), False)
    if critical_unknown:
        builder = builder.add_extension(x509.UnrecognizedExtension(UNKNOWN_CRITICAL_OID, b"\x05\x00"), True)
    signing_hash = signature_hash or default_hash(issuer.key)
    if pss:
        certificate = builder.sign(issuer.key, signing_hash, rsa_padding=padding.PSS(
            mgf=padding.MGF1(signing_hash), salt_length=signing_hash.digest_size))
    else:
        certificate = builder.sign(issuer.key, signing_hash)
    return certificate


def usage(**flags):
    names = ["digital_signature", "content_commitment", "key_encipherment", "data_encipherment",
             "key_agreement", "key_cert_sign", "crl_sign"]
    arguments = {name: flags.get(name, False) for name in names}
    return x509.KeyUsage(encipher_only=False, decipher_only=False, **arguments)


def root(common_name, key_kind="p256", **options):
    node = Node(common_name, key_kind)
    node.cert = issue(node, node, ca=True, san=None, **options)
    return node


def intermediate(common_name, issuer, key_kind="p256", **options):
    node = Node(common_name, key_kind)
    options.setdefault("ca", True)
    options.setdefault("san", None)
    node.cert = issue(node, issuer, **options)
    return node


def leaf(issuer, key_kind="p256", common_name="leaf.example", **options):
    node = Node(common_name, key_kind)
    node.cert = issue(node, issuer, **options)
    return node


def pem(*certificates):
    certs = [c.cert if isinstance(c, Node) else c for c in certificates]
    return b"".join(c.public_bytes(serialization.Encoding.PEM) for c in certs)


def tamper(certificate, find, replace):
    """The certificate's DER with `find` replaced by `replace`; PEM-encoded."""
    der = certificate.public_bytes(serialization.Encoding.DER)
    assert find in der and len(find) == len(replace)
    return der.replace(find, replace)


SHA256_WITH_RSA_OID = bytes.fromhex("2a864886f70d01010b")
SHA1_WITH_RSA_OID = bytes.fromhex("2a864886f70d010105")


def sha1_resigned(certificate, issuer_key):
    """The DER of `certificate` with its RSA signature algorithm changed from
    sha256WithRSAEncryption to sha1WithRSAEncryption (the same length, so no
    length fields move) and signed again, since `cryptography` will not issue
    a certificate under SHA-1 itself."""
    der = certificate.public_bytes(serialization.Encoding.DER)
    tbs = certificate.tbs_certificate_bytes
    assert tbs in der and tbs.count(SHA256_WITH_RSA_OID) == 1
    new_tbs = tbs.replace(SHA256_WITH_RSA_OID, SHA1_WITH_RSA_OID)
    signature = issuer_key.sign(new_tbs, padding.PKCS1v15(), hashes.SHA1())
    assert len(signature) == len(certificate.signature)
    patched = der.replace(tbs, new_tbs)
    patched = patched[:-len(signature)].replace(SHA256_WITH_RSA_OID, SHA1_WITH_RSA_OID) + signature
    return patched


def der_pem(der):
    import base64
    body = base64.encodebytes(der).decode().replace("\n", "")
    lines = [body[i:i + 64] for i in range(0, len(body), 64)]
    return ("-----BEGIN CERTIFICATE-----\n" + "\n".join(lines) + "\n-----END CERTIFICATE-----\n").encode()


def write(out, name, anchors, leaf_node, rest, host="localhost", expect="ok", live=True,
          leaf_pem=None, rest_pem=None):
    path = os.path.join(out, name)
    os.makedirs(path)
    leaf_bytes = leaf_pem if leaf_pem is not None else pem(leaf_node.cert)
    rest_bytes = rest_pem if rest_pem is not None else pem(*rest)
    files = {
        "anchors.pem": pem(*anchors),
        "leaf.pem": leaf_bytes,
        "rest.pem": rest_bytes,
        "chain.pem": leaf_bytes + rest_bytes,
        "key.pem": leaf_node.key.private_bytes(
            serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()),
    }
    for filename, data in files.items():
        with open(os.path.join(path, filename), "wb") as handle:
            handle.write(data)
    cases.append((name, host, expect, "live" if live else "offline"))


def generate(out):
    # --- chains that must verify ----------------------------------------------------
    root_p256 = root("Fixture Root P256")
    int_p256 = intermediate("Fixture Intermediate P256", root_p256)
    leaf_p256 = leaf(int_p256)
    write(out, "ok_ecdsa_p256", [root_p256], leaf_p256, [int_p256])
    write(out, "ok_sends_root_too", [root_p256], leaf_p256, [int_p256, root_p256])
    write(out, "ok_rest_out_of_order", [root_p256], leaf_p256, [root_p256, int_p256])
    write(out, "ok_extra_unrelated", [root_p256], leaf_p256,
          [intermediate("Fixture Unrelated", root("Fixture Elsewhere")), int_p256])

    root_p384 = root("Fixture Root P384", "p384")
    int_p384 = intermediate("Fixture Intermediate P384", root_p384, "p384")
    write(out, "ok_ecdsa_p384", [root_p384], leaf(int_p384, "p384"), [int_p384])

    root_rsa = root("Fixture Root RSA", "rsa")
    int_rsa = intermediate("Fixture Intermediate RSA", root_rsa, "rsa")
    write(out, "ok_rsa", [root_rsa], leaf(int_rsa, "rsa"), [int_rsa])
    int_rsa_384 = intermediate("Fixture Intermediate RSA 384", root_rsa, "rsa", signature_hash=hashes.SHA384())
    write(out, "ok_rsa_sha384_chain", [root_rsa], leaf(int_rsa_384, "rsa", signature_hash=hashes.SHA512()), [int_rsa_384])

    int_ec_under_rsa = intermediate("Fixture EC under RSA", root_rsa, "p256")
    write(out, "ok_mixed_ec_under_rsa", [root_rsa], leaf(int_ec_under_rsa), [int_ec_under_rsa])
    int_rsa_under_ec = intermediate("Fixture RSA under EC", root_p384, "rsa")
    write(out, "ok_mixed_rsa_under_ec", [root_p384], leaf(int_rsa_under_ec, "rsa"), [int_rsa_under_ec])
    int_p256_under_p384 = intermediate("Fixture P256 under P384", root_p384, "p256", signature_hash=hashes.SHA384())
    write(out, "ok_p256_signed_by_p384", [root_p384], leaf(int_p256_under_p384), [int_p256_under_p384])

    int_pss = intermediate("Fixture Intermediate PSS", root_rsa, "p256", pss=True, signature_hash=hashes.SHA256())
    write(out, "ok_rsa_pss_signature", [root_rsa], leaf(int_pss), [int_pss])
    int_pss_384 = intermediate("Fixture Intermediate PSS 384", root_rsa, "p256", pss=True, signature_hash=hashes.SHA384())
    write(out, "ok_rsa_pss_signature_sha384", [root_rsa], leaf(int_pss_384), [int_pss_384])

    cross_root = root("Fixture Cross Root")
    cross_signed = Node("Fixture Intermediate P256")
    cross_signed.key = int_p256.key
    cross_signed.cert = issue(int_p256, cross_root, ca=True, san=None)
    write(out, "ok_cross_signed_extra", [root_p256], leaf_p256, [cross_signed, int_p256])
    write(out, "ok_cross_signed_trusted_alternative", [cross_root, root_p256], leaf_p256, [cross_signed, int_p256])

    two_roots = [root("Fixture Root One"), root_p256, root_rsa, root_p384]
    write(out, "ok_several_roots", two_roots, leaf_p256, [int_p256])

    write(out, "ok_wildcard", [root_p256], leaf(int_p256, san=("*.wild.localhost",)), [int_p256],
          host="www.wild.localhost")
    write(out, "ok_san_case_and_dot", [root_p256], leaf(int_p256, san=("WWW.Case.Test",)), [int_p256],
          host="www.case.test.", live=False)
    write(out, "ok_second_san", [root_p256], leaf(int_p256, san=("other.test", "localhost")), [int_p256])

    chain_nodes = []
    parent = root_p256
    for number in range(8):
        parent = intermediate(f"Fixture Deep {number}", parent)
        chain_nodes.append(parent)
    write(out, "ok_deep_chain", [root_p256], leaf(parent), list(reversed(chain_nodes)), live=False)

    root_pathlen = root("Fixture Root Pathlen One", pathlen=1)
    int_under_pathlen = intermediate("Fixture Under Pathlen", root_pathlen, pathlen=0)
    write(out, "ok_pathlen_exactly", [root_pathlen], leaf(int_under_pathlen), [int_under_pathlen])

    old_root = Node("Fixture Old Root")
    old_root.cert = issue(old_root, old_root, ca=True, san=None, basic_constraints=False, key_usage=None)
    int_old = intermediate("Fixture Under Old Root", old_root)
    write(out, "ok_root_without_basic_constraints", [old_root], leaf(int_old), [int_old])

    # --- chains that must be refused --------------------------------------------------
    write(out, "bad_leaf_expired", [root_p256], leaf(int_p256, not_before=BEGIN, not_after=LONG_PAST),
          [int_p256], expect="Expired")
    write(out, "bad_leaf_not_yet_valid", [root_p256], leaf(int_p256, not_before=LONG_FUTURE, not_after=END),
          [int_p256], expect="NotYetValid")
    expired_int = intermediate("Fixture Expired Intermediate", root_p256, not_after=LONG_PAST)
    write(out, "bad_intermediate_expired", [root_p256], leaf(expired_int), [expired_int], expect="Expired")
    future_int = intermediate("Fixture Future Intermediate", root_p256, not_before=LONG_FUTURE)
    write(out, "bad_intermediate_not_yet_valid", [root_p256], leaf(future_int), [future_int], expect="NotYetValid")
    expired_root = root("Fixture Expired Root", not_after=LONG_PAST)
    int_expired_root = intermediate("Fixture Under Expired Root", expired_root)
    write(out, "bad_root_expired", [expired_root], leaf(int_expired_root), [int_expired_root], expect="Expired")

    write(out, "bad_hostname", [root_p256], leaf(int_p256, san=("elsewhere.test",)), [int_p256],
          expect="HostnameMismatch")
    write(out, "bad_hostname_common_name_only", [root_p256], leaf(int_p256, san=None, common_name="localhost"),
          [int_p256], expect="HostnameMismatch")
    write(out, "bad_wildcard_com", [root_p256], leaf(int_p256, san=("*.com",)), [int_p256],
          host="example.com", expect="HostnameMismatch", live=False)
    write(out, "bad_wildcard_tld", [root_p256], leaf(int_p256, san=("*.localhost",)), [int_p256],
          host="www.localhost", expect="HostnameMismatch")
    write(out, "bad_wildcard_depth", [root_p256], leaf(int_p256, san=("*.wild.localhost",)), [int_p256],
          host="a.b.wild.localhost", expect="HostnameMismatch")
    write(out, "bad_wildcard_apex", [root_p256], leaf(int_p256, san=("*.wild.localhost",)), [int_p256],
          host="wild.localhost", expect="HostnameMismatch")

    write(out, "bad_untrusted_root", [root("Fixture Some Other Root")], leaf_p256, [int_p256],
          expect="UnknownAuthority")
    write(out, "bad_no_roots_match_issuer_name", [root_rsa], leaf_p256, [int_p256, root_p256],
          expect="UnknownAuthority")
    write(out, "bad_missing_intermediate", [root_p256], leaf_p256, [], expect="UnknownAuthority")
    selfsigned = Node("localhost")
    selfsigned.cert = issue(selfsigned, selfsigned, ca=False, key_usage="leaf")
    write(out, "bad_self_signed_leaf", [root_p256], selfsigned, [], expect="UnknownAuthority")

    not_ca = intermediate("Fixture Not A CA", root_p256, ca=False, key_usage="leaf", eku=None)
    write(out, "bad_intermediate_not_ca", [root_p256], leaf(not_ca), [not_ca], expect="NotCertificateAuthority")
    no_bc = intermediate("Fixture No Basic Constraints", root_p256, basic_constraints=False)
    write(out, "bad_intermediate_no_basic_constraints", [root_p256], leaf(no_bc), [no_bc],
          expect="NotCertificateAuthority")
    no_sign = intermediate("Fixture No Cert Sign", root_p256, key_usage="no_cert_sign")
    write(out, "bad_intermediate_no_key_cert_sign", [root_p256], leaf(no_sign), [no_sign],
          expect="NotCertificateAuthority")
    root_not_ca = Node("Fixture Root Not CA")
    root_not_ca.cert = issue(root_not_ca, root_not_ca, ca=False, key_usage="leaf", san=None, eku=None)
    int_root_not_ca = intermediate("Fixture Under Non CA Root", root_not_ca)
    write(out, "bad_root_not_ca", [root_not_ca], leaf(int_root_not_ca), [int_root_not_ca],
          expect="NotCertificateAuthority")

    pathlen_root = root("Fixture Pathlen Zero Root", pathlen=0)
    pathlen_int = intermediate("Fixture Under Pathlen Zero Root", pathlen_root)
    write(out, "bad_pathlen_root", [pathlen_root], leaf(pathlen_int), [pathlen_int], expect="PathLengthExceeded")
    pathlen_top = intermediate("Fixture Pathlen Zero Intermediate", root_p256, pathlen=0)
    pathlen_low = intermediate("Fixture Below Pathlen Zero", pathlen_top)
    write(out, "bad_pathlen_intermediate", [root_p256], leaf(pathlen_low), [pathlen_low, pathlen_top],
          expect="PathLengthExceeded")

    write(out, "bad_leaf_client_auth_only", [root_p256],
          leaf(int_p256, eku=[ExtendedKeyUsageOID.CLIENT_AUTH]), [int_p256], expect="WrongPurpose")
    email_int = intermediate("Fixture Email Only Intermediate", root_p256, eku=[ExtendedKeyUsageOID.EMAIL_PROTECTION])
    write(out, "bad_intermediate_wrong_eku", [root_p256], leaf(email_int), [email_int], expect="WrongPurpose")
    write(out, "bad_leaf_key_usage_no_signature", [root_p256], leaf(int_p256, key_usage="no_digital_signature"),
          [int_p256], expect="WrongPurpose")
    write(out, "ok_leaf_any_eku", [root_p256],
          leaf(int_p256, eku=[x509.ObjectIdentifier("2.5.29.37.0")]), [int_p256])

    # A flipped byte in the leaf's signature (near its end, inside the s integer).
    good_der = leaf_p256.cert.public_bytes(serialization.Encoding.DER)
    bad_signature = bytearray(good_der)
    bad_signature[-10] ^= 0x01
    write(out, "bad_signature_leaf", [root_p256], leaf_p256, [int_p256], expect="BadSignature",
          leaf_pem=der_pem(bytes(bad_signature)))
    # A byte changed in the signed part: the subject name.
    tampered = tamper(leaf_p256.cert, b"leaf.example", b"leaf.exbmple")
    write(out, "bad_signature_tbs_changed", [root_p256], leaf_p256, [int_p256], expect="BadSignature",
          leaf_pem=der_pem(tampered))
    bad_int = bytearray(int_p256.cert.public_bytes(serialization.Encoding.DER))
    bad_int[-10] ^= 0x01
    write(out, "bad_signature_intermediate", [root_p256], leaf_p256, [], expect="BadSignature",
          rest_pem=der_pem(bytes(bad_int)))
    # A signature that verifies under a *different* key than the issuer's.
    forged_int = Node("Fixture Intermediate P256")
    forged_int.cert = issue(forged_int, int_p256, ca=True, san=None)
    forged_leaf = leaf(int_p256)
    forged_leaf.cert = issue(forged_leaf, forged_int, key_usage="leaf", authority_key_id=False)
    write(out, "bad_signature_wrong_issuer_key", [root_p256], forged_leaf, [int_p256], expect="BadSignature")
    # The same forgery with the forger's key identifier on it is not even a candidate path.
    named_leaf = leaf(int_p256)
    named_leaf.cert = issue(named_leaf, forged_int, key_usage="leaf")
    write(out, "bad_issuer_key_identifier_differs", [root_p256], named_leaf, [int_p256],
          expect="UnknownAuthority")
    sha1_leaf = leaf(int_rsa)
    write(out, "bad_weak_hash_sha1_leaf", [root_rsa], sha1_leaf, [int_rsa], expect="WeakAlgorithm",
          leaf_pem=der_pem(sha1_resigned(sha1_leaf.cert, int_rsa.key)))
    sha1_int = intermediate("Fixture SHA-1 Intermediate", root_rsa, "rsa")
    sha1_int_leaf = leaf(sha1_int, "p256")
    write(out, "bad_weak_hash_sha1_intermediate", [root_rsa], sha1_int_leaf, [sha1_int], expect="WeakAlgorithm",
          rest_pem=der_pem(sha1_resigned(sha1_int.cert, root_rsa.key)))
    small_int = intermediate("Fixture RSA 1024 Intermediate", root_rsa, "rsa1024")
    write(out, "bad_weak_rsa_1024", [root_rsa], leaf(small_int, "p256"), [small_int], expect="WeakAlgorithm")

    write(out, "bad_unknown_critical_leaf", [root_p256], leaf(int_p256, critical_unknown=True), [int_p256],
          expect="LeafUnparseable")
    critical_int = intermediate("Fixture Critical Intermediate", root_p256, critical_unknown=True)
    write(out, "bad_unknown_critical_intermediate", [root_p256], leaf(critical_int), [critical_int],
          expect="UnknownAuthority")

    too_deep = []
    parent = root_p256
    for number in range(11):
        parent = intermediate(f"Fixture TooDeep {number}", parent)
        too_deep.append(parent)
    write(out, "bad_too_deep", [root_p256], leaf(parent), list(reversed(too_deep)), expect="TooDeep", live=False)

    with open(os.path.join(out, "cases.tsv"), "w") as handle:
        for row in cases:
            handle.write("\t".join(row) + "\n")
    with open(os.path.join(out, "now"), "w") as handle:
        handle.write(str(NOW) + "\n")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    os.makedirs(sys.argv[1], exist_ok=True)
    generate(sys.argv[1])
