#!/usr/bin/env python3
"""A TLS 1.2 server that can misbehave on purpose: the oracle for `lib/tls.m31`'s
1.2 path.

  tls12_server.py MODE [AUTH [SUITE [GROUP]]]

  AUTH   p256 (default), p384 or rsa: the certificate's key
  SUITE  the cipher suite as hex (default c02b, c02f for rsa)
  GROUP  x25519 (default) or p256: the ECDHE group

Makes a throwaway key and self-signed certificate, listens on an ephemeral
loopback port, prints `READY <port> <SHA-256 of the SPKI, hex>` as its first
line (flushed), serves ONE connection, prints what it saw as `event: ...`
lines, and exits. Written from RFC 5246, 5288, 7627, 7905, 8422 and 5746 with
`hashlib`/`hmac` and the `cryptography` package -- nothing from this
repository -- so a client that completes a handshake with it has been checked
against an independent PRF, key block, record layer, Finished and
ServerKeyExchange signature.

The client under test must complete the handshake with the modes that are
right (`ok`, `ok_pkcs1*` and `ok_pss_sha512` with AUTH rsa, `ok_sha256_on_p384`,
`ok_sha384_on_p256`, `no_renegotiation_info`, `points_with_compressed`,
`wrong_explicit_nonce`, `finished_in_two_records`, `coalesce`, `split`,
`dribble`, `big`, `hello_request`, `close_notify`) and refuse the rest
(`warning_alert` included: every alert but close_notify is fatal). The
comment on each in `serve` says what is wrong with it. The `p256_*` modes
need GROUP p256.
"""
import hashlib
import hmac
import os
import socket
import struct
import sys
import datetime

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, padding, rsa, x25519
from cryptography.hazmat.primitives.ciphers.aead import AESGCM, ChaCha20Poly1305
from cryptography.x509.oid import NameOID

from oracle_tls_server import extension, handshake_message, parse_client_hello, say, sha256, TIMEOUT

SUITES = {
    0xCCA9: ("chacha", hashlib.sha256, 32, 12, "ecdsa"),
    0xCCA8: ("chacha", hashlib.sha256, 32, 12, "rsa"),
    0xC02B: ("gcm", hashlib.sha256, 16, 4, "ecdsa"),
    0xC02F: ("gcm", hashlib.sha256, 16, 4, "rsa"),
    0xC02C: ("gcm", hashlib.sha384, 32, 4, "ecdsa"),
    0xC030: ("gcm", hashlib.sha384, 32, 4, "rsa"),
}
GROUP_CODES = {"x25519": 0x001D, "p256": 0x0017}


def prf(hash_function, secret, label, seed, size):
    seed = label + seed
    out = b""
    a = seed
    while len(out) < size:
        a = hmac.new(secret, a, hash_function).digest()
        out += hmac.new(secret, a + seed, hash_function).digest()
    return out[:size]


class Direction:
    def __init__(self):
        self.active = False
        self.sequence = 0

    def install(self, kind, key, iv):
        self.kind = kind
        self.aead = AESGCM(key) if kind == "gcm" else ChaCha20Poly1305(key)
        self.iv = iv
        self.sequence = 0
        self.active = True


class Peer:
    def __init__(self, connection):
        self.connection = connection
        self.read = Direction()
        self.write = Direction()
        self.dribble = False

    def send_raw(self, data):
        if self.dribble:
            for index in range(len(data)):
                self.connection.sendall(data[index:index + 1])
        else:
            self.connection.sendall(data)

    def send_plain(self, content_type, content):
        self.send_raw(struct.pack(">BHH", content_type, 0x0303, len(content)) + content)

    def seal(self, content_type, content, explicit=None):
        state = self.write
        additional = struct.pack(">QBHH", state.sequence, content_type, 0x0303, len(content))
        if state.kind == "gcm":
            explicit = struct.pack(">Q", state.sequence) if explicit is None else explicit
            body = explicit + state.aead.encrypt(state.iv + explicit, content, additional)
        else:
            nonce = bytes(a ^ b for a, b in zip(state.iv, bytes(4) + struct.pack(">Q", state.sequence)))
            body = state.aead.encrypt(nonce, content, additional)
        state.sequence += 1
        return struct.pack(">BHH", content_type, 0x0303, len(body)) + body

    def send(self, content_type, content, flip_bit=False, explicit=None):
        record = self.seal(content_type, content, explicit)
        if flip_bit:
            record = record[:-1] + bytes([record[-1] ^ 1])
        self.send_raw(record)

    def receive_exactly(self, count):
        data = b""
        while len(data) < count:
            chunk = self.connection.recv(count - len(data))
            if not chunk:
                raise EOFError("the client closed the connection")
            data += chunk
        return data

    def receive(self):
        header = self.receive_exactly(5)
        content_type, _version, length = struct.unpack(">BHH", header)
        body = self.receive_exactly(length)
        state = self.read
        if not state.active or content_type == 20:
            return content_type, body
        if state.kind == "gcm":
            explicit, sealed = body[:8], body[8:]
            plain_length = len(sealed) - 16
            additional = struct.pack(">QBHH", state.sequence, content_type, 0x0303, plain_length)
            plain = state.aead.decrypt(state.iv + explicit, sealed, additional)
        else:
            nonce = bytes(a ^ b for a, b in zip(state.iv, bytes(4) + struct.pack(">Q", state.sequence)))
            additional = struct.pack(">QBHH", state.sequence, content_type, 0x0303, length - 16)
            plain = state.aead.decrypt(nonce, body, additional)
        state.sequence += 1
        return content_type, plain


EXTRA_CHAIN = []
STAPLE = []


def make_certificate(auth):
    if os.environ.get("OCSP_CHAIN"):
        # A real chain and the response to staple, for the stapling modes: the
        # certificate file is the leaf then its issuer.
        blocks = [b + b"-----END CERTIFICATE-----\n" for b in open(os.environ["OCSP_CHAIN"], "rb").read().split(b"-----END CERTIFICATE-----") if b"BEGIN" in b]
        ders = [x509.load_pem_x509_certificate(b).public_bytes(serialization.Encoding.DER) for b in blocks]
        EXTRA_CHAIN.extend(ders[1:])
        if os.environ.get("OCSP_RESPONSE"):
            STAPLE.append(open(os.environ["OCSP_RESPONSE"], "rb").read())
        key = serialization.load_pem_private_key(open(os.environ["OCSP_KEY"], "rb").read(), None)
        spki = key.public_key().public_bytes(serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)
        return key, ders[0], sha256(spki)
    if auth == "rsa":
        key = rsa.generate_private_key(65537, 2048)
    else:
        key = ec.generate_private_key(ec.SECP256R1() if auth == "p256" else ec.SECP384R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    now = datetime.datetime.now(datetime.timezone.utc)
    certificate = (
        x509.CertificateBuilder()
        .subject_name(name)
        .issuer_name(name)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(days=1))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), critical=False)
        .sign(key, hashes.SHA256())
    )
    der = certificate.public_bytes(serialization.Encoding.DER)
    spki = key.public_key().public_bytes(serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)
    return key, der, sha256(spki)


def sign(key, auth, scheme, content):
    """Sign `content` the way `scheme` says, with `key`."""
    hashes_by_scheme = {0x0403: hashes.SHA256(), 0x0503: hashes.SHA384(), 0x0804: hashes.SHA256(),
                        0x0805: hashes.SHA384(), 0x0806: hashes.SHA512(), 0x0401: hashes.SHA256(),
                        0x0501: hashes.SHA384(), 0x0601: hashes.SHA512()}
    algorithm = hashes_by_scheme[scheme]
    if auth == "rsa":
        if 0x0804 <= scheme <= 0x0806:
            return key.sign(content, padding.PSS(padding.MGF1(algorithm), algorithm.digest_size), algorithm)
        return key.sign(content, padding.PKCS1v15(), algorithm)
    return key.sign(content, ec.ECDSA(algorithm))


def certificate_message(der, tls13_format=False):
    """RFC 5246 §7.4.2's Certificate; `tls13_format` is RFC 8446's instead (context, entry extensions)."""
    entry = len(der).to_bytes(3, "big") + der
    for extra in EXTRA_CHAIN:
        entry += len(extra).to_bytes(3, "big") + extra
    if tls13_format:
        entry += b"\x00\x00"
        return handshake_message(11, b"\x00" + len(entry).to_bytes(3, "big") + entry)
    return handshake_message(11, len(entry).to_bytes(3, "big") + entry)


def serve(peer, mode, auth, suite, group, key, certificate_der):
    kind, content = peer.receive()
    assert kind == 22, "expected a ClientHello"
    client_hello = content
    client_random, session_id, suites, extensions = parse_client_hello(client_hello)
    by_kind = {kind: data for kind, data in extensions}
    say("event: offered suites=%s extensions=%s" % (
        ",".join("%04x" % s for s in suites), ",".join(str(kind) for kind, _ in extensions)))
    groups = [struct.unpack(">H", by_kind[10][2 + i:4 + i])[0] for i in range(0, len(by_kind[10]) - 2, 2)]
    sigalgs = [struct.unpack(">H", by_kind[13][2 + i:4 + i])[0] for i in range(0, len(by_kind[13]) - 2, 2)]
    say("event: groups=%s sigalgs=%s versions=%s ec_point_formats=%s renegotiation_info=%s" % (
        ",".join("%04x" % g for g in groups), ",".join("%04x" % s for s in sigalgs), by_kind[43][1:].hex(),
        by_kind[11].hex(), by_kind[0xFF01].hex()))
    assert 23 in by_kind and by_kind[23] == b"", "no extended_master_secret offered"
    say("event: session_id_length=%d" % len(session_id))
    assert suite in suites, "the suite was not offered"

    cipher_kind, hash_function, key_size, iv_size, _ = SUITES[suite]
    server_random = os.urandom(32)
    hello_session_id = os.urandom(32)

    # ---- ServerHello ----
    hello_extensions = [extension(0xFF01, b"\x00"), extension(23, b""), extension(11, b"\x01\x00")]
    chosen_suite = suite
    legacy_version = 0x0303
    if mode.startswith("staple_") and mode not in ("staple_unpromised",):
        # The stapling modes: the server answers the client's status_request, with the
        # empty extension RFC 6066 §8 says, or (staple_ext_data) with data it must not carry.
        hello_extensions.append(extension(5, b"\x00" if mode == "staple_ext_data" else b""))
    if mode == "no_renegotiation_info":
        hello_extensions.pop(0)  # RFC 5746 allows a server that has no support
    if mode == "no_ems":
        hello_extensions.pop(1)  # extended_master_secret absent: refused
    if mode == "ems_data":
        hello_extensions[1] = extension(23, b"\x00")  # not empty
    if mode == "renego_nonempty":
        hello_extensions[0] = extension(0xFF01, b"\x01\x00")  # claims a previous handshake
    if mode == "renego_empty_vector":
        hello_extensions[0] = extension(0xFF01, b"")
    if mode == "downgrade":
        server_random = os.urandom(24) + b"DOWNGRD\x01"
    if mode == "downgrade_tls11":
        server_random = os.urandom(24) + b"DOWNGRD\x00"
    if mode == "suite_tls13":
        chosen_suite = 0x1301  # a TLS 1.3 code point in a 1.2 hello
    if mode == "suite_cbc":
        chosen_suite = 0xC013  # TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA, never offered
    if mode == "echo_session_id":
        hello_session_id = session_id
    if mode == "long_session_id":
        hello_session_id = os.urandom(33)
    if mode == "ext_unoffered":
        hello_extensions.append(extension(16, b"\x00\x03\x02h2"))
    if mode == "ext_duplicate":
        hello_extensions.append(extension(23, b""))
    if mode == "ext_sni_data":
        hello_extensions.append(extension(0, b"\x00\x03\x00\x00\x00"))
    if mode == "ext_sni_empty":
        hello_extensions.append(extension(0, b""))  # allowed: server acknowledges the name
    if mode == "ext_session_ticket":
        hello_extensions.append(extension(35, b""))  # never offered
    if mode == "points_no_uncompressed":
        hello_extensions[2] = extension(11, b"\x01\x01")
    if mode == "points_with_compressed":
        hello_extensions[2] = extension(11, b"\x02\x01\x00")  # lists uncompressed too: fine
    if mode == "version_tls11":
        legacy_version = 0x0302
    if mode == "version_tls10":
        legacy_version = 0x0301
    if mode == "version_ssl3":
        legacy_version = 0x0300
    if mode == "version_future":
        legacy_version = 0x0304  # a 1.3-looking legacy_version with no supported_versions
    compression = 1 if mode == "compression" else 0
    body = (struct.pack(">H", legacy_version) + server_random + bytes([len(hello_session_id)]) + hello_session_id
            + struct.pack(">HB", chosen_suite, compression)
            + struct.pack(">H", sum(len(e) for e in hello_extensions)) + b"".join(hello_extensions))
    if mode == "hello_trailing":
        body += b"\x00"
    if mode == "hello_no_extensions":
        body = (struct.pack(">H", legacy_version) + server_random + bytes([len(hello_session_id)]) + hello_session_id
                + struct.pack(">HB", chosen_suite, 0))
    server_hello = handshake_message(2, body)
    peer.send_plain(22, server_hello)
    if mode in ("downgrade", "downgrade_tls11", "suite_tls13", "suite_cbc", "echo_session_id", "long_session_id",
                "ext_unoffered", "ext_duplicate", "ext_sni_data", "ext_sni_empty", "ext_session_ticket", "points_no_uncompressed",
                "version_tls11", "version_tls10", "version_ssl3", "version_future", "compression", "no_ems",
                "ems_data", "renego_nonempty", "renego_empty_vector", "hello_trailing", "hello_no_extensions",
                "staple_ext_data"):
        report_next(peer)
        return
    transcript = client_hello + server_hello

    # ---- Certificate, ServerKeyExchange, ServerHelloDone ----
    flight = []
    certificate = certificate_message(certificate_der)
    if mode == "cert_13_format":
        certificate = certificate_message(certificate_der, tls13_format=True)
    if mode == "cert_empty":
        certificate = handshake_message(11, b"\x00\x00\x00")
    flight.append(certificate)

    if group == "x25519":
        private = x25519.X25519PrivateKey.generate()
        point = private.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    else:
        private = ec.generate_private_key(ec.SECP256R1())
        point = private.public_key().public_bytes(serialization.Encoding.X962, serialization.PublicFormat.UncompressedPoint)
    group_code = GROUP_CODES[group]
    if mode == "zero_x25519":
        point = bytes(32)
    if mode == "x25519_short":
        point = point[:31]
    if mode == "x25519_long":
        point = point + b"\x00"
    if mode == "p256_off_curve":
        point = point[:-1] + bytes([point[-1] ^ 1])
    if mode == "p256_compressed":
        point = private.public_key().public_bytes(serialization.Encoding.X962, serialization.PublicFormat.CompressedPoint)
    if mode == "p256_infinity":
        point = b"\x00"
    if mode == "p256_wrong_prefix":
        point = b"\x05" + point[1:]
    if mode == "p256_not_reduced":
        prime = 0xffffffff00000001000000000000000000000000ffffffffffffffffffffffff
        x = int.from_bytes(point[1:33], "big") + prime
        point = b"\x04" + (x % (1 << 256)).to_bytes(32, "big") + point[33:]
    if mode == "group_p384":
        group_code = 0x0018
    if mode == "group_unknown":
        group_code = 0x0100
    curve_type = 3
    if mode == "curve_explicit_prime":
        curve_type = 1
    if mode == "curve_explicit_char2":
        curve_type = 2
    params = bytes([curve_type]) + struct.pack(">H", group_code) + bytes([len(point)]) + point

    scheme = {"p256": 0x0403, "p384": 0x0503, "rsa": 0x0804}[auth]
    if mode == "ok_pkcs1":
        scheme = 0x0401
    if mode == "ok_pkcs1_sha512":
        scheme = 0x0601
    if mode == "ok_pss_sha512":
        scheme = 0x0806
    if mode == "ok_sha256_on_p384":
        scheme = 0x0403  # 1.2 pairs the hash with whatever curve the key is on
    if mode == "ok_sha384_on_p256":
        scheme = 0x0503
    signed = client_random + server_random + params
    if mode == "sign_swapped_randoms":
        signed = server_random + client_random + params
    if mode == "sign_without_randoms":
        signed = params
    signature = sign(key, auth, scheme, signed)
    if mode == "bad_signature":
        signature = signature[:-1] + bytes([signature[-1] ^ 1])
    if mode == "signature_truncated":
        signature = signature[:-4]
    if mode == "signature_empty":
        signature = b""
    if mode == "scheme_ed25519":
        scheme = 0x0807
    if mode == "scheme_sha1":
        scheme = 0x0201 if auth == "rsa" else 0x0203
    if mode == "scheme_md5":
        scheme = 0x0101
    if mode == "scheme_rsa_on_ec":
        scheme = 0x0401 if auth != "rsa" else 0x0403  # RSA scheme for an EC key, and the reverse
    if mode == "scheme_ecdsa_on_rsa_suite":
        scheme = 0x0403
    if mode == "scheme_pss_on_ecdsa_suite":
        scheme = 0x0804
    if mode == "scheme_unknown":
        scheme = 0xFFFF
    exchange_body = params + struct.pack(">HH", scheme, len(signature)) + signature
    if mode == "exchange_trailing":
        exchange_body += b"\x00"
    if mode == "exchange_truncated":
        exchange_body = exchange_body[:-3]
    server_key_exchange = handshake_message(12, exchange_body)
    flight.append(server_key_exchange)
    if mode == "cert_request":
        flight.append(handshake_message(13, b"\x01\x40\x00\x04\x04\x03\x08\x04\x00\x00"))
    if mode == "cert_status":
        flight.insert(1, handshake_message(22, b"\x01\x00\x00\x00"))
    if mode.startswith("staple_") and STAPLE:
        response = STAPLE[0]
        sized = len(response).to_bytes(3, "big") + response
        status = {
            "staple_good": b"\x01" + sized,
            "staple_bad": b"\x01" + sized,
            "staple_unpromised": b"\x01" + sized,
            "staple_late": b"\x01" + sized,
            "staple_twice": b"\x01" + sized,
            "staple_badtype": b"\x02" + sized,
            "staple_empty": b"\x01\x00\x00\x00",
            "staple_trailing": b"\x01" + sized + b"\x00",
            "staple_truncated": b"\x01" + (len(response) + 50).to_bytes(3, "big") + response,
        }.get(mode)
        if status is not None:
            message = handshake_message(22, status)
            if mode == "staple_late":
                flight.insert(2, message)
            elif mode == "staple_twice":
                flight.insert(1, message)
                flight.insert(1, message)
            else:
                flight.insert(1, message)
    hello_done = handshake_message(14, b"\x00" if mode == "done_body" else b"")
    if mode == "empty_done_record":
        hello_done = b""
    flight.append(hello_done)
    if mode == "done_before_exchange":
        flight = [certificate, hello_done, server_key_exchange]
    if mode == "no_exchange":
        flight = [certificate, hello_done]
    if mode == "exchange_before_certificate":
        flight = [server_key_exchange, certificate, hello_done]
    if mode == "coalesce":
        peer.send_plain(22, b"".join(flight))
    elif mode == "split":
        stream = b"".join(flight)
        for start in range(0, len(stream), 7):
            peer.send_plain(22, stream[start:start + 7])
    elif mode == "dribble":
        peer.dribble = True
        for message in flight:
            peer.send_plain(22, message)
        peer.dribble = False
    else:
        for message in flight:
            peer.send_plain(22, message)
    if mode in ("cert_13_format", "cert_empty", "cert_request", "cert_status", "done_body", "empty_done_record",
                "done_before_exchange", "no_exchange", "exchange_before_certificate",
                "staple_bad", "staple_unpromised", "staple_late", "staple_twice", "staple_badtype", "staple_empty",
                "staple_trailing", "staple_truncated"):
        report_next(peer)
        return
    transcript += b"".join(flight)
    if mode in ("zero_x25519", "x25519_short", "x25519_long", "p256_off_curve", "p256_compressed", "p256_infinity",
                "p256_wrong_prefix", "p256_not_reduced", "group_p384", "group_unknown", "curve_explicit_prime",
                "curve_explicit_char2", "bad_signature", "signature_truncated", "signature_empty", "scheme_ed25519",
                "scheme_sha1", "scheme_md5", "scheme_rsa_on_ec", "scheme_ecdsa_on_rsa_suite",
                "scheme_pss_on_ecdsa_suite", "scheme_unknown", "sign_swapped_randoms", "sign_without_randoms",
                "exchange_trailing", "exchange_truncated"):
        report_next(peer)
        return

    # ---- the client's flight: ClientKeyExchange, ChangeCipherSpec, Finished ----
    kind, content = peer.receive()
    if kind == 21:
        say("event: client alert %d" % content[1])
        return
    assert kind == 22 and content[0] == 16, "expected a ClientKeyExchange, got type %d" % kind
    client_key_exchange = content
    client_point = client_key_exchange[5:5 + client_key_exchange[4]]
    assert len(client_key_exchange) == 5 + len(client_point)
    say("event: client key exchange %d octets" % len(client_point))
    if group == "x25519":
        assert len(client_point) == 32
        pre_master = private.exchange(x25519.X25519PublicKey.from_public_bytes(client_point))
    else:
        assert len(client_point) == 65 and client_point[0] == 4
        pre_master = private.exchange(ec.ECDH(), ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), client_point))
    transcript += client_key_exchange
    master = prf(hash_function, pre_master, b"extended master secret", hash_function(transcript).digest(), 48)
    block = prf(hash_function, master, b"key expansion", server_random + client_random, 2 * key_size + 2 * iv_size)
    client_key, server_key = block[:key_size], block[key_size:2 * key_size]
    client_iv, server_iv = block[2 * key_size:2 * key_size + iv_size], block[2 * key_size + iv_size:]
    kind, content = peer.receive()
    if kind == 21:
        say("event: client alert %d" % content[1])
        return
    assert kind == 20 and content == b"\x01", "expected ChangeCipherSpec"
    peer.read.install(cipher_kind, client_key, client_iv)
    kind, content = peer.receive()
    assert kind == 22 and content[0] == 20, "expected the client's Finished"
    expected = prf(hash_function, master, b"client finished", hash_function(transcript).digest(), 12)
    say("event: client_finished " + ("ok" if content[4:] == expected else "BAD"))
    transcript += content

    # ---- the server's ChangeCipherSpec and Finished ----
    verify_data = prf(hash_function, master, b"server finished", hash_function(transcript).digest(), 12)
    if mode == "bad_finished":
        verify_data = verify_data[:-1] + bytes([verify_data[-1] ^ 1])
    if mode == "finished_long":
        verify_data = prf(hash_function, master, b"server finished", hash_function(transcript).digest(), 32)
    if mode == "finished_short":
        verify_data = verify_data[:11]
    if mode == "finished_client_label":
        verify_data = prf(hash_function, master, b"client finished", hash_function(transcript).digest(), 12)
    if mode == "finished_without_client_finished":
        verify_data = prf(hash_function, master, b"server finished", hash_function(transcript[:-len(content)]).digest(), 12)
    if mode == "finished_unprotected":
        peer.send_plain(20, b"\x01")
        peer.send_plain(22, handshake_message(20, verify_data))
        report_next(peer)
        return
    if mode == "no_ccs":
        peer.send_plain(22, handshake_message(20, verify_data))
        report_next(peer)
        return
    if mode == "ccs_body":
        peer.send_plain(20, b"\x02")
        report_next(peer)
        return
    if mode == "ccs_two_octets":
        peer.send_plain(20, b"\x01\x01")
        report_next(peer)
        return
    peer.send_plain(20, b"\x01")
    peer.write.install(cipher_kind, server_key, server_iv)
    if mode == "alert_instead_of_finished":
        peer.send(21, b"\x02\x28")
        report_next(peer)
        return
    if mode == "bad_record":
        peer.send(22, handshake_message(20, verify_data), flip_bit=True)
        report_next(peer)
        return
    if mode == "wrong_explicit_nonce" and cipher_kind == "gcm":
        peer.send(22, handshake_message(20, verify_data), explicit=struct.pack(">Q", 7))
        report_next(peer)
        return
    if mode == "short_record":
        peer.send_plain(22, os.urandom(10))
        report_next(peer)
        return
    if mode == "finished_in_two_records":
        message = handshake_message(20, verify_data)
        peer.send(22, message[:5])
        peer.send(22, message[5:])
    elif mode == "finished_empty_record_first":
        peer.send(22, b"")
        peer.send(22, handshake_message(20, verify_data))
    elif mode == "app_data_before_finished":
        peer.send(23, b"early")
        peer.send(22, handshake_message(20, verify_data))
    else:
        peer.send(22, handshake_message(20, verify_data))
    if mode in ("bad_finished", "finished_long", "finished_short", "finished_client_label",
                "finished_without_client_finished", "finished_empty_record_first",
                "app_data_before_finished"):
        report_next(peer)
        return
    say("event: handshake complete")
    after_handshake(peer, mode)


def report_next(peer):
    try:
        kind, content = peer.receive()
        while kind == 20:
            kind, content = peer.receive()
    except (EOFError, ConnectionError):
        say("event: client closed")
        return
    except Exception as error:
        say("event: unreadable %s" % type(error).__name__)
        return
    if kind == 21:
        say("event: client alert %d" % content[1])
    else:
        say("event: client sent type %d" % kind)


def read_line(peer):
    """Application data up to a newline; None at the client's close_notify or end."""
    data = b""
    while b"\n" not in data:
        try:
            kind, content = peer.receive()
        except EOFError:
            say("event: client closed without close_notify")
            return None
        if kind == 21:
            say("event: client alert %d level %d" % (content[1], content[0]))
            if content[0] == 2 or content[1] == 0:
                return None
            continue
        assert kind == 23, "unexpected record type %d" % kind
        data += content
    return data


def close_notify(peer):
    peer.send(21, b"\x01\x00")
    say("event: sent close_notify")


def after_handshake(peer, mode):
    if mode == "peer_alert":
        peer.send(21, b"\x02\x50")
        return
    if mode == "warning_alert":
        peer.send(21, b"\x01\x64")  # a warning the client may ignore
        say("event: sent warning")
    if mode == "hello_request":
        peer.send(22, handshake_message(0, b""))
        say("event: sent HelloRequest")
    if mode == "unexpected_handshake":
        peer.send(22, handshake_message(4, struct.pack(">IH", 60, 0) + b"\x00\x00"))
        report_next(peer)
        return
    if mode == "replay_record":
        record = peer.seal(23, b"hello\n")
        peer.write.sequence -= 1
        peer.send_raw(record)
        peer.send_raw(record)
        report_next(peer)
        return
    if mode == "truncate":
        read_line(peer)
        peer.send(23, b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n0123456789")
        say("event: closing without close_notify")
        peer.connection.shutdown(socket.SHUT_RDWR)
        return
    if mode == "big":
        read_line(peer)
        payload = bytes((index * 7) & 0xFF for index in range(100_000))
        for start in range(0, len(payload), 16384):
            peer.send(23, payload[start:start + 16384])
        say("event: sent %d octets" % len(payload))
        close_notify(peer)
        return
    if mode == "oversized_record":
        read_line(peer)
        peer.send(23, bytes(16384 + 1))
        report_next(peer)
        return
    if mode == "close_notify":
        read_line(peer)
        peer.send(23, b"all of it")
        close_notify(peer)
        return
    while True:
        line = read_line(peer)
        if line is None:
            break
        say("event: line %r" % line)
        if line.startswith(b"GET "):
            body = b"hello over tls\n"
            peer.send(23, b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body) + body)
            break
        peer.send(23, b"echo: " + line)
    close_notify(peer)


def main():
    mode = sys.argv[1]
    auth = sys.argv[2] if len(sys.argv) > 2 else "p256"
    default_suite = 0xC02F if auth == "rsa" else 0xC02B
    suite = int(sys.argv[3], 16) if len(sys.argv) > 3 else default_suite
    group = sys.argv[4] if len(sys.argv) > 4 else "x25519"
    key, certificate_der, pin = make_certificate(auth)
    listener = socket.socket(socket.AF_INET)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(TIMEOUT)
    say("READY %d %s" % (listener.getsockname()[1], pin.hex()))
    try:
        connection, _address = listener.accept()
    except socket.timeout:
        say("event: nobody connected")
        return 0
    connection.settimeout(TIMEOUT)
    connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    peer = Peer(connection)
    try:
        serve(peer, mode, auth, suite, group, key, certificate_der)
    except (EOFError, ConnectionError, socket.timeout) as error:
        say("event: connection ended: %s" % type(error).__name__)
    except Exception as error:
        say("event: server error: %s %s" % (type(error).__name__, error))
        return 1
    finally:
        try:
            connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        connection.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
