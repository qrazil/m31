#!/usr/bin/env python3
"""The other half of `t_chacha20_openssh.m31`: the same lines, computed
independently through `cryptography`/OpenSSL instead of
`lib/crypto/chacha20poly1305.m31`'s `openssh_block`. `test.sh` diffs the two.

`chacha20-poly1305@openssh.com` (`/usr/share/doc/openssh/PROTOCOL.chacha20poly1305`)
uses the original (pre-IETF) ChaCha20 word layout: a 64-bit little-endian
counter across words 12-13, and an 8-octet nonce across words 14-15 --
different from RFC 8439's layout (a 32-bit counter in word 12, a 96-bit
nonce in words 13-15), which is all `cryptography`'s `ChaCha20` cipher
object exposes directly.

The two layouts are not independent code, though: both ultimately feed one
16-octet value into the same four state words (12, 13, 14, 15), as four
little-endian 32-bit words in order -- `cryptography`'s own API just labels
those first four octets "counter" and the last twelve "nonce". Handing it
`counter.to_bytes(8, "little") + nonce` instead of
`counter.to_bytes(4, "little") + nonce` produces exactly the openssh word
assignment, through the same real, independent ChaCha20 implementation
(OpenSSL) this project's other oracles already trust -- not a second,
hand-rolled permutation that could share a bug with `openssh_block` itself.

    pip install cryptography   # 46.0.4 when this was written
    python3 apps/ssh/oracle_chacha20_openssh.py
"""
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms

out = []


def openssh_block(key, counter, nonce):
    full = counter.to_bytes(8, "little") + nonce
    enc = Cipher(algorithms.ChaCha20(key, full), mode=None).encryptor()
    return enc.update(bytes(64))


# --- a handful of hand-picked edge cases -------------------------------------

ZERO32 = bytes(32)
ZERO8 = bytes(8)
ONES32 = bytes([0xFF] * 32)

out.append("fixed#0 " + openssh_block(ZERO32, 0, ZERO8).hex())
out.append("fixed#1 " + openssh_block(ZERO32, 1, ZERO8).hex())
out.append("fixed#2 " + openssh_block(ONES32, 0, bytes([0xFF] * 8)).hex())
# Exercises word 13 (the counter's high 32 bits) being nonzero -- the one
# state word `block`'s own IETF layout never sets to anything but part of
# the nonce, so a layout mix-up would be invisible without a case like this.
out.append("fixed#3 " + openssh_block(ZERO32, 1 << 32, ZERO8).hex())
out.append("fixed#4 " + openssh_block(ZERO32, (1 << 40) + 7, bytes(range(8))).hex())
out.append(
    "fixed#5 "
    + openssh_block(bytes(range(32)), 0, bytes(range(8, 16))).hex()
)


# --- a small deterministic sweep, shared in spirit with -----------------------
# `oracle_chacha20poly1305.py`'s own `Rng`: the same glibc-constants LCG, so
# this file and `t_chacha20_openssh.m31` produce the same stream of
# "random-looking" keys/counters/nonces from the same seed with no shared
# fixture file.
class Rng:
    def __init__(self, seed):
        self.state = seed

    def next(self):
        self.state = (self.state * 1_103_515_245 + 12_345) & 0x7FFF_FFFF
        return self.state

    def gen(self, n):
        return bytes(self.next() & 0xFF for _ in range(n))


rng = Rng(3)
for i in range(64):
    key = rng.gen(32)
    nonce = rng.gen(8)
    # Counters spanning both the low and the high 32-bit half, including a
    # handful of values that exercise bit 31 (the IETF layout's own ceiling)
    # and bit 32 (impossible to reach in that layout at all).
    counter = (rng.next() << 32) | rng.next() if i % 3 == 0 else rng.next() & 0xFFFF_FFFF
    out.append(f"sweep#{i} " + openssh_block(key, counter, nonce).hex())

print("\n".join(out))
