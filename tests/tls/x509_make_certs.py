#!/usr/bin/env python3
"""Generates the synthetic certificates `x509_cases.m31` is checked on.

  x509_make_certs.py <outdir>

Writes, into <outdir>:
  good_*.pem        well-formed certificates (RSA, P-256, P-384, CA, leaf),
                    also fed to `x509_oracle.py dump` for the field diff
  mutant_*.pem      the same certificate, broken in exactly one way
  x509_mutants.tsv  `name <TAB> strict outcome <TAB> trust-anchor outcome`,
                    an outcome being an `x509.Error` variant or `Ok`
  names_*.pem       certificates with chosen SAN lists and subjects
  x509_hostnames.tsv  `file <TAB> host <TAB> 1|0` expected matches
  x509_semantics.tsv  `file <TAB> server_auth <TAB> is_ca <TAB> path_length <TAB> key_usage`

Every validity period is fixed (2020 to 2040), so nothing here depends on
the clock. The expected outcomes are written down by hand from RFC 5280 and
from `lib/pki/x509.m31`'s header, not computed by the parser under test.
"""
import base64
import datetime
import os
import sys

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, ed25519, rsa
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

DER = serialization.Encoding.DER
NOT_BEFORE = datetime.datetime(2020, 1, 1)
NOT_AFTER = datetime.datetime(2040, 1, 1)
EC256 = ec.generate_private_key(ec.SECP256R1())
EC384 = ec.generate_private_key(ec.SECP384R1())
ED25519 = ed25519.Ed25519PrivateKey.generate()
RSA2048 = rsa.generate_private_key(public_exponent=65537, key_size=2048)


def build(common_name, key, san=None, ca=None, pathlen=None, usage=None, eku=None,
          sign_with=None, extra=(), digest=hashes.SHA256(), include_ski=True):
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])
    builder = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Test Issuer")]))
        .public_key(key.public_key())
        .serial_number(0x1234567890ABCDEF)
        .not_valid_before(NOT_BEFORE)
        .not_valid_after(NOT_AFTER)
    )
    if san is not None:
        builder = builder.add_extension(x509.SubjectAlternativeName(san), critical=False)
    if ca is not None:
        builder = builder.add_extension(x509.BasicConstraints(ca=ca, path_length=pathlen), critical=True)
    if usage is not None:
        builder = builder.add_extension(usage, critical=True)
    if eku is not None:
        builder = builder.add_extension(x509.ExtendedKeyUsage(eku), critical=False)
    if include_ski:
        builder = builder.add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
        builder = builder.add_extension(
            x509.AuthorityKeyIdentifier.from_issuer_public_key(EC256.public_key()), critical=False)
    for extension, critical in extra:
        builder = builder.add_extension(extension, critical=critical)
    signer = sign_with or EC256
    if isinstance(signer, ed25519.Ed25519PrivateKey):
        digest = None
    return builder.sign(signer, digest).public_bytes(DER)


def pem(der_bytes):
    body = base64.b64encode(der_bytes).decode()
    lines = [body[i : i + 64] for i in range(0, len(body), 64)]
    return "-----BEGIN CERTIFICATE-----\n" + "\n".join(lines) + "\n-----END CERTIFICATE-----\n"


# ---- a minimal DER tree, only for breaking certificates on purpose ------------

def tlv(tag, content):
    size = len(content)
    if size < 0x80:
        head = bytes([tag, size])
    else:
        octets = size.to_bytes((size.bit_length() + 7) // 8, "big")
        head = bytes([tag, 0x80 | len(octets)]) + octets
    return head + content


class Node:
    def __init__(self, tag, content):
        self.tag, self.content = tag, content  # content: bytes, or list of Node

    def encode(self):
        if isinstance(self.content, list):
            return tlv(self.tag, b"".join(n.encode() for n in self.content))
        return tlv(self.tag, self.content)


def read(data, at=0):
    tag = data[at]
    first = data[at + 1]
    if first < 0x80:
        size, body = first, at + 2
    else:
        count = first & 0x7F
        size = int.from_bytes(data[at + 2 : at + 2 + count], "big")
        body = at + 2 + count
    stop = body + size
    if tag & 0x20:
        kids, cursor = [], body
        while cursor < stop:
            kid, cursor = read(data, cursor)
            kids.append(kid)
        return Node(tag, kids), stop
    return Node(tag, data[body:stop]), stop


def oid(dotted):
    arcs = [int(x) for x in dotted.split(".")]
    out = bytearray()
    for value in [arcs[0] * 40 + arcs[1]] + arcs[2:]:
        chunk = [value & 0x7F]
        value >>= 7
        while value:
            chunk.append(0x80 | (value & 0x7F))
            value >>= 7
        out.extend(reversed(chunk))
    return Node(0x06, bytes(out))


def decode_oid(node):
    data, arcs, value = node.content, [], 0
    for octet in data:
        value = value * 128 + (octet & 0x7F)
        if not octet & 0x80:
            arcs.append(value)
            value = 0
    first = arcs[0]
    head = [first // 40, first % 40] if first < 80 else [2, first - 80]
    return ".".join(str(a) for a in head + arcs[1:])


OID_SAN = "2.5.29.17"
OID_BASIC = "2.5.29.19"
OID_KEY_USAGE = "2.5.29.15"
OID_EKU = "2.5.29.37"
INT = lambda n: Node(0x02, n.to_bytes(max(1, (n.bit_length() + 8) // 8), "big"))
NULL = Node(0x05, b"")
OCTETS = lambda data: Node(0x04, data)

# tbsCertificate children: 0 version, 1 serial, 2 sigalg, 3 issuer, 4 validity,
# 5 subject, 6 spki, 7 extensions.


def split(der_bytes):
    top, _ = read(der_bytes)
    return top, top.content[0], top.content[0].content


def extension_list(tbs):
    return tbs.content[7].content[0].content


def find_extension(tbs, dotted):
    for node in extension_list(tbs):
        if decode_oid(node.content[0]) == dotted:
            return node
    raise KeyError(dotted)


def mutant(base, edit):
    top, tbs, _ = split(base)
    edit(top, tbs)
    return top.encode()


def set_ext_value(tbs, dotted, value_der):
    find_extension(tbs, dotted).content[-1] = OCTETS(value_der)


def make_mutants(base, rsa_base):
    cases = []  # (name, bytes, strict, anchor)

    def add(name, data, strict, anchor=None):
        cases.append((name, data, strict, strict if anchor is None else anchor))

    add("baseline_ok", base, "Ok")
    add("rsa_baseline_ok", rsa_base, "Ok")

    add("v1_with_extensions", mutant(base, lambda t, b: b.content.pop(0)), "UnexpectedExtensions")
    add("v2_with_extensions", mutant(base, lambda t, b: b.content.__setitem__(0, Node(0xA0, [INT(1)]))), "UnexpectedExtensions")
    add("explicit_v1", mutant(base, lambda t, b: b.content.__setitem__(0, Node(0xA0, [INT(0)]))), "BadVersion")
    add("version_4", mutant(base, lambda t, b: b.content.__setitem__(0, Node(0xA0, [INT(3)]))), "BadVersion")
    add("version_not_integer", mutant(base, lambda t, b: b.content.__setitem__(0, Node(0xA0, [OCTETS(b"\x02")]))), "BadVersion")
    add("version_two_values", mutant(base, lambda t, b: b.content.__setitem__(0, Node(0xA0, [INT(2), INT(2)]))), "BadVersion")

    def reverse_validity(t, b):
        b.content[4].content.reverse()
    add("validity_reversed", mutant(base, reverse_validity), "BadValidity")
    add("validity_bad_calendar_time",
        mutant(base, lambda t, b: b.content[4].content.__setitem__(0, Node(0x17, b"250230000000Z"))), "BadValidity")
    add("validity_not_a_time",
        mutant(base, lambda t, b: b.content[4].content.__setitem__(1, NULL)), "BadValidity")
    add("validity_three_values",
        mutant(base, lambda t, b: b.content[4].content.append(NULL)), "BadValidity")

    add("subject_not_a_name", mutant(base, lambda t, b: b.content.__setitem__(5, INT(5))), "BadName")
    add("issuer_empty_set", mutant(base, lambda t, b: b.content.__setitem__(3, Node(0x30, [Node(0x31, [])]))), "BadName")
    add("issuer_attribute_not_oid",
        mutant(base, lambda t, b: b.content.__setitem__(3, Node(0x30, [Node(0x31, [Node(0x30, [INT(1), INT(2)])])]))), "BadName")

    add("tbs_extra_trailing_field", mutant(base, lambda t, b: b.content.append(NULL)), "Malformed")
    add("serial_missing", mutant(base, lambda t, b: b.content.pop(1)), "Malformed")
    add("tbs_too_short", mutant(base, lambda t, b: b.content.__delitem__(slice(3, None))), "Malformed")
    add("top_level_extra_child", mutant(base, lambda t, b: t.content.append(NULL)), "Malformed")
    add("top_level_missing_signature", mutant(base, lambda t, b: t.content.pop()), "Malformed")
    add("tbs_not_a_sequence", mutant(base, lambda t, b: t.content.__setitem__(0, OCTETS(b"x"))), "Malformed")
    add("signature_unused_bits", mutant(base, lambda t, b: t.content.__setitem__(2, Node(0x03, b"\x01" + t.content[2].content[1:]))), "Malformed")
    add("signature_not_a_bit_string", mutant(base, lambda t, b: t.content.__setitem__(2, OCTETS(b"abc"))), "Malformed")

    def other_outer_algorithm(t, b):
        t.content[1].content[0] = oid("1.2.840.10045.4.3.3")
    add("signature_algorithm_differs", mutant(base, other_outer_algorithm), "BadSignatureAlgorithm")

    def extra_algorithm_parameter(t, b):
        t.content[1].content.append(NULL)
    add("signature_algorithm_parameters_differ", mutant(base, extra_algorithm_parameter), "BadSignatureAlgorithm")
    add("signature_algorithm_empty", mutant(base, lambda t, b: (t.content[1].content.clear(), b.content[2].content.clear())), "BadSignatureAlgorithm")

    def spki_unused(t, b):
        bits = b.content[6].content[1]
        bits.content = b"\x01" + bits.content[1:]
    add("spki_unused_bits", mutant(base, spki_unused), "BadPublicKey")
    add("spki_not_two_parts", mutant(base, lambda t, b: b.content[6].content.append(NULL)), "BadPublicKey")
    add("spki_empty_key", mutant(base, lambda t, b: b.content[6].content.__setitem__(1, Node(0x03, b"\x00"))), "BadPublicKey")
    add("ec_curve_parameters_null", mutant(base, lambda t, b: b.content[6].content[0].content.__setitem__(1, NULL)), "BadPublicKey")
    add("ec_curve_parameters_missing", mutant(base, lambda t, b: b.content[6].content[0].content.pop()), "BadPublicKey")
    add("rsa_parameters_missing", mutant(rsa_base, lambda t, b: b.content[6].content[0].content.pop()), "BadPublicKey")
    add("rsa_parameters_not_null", mutant(rsa_base, lambda t, b: b.content[6].content[0].content.__setitem__(1, INT(0))), "BadPublicKey")

    # Ed25519 (RFC 8410): no parameters, a 32-octet key.
    ed_base = build("ed.example", ED25519, san=[x509.DNSName("ed.example")], ca=None)
    add("ed25519_baseline_ok", ed_base, "Ok")
    add("ed25519_parameters_null", mutant(ed_base, lambda t, b: b.content[6].content[0].content.append(NULL)), "BadPublicKey")
    add("ed25519_key_31_octets", mutant(ed_base, lambda t, b: b.content[6].content.__setitem__(1, Node(0x03, b"\x00" + b.content[6].content[1].content[1:32]))), "BadPublicKey")
    add("ed25519_key_33_octets", mutant(ed_base, lambda t, b: b.content[6].content.__setitem__(1, Node(0x03, b.content[6].content[1].content + b"\x00"))), "BadPublicKey")
    add("ed25519_signature_parameters_null", mutant(ed_base, lambda t, b: (t.content[1].content.append(NULL), b.content[2].content.append(NULL))), "Ok")

    def rsa_key_one_integer(t, b):
        b.content[6].content[1] = Node(0x03, b"\x00" + Node(0x30, [INT(5)]).encode())
    add("rsa_key_one_integer", mutant(rsa_base, rsa_key_one_integer), "BadPublicKey")

    def rsa_negative_modulus(t, b):
        b.content[6].content[1] = Node(0x03, b"\x00" + Node(0x30, [Node(0x02, b"\x80\x01"), INT(65537)]).encode())
    add("rsa_negative_modulus", mutant(rsa_base, rsa_negative_modulus), "BadPublicKey")

    def duplicate_san(t, b):
        extension_list(b).append(find_extension(b, OID_SAN))
    add("extension_duplicate", mutant(base, duplicate_san), "DuplicateExtension")

    def explicit_false_critical(t, b):
        find_extension(b, OID_SAN).content.insert(1, Node(0x01, b"\x00"))
    add("extension_critical_false_written", mutant(base, explicit_false_critical), "BadExtension")

    def critical_not_boolean(t, b):
        find_extension(b, OID_SAN).content.insert(1, NULL)
    add("extension_critical_not_boolean", mutant(base, critical_not_boolean), "BadExtension")

    def unknown(critical):
        def edit(t, b):
            parts = [oid("1.3.6.1.4.1.99999.9")]
            if critical:
                parts.append(Node(0x01, b"\xff"))
            parts.append(OCTETS(b"\x05\x00"))
            extension_list(b).append(Node(0x30, parts))
        return edit
    add("extension_unknown_critical", mutant(base, unknown(True)), "UnknownCriticalExtension", "Ok")
    add("extension_unknown_noncritical", mutant(base, unknown(False)), "Ok")

    def known_critical_unimplemented(t, b):
        extension_list(b).append(Node(0x30, [oid("2.5.29.30"), Node(0x01, b"\xff"), OCTETS(Node(0x30, []).encode())]))
    add("extension_name_constraints_critical", mutant(base, known_critical_unimplemented), "UnknownCriticalExtension", "Ok")

    def nest(levels):
        inner = b""
        for _ in range(levels):
            inner = tlv(0x30, inner)
        return inner
    add("extension_value_10000_levels_deep", mutant(base, lambda t, b: set_ext_value(b, OID_SAN, nest(10000))), "BadExtension")
    add("unknown_extension_with_opaque_deep_value", mutant(
        base, lambda t, b: extension_list(b).append(Node(0x30, [oid("1.3.6.1.4.1.99999.8"), OCTETS(nest(10000))]))), "Ok")
    add("subject_10000_levels_deep", mutant(base, lambda t, b: b.content.__setitem__(5, Node(0x30, nest(9999)))), "Malformed")
    add("issuer_10000_levels_deep", mutant(base, lambda t, b: b.content.__setitem__(3, Node(0x30, nest(9999)))), "Malformed")
    add("extension_list_empty", mutant(base, lambda t, b: b.content[7].content.__setitem__(0, Node(0x30, []))), "Malformed")
    add("extension_wrapper_two_children", mutant(base, lambda t, b: b.content[7].content.append(NULL)), "Malformed")
    add("fields_after_extensions", mutant(base, lambda t, b: b.content.append(Node(0xA3, [Node(0x30, [])]))), "Malformed")
    add("extension_value_not_octets",
        mutant(base, lambda t, b: find_extension(b, OID_SAN).content.__setitem__(-1, NULL)), "BadExtension")
    add("extension_four_parts",
        mutant(base, lambda t, b: find_extension(b, OID_SAN).content.extend([NULL, NULL])), "BadExtension")
    add("extension_one_part",
        mutant(base, lambda t, b: find_extension(b, OID_SAN).content.pop()), "BadExtension")
    add("extension_oid_not_oid",
        mutant(base, lambda t, b: find_extension(b, OID_SAN).content.__setitem__(0, INT(1))), "BadExtension")

    def san_value(value):
        return lambda t, b: set_ext_value(b, OID_SAN, value)
    add("san_empty_sequence", mutant(base, san_value(Node(0x30, []).encode())), "BadExtension")
    add("san_empty_dns_name", mutant(base, san_value(Node(0x30, [Node(0x82, b"")]).encode())), "BadExtension")
    add("san_space_in_dns_name", mutant(base, san_value(Node(0x30, [Node(0x82, b"a b.example")]).encode())), "BadExtension")
    add("san_non_ascii_dns_name", mutant(base, san_value(Node(0x30, [Node(0x82, "bücher.example".encode())]).encode())), "BadExtension")
    add("san_nul_in_dns_name", mutant(base, san_value(Node(0x30, [Node(0x82, b"a\x00b.example")]).encode())), "BadExtension")
    add("san_unknown_general_name", mutant(base, san_value(Node(0x30, [Node(0x89, b"x")]).encode())), "BadExtension")
    add("san_ip_v4_ok", mutant(base, san_value(Node(0x30, [Node(0x87, bytes(4))]).encode())), "Ok")
    add("san_ip_v6_ok", mutant(base, san_value(Node(0x30, [Node(0x87, bytes(16))]).encode())), "Ok")
    add("san_ip_empty", mutant(base, san_value(Node(0x30, [Node(0x87, b"")]).encode())), "BadExtension")
    add("san_ip_three_octets", mutant(base, san_value(Node(0x30, [Node(0x87, bytes(3))]).encode())), "BadExtension")
    add("san_ip_five_octets", mutant(base, san_value(Node(0x30, [Node(0x87, bytes(5))]).encode())), "BadExtension")
    add("san_ip_v4_with_mask", mutant(base, san_value(Node(0x30, [Node(0x87, bytes(8))]).encode())), "BadExtension")
    add("san_ip_v6_with_mask", mutant(base, san_value(Node(0x30, [Node(0x87, bytes(32))]).encode())), "BadExtension")
    add("san_trailing_byte", mutant(base, san_value(Node(0x30, [Node(0x82, b"a.example")]).encode() + b"\x00")), "BadExtension")
    add("san_not_a_sequence", mutant(base, san_value(OCTETS(b"x").encode())), "BadExtension")

    def basic(value):
        return lambda t, b: set_ext_value(b, OID_BASIC, value.encode())
    add("basic_path_length_without_ca", mutant(base, basic(Node(0x30, [INT(5)]))), "BadExtension")
    add("basic_ca_false_written", mutant(base, basic(Node(0x30, [Node(0x01, b"\x00")]))), "BadExtension")
    add("basic_extra_member", mutant(base, basic(Node(0x30, [Node(0x01, b"\xff"), INT(1), NULL]))), "BadExtension")
    add("basic_negative_path_length", mutant(base, basic(Node(0x30, [Node(0x01, b"\xff"), Node(0x02, b"\xff")]))), "BadExtension")
    add("basic_ca_not_boolean", mutant(base, basic(Node(0x30, [NULL]))), "BadExtension")
    add("basic_ca_true_ok", mutant(base, basic(Node(0x30, [Node(0x01, b"\xff"), INT(3)]))), "Ok")
    add("basic_empty_ok", mutant(base, basic(Node(0x30, []))), "Ok")

    add("key_usage_empty_bit_string", mutant(base, lambda t, b: set_ext_value(b, OID_KEY_USAGE, Node(0x03, b"\x00").encode())), "BadExtension")
    add("key_usage_not_a_bit_string", mutant(base, lambda t, b: set_ext_value(b, OID_KEY_USAGE, NULL.encode())), "BadExtension")
    add("key_usage_bad_unused_bits", mutant(base, lambda t, b: set_ext_value(b, OID_KEY_USAGE, Node(0x03, b"\x01\x81").encode())), "BadExtension")
    add("eku_empty", mutant(base, lambda t, b: set_ext_value(b, OID_EKU, Node(0x30, []).encode())), "BadExtension")
    add("eku_not_oids", mutant(base, lambda t, b: set_ext_value(b, OID_EKU, Node(0x30, [INT(1)]).encode())), "BadExtension")

    # Raw byte surgery, below what the tree can express.
    top, tbs, _ = split(base)
    pieces = [n.encode() for n in top.content]
    add("ber_indefinite_length", bytes([0x30, 0x80]) + b"".join(pieces) + b"\x00\x00", "Malformed")
    body = b"".join(pieces)
    add("non_minimal_top_length", bytes([0x30, 0x83]) + len(body).to_bytes(3, "big") + body, "Malformed")
    add("top_length_one_too_long", bytes([0x30, 0x82]) + (len(body) + 1).to_bytes(2, "big") + body, "Malformed")
    add("top_length_one_too_short", bytes([0x30, 0x82]) + (len(body) - 1).to_bytes(2, "big") + body, "Malformed")
    add("trailing_garbage", base + b"\x00", "Malformed")
    add("truncated_by_one", base[:-1], "Malformed")
    add("one_byte", b"\x30", "Malformed")
    return cases


def main(out):
    os.makedirs(out, exist_ok=True)

    def write(name, text):
        with open(os.path.join(out, name), "w") as handle:
            handle.write(text)

    san = [x509.DNSName("leaf.example"), x509.DNSName("www.leaf.example"), x509.IPAddress(__import__("ipaddress").ip_address("192.0.2.7")), x509.RFC822Name("a@leaf.example"), x509.UniformResourceIdentifier("https://leaf.example/")]
    leaf_usage = x509.KeyUsage(True, False, False, False, False, False, False, False, False)
    base = build("leaf.example", EC256, san=san, ca=False, usage=leaf_usage,
                 eku=[ExtendedKeyUsageOID.SERVER_AUTH, ExtendedKeyUsageOID.CLIENT_AUTH])
    # `ca=False` writes `cA FALSE` explicitly, which DER forbids; the baseline
    # must be valid, so give it a real CA constraint.
    base = build("leaf.example", EC256, san=san, ca=True, pathlen=2,
                 usage=x509.KeyUsage(True, False, False, False, False, True, True, False, False),
                 eku=[ExtendedKeyUsageOID.SERVER_AUTH, ExtendedKeyUsageOID.CLIENT_AUTH])
    rsa_base = build("rsa.example", RSA2048, san=[x509.DNSName("rsa.example")], ca=None,
                     usage=x509.KeyUsage(True, False, True, False, False, False, False, False, False),
                     eku=[ExtendedKeyUsageOID.SERVER_AUTH], digest=hashes.SHA384())

    # ---- mutants ----------------------------------------------------------
    manifest = []
    for name, data, strict, anchor in make_mutants(base, rsa_base):
        write("mutant_%s.pem" % name, pem(data))
        manifest.append("%s\t%s\t%s" % (name, strict, anchor))
    write("x509_mutants.tsv", "\n".join(manifest) + "\n")

    # ---- good certificates, for the field diff and the semantics table ----
    good = {
        "good_ec256_leaf": base,
        "good_rsa_leaf": rsa_base,
        "good_ec384_ca": build("ca384", EC384, ca=True, pathlen=0,
                               usage=x509.KeyUsage(False, False, False, False, False, True, True, False, False)),
        "good_ec256_no_extensions_but_ski": build("plain", EC256, include_ski=False),
        "good_ed25519_leaf": build("ed", ED25519, san=[x509.DNSName("ed.example"), x509.IPAddress(__import__("ipaddress").ip_address("127.0.0.1")), x509.IPAddress(__import__("ipaddress").ip_address("::1"))],
                                   ca=None, usage=x509.KeyUsage(True, False, False, False, False, False, False, False, False)),
        "good_ed25519_signed": build("signed", EC256, san=[x509.DNSName("signed.example")], ca=None, sign_with=ED25519),
        "good_agreement_leaf": build("agree", EC256, san=[x509.DNSName("agree.example")], ca=None,
                                     usage=x509.KeyUsage(False, False, False, False, True, False, False, True, False),
                                     eku=[ExtendedKeyUsageOID.SERVER_AUTH], include_ski=False),
    }
    semantics = []

    def semantic(name, data, server_auth, is_ca, path_length, usage):
        write(name + ".pem", pem(data))
        semantics.append("%s.pem\t%d\t%d\t%d\t%d" % (name, server_auth, is_ca, path_length, usage))

    semantic("good_ec256_leaf", base, 1, 1, 2, 0x001 | 0x020 | 0x040)
    semantic("good_rsa_leaf", rsa_base, 1, 0, -1, 0x001 | 0x004)
    semantic("good_ec384_ca", good["good_ec384_ca"], 1, 1, 0, 0x020 | 0x040)
    semantic("good_ec256_no_extensions_but_ski", good["good_ec256_no_extensions_but_ski"], 1, 0, -1, 0)
    semantic("good_ed25519_leaf", good["good_ed25519_leaf"], 1, 0, -1, 0x001)
    semantic("good_ed25519_signed", good["good_ed25519_signed"], 1, 0, -1, 0)
    semantic("good_agreement_leaf", good["good_agreement_leaf"], 1, 0, -1, 0x010 | 0x080)
    semantic("eku_client_only", build("c", EC256, eku=[ExtendedKeyUsageOID.CLIENT_AUTH]), 0, 0, -1, 0)
    semantic("eku_server_only", build("s", EC256, eku=[ExtendedKeyUsageOID.SERVER_AUTH]), 1, 0, -1, 0)
    semantic("eku_any", build("a", EC256, eku=[x509.ObjectIdentifier("2.5.29.37.0")]), 1, 0, -1, 0)
    semantic("eku_email_and_code", build("e", EC256, eku=[ExtendedKeyUsageOID.EMAIL_PROTECTION, ExtendedKeyUsageOID.CODE_SIGNING]), 0, 0, -1, 0)
    semantic("ku_all_bits", build("k", EC256, usage=x509.KeyUsage(True, True, True, True, True, True, True, True, True)), 1, 0, -1, 0x1FF)
    write("x509_semantics.tsv", "\n".join(semantics) + "\n")

    # ---- hostnames -----------------------------------------------------------
    def names(file, dns, cn="cn.invalid", extra_san=()):
        general = [x509.DNSName(n) for n in dns] + list(extra_san)
        data = build(cn, EC256, san=general if general else None, include_ski=False)
        write(file, pem(data))

    ip = __import__("ipaddress")
    names("names_wildcard.pem", ["*.example.com", "Example.ORG", "exact.example.net"])
    names("names_wildcard_misuse.pem", ["w*.example.com", "*.com", "a.*.example.com", "*", "*.*.example.com", "*.", "*foo.example.com"])
    names("names_deep_wildcard.pem", ["*.b.example.net"])
    names("names_trailing_dot.pem", ["trailing.example.io."])
    names("names_idna.pem", ["xn--bcher-kva.example", "*.xn--bcher-kva.example"])
    names("names_ip.pem", ["192.0.2.2", "example.test"], extra_san=[x509.IPAddress(ip.ip_address("192.0.2.1")), x509.IPAddress(ip.ip_address("::1")), x509.IPAddress(ip.ip_address("2001:db8::7"))])
    names("names_ip_only.pem", [], extra_san=[x509.IPAddress(ip.ip_address("192.0.2.1"))])
    names("names_ip_v6_only.pem", [], extra_san=[x509.IPAddress(ip.ip_address("2001:db8::1"))])
    names("names_dns_spelled_ip.pem", ["192.0.2.3", "::1", "[::1]"])
    names("names_ip_cn.pem", [], cn="192.0.2.4")
    names("names_cn_only.pem", [], cn="cn.example.com")
    names("names_cn_and_san.pem", ["san.example.com"], cn="cn.example.com")
    names("names_public_suffix.pem", ["*.example.co.uk"])
    names("names_underscore.pem", ["_dmarc.example.com", "under_score.example.com"])
    rows = []

    def expect(file, host, wanted):
        rows.append("%s\t%s\t%d" % (file, host, wanted))

    for host, wanted in [("www.example.com", 1), ("WWW.Example.COM", 1), ("www.example.com.", 1), ("example.com", 0),
                         ("a.b.example.com", 0), (".example.com", 0), ("..example.com", 0), ("", 0), (".", 0),
                         ("example.org", 1), ("EXAMPLE.org.", 1), ("sub.example.org", 0), ("exact.example.net", 1),
                         ("x.exact.example.net", 0), ("xexample.com", 0), ("www.example.com.evil", 0),
                         ("*.example.com", 0), ("ww w.example.com", 0), ("www.example.com\x00", 0)]:
        expect("names_wildcard.pem", host, wanted)
    for host in ["wx.example.com", "w.example.com", "foo.com", "a.b.example.com", "anything", "x.y.example.com",
                 "*.example.com", "a.*.example.com", "*", "foo.example.com", "afoo.example.com", "*foo.example.com", "com", "."]:
        expect("names_wildcard_misuse.pem", host, 0)
    for host, wanted in [("x.b.example.net", 1), ("X.B.EXAMPLE.NET", 1), ("b.example.net", 0), ("y.x.b.example.net", 0), ("x.example.net", 0)]:
        expect("names_deep_wildcard.pem", host, wanted)
    for host, wanted in [("trailing.example.io", 1), ("trailing.example.io.", 1), ("trailing.example.io..", 0), ("x.trailing.example.io", 0)]:
        expect("names_trailing_dot.pem", host, wanted)
    for host, wanted in [("xn--bcher-kva.example", 1), ("www.xn--bcher-kva.example", 1), ("bücher.example", 0), ("xn--bcher-kvb.example", 0)]:
        expect("names_idna.pem", host, wanted)
    # An IP-literal host matches an iPAddress entry only (RFC 6125 §6.4.4): by its
    # octets, so any spelling of the same address does, and no dNSName does, not
    # even one that spells the address (`192.0.2.2`).
    for host, wanted in [("192.0.2.1", 1), ("192.0.2.2", 0), ("192.0.2.3", 0), ("::1", 1), ("0:0:0:0:0:0:0:1", 1),
                         ("0000::0001", 1), ("2001:db8::7", 1), ("2001:DB8:0:0:0:0:0:7", 1), ("2001:db8::8", 0),
                         ("::2", 0), ("::ffff:192.0.2.1", 0), ("192.0.2.10", 0), ("192.0.2.1.", 0), ("[::1]", 0),
                         ("example.test", 1), ("1.2.3", 0), ("0x7f.1", 0), ("example.test.", 1), ("EXAMPLE.TEST", 1)]:
        expect("names_ip.pem", host, wanted)
    for host in ["example.test", "", "192.0.2.2", "192.0.2.10", "::2", "[::1]", "::ffff:192.0.2.1"]:
        expect("names_ip_only.pem", host, 0)
    expect("names_ip_only.pem", "192.0.2.1", 1)
    for host, wanted in [("2001:db8::1", 1), ("2001:0db8:0000:0000:0000:0000:0000:0001", 1), ("2001:db8::2", 0),
                         ("192.0.2.1", 0), ("::1", 0), ("::ffff:1.2.3.4", 0), ("example.test", 0)]:
        expect("names_ip_v6_only.pem", host, wanted)
    for host in ["192.0.2.3", "::1", "[::1]"]:
        expect("names_dns_spelled_ip.pem", host, 0)
    for host in ["192.0.2.4", "cn.invalid"]:
        expect("names_ip_cn.pem", host, 0)
    for host, wanted in [("san.example.com", 1), ("cn.example.com", 0), ("cn.invalid", 0)]:
        expect("names_cn_and_san.pem", host, wanted)
    for host, wanted in [("a.example.co.uk", 1), ("example.co.uk", 0), ("a.b.example.co.uk", 0)]:
        expect("names_public_suffix.pem", host, wanted)
    for host, wanted in [("_dmarc.example.com", 1), ("under_score.example.com", 1), ("under-score.example.com", 0)]:
        expect("names_underscore.pem", host, wanted)
    write("x509_hostnames.tsv", "\n".join(rows) + "\n")
    print("wrote %d mutants, %d hostname rows, %d semantics rows to %s" % (len(manifest), len(rows), len(semantics), out))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
