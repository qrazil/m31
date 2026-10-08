#!/usr/bin/env python3
"""A TLS 1.3 server that issues session tickets and resumes them, and can
misbehave on purpose: the oracle for the resumption half of `lib/tls.m31`.

  resume_server.py MODE[,MODE...] [BIND_ADDRESS]

One MODE per connection, served in order; the server exits after the last.
Prints `READY <port> <SHA-256 of the SPKI, hex>` first, then `event: N ...`
lines (N is the connection number from 1). Built on the helpers of
`oracle_tls_server.py` and written from RFC 8446 with `hashlib`/`hmac`/the
`cryptography` package -- nothing from this repository -- so `binder=ok` and
the client's Finished verifying are an independent implementation's word that
the client's PSK schedule, binder and ticket handling are right.

Tickets are remembered per process: 24 random octets naming a PSK. A
ClientHello that offers one is checked the way a real server does: the
binder is recomputed over the ClientHello cut off before the binders list,
and a wrong binder is a fatal `decrypt_error`.

MODEs that complete a handshake, and what they do on top of a full one:
  full             issue one ticket (lifetime 3600)
  full_noticket    issue none
  resume           accept a valid PSK (no Certificate), else fall back to full;
                   issue one ticket either way
  ignore           ignore any PSK and do a full handshake
  ticket_zero      ticket_lifetime 0 (do not use)
  ticket_short     lifetime 100
  ticket_long      lifetime 8 days (the client caps at 7)
  ticket_many      20 tickets, each with its own nonce
  ticket_early_data  the ticket carries an early_data extension (never used)
MODEs the client must refuse (all but the ticket ones fail the handshake):
  unsolicited      a pre_shared_key in the ServerHello nobody offered
  bad_identity     a PSK accepted, but identity 1 selected
  wrong_psk        a PSK accepted, but the schedule runs on another one
  psk_bad_finished a PSK accepted, the server Finished has a flipped bit
  psk_certificate  a PSK accepted, then a Certificate anyway
  psk_wrong_suite  a PSK accepted under a different suite than the session's
  psk_no_dhe       a PSK accepted without a key_share (psk_ke)
Ticket MODEs that are a hostile NewSessionTicket (the connection must fail):
  ticket_malformed   lengths run past the end
  ticket_trailing    an octet after the extensions
  ticket_empty       a zero-length ticket
  ticket_bad_ext     an extension that runs past the extensions block
"""
import hashlib
import hmac
import os
import socket
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import oracle_tls_server as base  # noqa: E402
from cryptography.hazmat.primitives import hashes, serialization  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import ec, x25519  # noqa: E402

say = base.say
sha256 = base.sha256
extension = base.extension
handshake_message = base.handshake_message
expand_label = base.expand_label
derive_secret = base.derive_secret
hkdf_extract = base.hkdf_extract

ACCEPTING = ("resume", "bad_identity", "wrong_psk", "psk_bad_finished", "psk_certificate",
             "psk_wrong_suite", "psk_no_dhe")


def hmac_sha256(key, data):
    return hmac.new(key, data, hashlib.sha256).digest()


def parse_psk(data):
    identities_length = struct.unpack(">H", data[:2])[0]
    cursor, identities = 2, []
    while cursor < 2 + identities_length:
        length = struct.unpack(">H", data[cursor:cursor + 2])[0]
        identity = data[cursor + 2:cursor + 2 + length]
        age = struct.unpack(">I", data[cursor + 2 + length:cursor + 6 + length])[0]
        identities.append((identity, age))
        cursor += 6 + length
    binders_length = struct.unpack(">H", data[cursor:cursor + 2])[0]
    binders, cursor = [], cursor + 2
    end = cursor + binders_length
    while cursor < end:
        length = data[cursor]
        binders.append(data[cursor + 1:cursor + 1 + length])
        cursor += 1 + length
    assert cursor == end == len(data), "pre_shared_key has trailing octets"
    return identities, binders, binders_length


def binder_for(psk, truncated_hello):
    early = hkdf_extract(bytes(32), psk)
    key = derive_secret(early, b"res binder", sha256(b""))
    return hmac_sha256(expand_label(key, b"finished", b"", 32), sha256(truncated_hello))


def build_ticket(mode, lifetime, age_add, nonce, ticket):
    extensions = b""
    if mode == "ticket_early_data":
        extensions = extension(42, struct.pack(">I", 16384))
    body = struct.pack(">II", lifetime, age_add) + bytes([len(nonce)]) + nonce
    body += struct.pack(">H", len(ticket)) + ticket
    if mode == "ticket_bad_ext":
        extensions = struct.pack(">HH", 42, 40) + b"\x00"
    body += struct.pack(">H", len(extensions)) + extensions
    if mode == "ticket_malformed":
        body = body[:-6]
    if mode == "ticket_trailing":
        body += b"\x00"
    return handshake_message(4, body)


def issue_tickets(peer, number, mode, resumption_master, store):
    count, lifetime = 1, 3600
    if mode == "ticket_many":
        count = 20
    if mode == "ticket_zero":
        lifetime = 0
    if mode == "ticket_short":
        lifetime = 100
    if mode == "ticket_long":
        lifetime = 8 * 24 * 3600
    if mode == "full_noticket":
        return
    for index in range(count):
        nonce = struct.pack(">Q", number * 1000 + index)
        ticket = os.urandom(24)
        if mode == "ticket_empty":
            ticket = b""
        age_add = struct.unpack(">I", os.urandom(4))[0]
        psk = expand_label(resumption_master, b"resumption", nonce, 32)
        store[ticket] = {"psk": psk, "age_add": age_add, "used": False}
        peer.send(22, build_ticket(mode, lifetime, age_add, nonce, ticket))
    say("event: %d sent %d ticket(s)" % (number, count))


def report_alert(peer, number):
    """Read until the client's alert (skipping its application data) and log it."""
    try:
        while True:
            kind, content = peer.receive()
            if kind == 21:
                say("event: %d client alert %d" % (number, content[1]))
                return
    except (EOFError, ConnectionError):
        say("event: %d client closed without an alert" % number)


HOSTILE_TICKETS = ("ticket_malformed", "ticket_trailing", "ticket_empty", "ticket_bad_ext")


def serve_one(peer, number, mode, store, key, certificate_der):
    kind, client_hello = peer.receive()
    assert kind == 22, "expected a ClientHello"
    _random, session_id, suites, extensions = base.parse_client_hello(client_hello)
    by_kind = {kind: data for kind, data in extensions}
    say("event: %d offered extensions=%s" % (number, ",".join(str(kind) for kind, _ in extensions)))
    if 0 in by_kind:
        name_length = struct.unpack(">H", by_kind[0][3:5])[0]
        say("event: %d sni=%s" % (number, by_kind[0][5:5 + name_length].decode()))
    else:
        say("event: %d sni=none" % number)
    modes = by_kind.get(45)
    say("event: %d psk_modes=%s" % (number, modes.hex() if modes is not None else "none"))
    say("event: %d early_data=%s" % (number, "present" if 42 in by_kind else "absent"))
    client_public = by_kind[51][6:38]
    private = x25519.X25519PrivateKey.generate()
    public = private.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    shared = private.exchange(x25519.X25519PublicKey.from_public_bytes(client_public))

    psk = None
    if 41 in by_kind:
        say("event: %d psk_last=%s" % (number, "yes" if extensions[-1][0] == 41 else "NO"))
        identities, binders, binders_length = parse_psk(by_kind[41])
        say("event: %d psk_offered identities=%d binders=%d" % (number, len(identities), len(binders)))
        identity, obfuscated_age = identities[0]
        record = store.get(identity)
        if record is None:
            say("event: %d psk unknown_ticket" % number)
        else:
            truncated = client_hello[:len(client_hello) - 2 - binders_length]
            expected = binder_for(record["psk"], truncated)
            if binders[0] != expected:
                say("event: %d psk binder=BAD" % number)
                peer.send_plain(21, b"\x02\x33")
                return
            say("event: %d psk binder=ok" % number)
            say("event: %d psk age_ms=%d" % (number, (obfuscated_age - record["age_add"]) % (1 << 32)))
            if record["used"]:
                say("event: %d psk ticket_reused" % number)
            record["used"] = True
            psk = record["psk"]
    accepted = psk is not None and mode in ACCEPTING
    say("event: %d resumption=%s" % (number, "accepted" if accepted else "no"))

    suite = 0x1303
    hello_extensions = [extension(43, b"\x03\x04"), extension(51, struct.pack(">HH", 0x001D, 32) + public)]
    if accepted:
        hello_extensions.append(extension(41, struct.pack(">H", 1 if mode == "bad_identity" else 0)))
    if mode == "unsolicited":
        hello_extensions.append(extension(41, b"\x00\x00"))
    if mode == "psk_wrong_suite":
        suite = 0x1301
    if mode == "psk_no_dhe":
        del hello_extensions[1]
    server_random = os.urandom(32)
    body = (struct.pack(">H", 0x0303) + server_random + bytes([len(session_id)]) + session_id
            + struct.pack(">HB", suite, 0) + struct.pack(">H", sum(len(e) for e in hello_extensions))
            + b"".join(hello_extensions))
    server_hello = handshake_message(2, body)
    peer.send_plain(22, server_hello)
    if mode in ("bad_identity", "psk_wrong_suite", "psk_no_dhe", "unsolicited"):
        base.report_next(peer)
        return
    peer.send_plain(20, b"\x01")

    transcript = client_hello + server_hello
    schedule_psk = psk if accepted else None
    if mode == "wrong_psk":
        schedule_psk = os.urandom(32)
    early = hkdf_extract(bytes(32), schedule_psk if schedule_psk is not None else bytes(32))
    handshake_secret = hkdf_extract(derive_secret(early, b"derived", sha256(b"")), shared)
    client_handshake = derive_secret(handshake_secret, b"c hs traffic", sha256(transcript))
    server_handshake = derive_secret(handshake_secret, b"s hs traffic", sha256(transcript))
    peer.write.install(server_handshake)
    peer.read.install(client_handshake)

    flight = [handshake_message(8, b"\x00\x00")]
    transcript += flight[0]
    if not accepted or mode == "psk_certificate":
        entry = len(certificate_der).to_bytes(3, "big") + certificate_der + b"\x00\x00"
        certificate = handshake_message(11, b"\x00" + len(entry).to_bytes(3, "big") + entry)
        transcript += certificate
        context = b" " * 64 + b"TLS 1.3, server CertificateVerify\x00" + sha256(transcript)
        signature = key.sign(context, ec.ECDSA(hashes.SHA256()))
        verify = handshake_message(15, struct.pack(">HH", 0x0403, len(signature)) + signature)
        transcript += verify
        flight += [certificate, verify]
    verify_data = hmac_sha256(expand_label(server_handshake, b"finished", b"", 32), sha256(transcript))
    if mode == "psk_bad_finished":
        verify_data = verify_data[:-1] + bytes([verify_data[-1] ^ 1])
    finished = handshake_message(20, verify_data)
    flight.append(finished)
    transcript += finished
    for message in flight:
        peer.send(22, message)

    master = hkdf_extract(derive_secret(handshake_secret, b"derived", sha256(b"")), bytes(32))
    client_application = derive_secret(master, b"c ap traffic", sha256(transcript))
    server_application = derive_secret(master, b"s ap traffic", sha256(transcript))
    while True:
        kind, content = peer.receive()
        if kind == 20:
            continue
        break
    if kind == 21:
        say("event: %d client alert %d" % (number, content[1]))
        return
    assert kind == 22 and content[0] == 20, "expected the client's Finished"
    expected = hmac_sha256(expand_label(client_handshake, b"finished", b"", 32), sha256(transcript))
    say("event: %d client_finished %s" % (number, "ok" if content[4:] == expected else "BAD"))
    transcript += content
    resumption_master = derive_secret(master, b"res master", sha256(transcript))
    peer.write.install(server_application)
    peer.read.install(client_application)
    say("event: %d handshake complete" % number)
    if modes is not None and 1 in modes[1:]:
        issue_tickets(peer, number, mode, resumption_master, store)
        if mode in HOSTILE_TICKETS:
            base.read_line(peer)
            peer.send(23, b"ok")
            report_alert(peer, number)
            return
    else:
        say("event: %d no tickets: psk_dhe_ke was not offered" % number)
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
    store = {}
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
            serve_one(peer, number, mode, store, key, certificate_der)
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
