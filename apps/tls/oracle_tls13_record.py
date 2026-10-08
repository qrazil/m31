#!/usr/bin/env python3
"""Expected output of t_tls13_record.m31.

Seals and opens TLS 1.3 records with the `cryptography` package's
ChaCha20Poly1305 or AESGCM (argument: chacha, aes128 or aes256) and RFC 8446 section 5.2-5.4 written out from the RFC:

  nonce = iv XOR (sequence as 64 bits big-endian, left-padded to 12 octets)
  record = 17 03 03 len || AEAD(key, nonce, content || type || zeros, aad=header)
  limits: body <= 2^14 + 256, content <= 2^14, plaintext must hold a non-zero octet

Nothing here imports or calls the program under test. Run with no arguments to
print the lines the m31 program must print.
"""
import hashlib

from cryptography.exceptions import InvalidTag
import sys

from cryptography.hazmat.primitives.ciphers.aead import AESGCM, ChaCha20Poly1305

MAX_PLAINTEXT = 1 << 14
MAX_CIPHERTEXT = (1 << 14) + 256
MAX_SEQUENCE = (1 << 63) - 1


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


SUITE = sys.argv[1] if len(sys.argv) > 1 else "chacha"
KEY_SIZE = {"chacha": 32, "aes128": 16, "aes256": 32}[SUITE]


def aead(key):
    return ChaCha20Poly1305(key) if SUITE == "chacha" else AESGCM(key)


KEY = pattern(KEY_SIZE, 5, 1)
IV = pattern(12, 11, 7)


def nonce(iv, sequence):
    return bytes(a ^ b for a, b in zip(iv, sequence.to_bytes(12, "big")))


def header(length):
    return bytes([23, 3, 3, length >> 8, length & 0xFF])


def seal_inner(key, iv, sequence, inner):
    head = header(len(inner) + 16)
    return head + aead(key).encrypt(nonce(iv, sequence), inner, head)


def seal(sequence, content_type, content, padding, key=KEY, iv=IV):
    assert len(content) <= MAX_PLAINTEXT
    return seal_inner(key, iv, sequence, content + bytes([content_type]) + bytes(padding))


def open_reference(wire, sequence, key=KEY, iv=IV):
    head, body = wire[:5], wire[5:]
    if sequence >= MAX_SEQUENCE:
        return "SequenceExhausted"
    if len(body) > MAX_CIPHERTEXT:
        return "RecordOverflow"
    try:
        inner = aead(key).decrypt(nonce(iv, sequence), body, head)
    except (InvalidTag, ValueError):
        return "BadRecordMac"
    end = len(inner)
    while end > 0 and inner[end - 1] == 0:
        end -= 1
    if end == 0:
        return "UnexpectedMessage"
    content = inner[: end - 1]
    if len(content) > MAX_PLAINTEXT:
        return "RecordOverflow"
    return "Ok type=%d size=%d sha256=%s" % (inner[end - 1], len(content), hashlib.sha256(content).hexdigest())


def outcome(wire, sequence, **keys):
    return open_reference(wire, sequence, **keys)


lines = []
out = lines.append

SEQUENCES = [0, 1, 2, 255, 256, 65535, 0xFFFFFFFF, 1 << 32, (1 << 40) + 5, (1 << 62) + 3, MAX_SEQUENCE - 1]
for s in SEQUENCES:
    out("nonce %d %s" % (s, nonce(IV, s).hex()))

SIZES = [0, 1, 2, 15, 16, 17, 63, 64, 65, 255, 1000, 16383, 16384]
TYPES = [22, 23, 21, 23]
PADDINGS = [0, 1, 7, 255]
for i, size in enumerate(SIZES):
    content_type = TYPES[i % 4]
    padding = PADDINGS[(i // 2) % 4]
    padding = min(padding, MAX_CIPHERTEXT - 16 - 1 - size)
    sequence = SEQUENCES[i % len(SEQUENCES)]
    wire = seal(sequence, content_type, pattern(size, 7, 3), padding)
    label = "seal seq=%d type=%d size=%d pad=%d" % (sequence, content_type, size, padding)
    out("%s wire=%d sha256=%s" % (label, len(wire), hashlib.sha256(wire).hexdigest()))
    if size <= 255:
        out("%s hex=%s" % (label, wire.hex()))
    out("open %d %s" % (i, outcome(wire, sequence)))

out("zero_content " + outcome(seal(4, 23, bytes(10), 3), 4))
out("trailing_zero_content " + outcome(seal(5, 22, bytes([1, 2, 0, 0]), 0), 5))

base = seal(9, 23, pattern(40, 3, 1), 3)
out("neg base " + outcome(base, 9))
for place in [0, 1, 2, 3, 4, 5, 6, 20, len(base) - 17, len(base) - 16, len(base) - 1]:
    flipped = bytearray(base)
    flipped[place] ^= 0x01
    out("neg flip_bit_%d %s" % (place, outcome(bytes(flipped), 9)))
flipped = bytearray(base)
flipped[30] ^= 0x80
out("neg flip_high_bit_30 " + outcome(bytes(flipped), 9))
out("neg replay_sequence_8 " + outcome(base, 8))
out("neg reorder_sequence_10 " + outcome(base, 10))
out("neg sequence_0 " + outcome(base, 0))
out("neg truncated_by_one " + outcome(base[:-1], 9))
out("neg truncated_to_tag " + outcome(base[: 5 + 16], 9))
out("neg shorter_than_tag " + outcome(base[: 5 + 15], 9))
out("neg header_only " + outcome(base[:5], 9))
out("neg extended_by_one " + outcome(base + b"\0", 9))
out("neg wrong_key " + outcome(base, 9, key=pattern(KEY_SIZE, 5, 2)))
out("neg wrong_iv " + outcome(base, 9, iv=pattern(12, 11, 8)))

largest = seal(0, 23, pattern(16384, 1, 0), 239)
out("limit largest_body %d %s" % (len(largest) - 5, outcome(largest, 0)))
out("neg body_16641 " + outcome(bytes([23, 3, 3, 0x41, 0x01]) + pattern(16641, 1, 0), 0))
out("neg body_65535 " + outcome(bytes([23, 3, 3, 0xFF, 0xFF]) + pattern(65535, 1, 0), 0))
out("neg content_16385 " + outcome(seal_inner(KEY, IV, 3, pattern(16385, 1, 0) + bytes([23])), 3))
out("neg content_16600 " + outcome(seal_inner(KEY, IV, 3, pattern(16600, 1, 0) + bytes([22])), 3))
out("limit content_16384_padded " + outcome(seal_inner(KEY, IV, 3, pattern(16384, 1, 0) + bytes([22]) + bytes(100)), 3))

out("neg all_zero_plaintext " + outcome(seal_inner(KEY, IV, 6, bytes(20)), 6))
out("neg empty_plaintext " + outcome(seal_inner(KEY, IV, 6, b""), 6))
out("empty_content_with_type " + outcome(seal_inner(KEY, IV, 6, bytes([22])), 6))

# Sealing at the last sequence number is refused by rule, not by the cipher.
out("neg seal_at_max_sequence SequenceExhausted")
out("neg open_at_max_sequence " + outcome(base, MAX_SEQUENCE))
out("open_at_max_sequence_minus_one " + outcome(seal(MAX_SEQUENCE - 1, 21, bytes([1, 0]), 0), MAX_SEQUENCE - 1))

out("plain " + bytes([22, 3, 3, 0, 3, 1, 2, 3]).hex())
out("plain_empty " + bytes([23, 3, 3, 0, 0]).hex())

print("\n".join(lines))
