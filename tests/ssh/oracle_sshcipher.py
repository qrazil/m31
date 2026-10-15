#!/usr/bin/env python3
"""Checks `lib/ssh/sshcipher.m31` (through the `t_ssh_cipher` binary given as
argv[1]) against an independent implementation of the SSH binary packet
protocol built on OpenSSL via the `cryptography` package:

  * packets the oracle builds (random payloads, random valid padding, 1 to
    200-odd octets, several packets in a row, sequence numbers including the
    32-bit wrap) must open to exactly the payload;
  * packets m31 builds must open under the oracle's own reader, with a legal
    padding length and alignment, for the same payload;
  * every single-byte corruption of a packet must be refused, never opened.

Kinds: chacha20-poly1305@openssh.com, aes{128,256}-gcm@openssh.com (RFC 5647),
aes{128,256}-ctr with hmac-sha2-256 and hmac-sha2-256-etm@openssh.com.
"""
import hashlib
import hmac
import os
import random
import struct
import subprocess
import sys

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from cryptography.hazmat.primitives import poly1305

BIN = sys.argv[1]
rnd = random.Random(20261015)


def chacha_block(key, counter, seq8, n):
    nonce = counter.to_bytes(8, "little") + seq8
    return Cipher(algorithms.ChaCha20(key, nonce), mode=None).encryptor().update(bytes(n))


class Ref:
    """One direction of one connection."""

    def __init__(self, kind, key, iv, mac):
        self.kind, self.key, self.iv, self.mac = kind, key, bytearray(iv), mac
        if kind in ("ctr", "ctretm"):
            self.stream = Cipher(algorithms.AES(key), modes.CTR(bytes(iv))).encryptor()
            self.dstream = Cipher(algorithms.AES(key), modes.CTR(bytes(iv))).decryptor()

    def pad(self, covered, block, extra_blocks=0):
        pad = block - covered % block
        if pad < 4:
            pad += block
        pad += block * extra_blocks
        while pad > 255:
            pad -= block
        return pad

    def build(self, payload, seq, extra=0):
        k = self.kind
        if k == "chacha":
            k2, k1 = self.key[:32], self.key[32:]
            pad = self.pad(1 + len(payload), 8, extra)
            body = bytes([pad]) + payload + os.urandom(pad)
            seq8 = struct.pack(">Q", seq & 0xFFFFFFFF)
            ln = struct.pack(">I", len(body))
            ln_ct = bytes(a ^ b for a, b in zip(ln, chacha_block(k1, 0, seq8, 4)))
            ks = chacha_block(k2, 1, seq8, len(body))
            body_ct = bytes(a ^ b for a, b in zip(body, ks))
            otk = chacha_block(k2, 0, seq8, 64)[:32]
            tag = poly1305.Poly1305.generate_tag(otk, ln_ct + body_ct)
            return ln_ct + body_ct + tag
        if k == "gcm":
            pad = self.pad(1 + len(payload), 16, extra)
            body = bytes([pad]) + payload + os.urandom(pad)
            ln = struct.pack(">I", len(body))
            out = ln + AESGCM(self.key).encrypt(bytes(self.iv), body, ln)
            self.bump()
            return out
        if k == "ctretm":
            pad = self.pad(1 + len(payload), 16, extra)
            body = bytes([pad]) + payload + os.urandom(pad)
            ln = struct.pack(">I", len(body))
            ct = self.stream.update(body)
            mac = hmac.new(self.mac, struct.pack(">I", seq & 0xFFFFFFFF) + ln + ct, hashlib.sha256).digest()
            return ln + ct + mac
        pad = self.pad(5 + len(payload), 16, extra)
        pkt = struct.pack(">I", 1 + len(payload) + pad) + bytes([pad]) + payload + os.urandom(pad)
        mac = hmac.new(self.mac, struct.pack(">I", seq & 0xFFFFFFFF) + pkt, hashlib.sha256).digest()
        return self.stream.update(pkt) + mac

    def bump(self):
        c = int.from_bytes(self.iv[4:], "big") + 1
        self.iv[4:] = (c & (2**64 - 1)).to_bytes(8, "big")

    def read(self, wire, seq):
        """-> payload, checking structure; raises on any violation."""
        k = self.kind
        if k == "chacha":
            k2, k1 = self.key[:32], self.key[32:]
            seq8 = struct.pack(">Q", seq & 0xFFFFFFFF)
            ln_ct, rest = wire[:4], wire[4:]
            n = struct.unpack(">I", bytes(a ^ b for a, b in zip(ln_ct, chacha_block(k1, 0, seq8, 4))))[0]
            assert len(rest) == n + 16, "length"
            otk = chacha_block(k2, 0, seq8, 64)[:32]
            poly1305.Poly1305.verify_tag(otk, ln_ct + rest[:n], rest[n:])
            body = bytes(a ^ b for a, b in zip(rest[:n], chacha_block(k2, 1, seq8, n)))
            align = 8
            covered = n
        elif k == "gcm":
            ln, rest = wire[:4], wire[4:]
            n = struct.unpack(">I", ln)[0]
            assert len(rest) == n + 16, "length"
            body = AESGCM(self.key).decrypt(bytes(self.iv), rest, ln)
            self.bump()
            align, covered = 16, n
        elif k == "ctretm":
            ln, rest = wire[:4], wire[4:]
            n = struct.unpack(">I", ln)[0]
            assert len(rest) == n + 32, "length"
            want = hmac.new(self.mac, struct.pack(">I", seq & 0xFFFFFFFF) + ln + rest[:n], hashlib.sha256).digest()
            assert hmac.compare_digest(want, rest[n:]), "mac"
            body = self.dstream.update(rest[:n])
            align, covered = 16, n
        else:
            first = self.dstream.update(wire[:16])
            n = struct.unpack(">I", first[:4])[0]
            assert len(wire) == 4 + n + 32, "length"
            plain = first + self.dstream.update(wire[16:4 + n])
            want = hmac.new(self.mac, struct.pack(">I", seq & 0xFFFFFFFF) + plain, hashlib.sha256).digest()
            assert hmac.compare_digest(want, wire[4 + n:]), "mac"
            body = plain[4:]
            align, covered = 16, n + 4
        assert covered % align == 0, "alignment"
        pad = body[0]
        assert 4 <= pad <= len(body) - 1, "padding"
        return body[1:len(body) - pad]


KINDS = [
    ("chacha", 64, 0, 0),
    ("gcm", 16, 12, 0),
    ("gcm", 32, 12, 0),
    ("ctr", 16, 16, 32),
    ("ctr", 32, 16, 32),
    ("ctretm", 16, 16, 32),
    ("ctretm", 32, 16, 32),
]


def hx(b):
    return b.hex() if b else "-"


def run_m31(lines):
    p = subprocess.run([BIN], input="\n".join(lines) + "\n", capture_output=True, text=True, timeout=600)
    if p.returncode != 0:
        print("FAIL t_ssh_cipher exited %d: %s" % (p.returncode, p.stderr[-300:]))
        sys.exit(1)
    return p.stdout.strip().split("\n")


checks = 0
failures = []


def check(cond, label):
    global checks
    checks += 1
    if not cond:
        failures.append(label)


# 1. oracle builds, m31 opens --------------------------------------------------------
lines, expect = [], []
for kind, ks, ivs, ms in KINDS:
    for seq0 in (0, 3, 4294967294):
        for trial in range(3):
            key, iv, mac = os.urandom(ks), os.urandom(ivs), os.urandom(ms)
            ref = Ref(kind, key, iv, mac)
            payloads = [os.urandom(rnd.choice([1, 2, 5, 11, 15, 16, 17, 31, 32, 47, 100, 199, 1000, 4000])) for _ in range(5)]
            wires = [ref.build(pl, seq0 + i, extra=rnd.choice([0, 0, 1, 3])) for i, pl in enumerate(payloads)]
            lines.append("open %s %s %s %s %d %s" % (kind, hx(key), hx(iv), hx(mac), seq0, " ".join(w.hex() for w in wires)))
            expect.append("payload " + " ".join(pl.hex() for pl in payloads))
got = run_m31(lines)
for i, (g, e) in enumerate(zip(got, expect)):
    check(g == e, "open vector %d: %s != %s" % (i, g[:60], e[:60]))
check(len(got) == len(expect), "open: one answer per vector")

# 2. m31 builds, oracle opens ----------------------------------------------------------
lines, meta = [], []
for kind, ks, ivs, ms in KINDS:
    for seq0 in (0, 7, 4294967295):
        key, iv, mac = os.urandom(ks), os.urandom(ivs), os.urandom(ms)
        payloads = [os.urandom(rnd.choice([1, 3, 8, 12, 13, 14, 15, 16, 17, 33, 64, 300, 2000, 20000])) for _ in range(6)]
        lines.append("seal %s %s %s %s %d %s" % (kind, hx(key), hx(iv), hx(mac), seq0, " ".join(p.hex() for p in payloads)))
        meta.append((kind, key, iv, mac, seq0, payloads))
got = run_m31(lines)
for (kind, key, iv, mac, seq0, payloads), g in zip(meta, got):
    wires = g.split(" ")[1:]
    ref = Ref(kind, key, iv, mac)
    ok = len(wires) == len(payloads)
    for i, (w, pl) in enumerate(zip(wires, payloads)):
        try:
            ok = ok and ref.read(bytes.fromhex(w), seq0 + i) == pl
        except Exception as e:  # any assertion is a failure
            ok = False
            failures.append("seal %s seq %d packet %d: %r" % (kind, seq0, i, e))
    check(ok, "m31-sealed packets (%s, %d keybytes, seq %d) open under OpenSSL" % (kind, len(key), seq0))

# 3. every single-byte corruption is refused ------------------------------------------------
lines, count = [], 0
for kind, ks, ivs, ms in KINDS:
    key, iv, mac = os.urandom(ks), os.urandom(ivs), os.urandom(ms)
    ref = Ref(kind, key, iv, mac)
    wire = ref.build(os.urandom(40), 9)
    for i in range(len(wire)):
        bad = bytearray(wire)
        bad[i] ^= 1 << rnd.randrange(8)
        lines.append("open %s %s %s %s 9 %s" % (kind, hx(key), hx(iv), hx(mac), bytes(bad).hex()))
got = run_m31(lines)
for l, g in zip(lines, got):
    count += 1
    check(g.startswith("error"), "a corrupted packet was refused (%s)" % l.split()[1])

# 4. absurd lengths are refused before anything is read ------------------------------------------
lines = []
for kind, ks, ivs, ms in KINDS:
    key, iv, mac = os.urandom(ks), os.urandom(ivs), os.urandom(ms)
    for header in (b"\xff\xff\xff\xff", b"\x00\x00\x00\x00", b"\x7f\xff\xff\xff", b"\x00\x00\x88\xb9"):
        lines.append("open %s %s %s %s 0 %s" % (kind, hx(key), hx(iv), hx(mac), (header + bytes(48)).hex()))
got = run_m31(lines)
for l, g in zip(lines, got):
    check(g.startswith("error"), "absurd length refused (%s)" % l.split()[1])

if failures:
    for f in failures[:20]:
        print("FAIL " + f)
    sys.exit(1)
print("%d checks, 0 failures" % checks)
