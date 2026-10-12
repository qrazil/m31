#!/usr/bin/env python3
"""Expected output of tls12_record.m31.

Seals and opens TLS 1.2 records with the `cryptography` package's AEADs
(argument: chacha, aes128 or aes256), written out from the RFCs and nothing
else:

  RFC 5246 6.2.3.3  additional_data = seq_num(8) || type || 03 03 || length
  RFC 5288 3        AES-GCM: nonce = salt(4) || explicit(8); the body starts
                    with the explicit nonce (here: the sequence number)
  RFC 7905 2        ChaCha20-Poly1305: nonce = iv(12) XOR sequence, nothing
                    explicit
  limits            body <= 2^14 + 256 (16640); plaintext <= 2^14

Nothing here imports or calls the program under test.
"""
import hashlib
import sys

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers.aead import AESGCM, ChaCha20Poly1305

MAX_PLAINTEXT = 1 << 14
MAX_BODY = (1 << 14) + 256
MAX_SEQUENCE = (1 << 63) - 1

SUITE = sys.argv[1] if len(sys.argv) > 1 else "chacha"
KEY_SIZE = {"chacha": 32, "aes128": 16, "aes256": 32}[SUITE]
SALT_SIZE = 12 if SUITE == "chacha" else 4
EXPLICIT = 0 if SUITE == "chacha" else 8


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


KEY = pattern(KEY_SIZE, 5, 1)
IV = pattern(SALT_SIZE, 11, 7)


def aead():
    return ChaCha20Poly1305(KEY) if SUITE == "chacha" else AESGCM(KEY)


def additional(sequence, content_type, size):
    return sequence.to_bytes(8, "big") + bytes([content_type, 3, 3]) + size.to_bytes(2, "big")


def nonce_for(sequence, explicit):
    if SUITE == "chacha":
        padded = sequence.to_bytes(12, "big")
        return bytes(a ^ b for a, b in zip(IV, padded))
    return IV + explicit


def seal(sequence, content_type, content):
    explicit = sequence.to_bytes(8, "big") if EXPLICIT else b""
    sealed = aead().encrypt(nonce_for(sequence, explicit), content, additional(sequence, content_type, len(content)))
    body = explicit + sealed
    return bytes([content_type, 3, 3]) + len(body).to_bytes(2, "big") + body


def outcome(wire, sequence):
    header, body = wire[:5], wire[5:]
    if sequence >= MAX_SEQUENCE:
        return "SequenceExhausted"
    if len(body) > MAX_BODY:
        return "RecordOverflow"
    plain_size = len(body) - EXPLICIT - 16
    if plain_size < 0:
        return "BadRecordMac"
    if plain_size > MAX_PLAINTEXT:
        return "RecordOverflow"
    try:
        plain = aead().decrypt(
            nonce_for(sequence, body[:EXPLICIT]), body[EXPLICIT:], additional(sequence, header[0], plain_size)
        )
    except InvalidTag:
        return "BadRecordMac"
    return "Ok type=%d size=%d sha256=%s" % (header[0], len(plain), hashlib.sha256(plain).hexdigest())


SIZES = [0, 1, 2, 15, 16, 17, 63, 64, 65, 255, 1000, 16383, 16384]
TYPES = [22, 23, 21, 23]
SEQUENCES = [0, 1, 2, 255, 256, 0xFFFFFFFF, 0x100000000, 0x4000000000000003]

for i, size in enumerate(SIZES):
    sequence = SEQUENCES[i % len(SEQUENCES)]
    content_type = TYPES[i % 4]
    wire = seal(sequence, content_type, pattern(size, 3 + i, i))
    shown = wire.hex()
    if len(wire) > 100:
        shown = "%d %s" % (len(wire), hashlib.sha256(wire).hexdigest())
    print("seal %d %d %d %s" % (size, sequence, content_type, shown))
    print("open %d %s" % (size, outcome(wire, sequence)))

good = seal(5, 23, pattern(40, 3, 1))
print("open_good " + outcome(good, 5))
print("open_wrong_sequence " + outcome(good, 6))
print("open_wrong_sequence_zero " + outcome(good, 0))
for i in range(len(good)):
    bent = bytearray(good)
    bent[i] ^= 1
    if i < 5:
        print("flip_header %d %s" % (i, outcome(bytes(bent), 5)))
    else:
        print("flip_body %d %s" % (i - 5, outcome(bytes(bent), 5)))
for cut in range(5, len(good)):
    print("short %d %s" % (cut - 5, outcome(good[:cut], 5)))
print("trailing_octet " + outcome(good + b"\x00", 5))
empty = seal(9, 23, b"")
print("empty_record " + outcome(empty, 9))
print("empty_wrong_type " + outcome(bytes([22]) + empty[1:], 9))
print("exhausted " + outcome(good, 0x7FFFFFFFFFFFFFFF))
