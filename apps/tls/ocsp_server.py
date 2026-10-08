#!/usr/bin/env python3
"""A TLS 1.3 server that staples an OCSP response, and can staple it wrongly:
the oracle for the stapling half of `lib/tls.m31`.

  ocsp_server.py CHAIN.pem KEY.pem MODE[:RESPONSE.der][,MODE[:RESPONSE.der]...]

CHAIN.pem is the server's certificate then its issuer; KEY.pem is the leaf's
P-256 key. One MODE per connection, served in order; exits after the last.
Prints `READY <port>`, then `event: N ...` lines. Written from RFC 8446 §4.4.2.1
and RFC 6066 §8 with `hashlib`/`hmac`/`cryptography` and the helpers of
`oracle_tls_server.py` -- nothing from this repository.

MODEs (the RESPONSE is the DER OCSPResponse that goes in the staple):
  good        status_request in the leaf's CertificateEntry: CertificateStatus
              { ocsp(1), RESPONSE }
  none        no staple
  badtype     status_type 2
  empty       a zero-length response
  trailing    an octet after the response, inside the extension
  truncated   a response length longer than the extension
  noextdata   a status_request with no data at all
  oneoctet    a status_request with a single octet
  dup         the status_request twice
  garbage     a well-formed CertificateStatus around 40 octets of 0xA5
  onissuer    the staple on the issuer's entry only: the leaf has none
  withother   an unknown extension (0x7777) first, then the good status_request
  inee        the staple in EncryptedExtensions (not allowed there)
  inhello     status_request in the ServerHello (not allowed there)
"""
import os
import socket
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle_tls_server as base  # noqa: E402
import hmac  # noqa: E402
from cryptography import x509 as cx509  # noqa: E402
from cryptography.hazmat.primitives import hashes, serialization  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import ec, x25519  # noqa: E402

say = base.say
sha256 = base.sha256
extension = base.extension
handshake_message = base.handshake_message
expand_label = base.expand_label
derive_secret = base.derive_secret
hkdf_extract = base.hkdf_extract
ECDSA_P256 = 0x0403


def hmac_sha256(key, data):
    return hmac.new(key, data, "sha256").digest()


def status_data(mode, response):
    if mode == "badtype":
        return b"\x02" + len(response).to_bytes(3, "big") + response
    if mode == "empty":
        return b"\x01\x00\x00\x00"
    if mode == "trailing":
        return b"\x01" + len(response).to_bytes(3, "big") + response + b"\x00"
    if mode == "truncated":
        return b"\x01" + (len(response) + 50).to_bytes(3, "big") + response
    if mode == "garbage":
        return b"\x01" + (40).to_bytes(3, "big") + b"\xa5" * 40
    return b"\x01" + len(response).to_bytes(3, "big") + response


def leaf_extensions(mode, response):
    if mode in ("none", "onissuer", "inee", "inhello"):
        return b""
    if mode == "noextdata":
        return extension(5, b"")
    if mode == "oneoctet":
        return extension(5, b"\x01")
    good = extension(5, status_data(mode, response))
    if mode == "dup":
        return good + good
    if mode == "withother":
        return extension(0x7777, b"xyz") + good
    return good


def serve_one(peer, number, mode, response, chain, key):
    kind, client_hello = peer.receive()
    assert kind == 22, "expected a ClientHello"
    _random, session_id, _suites, extensions = base.parse_client_hello(client_hello)
    by_kind = {kind: data for kind, data in extensions}
    say("event: %d offered extensions=%s" % (number, ",".join(str(kind) for kind, _ in extensions)))
    say("event: %d status_request=%s" % (number, by_kind[5].hex() if 5 in by_kind else "absent"))
    client_public = by_kind[51][6:38]
    private = x25519.X25519PrivateKey.generate()
    public = private.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    shared = private.exchange(x25519.X25519PublicKey.from_public_bytes(client_public))

    hello_extensions = [extension(43, b"\x03\x04"), extension(51, struct.pack(">HH", 0x001D, 32) + public)]
    if mode == "inhello":
        hello_extensions.append(extension(5, b""))
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

    if mode == "inee":
        block = extension(5, status_data("good", response))
        send(handshake_message(8, struct.pack(">H", len(block)) + block))
    else:
        send(handshake_message(8, b"\x00\x00"))
    entries = b""
    for index, der in enumerate(chain):
        extensions_block = leaf_extensions(mode, response) if index == 0 else b""
        if mode == "onissuer" and index == 1:
            extensions_block = extension(5, status_data("good", response))
        entries += len(der).to_bytes(3, "big") + der + struct.pack(">H", len(extensions_block)) + extensions_block
    send(handshake_message(11, b"\x00" + len(entries).to_bytes(3, "big") + entries))
    context = b" " * 64 + b"TLS 1.3, server CertificateVerify\x00" + sha256(transcript)
    signature = key.sign(context, ec.ECDSA(hashes.SHA256()))
    send(handshake_message(15, struct.pack(">HH", ECDSA_P256, len(signature)) + signature))
    verify_data = hmac_sha256(expand_label(server_handshake, b"finished", b"", 32), sha256(transcript))
    send(handshake_message(20, verify_data))

    master = hkdf_extract(derive_secret(handshake_secret, b"derived", sha256(b"")), bytes(32))
    client_application = derive_secret(master, b"c ap traffic", sha256(transcript))
    server_application = derive_secret(master, b"s ap traffic", sha256(transcript))
    # Whatever the client sends next: its Finished, or the alert that refuses us.
    while True:
        record_kind, content = peer.receive()
        if record_kind == 20:
            continue
        break
    if record_kind == 21:
        say("event: %d client alert %d" % (number, content[1]))
        return
    expected = hmac_sha256(expand_label(client_handshake, b"finished", b"", 32), sha256(transcript))
    say("event: %d client_finished %s" % (number, "ok" if content[4:] == expected else "BAD"))
    peer.write.install(server_application)
    peer.read.install(client_application)
    say("event: %d handshake complete" % number)
    base.after_handshake(peer, "plain")


def main():
    chain = [cx509.load_pem_x509_certificate(block).public_bytes(serialization.Encoding.DER)
             for block in open(sys.argv[1], "rb").read().split(b"-----END CERTIFICATE-----") if b"BEGIN" in block
             for block in [block + b"-----END CERTIFICATE-----\n"]]
    key = serialization.load_pem_private_key(open(sys.argv[2], "rb").read(), None)
    plans = []
    for item in sys.argv[3].split(","):
        mode, _, path = item.partition(":")
        plans.append((mode, open(path, "rb").read() if path else b""))
    listener = socket.socket(socket.AF_INET)
    listener.bind(("127.0.0.1", 0))
    listener.listen(4)
    listener.settimeout(base.TIMEOUT)
    say("READY %d" % listener.getsockname()[1])
    for number, (mode, response) in enumerate(plans, 1):
        try:
            connection, _address = listener.accept()
        except socket.timeout:
            say("event: nobody connected")
            return 0
        connection.settimeout(base.TIMEOUT)
        connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        peer = base.Peer(connection)
        try:
            serve_one(peer, number, mode, response, chain, key)
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
