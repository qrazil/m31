#!/usr/bin/env python3
"""Expected output of t_tls13_connection.m31, and its input.

`--args` prints the hex of the records the scripted peer sends (argument 1 of
the m31 program). With no argument this prints the lines the m31 program must
print.

Records are built here with the `cryptography` package's ChaCha20Poly1305,
HKDF-Expand-Label from RFC 8446 section 7.1 and the section 5.2 nonce rule,
written out separately from the program under test. What each refused stream
must produce is written down below as plain expectations (the error, the
alert's description from RFC 8446 section 6, and the alert record itself),
not computed from the library.
"""
import hashlib
import sys

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.kdf.hkdf import HKDFExpand


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


def expand_label(secret, label, context, length):
    full = b"tls13 " + label
    info = length.to_bytes(2, "big") + bytes([len(full)]) + full + bytes([len(context)]) + context
    return HKDFExpand(hashes.SHA256(), length, info).derive(secret)


def key_iv(secret):
    return expand_label(secret, b"key", b"", 32), expand_label(secret, b"iv", b"", 12)


def next_secret(secret):
    return expand_label(secret, b"traffic upd", b"", 32)


def header(content_type, length):
    return bytes([content_type, 3, 3, length >> 8, length & 0xFF])


def plain(content_type, content):
    return header(content_type, len(content)) + content


class Direction:
    def __init__(self, secret):
        self.secret = secret
        self.sequence = 0
        self.records = 0
        self.key, self.iv = key_iv(secret)

    def seal(self, content_type, content, padding=0):
        padding = min(padding, 16640 - 16 - 1 - len(content))
        inner = content + bytes([content_type]) + bytes(padding)
        nonce = bytes(a ^ b for a, b in zip(self.iv, self.sequence.to_bytes(12, "big")))
        head = header(23, len(inner) + 16)
        self.sequence += 1
        return head + ChaCha20Poly1305(self.key).encrypt(nonce, inner, head)

    def fragments(self, content_type, content, padding=0):
        out = b""
        for start in range(0, len(content), 16384):
            out += self.seal(content_type, content[start:start + 16384], padding)
        return out

    def update(self):
        self.__init__(next_secret(self.secret))


def message(kind, body):
    return bytes([kind]) + len(body).to_bytes(3, "big") + body


def describe(data):
    return "size=%d sha256=%s" % (len(data), hashlib.sha256(data).hexdigest())


READ_SECRET = pattern(32, 3, 1)
APPLICATION_SECRET = pattern(32, 13, 2)
WRITE_SECRET = pattern(32, 17, 3)

MESSAGE_A = message(2, pattern(8, 1, 1))
MESSAGE_B = message(11, pattern(39996, 3, 5))
MESSAGE_C = message(15, pattern(4, 1, 0xC0))
FINISHED = message(20, pattern(32, 1, 0x40))
DATA = [pattern(100, 1, 0), pattern(16384, 1, 9), pattern(30, 1, 1), pattern(7, 1, 2), pattern(3, 1, 3)]


def scripted_wire():
    wire = plain(22, MESSAGE_A) + plain(20, b"\x01")
    hs = Direction(READ_SECRET)
    wire += hs.seal(22, MESSAGE_B[:16384])
    wire += hs.seal(22, MESSAGE_B[16384:32768])
    wire += hs.seal(22, MESSAGE_B[32768:] + MESSAGE_C, 20)
    wire += hs.seal(22, FINISHED[:2])
    wire += hs.seal(22, FINISHED[2:], 5)
    app = Direction(APPLICATION_SECRET)
    wire += app.seal(23, DATA[0], 50)
    wire += app.seal(23, b"", 3)
    wire += app.seal(23, DATA[1])
    wire += app.seal(22, message(4, bytes(5)))
    wire += app.seal(22, bytes([24, 0, 0, 1, 1]))
    app.update()
    wire += app.seal(23, DATA[2])
    wire += app.seal(22, bytes([24, 0, 0, 1, 0]))
    app.update()
    wire += app.seal(23, DATA[3])
    wire += app.seal(21, bytes([1, 90]))
    wire += app.seal(23, DATA[4])
    wire += app.seal(21, bytes([1, 0]))
    return wire


def scripted_written():
    out = plain(20, b"\x01")
    w = Direction(WRITE_SECRET)
    out += w.fragments(22, message(11, pattern(39996, 5, 7)))
    out += w.fragments(23, pattern(40000, 5, 5))
    out += w.seal(23, pattern(20, 1, 1), 10)
    out += w.seal(23, pattern(16384, 1, 2), 255)
    out += w.seal(23, pattern(100, 1, 3), 255)
    out += w.seal(22, bytes([24, 0, 0, 1, 1]))
    w.update()
    out += w.seal(23, pattern(5, 1, 4))
    records_before = w.sequence
    out += w.seal(22, bytes([24, 0, 0, 1, 0]))
    w.update()
    out += w.seal(21, bytes([1, 90]))
    out += w.seal(21, bytes([1, 0]))
    return out, records_before


def alert(description):
    return plain(21, bytes([2, description]))


def protected_alert(description):
    return Direction(WRITE_SECRET).seal(21, bytes([2, description]))


lines = []
out = lines.append

written, records_before = scripted_written()
out("scripted written_records=%d" % records_before)
for text in (MESSAGE_A,):
    out("scripted handshake " + describe(text))
out("scripted handshake " + describe(MESSAGE_B))
out("scripted handshake " + describe(MESSAGE_C))
out("scripted handshake " + describe(FINISHED))
out("scripted read_records=5")
for data in DATA[:5]:
    out("scripted data " + describe(data))
out("scripted end close_notify=true")
out("scripted written_records=0")
out("scripted send_after_close_notify Closed")
out("scripted sent_close_notify=true")
out("scripted written " + describe(written))

A = lambda d: alert(d).hex()
P = lambda d: protected_alert(d).hex()
NONE = ""


def case(name, results, alert_seen, sent):
    out("case %s | %s | alert=%d | sent=%s" % (name, " | ".join(results), alert_seen, sent))


MAC = "err:BadRecordMac"
CLOSED = "err:Closed"
UNEXPECTED = "err:UnexpectedMessage"
DECODE = "err:DecodeError"
TRUNCATED = "err:Truncated"

case("flipped_body", [MAC, CLOSED], -1, A(20))
case("flipped_body_protected_alert", [MAC], -1, P(20))
case("replayed_record", ["data:10", MAC], -1, A(20))
case("reordered_records", [MAC], -1, A(20))
case("skipped_record", [MAC], -1, A(20))
case("protected_header_over_16640", ["err:RecordOverflow"], -1, A(22))
case("protected_header_16640_then_eof", [TRUNCATED], -1, NONE)
case("plain_header_over_16384", ["err:RecordOverflow"], -1, A(22))
case("content_16385", ["err:RecordOverflow"], -1, A(22))
case("not_tls", ["err:BadVersion"], -1, A(70))
case("version_2", ["err:BadVersion"], -1, A(70))
case("version_3_1_accepted", ["hs:12"], -1, NONE)
case("eof_in_header", [TRUNCATED, CLOSED], -1, NONE)
case("eof_in_body", [TRUNCATED], -1, NONE)
case("eof_after_data", ["data:10", TRUNCATED, CLOSED], -1, NONE)
case("eof_at_start", [TRUNCATED], -1, NONE)
case("eof_after_partial_message", [TRUNCATED], -1, NONE)
case("close_notify", ["data:10", "end", "end"], -1, NONE)
case("close_notify_in_receive", ["end", "end"], -1, NONE)
case("close_notify_for_handshake", [UNEXPECTED], -1, A(10))
case("close_notify_plain", ["end", "end"], -1, NONE)
case("fatal_alert", ["err:PeerAlert", CLOSED], 40, NONE)
case("warning_alert_is_fatal", ["err:PeerAlert"], 40, NONE)
case("user_canceled_ignored", ["data:10"], -1, NONE)
for name in ("alert_one_octet", "alert_three_octets", "alerts_coalesced", "alert_fragmented", "alert_empty"):
    case(name, [DECODE], -1, A(50))
for name in ("alert_between_fragments", "data_between_fragments", "empty_handshake_record",
             "empty_handshake_record_plain", "outer_handshake", "outer_alert", "all_zero_plaintext",
             "inner_change_cipher_spec", "inner_unknown_type", "plain_unknown_type",
             "plain_application_data"):
    case(name, [UNEXPECTED], -1, A(10))
case("ccs_between_plain_messages", ["hs:12", "hs:8"], -1, NONE)
case("ccs_while_protected", ["hs:12"], -1, NONE)
for name in ("ccs_body_2", "ccs_body_two_octets", "ccs_empty"):
    case(name, [UNEXPECTED], -1, A(10))
case("ccs_after_finished", ["hs:36", UNEXPECTED], -1, A(10))
case("ccs_before_finished_in_record", ["hs:12", TRUNCATED], -1, NONE)
case("handshake_message_196608", ["err:HandshakeTooLarge"], -1, A(50))
case("handshake_message_131072", ["hs:131072", TRUNCATED], -1, NONE)
case("handshake_message_131073", ["err:HandshakeTooLarge"], -1, A(50))
case("handshake_for_application", [UNEXPECTED], -1, A(10))
case("certificate_after_handshake", [UNEXPECTED], -1, A(10))
case("session_ticket_skipped", ["data:10"], -1, NONE)
for name in ("key_update_value_2", "key_update_two_octets", "key_update_empty"):
    case(name, [DECODE], -1, P(50))
case("key_update_without_write_keys", [UNEXPECTED], -1, A(10))
case("two_messages_in_a_record", ["hs:12", "hs:8", TRUNCATED], -1, NONE)
case("message_across_records", ["hs:12", TRUNCATED], -1, NONE)
case("sends_after_failure", [MAC, CLOSED, CLOSED], -1, P(20))

out("case key_change_with_partial_message | hs:12 |")
out("case key_change_with_partial_message | install UnexpectedMessage | sent=" + A(10))
out("case key_change_with_partial_message | send Closed")
out("case key_change_with_whole_message_buffered | hs:12 |")
out("case key_change_with_whole_message_buffered | install UnexpectedMessage")

out("loopback server handshake " + describe(message(1, pattern(36000, 3, 3))))
out("loopback server data " + describe(pattern(50000, 7, 7)))
out("loopback client data " + describe(pattern(300, 3, 5)))
out("loopback server data " + describe(pattern(1000, 1, 1)))
out("loopback client data " + describe(pattern(2000, 1, 2)))
out("loopback records client_sent=1 server_read=1")
out("loopback server end " + describe(b""))
out("loopback client end " + describe(b""))
out("loopback cut data:10 err:Truncated")

if len(sys.argv) > 1 and sys.argv[1] == "--args":
    print(scripted_wire().hex())
else:
    print("\n".join(lines))
