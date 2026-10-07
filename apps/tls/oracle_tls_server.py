#!/usr/bin/env python3
"""A TLS 1.3 server that can misbehave on purpose: the oracle for `lib/tls.m31`.

  oracle_tls_server.py MODE [p256|p384 [BIND_ADDRESS]]

Makes a throwaway key and self-signed certificate, listens on an ephemeral
loopback port (127.0.0.1 unless told otherwise), prints `READY <port> <SHA-256 of the SPKI, hex>` as its first
line (flushed), serves ONE connection, prints what it saw as `event: ...`
lines, and exits. Written from RFC 8446 with `hashlib`/`hmac` and the
`cryptography` package's X25519, ECDSA and ChaCha20-Poly1305 -- nothing from
this repository -- so a client that completes a handshake with it has been
checked against an independent implementation, key schedule, record layer,
CertificateVerify and Finished included.

Every MODE is a way of being right or wrong. The client under test must
complete the handshake with the modes that are right (`ok`, `ticket`,
`keyupdate`, `chain`, `coalesce`, `split`, `dribble`, `big`, `close_notify`,
`truncate`, `truncate_clean`, `peer_alert`) and refuse the rest:
  bad_finished        the server's Finished has one bit flipped
  bad_signature       CertificateVerify signs the client's context string
  wrong_scheme        CertificateVerify names rsa_pss_rsae_sha256, never offered
  scheme_mismatch     a P-256 key, but ecdsa_secp384r1_sha384 claimed
  bad_record          one bit of the EncryptedExtensions record is flipped
  hrr                 a HelloRetryRequest
  downgrade           a TLS 1.2 ServerHello with the downgrade sentinel
  tls12               a TLS 1.2 ServerHello
  bad_session_id      the ServerHello does not echo the session id
  zero_share          a key_share that is the all-zero (low-order) point
  server_hello_extra  a ServerHello extension that was not offered
  ee_extra            an EncryptedExtensions extension that was not offered
  cert_request        a CertificateRequest
  no_certificate      an empty certificate_list
  alert_after_hello   a fatal alert where EncryptedExtensions belongs
"""
import hashlib
import hmac
import os
import socket
import struct
import sys
import time
import datetime

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, x25519
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.x509.oid import NameOID

TIMEOUT = 20
SUITE = 0x1303


def say(text):
    print(text, flush=True)


# --- key schedule, RFC 8446 section 7 --------------------------------------------

def hkdf_extract(salt, ikm):
    return hmac.new(salt, ikm, hashlib.sha256).digest()


def hkdf_expand(secret, info, length):
    out, block = b"", b""
    counter = 1
    while len(out) < length:
        block = hmac.new(secret, block + info + bytes([counter]), hashlib.sha256).digest()
        out += block
        counter += 1
    return out[:length]


def expand_label(secret, label, context, length):
    full = b"tls13 " + label
    info = struct.pack(">HB", length, len(full)) + full + bytes([len(context)]) + context
    return hkdf_expand(secret, info, length)


def derive_secret(secret, label, transcript_hash):
    return expand_label(secret, label, transcript_hash, 32)


def sha256(data):
    return hashlib.sha256(data).digest()


class Direction:
    """One direction of the record layer: a traffic secret, its key, IV and sequence."""

    def __init__(self):
        self.secret = None
        self.aead = None

    def install(self, secret):
        self.secret = secret
        self.aead = ChaCha20Poly1305(expand_label(secret, b"key", b"", 32))
        self.iv = expand_label(secret, b"iv", b"", 12)
        self.sequence = 0

    def update(self):
        self.install(expand_label(self.secret, b"traffic upd", b"", 32))

    def nonce(self):
        padded = bytes(4) + struct.pack(">Q", self.sequence)
        return bytes(a ^ b for a, b in zip(self.iv, padded))


# --- records --------------------------------------------------------------------

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

    def seal(self, content_type, content):
        inner = content + bytes([content_type])
        header = struct.pack(">BHH", 23, 0x0303, len(inner) + 16)
        body = self.write.aead.encrypt(self.write.nonce(), inner, header)
        self.write.sequence += 1
        return header + body

    def send(self, content_type, content, flip_bit=False):
        record = self.seal(content_type, content)
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
        """The next record as (content_type, content), decrypting once keys are in."""
        header = self.receive_exactly(5)
        content_type, _version, length = struct.unpack(">BHH", header)
        body = self.receive_exactly(length)
        if self.read.aead is None or content_type == 20:
            return content_type, body
        inner = self.read.aead.decrypt(self.read.nonce(), body, header)
        self.read.sequence += 1
        end = len(inner)
        while inner[end - 1] == 0:
            end -= 1
        return inner[end - 1], inner[:end - 1]


def handshake_message(kind, body):
    return bytes([kind]) + len(body).to_bytes(3, "big") + body


def extension(kind, data):
    return struct.pack(">HH", kind, len(data)) + data


def parse_client_hello(message):
    body = message[4:]
    cursor = 2 + 32
    random = body[2:34]
    session_id_length = body[cursor]
    session_id = body[cursor + 1:cursor + 1 + session_id_length]
    cursor += 1 + session_id_length
    suites_length = struct.unpack(">H", body[cursor:cursor + 2])[0]
    suites = [struct.unpack(">H", body[cursor + 2 + i:cursor + 4 + i])[0] for i in range(0, suites_length, 2)]
    cursor += 2 + suites_length
    cursor += 1 + body[cursor]
    extensions_length = struct.unpack(">H", body[cursor:cursor + 2])[0]
    cursor += 2
    end = cursor + extensions_length
    assert end == len(body), "ClientHello has trailing octets"
    extensions = []
    while cursor < end:
        kind, length = struct.unpack(">HH", body[cursor:cursor + 4])
        extensions.append((kind, body[cursor + 4:cursor + 4 + length]))
        cursor += 4 + length
    return random, session_id, suites, extensions


def make_certificate(curve):
    key = ec.generate_private_key(ec.SECP256R1() if curve == "p256" else ec.SECP384R1())
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


# --- the handshake -----------------------------------------------------------------

def serve(peer, mode, curve, key, certificate_der):
    kind, content = peer.receive()
    assert kind == 22, "expected a ClientHello"
    client_hello = content
    random, session_id, suites, extensions = parse_client_hello(client_hello)
    by_kind = {kind: data for kind, data in extensions}
    groups = struct.unpack(">H", by_kind[10][:2])[0] // 2
    sigalgs = [struct.unpack(">H", by_kind[13][2 + i:4 + i])[0] for i in range(0, len(by_kind[13]) - 2, 2)]
    say("event: offered suites=%s groups=%s versions=%s sigalgs=%s extensions=%s" % (
        ",".join("%04x" % s for s in suites),
        ",".join("%04x" % struct.unpack(">H", by_kind[10][2 + 2 * i:4 + 2 * i])[0] for i in range(groups)),
        by_kind[43][1:].hex(),
        ",".join("%04x" % s for s in sigalgs),
        ",".join(str(kind) for kind, _ in extensions)))
    if 0 in by_kind:
        name_length = struct.unpack(">H", by_kind[0][3:5])[0]
        say("event: sni=" + by_kind[0][5:5 + name_length].decode())
    else:
        say("event: sni=none")
    say("event: session_id_length=%d" % len(session_id))
    shares = by_kind[51]
    group, share_length = struct.unpack(">HH", shares[2:6])
    assert group == 0x001D and share_length == 32
    client_public = shares[6:38]

    private = x25519.X25519PrivateKey.generate()
    public = private.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    shared = private.exchange(x25519.X25519PublicKey.from_public_bytes(client_public))

    hello_session_id = session_id
    server_random = os.urandom(32)
    legacy_extensions = [extension(43, b"\x03\x04"), extension(51, struct.pack(">HH", 0x001D, 32) + public)]
    if mode == "bad_session_id":
        hello_session_id = bytes(32)
    if mode == "zero_share":
        legacy_extensions[1] = extension(51, struct.pack(">HH", 0x001D, 32) + bytes(32))
    if mode == "server_hello_extra":
        legacy_extensions.append(extension(16, b"\x00\x03\x02h2"))
    if mode == "hrr":
        server_random = sha256(b"HelloRetryRequest")
        legacy_extensions = [extension(43, b"\x03\x04"), extension(51, struct.pack(">H", 0x001D))]
    suite = SUITE
    legacy_version = 0x0303
    if mode in ("downgrade", "tls12"):
        suite = 0xC02F
        if mode == "downgrade":
            server_random = os.urandom(24) + b"DOWNGRD\x01"
        legacy_extensions = [extension(0xFF01, b"\x00")]
    body = (struct.pack(">H", legacy_version) + server_random + bytes([len(hello_session_id)]) + hello_session_id
            + struct.pack(">HB", suite, 0) + struct.pack(">H", sum(len(e) for e in legacy_extensions))
            + b"".join(legacy_extensions))
    server_hello = handshake_message(2, body)
    peer.send_plain(22, server_hello)
    if mode in ("hrr", "downgrade", "tls12", "bad_session_id", "zero_share", "server_hello_extra"):
        # Whatever the client does next is the point: a fatal alert, then silence.
        report_next(peer)
        return

    peer.send_plain(20, b"\x01")
    transcript = client_hello + server_hello
    early = hkdf_extract(bytes(32), bytes(32))
    handshake_secret = hkdf_extract(derive_secret(early, b"derived", sha256(b"")), shared)
    client_handshake = derive_secret(handshake_secret, b"c hs traffic", sha256(transcript))
    server_handshake = derive_secret(handshake_secret, b"s hs traffic", sha256(transcript))
    peer.write.install(server_handshake)
    peer.read.install(client_handshake)

    if mode == "alert_after_hello":
        peer.send(21, b"\x02\x28")
        report_next(peer)
        return

    flight = []
    encrypted_extensions = [extension(16, b"\x00\x03\x02h2")] if mode == "ee_extra" else []
    flight.append(handshake_message(8, struct.pack(">H", sum(len(e) for e in encrypted_extensions)) + b"".join(encrypted_extensions)))
    if mode == "cert_request":
        flight.append(handshake_message(13, b"\x00" + struct.pack(">H", 6) + extension(13, b"\x00\x02\x04\x03")))
    entries = [len(certificate_der).to_bytes(3, "big") + certificate_der + b"\x00\x00"]
    if mode == "chain":
        entries.append(len(certificate_der).to_bytes(3, "big") + certificate_der + b"\x00\x04\x00\x12\x00\x00")
    entry_bytes = b"".join(entries)
    if mode == "no_certificate":
        entry_bytes = b""
    flight.append(handshake_message(11, b"\x00" + len(entry_bytes).to_bytes(3, "big") + entry_bytes))
    for message in flight:
        transcript += message
    context = b" " * 64 + (b"TLS 1.3, client CertificateVerify" if mode == "bad_signature" else b"TLS 1.3, server CertificateVerify") + b"\x00" + sha256(transcript)
    if curve == "p256":
        scheme, algorithm = 0x0403, ec.ECDSA(hashes.SHA256())
    else:
        scheme, algorithm = 0x0503, ec.ECDSA(hashes.SHA384())
    signature = key.sign(context, algorithm)
    if mode == "wrong_scheme":
        scheme = 0x0804
    if mode == "scheme_mismatch":
        scheme = 0x0503 if curve == "p256" else 0x0403
    certificate_verify = handshake_message(15, struct.pack(">HH", scheme, len(signature)) + signature)
    transcript += certificate_verify
    finished_key = expand_label(server_handshake, b"finished", b"", 32)
    verify_data = hmac.new(finished_key, sha256(transcript), hashlib.sha256).digest()
    if mode == "bad_finished":
        verify_data = verify_data[:-1] + bytes([verify_data[-1] ^ 1])
    finished = handshake_message(20, verify_data)
    flight += [certificate_verify, finished]
    transcript += finished

    if mode == "coalesce":
        peer.send(22, b"".join(flight))
    elif mode == "split":
        stream = b"".join(flight)
        for start in range(0, len(stream), 7):
            peer.send(22, stream[start:start + 7])
    elif mode == "bad_record":
        peer.send(22, flight[0], flip_bit=True)
    else:
        peer.dribble = mode == "dribble"
        for message in flight:
            peer.send(22, message)
        peer.dribble = False
    if mode == "bad_record":
        report_next(peer)
        return

    master = hkdf_extract(derive_secret(handshake_secret, b"derived", sha256(b"")), bytes(32))
    client_application = derive_secret(master, b"c ap traffic", sha256(transcript))
    server_application = derive_secret(master, b"s ap traffic", sha256(transcript))

    # The client's Finished comes under its handshake keys, after its
    # compatibility ChangeCipherSpec (which `receive` hands back unprotected).
    while True:
        kind, content = peer.receive()
        if kind == 20:
            continue
        break
    if kind == 21:
        say("event: client alert %d" % content[1])
        return
    assert kind == 22 and content[0] == 20, "expected the client's Finished"
    expected = hmac.new(expand_label(client_handshake, b"finished", b"", 32), sha256(transcript), hashlib.sha256).digest()
    say("event: client_finished " + ("ok" if content[4:] == expected else "BAD"))
    peer.write.install(server_application)
    peer.read.install(client_application)
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
            say("event: client alert %d" % content[1])
            return None
        if kind == 22:
            handle_post_handshake(peer, content)
            continue
        assert kind == 23, "unexpected record type %d" % kind
        data += content
    return data


def handle_post_handshake(peer, message):
    if message[0] == 24:
        say("event: client KeyUpdate")
        peer.read.update()
        if message[4] == 1:
            peer.send(22, handshake_message(24, b"\x00"))
            peer.write.update()
    else:
        say("event: client handshake message %d" % message[0])


def close_notify(peer):
    peer.send(21, b"\x01\x00")
    say("event: sent close_notify")


def after_handshake(peer, mode):
    if mode == "ticket":
        peer.send(22, handshake_message(4, struct.pack(">IIB", 3600, 1, 0) + b"\x00\x04abcd" + b"\x00\x00"))
        say("event: sent NewSessionTicket")
    if mode == "keyupdate":
        peer.send(22, handshake_message(24, b"\x01"))
        peer.write.update()
        say("event: sent KeyUpdate requesting one")
    if mode == "peer_alert":
        peer.send(21, b"\x02\x50")
        return
    if mode in ("truncate", "truncate_clean"):
        line = read_line(peer)
        if mode == "truncate":
            peer.send(23, b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n0123456789")
        else:
            peer.send(23, b"all of it")
        say("event: closing without close_notify")
        peer.connection.shutdown(socket.SHUT_RDWR)
        return
    if mode == "big":
        line = read_line(peer)
        payload = bytes((index * 7) & 0xFF for index in range(100_000))
        for start in range(0, len(payload), 16384):
            peer.send(23, payload[start:start + 16384])
        say("event: sent %d octets" % len(payload))
        close_notify(peer)
        return
    if mode == "close_notify":
        line = read_line(peer)
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
        if mode == "keyupdate":
            peer.send(22, handshake_message(24, b"\x00"))
            peer.write.update()
    close_notify(peer)


def main():
    mode = sys.argv[1]
    curve = sys.argv[2] if len(sys.argv) > 2 else "p256"
    key, certificate_der, pin = make_certificate(curve)
    bind_host = sys.argv[3] if len(sys.argv) > 3 else "127.0.0.1"
    listener = socket.socket(socket.AF_INET6 if ":" in bind_host else socket.AF_INET)
    listener.bind((bind_host, 0))
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
        serve(peer, mode, curve, key, certificate_der)
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
