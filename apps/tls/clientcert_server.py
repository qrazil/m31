#!/usr/bin/env python3
"""A TLS 1.3 server that asks the client for a certificate, checks what comes
back with the `cryptography` package, and can misbehave on purpose: the oracle
for the client-certificate half of `lib/tls.m31`.

  clientcert_server.py MODE[,MODE...] [BIND_ADDRESS]

One MODE per connection, served in order; exits after the last. Prints
`READY <port> <SHA-256 of the SPKI, hex>`, then `event: N ...` lines. Written
from RFC 8446 §4.3.2, §4.4.2-§4.4.4 with `hashlib`/`hmac`/`cryptography` and
helpers from `oracle_tls_server.py` -- nothing from this repository -- so
`verify=ok` and `client_finished ok` are an independent implementation's word
that the client's Certificate, CertificateVerify (content, scheme, signature)
and Finished-after-client-auth are right.

MODEs (request = the CertificateRequest the server sends after EncryptedExtensions):
  good         signature_algorithms [ecdsa_secp256r1_sha256, ed25519, rsa_pss_rsae_sha256]
  p256only     [ecdsa_secp256r1_sha256]
  ed25519only  [ed25519]
  nomatch      [rsa_pss_rsae_sha256, rsa_pkcs1_sha256]: nothing an EC/Ed25519 key signs
  cas          good, plus certificate_authorities naming some other CA
  none         no CertificateRequest at all
  ctx          a non-empty certificate_request_context
  nosigalgs    extensions without signature_algorithms
  dupext       signature_algorithms twice
  trailing     an octet after the extensions
  truncated    an extension longer than the message
  oddlist      a signature_algorithms list of odd length
  emptylist    an empty signature_algorithms list
  aftercert    the request after the server's Certificate (not allowed there)
  twice        two requests in a row
"""
import os
import socket
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle_tls_server as base  # noqa: E402
import hashlib  # noqa: E402
import hmac  # noqa: E402
from cryptography import x509 as cx509  # noqa: E402
from cryptography.hazmat.primitives import hashes, serialization  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import ec, ed25519, x25519  # noqa: E402

say = base.say
sha256 = base.sha256
extension = base.extension
handshake_message = base.handshake_message
expand_label = base.expand_label
derive_secret = base.derive_secret
hkdf_extract = base.hkdf_extract

ECDSA_P256 = 0x0403
ED25519 = 0x0807
PSS_SHA256 = 0x0804
PKCS1_SHA256 = 0x0401


def hmac_sha256(key, data):
    return hmac.new(key, data, hashlib.sha256).digest()


def u16_list(values):
    return struct.pack(">H", 2 * len(values)) + b"".join(struct.pack(">H", v) for v in values)


def request_body(mode):
    schemes = {"p256only": [ECDSA_P256], "ed25519only": [ED25519], "nomatch": [PSS_SHA256, PKCS1_SHA256]}.get(
        mode, [ECDSA_P256, ED25519, PSS_SHA256])
    context = b"\x01\xAA" if mode == "ctx" else b"\x00"
    sigalgs = extension(13, u16_list(schemes))
    extensions = sigalgs
    if mode == "nosigalgs":
        extensions = extension(47, struct.pack(">H", 0))
    if mode == "dupext":
        extensions = sigalgs + sigalgs
    if mode == "cas":
        name = b"\x30\x0f\x31\x0d\x30\x0b\x06\x03\x55\x04\x03\x0c\x04nope"
        extensions = sigalgs + extension(47, struct.pack(">H", len(name)) + name)
    if mode == "oddlist":
        extensions = extension(13, struct.pack(">H", 3) + b"\x04\x03\x08")
    if mode == "emptylist":
        extensions = extension(13, struct.pack(">H", 0))
    body = context + struct.pack(">H", len(extensions)) + extensions
    if mode == "trailing":
        body += b"\x00"
    if mode == "truncated":
        body = context + struct.pack(">H", 4) + struct.pack(">HH", 13, 200)
    return body


class Handshakes:
    """Client handshake messages out of the (decrypted) record stream."""

    def __init__(self, peer):
        self.peer = peer
        self.buffer = b""

    def next(self):
        while len(self.buffer) < 4 or len(self.buffer) < 4 + int.from_bytes(self.buffer[1:4], "big"):
            kind, content = self.peer.receive()
            if kind == 21:
                return "alert", content
            if kind == 20:
                continue
            assert kind == 22, "expected handshake, got record type %d" % kind
            self.buffer += content
        length = 4 + int.from_bytes(self.buffer[1:4], "big")
        message, self.buffer = self.buffer[:length], self.buffer[length:]
        return "message", message


def parse_certificates(message):
    body = message[4:]
    assert body[0] == 0, "request context in a handshake Certificate must be empty"
    total = int.from_bytes(body[1:4], "big")
    cursor, certificates = 4, []
    assert 4 + total == len(body), "Certificate list length"
    while cursor < len(body):
        length = int.from_bytes(body[cursor:cursor + 3], "big")
        certificates.append(body[cursor + 3:cursor + 3 + length])
        extensions_length = struct.unpack(">H", body[cursor + 3 + length:cursor + 5 + length])[0]
        assert extensions_length == 0, "per-certificate extensions"
        cursor += 5 + length + extensions_length
    return certificates


def serve_one(peer, number, mode, key, certificate_der):
    kind, client_hello = peer.receive()
    assert kind == 22, "expected a ClientHello"
    _random, session_id, _suites, extensions = base.parse_client_hello(client_hello)
    by_kind = {kind: data for kind, data in extensions}
    client_public = by_kind[51][6:38]
    private = x25519.X25519PrivateKey.generate()
    public = private.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    shared = private.exchange(x25519.X25519PublicKey.from_public_bytes(client_public))

    hello_extensions = [extension(43, b"\x03\x04"), extension(51, struct.pack(">HH", 0x001D, 32) + public)]
    body = (struct.pack(">H", 0x0303) + os.urandom(32) + bytes([len(session_id)]) + session_id
            + struct.pack(">HB", 0x1303, 0) + struct.pack(">H", sum(len(e) for e in hello_extensions))
            + b"".join(hello_extensions))
    server_hello = handshake_message(2, body)
    peer.send_plain(22, server_hello)
    peer.send_plain(20, b"\x01")

    transcript = client_hello + server_hello
    early = hkdf_extract(bytes(32), bytes(32))
    handshake_secret = hkdf_extract(derive_secret(early, b"derived", sha256(b"")), shared)
    client_handshake = derive_secret(handshake_secret, b"c hs traffic", sha256(transcript))
    server_handshake = derive_secret(handshake_secret, b"s hs traffic", sha256(transcript))
    peer.write.install(server_handshake)
    peer.read.install(client_handshake)

    def send(message):
        nonlocal transcript
        peer.send(22, message)
        transcript += message

    send(handshake_message(8, b"\x00\x00"))
    requested = mode != "none"
    request = handshake_message(13, request_body(mode))
    if requested and mode != "aftercert":
        send(request)
        if mode == "twice":
            send(request)
    entry = len(certificate_der).to_bytes(3, "big") + certificate_der + b"\x00\x00"
    send(handshake_message(11, b"\x00" + len(entry).to_bytes(3, "big") + entry))
    if mode == "aftercert":
        send(request)
    context = b" " * 64 + b"TLS 1.3, server CertificateVerify\x00" + sha256(transcript)
    signature = key.sign(context, ec.ECDSA(hashes.SHA256()))
    send(handshake_message(15, struct.pack(">HH", ECDSA_P256, len(signature)) + signature))
    verify_data = hmac_sha256(expand_label(server_handshake, b"finished", b"", 32), sha256(transcript))
    send(handshake_message(20, verify_data))

    master = hkdf_extract(derive_secret(handshake_secret, b"derived", sha256(b"")), bytes(32))
    client_application = derive_secret(master, b"c ap traffic", sha256(transcript))
    server_application = derive_secret(master, b"s ap traffic", sha256(transcript))

    reader = Handshakes(peer)
    if requested:
        what, message = reader.next()
        if what == "alert":
            say("event: %d client alert %d" % (number, message[1]))
            return
        assert message[0] == 11, "expected the client's Certificate, got type %d" % message[0]
        certificates = parse_certificates(message)
        say("event: %d client_certificates count=%d" % (number, len(certificates)))
        transcript += message
        if certificates:
            leaf = cx509.load_der_x509_certificate(certificates[0])
            say("event: %d client_leaf subject=%s" % (number, leaf.subject.rfc4514_string()))
            say("event: %d client_leaf fingerprint=%s" % (number, leaf.fingerprint(hashes.SHA256()).hex()))
            what, verify = reader.next()
            if what == "alert":
                say("event: %d client alert %d" % (number, verify[1]))
                return
            assert verify[0] == 15, "expected CertificateVerify, got type %d" % verify[0]
            scheme = struct.unpack(">H", verify[4:6])[0]
            length = struct.unpack(">H", verify[6:8])[0]
            signature = verify[8:8 + length]
            assert 8 + length == len(verify), "CertificateVerify has trailing octets"
            content = b" " * 64 + b"TLS 1.3, client CertificateVerify\x00" + sha256(transcript)
            public_key = leaf.public_key()
            try:
                if scheme == ECDSA_P256:
                    public_key.verify(signature, content, ec.ECDSA(hashes.SHA256()))
                elif scheme == ED25519:
                    public_key.verify(signature, content)
                else:
                    raise ValueError("unexpected scheme")
                outcome = "ok"
            except Exception:
                outcome = "BAD"
            say("event: %d client_verify scheme=0x%04x verify=%s" % (number, scheme, outcome))
            transcript += verify
        else:
            say("event: %d no client certificate sent" % number)
    what, finished = reader.next()
    if what == "alert":
        say("event: %d client alert %d" % (number, finished[1]))
        return
    assert finished[0] == 20, "expected the client's Finished"
    expected = hmac_sha256(expand_label(client_handshake, b"finished", b"", 32), sha256(transcript))
    say("event: %d client_finished %s" % (number, "ok" if finished[4:] == expected else "BAD"))
    peer.write.install(server_application)
    peer.read.install(client_application)
    say("event: %d handshake complete" % number)
    base.after_handshake(peer, "plain")


def main():
    modes = sys.argv[1].split(",")
    bind_host = sys.argv[2] if len(sys.argv) > 2 else "127.0.0.1"
    key, certificate_der, pin = base.make_certificate("p256")
    listener = socket.socket(socket.AF_INET6 if ":" in bind_host else socket.AF_INET)
    listener.bind((bind_host, 0))
    listener.listen(4)
    listener.settimeout(base.TIMEOUT)
    say("READY %d %s" % (listener.getsockname()[1], pin.hex()))
    for number, mode in enumerate(modes, 1):
        try:
            connection, _address = listener.accept()
        except socket.timeout:
            say("event: nobody connected")
            return 0
        connection.settimeout(base.TIMEOUT)
        connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        peer = base.Peer(connection)
        try:
            serve_one(peer, number, mode, key, certificate_der)
        except (EOFError, ConnectionError, socket.timeout) as error:
            say("event: %d connection ended: %s" % (number, type(error).__name__))
        except Exception as error:
            say("event: %d server error: %s %s" % (number, type(error).__name__, error))
        finally:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            connection.close()
    say("event: done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
