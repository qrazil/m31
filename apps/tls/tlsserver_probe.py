#!/usr/bin/env python3
"""A minimal TLS 1.3 client for probing `lib/tlsserver.m31`, written from RFC 8446.

    tlsserver_probe.py PORT CA_PEM CASE [CASE ...]

Each CASE opens one connection to 127.0.0.1:PORT, does what the case says and
prints `ok CASE` or `FAIL CASE reason`; the exit status is 1 if any failed.
Every read has a deadline, so a server that hangs is a failure, not a stall.

Cases that complete a handshake verify what a real client verifies: the
leaf's signature by the CA, CertificateVerify under the leaf's public key,
the server's Finished, and then an HTTP request over the application keys.

    full            handshake, GET, expect a 200 (the body is checked if sized)
    full_no_sni     as `full`, with no server_name extension
    full_big        as `full`, expect a 1 MiB body of octet i mod 251
    hrr             offer x25519 without a key share: HelloRetryRequest, then full
    hrr_twice       answer the HelloRetryRequest with no share again: illegal_parameter
    bad_finished    flip a bit of the client Finished: decrypt_error
    cert_as_finished  a Certificate message where Finished belongs: unexpected_message
    no_extensions   a ClientHello with no extensions: protocol_version or missing_extension
    garbage         200 random octets: an alert or a close, promptly
    truncated       half a ClientHello record, then shutdown: a close, promptly
    stalled         half a ClientHello record, then silence: a close (the timeout)
    oversized       a record header claiming 65535 octets and then silence
    tls12_only      supported_versions lists only TLS 1.2: protocol_version
    aes_only        only TLS_AES_128_GCM_SHA256: handshake_failure
    no_sigalg       signature_algorithms with nothing we sign: handshake_failure
    wrong_sni       a name the certificate does not cover: unrecognized_name
    alpn_mismatch   ALPN list with no common entry: no_application_protocol
    no_x25519       supported_groups without x25519: handshake_failure
    dup_extension   the same extension twice: illegal_parameter
    alpn_http       ALPN [h2, http/1.1]: the server must choose http/1.1
    psk_ignored     a pre_shared_key extension: ignored, a full handshake
"""

import hashlib
import hmac
import os
import socket
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from cryptography import x509
from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, ed25519, x25519

from oracle_tls_server import (Direction, Peer, derive_secret, expand_label, extension,
                               handshake_message, hkdf_extract, parse_client_hello, sha256)

DEADLINE = 8.0
SUITE_CHACHA = 0x1303
SUITE_AES128 = 0x1301
GROUP_X25519 = 0x001D
GROUP_P256 = 0x0017
SCHEME_P256 = 0x0403
SCHEME_ED25519 = 0x0807
SCHEME_RSA_PSS = 0x0804

ALERTS = {10: "unexpected_message", 40: "handshake_failure", 47: "illegal_parameter",
          50: "decode_error", 51: "decrypt_error", 70: "protocol_version",
          80: "internal_error", 109: "missing_extension", 112: "unrecognized_name",
          120: "no_application_protocol", 0: "close_notify"}


class Failure(Exception):
    pass


class Alert(Exception):
    def __init__(self, description):
        super().__init__(ALERTS.get(description, str(description)))
        self.description = description


class Closed(Exception):
    pass


def alert_name(description):
    return ALERTS.get(description, "alert %d" % description)


# --- ClientHello ---------------------------------------------------------------------

def server_name_extension(name):
    raw = name.encode()
    entry = b"\x00" + struct.pack(">H", len(raw)) + raw
    return extension(0, struct.pack(">H", len(entry)) + entry)


def vector16(items, width=2):
    body = b"".join(items)
    return struct.pack(">H", len(body)) + body


class Hello:
    """What to put in a ClientHello; `build` makes the handshake message."""

    def __init__(self):
        self.suites = [SUITE_CHACHA]
        self.versions = [0x0304]
        self.groups = [GROUP_X25519]
        self.sigalgs = [SCHEME_ED25519, SCHEME_P256]
        self.names = "localhost"
        self.protocols = None
        self.shares = None
        self.private = x25519.X25519PrivateKey.generate()
        self.extras = []
        self.duplicate_groups = False
        self.bare = False
        self.random = os.urandom(32)
        self.session_id = os.urandom(32)

    def public(self):
        return self.private.public_key().public_bytes(serialization.Encoding.Raw,
                                                      serialization.PublicFormat.Raw)

    def build(self):
        if self.bare:
            tail = b""
        else:
            shares = self.shares
            if shares is None:
                shares = [(GROUP_X25519, self.public())]
            entries = b"".join(struct.pack(">HH", group, len(data)) + data for group, data in shares)
            parts = []
            if self.names is not None:
                parts.append(server_name_extension(self.names))
            parts.append(extension(43, bytes([2 * len(self.versions)]) + b"".join(struct.pack(">H", v) for v in self.versions)))
            parts.append(extension(10, vector16([struct.pack(">H", g) for g in self.groups])))
            if self.duplicate_groups:
                parts.append(extension(10, vector16([struct.pack(">H", g) for g in self.groups])))
            parts.append(extension(13, vector16([struct.pack(">H", s) for s in self.sigalgs])))
            parts.append(extension(51, struct.pack(">H", len(entries)) + entries))
            if self.protocols is not None:
                names = b"".join(bytes([len(p)]) + p.encode() for p in self.protocols)
                parts.append(extension(16, struct.pack(">H", len(names)) + names))
            parts.extend(self.extras)
            block = b"".join(parts)
            tail = struct.pack(">H", len(block)) + block
        suites = b"".join(struct.pack(">H", s) for s in self.suites)
        body = (b"\x03\x03" + self.random + bytes([len(self.session_id)]) + self.session_id
                + struct.pack(">H", len(suites)) + suites + b"\x01\x00" + tail)
        return handshake_message(1, body)


# --- the connection --------------------------------------------------------------------

def connect(port):
    sock = socket.create_connection(("127.0.0.1", port), timeout=DEADLINE)
    sock.settimeout(DEADLINE)
    return sock


class Client:
    def __init__(self, port):
        self.sock = connect(port)
        self.peer = Peer(self.sock)
        self.buffer = b""
        self.transcript = b""

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass

    def record(self):
        """The next record as (type, content); raises Closed on end of stream."""
        try:
            return self.peer.receive()
        except EOFError:
            raise Closed()
        except (ConnectionResetError, BrokenPipeError):
            raise Closed()
        except socket.timeout:
            raise Failure("the server hung: nothing within %.0f s" % DEADLINE)
        except Exception as error:
            raise Failure("could not read a record: %s %s" % (type(error).__name__, error))

    def next_message(self):
        """The next handshake message as (kind, body, whole), skipping CCS records."""
        while True:
            if len(self.buffer) >= 4:
                length = int.from_bytes(self.buffer[1:4], "big")
                if len(self.buffer) >= 4 + length:
                    whole, self.buffer = self.buffer[:4 + length], self.buffer[4 + length:]
                    return whole[0], whole[4:], whole
            content_type, content = self.record()
            if content_type == 20:
                continue
            if content_type == 21:
                if len(content) != 2:
                    raise Failure("an alert record of %d octets" % len(content))
                raise Alert(content[1])
            if content_type != 22:
                raise Failure("record type %d during the handshake" % content_type)
            self.buffer += content

    def expect_alert(self):
        """Read until an alert or the end of the stream; returns the description or None."""
        while True:
            try:
                content_type, content = self.record()
            except Closed:
                return None
            if content_type == 21 and len(content) == 2:
                return content[1]


def schedule_handshake(shared, transcript_hash):
    early = hkdf_extract(bytes(32), bytes(32))
    derived = derive_secret(early, b"derived", sha256(b""))
    handshake = hkdf_extract(derived, shared)
    client = derive_secret(handshake, b"c hs traffic", transcript_hash)
    server = derive_secret(handshake, b"s hs traffic", transcript_hash)
    return handshake, client, server


def finished_value(secret, transcript_hash):
    key = expand_label(secret, b"finished", b"", 32)
    return hmac.new(key, transcript_hash, hashlib.sha256).digest()


def verify_signature(certificate, scheme, signature, transcript_hash):
    content = b" " * 64 + b"TLS 1.3, server CertificateVerify\x00" + transcript_hash
    public = certificate.public_key()
    try:
        if scheme == SCHEME_ED25519:
            if not isinstance(public, ed25519.Ed25519PublicKey):
                raise Failure("an Ed25519 signature under a %s key" % type(public).__name__)
            public.verify(signature, content)
        elif scheme == SCHEME_P256:
            if not isinstance(public, ec.EllipticCurvePublicKey):
                raise Failure("an ECDSA signature under a %s key" % type(public).__name__)
            public.verify(signature, content, ec.ECDSA(hashes.SHA256()))
        else:
            raise Failure("CertificateVerify scheme 0x%04x was not offered" % scheme)
    except InvalidSignature:
        raise Failure("CertificateVerify does not verify")


def parse_certificate(body):
    context_length = body[0]
    cursor = 1 + context_length
    total = int.from_bytes(body[cursor:cursor + 3], "big")
    cursor += 3
    if cursor + total != len(body):
        raise Failure("Certificate list length is wrong")
    entries = []
    while cursor < len(body):
        size = int.from_bytes(body[cursor:cursor + 3], "big")
        cursor += 3
        entries.append(x509.load_der_x509_certificate(body[cursor:cursor + size]))
        cursor += size
        extensions_length = struct.unpack(">H", body[cursor:cursor + 2])[0]
        cursor += 2 + extensions_length
    return entries


def parse_extensions(block):
    found = {}
    cursor = 0
    while cursor < len(block):
        kind, length = struct.unpack(">HH", block[cursor:cursor + 4])
        found[kind] = block[cursor + 4:cursor + 4 + length]
        cursor += 4 + length
    return found


def read_server_hello(client, hello, first=True):
    kind, body, whole = client.next_message()
    if kind != 2:
        raise Failure("expected ServerHello, got handshake type %d" % kind)
    server_random = body[2:34]
    cursor = 34
    cursor += 1 + body[cursor]
    suite = struct.unpack(">H", body[cursor:cursor + 2])[0]
    cursor += 3
    block_length = struct.unpack(">H", body[cursor:cursor + 2])[0]
    found = parse_extensions(body[cursor + 2:cursor + 2 + block_length])
    return server_random, suite, found, whole


HELLO_RETRY_REQUEST_RANDOM = bytes.fromhex(
    "cf21ad74e59a6111be1d8c021e65b891c2a211167abb8c5e079e09e2c8a8339c")


def handshake(client, hello, port_note=""):
    """Run the handshake to the point the server has sent Finished; returns the keys."""
    first = hello.build()
    client.peer.send_plain(22, first)
    random_bytes, suite, found, whole = read_server_hello(client, hello)
    if random_bytes == HELLO_RETRY_REQUEST_RANDOM:
        if suite != SUITE_CHACHA:
            raise Failure("HelloRetryRequest chose suite 0x%04x" % suite)
        if struct.unpack(">H", found.get(51, b"\0\0"))[0] != GROUP_X25519:
            raise Failure("HelloRetryRequest did not select x25519")
        transcript = b"\xfe\x00\x00\x20" + sha256(first) + whole
        second = hello.build()
        if getattr(hello, "after_retry", None):
            hello.after_retry()
            second = hello.build()
        client.peer.send_plain(22, second)
        transcript += second
        random_bytes, suite, found, whole = read_server_hello(client, hello, first=False)
        if random_bytes == HELLO_RETRY_REQUEST_RANDOM:
            raise Failure("a second HelloRetryRequest")
        transcript += whole
    else:
        transcript = first + whole
    if suite != SUITE_CHACHA:
        raise Failure("ServerHello chose suite 0x%04x" % suite)
    if struct.unpack(">H", found.get(43, b"\0\0"))[0] != 0x0304:
        raise Failure("ServerHello is not for TLS 1.3")
    share = found.get(51)
    if share is None or struct.unpack(">H", share[:2])[0] != GROUP_X25519 or share[2:4] != b"\x00\x20":
        raise Failure("ServerHello carries no x25519 share")
    shared = hello.private.exchange(x25519.X25519PublicKey.from_public_bytes(share[4:36]))
    handshake_secret, client_secret, server_secret = schedule_handshake(shared, sha256(transcript))
    client.peer.read.install(server_secret)
    client.peer.write.install(client_secret)
    return transcript, handshake_secret, client_secret, server_secret


def read_flight(client, transcript, ca, hello):
    """EncryptedExtensions, Certificate, CertificateVerify, Finished: verified."""
    kind, body, whole = client.next_message()
    if kind != 8:
        raise Failure("expected EncryptedExtensions, got %d" % kind)
    transcript += whole
    extensions = parse_extensions(body[2:])
    selected = None
    if 16 in extensions:
        data = extensions[16]
        selected = data[3:3 + data[2]].decode()
    kind, body, whole = client.next_message()
    if kind != 11:
        raise Failure("expected Certificate, got %d" % kind)
    transcript += whole
    chain = parse_certificate(body)
    if not chain:
        raise Failure("an empty Certificate")
    leaf = chain[0]
    if ca is not None:
        try:
            leaf.verify_directly_issued_by(ca)
        except Exception as error:
            raise Failure("the leaf is not issued by the test CA: %s" % error)
    kind, body, whole = client.next_message()
    if kind != 15:
        raise Failure("expected CertificateVerify, got %d" % kind)
    scheme = struct.unpack(">H", body[:2])[0]
    length = struct.unpack(">H", body[2:4])[0]
    if scheme not in hello.sigalgs:
        raise Failure("CertificateVerify uses scheme 0x%04x, which was not offered" % scheme)
    verify_signature(leaf, scheme, body[4:4 + length], sha256(transcript))
    transcript += whole
    kind, body, whole = client.next_message()
    if kind != 20:
        raise Failure("expected Finished, got %d" % kind)
    if body != finished_value(client.server_secret, sha256(transcript)):
        raise Failure("the server's Finished does not verify")
    transcript += whole
    return transcript, selected, scheme


def finish(client, transcript, handshake_secret, client_secret, bad=False):
    value = finished_value(client_secret, sha256(transcript))
    if bad:
        value = value[:-1] + bytes([value[-1] ^ 1])
    message = handshake_message(20, value)
    client.peer.send(22, message)
    full = transcript + message
    derived = derive_secret(handshake_secret, b"derived", sha256(b""))
    master = hkdf_extract(derived, bytes(32))
    server_application = derive_secret(master, b"s ap traffic", sha256(transcript))
    client_application = derive_secret(master, b"c ap traffic", sha256(transcript))
    client.peer.write.install(client_application)
    client.peer.read.install(server_application)
    return full


def read_application(client, wanted):
    """Application data until `wanted` octets arrive, the stream ends or an alert comes."""
    data = b""
    while len(data) < wanted:
        try:
            content_type, content = client.record()
        except Closed:
            break
        if content_type == 23:
            data += content
        elif content_type == 22:
            continue
        elif content_type == 21:
            if len(content) == 2 and content[1] == 0:
                break
            raise Alert(content[1])
    return data


def full_exchange(client, hello, ca, expect_big=False, expect_alpn=None, note=None):
    transcript, handshake_secret, client_secret, server_secret = handshake(client, hello)
    client.server_secret = server_secret
    transcript, selected, scheme = read_flight(client, transcript, ca, hello)
    if expect_alpn is not None and selected != expect_alpn:
        raise Failure("ALPN selected %r, expected %r" % (selected, expect_alpn))
    finish(client, transcript, handshake_secret, client_secret)
    request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    client.peer.send(23, request)
    wanted = (1048576 + 200) if expect_big else 1
    data = read_application(client, wanted)
    if not data.startswith(b"HTTP/1.1 200"):
        raise Failure("the response was %r" % data[:60])
    if expect_big:
        head, _, body = data.partition(b"\r\n\r\n")
        while len(body) < 1048576:
            more = read_application(client, 1)
            if not more:
                break
            body += more
        if len(body) != 1048576:
            raise Failure("the body has %d octets, expected 1048576" % len(body))
        if any(body[index] != index % 251 for index in range(len(body))):
            raise Failure("the body is corrupt")
    if note is not None:
        note.append("scheme=0x%04x" % scheme)
    return scheme


# --- the cases ---------------------------------------------------------------------------

def expect_alert_after_hello(client, hello, wanted, alert_after=None):
    """Send `hello`; the server must answer with one of the `wanted` alerts."""
    client.peer.send_plain(22, hello.build())
    try:
        while True:
            kind, body, whole = client.next_message()
            raise Failure("the server answered handshake type %d, expected alert %s"
                          % (kind, "/".join(alert_name(w) for w in wanted)))
    except Alert as alert:
        if alert.description not in wanted:
            raise Failure("alert %s, expected %s" % (alert_name(alert.description),
                                                      "/".join(alert_name(w) for w in wanted)))
    except Closed:
        raise Failure("closed without an alert, expected %s" % "/".join(alert_name(w) for w in wanted))


def case_full(port, ca, **options):
    client = Client(port)
    try:
        hello = Hello()
        if options.get("no_sni"):
            hello.names = None
        if options.get("protocols"):
            hello.protocols = options["protocols"]
        note = []
        full_exchange(client, hello, ca, expect_big=options.get("big", False),
                      expect_alpn=options.get("alpn"), note=note)
        return " ".join(note)
    finally:
        client.close()


def case_psk(port, ca):
    client = Client(port)
    try:
        hello = Hello()
        identity = os.urandom(16)
        identities = struct.pack(">H", len(identity)) + identity + bytes(4)
        binders = bytes([32]) + os.urandom(32)
        hello.extras = [extension(45, b"\x01\x01"),
                        extension(41, struct.pack(">H", len(identities)) + identities
                                  + struct.pack(">H", len(binders)) + binders)]
        full_exchange(client, hello, ca)
    finally:
        client.close()


def case_hrr(port, ca):
    client = Client(port)
    try:
        hello = Hello()
        hello.shares = []
        hello.after_retry = lambda: setattr(hello, "shares", None)
        full_exchange(client, hello, ca)
    finally:
        client.close()


def case_hrr_twice(port, ca):
    client = Client(port)
    try:
        hello = Hello()
        hello.shares = []
        first = hello.build()
        client.peer.send_plain(22, first)
        random_bytes, suite, found, whole = read_server_hello(client, hello)
        if random_bytes != HELLO_RETRY_REQUEST_RANDOM:
            raise Failure("no HelloRetryRequest for a ClientHello without a share")
        client.peer.send_plain(22, hello.build())
        try:
            client.next_message()
            raise Failure("the server went on after a second empty key_share")
        except Alert as alert:
            if alert.description != 47:
                raise Failure("alert %s, expected illegal_parameter" % alert_name(alert.description))
        except Closed:
            raise Failure("closed without an alert")
    finally:
        client.close()


def case_finished_fault(port, ca, kind):
    client = Client(port)
    try:
        hello = Hello()
        transcript, handshake_secret, client_secret, server_secret = handshake(client, hello)
        client.server_secret = server_secret
        transcript, selected, scheme = read_flight(client, transcript, ca, hello)
        if kind == "bad_finished":
            value = finished_value(client_secret, sha256(transcript))
            message = handshake_message(20, value[:-1] + bytes([value[-1] ^ 1]))
            wanted = 51
        else:
            message = handshake_message(11, b"\x00\x00\x00\x00")
            wanted = 10
        client.peer.send(22, message)
        while True:
            try:
                content_type, content = client.record()
            except Closed:
                raise Failure("closed without an alert, expected %s" % alert_name(wanted))
            if content_type == 21 and len(content) == 2:
                if content[1] != wanted:
                    raise Failure("alert %s, expected %s" % (alert_name(content[1]), alert_name(wanted)))
                return
            if content_type == 23:
                raise Failure("application data after a bad client flight")
    finally:
        client.close()


def case_hello_alert(port, wanted, configure):
    client = Client(port)
    try:
        hello = Hello()
        configure(hello)
        expect_alert_after_hello(client, hello, wanted)
    finally:
        client.close()


def case_garbage(port):
    client = Client(port)
    try:
        client.sock.sendall(os.urandom(200))
        reason = client.expect_alert()
        return "alert %s" % alert_name(reason) if reason is not None else "closed"
    finally:
        client.close()


def case_truncated(port, shutdown):
    client = Client(port)
    try:
        record = Hello().build()
        framed = struct.pack(">BHH", 22, 0x0301, len(record)) + record
        client.sock.sendall(framed[:len(framed) // 2])
        if shutdown:
            client.sock.shutdown(socket.SHUT_WR)
        reason = client.expect_alert()
        return "alert %s" % alert_name(reason) if reason is not None else "closed"
    finally:
        client.close()


def case_oversized(port):
    client = Client(port)
    try:
        client.sock.sendall(struct.pack(">BHH", 22, 0x0301, 0xFFFF))
        reason = client.expect_alert()
        return "alert %s" % alert_name(reason) if reason is not None else "closed"
    finally:
        client.close()


def run(port, ca, name):
    big_ok = lambda: case_full(port, ca, big=True)
    if name == "full":
        return case_full(port, ca)
    if name == "full_no_sni":
        return case_full(port, ca, no_sni=True)
    if name == "full_big":
        return big_ok()
    if name == "alpn_http":
        return case_full(port, ca, protocols=["h2", "http/1.1"], alpn="http/1.1")
    if name == "hrr":
        return case_hrr(port, ca)
    if name == "hrr_twice":
        return case_hrr_twice(port, ca)
    if name in ("bad_finished", "cert_as_finished"):
        return case_finished_fault(port, ca, name)
    if name == "no_extensions":
        return case_hello_alert(port, [70, 109, 40, 47, 50], lambda h: setattr(h, "bare", True))
    if name == "garbage":
        return case_garbage(port)
    if name == "truncated":
        return case_truncated(port, True)
    if name == "stalled":
        return case_truncated(port, False)
    if name == "oversized":
        return case_oversized(port)
    if name == "tls12_only":
        return case_hello_alert(port, [70], lambda h: setattr(h, "versions", [0x0303]))
    if name == "aes_only":
        return case_hello_alert(port, [40], lambda h: setattr(h, "suites", [SUITE_AES128]))
    if name == "no_sigalg":
        return case_hello_alert(port, [40], lambda h: setattr(h, "sigalgs", [SCHEME_RSA_PSS]))
    if name == "wrong_sni":
        return case_hello_alert(port, [112], lambda h: setattr(h, "names", "other.example"))
    if name == "alpn_mismatch":
        return case_hello_alert(port, [120], lambda h: setattr(h, "protocols", ["spdy/3", "gopher"]))
    if name == "no_x25519":
        def configure(h):
            h.groups = [GROUP_P256]
            h.shares = [(GROUP_P256, b"\x04" + os.urandom(64))]
        return case_hello_alert(port, [40, 47], configure)
    if name == "dup_extension":
        return case_hello_alert(port, [47], lambda h: setattr(h, "duplicate_groups", True))
    if name == "psk_ignored":
        return case_psk(port, ca)
    raise Failure("no such case")


def main():
    if len(sys.argv) < 4:
        sys.stderr.write(__doc__)
        return 2
    port = int(sys.argv[1])
    with open(sys.argv[2], "rb") as handle:
        ca = x509.load_pem_x509_certificate(handle.read())
    failed = 0
    for name in sys.argv[3:]:
        try:
            detail = run(port, ca, name)
            print("ok %s%s" % (name, (" " + detail) if detail else ""), flush=True)
        except Failure as failure:
            print("FAIL %s %s" % (name, failure), flush=True)
            failed += 1
        except Alert as alert:
            print("FAIL %s unexpected alert %s" % (name, alert), flush=True)
            failed += 1
        except Closed:
            print("FAIL %s the server closed the connection" % name, flush=True)
            failed += 1
        except Exception as error:
            print("FAIL %s %s: %s" % (name, type(error).__name__, error), flush=True)
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
