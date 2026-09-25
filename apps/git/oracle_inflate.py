#!/usr/bin/env python3
"""Fixtures for `t_inflate.src`, and the expected answers, from Python's zlib.

    python3 apps/git/oracle_inflate.py <dir>

writes one `.z` file per case into <dir> and prints, for each, the line the
language program must print:

    <name> ok <octets> <sha1-of-the-output>
    <name> err

Only "did it decompress, and to exactly what" is compared. The *message* on a
refusal is this program's own and no oracle has an opinion about it; what an
oracle can say is that a refusal was the right answer, and that is what `err`
records.

The good cases are mostly zlib's own output, because a decoder should be
tested against the encoder people actually run. Three are hand-built with the
bit writer below, because zlib will not emit them on demand: a fixed-Huffman
block (zlib picks dynamic as soon as a block is worth compressing), a stored
block with a deliberately wrong NLEN, and reserved block type 3.
"""
import hashlib
import os
import sys
import zlib


class BitWriter:
    """DEFLATE's bit order: header fields low bit first, Huffman codes high
    bit first. RFC 1951 §3.1.1."""

    def __init__(self):
        self.out = bytearray()
        self.cur = 0
        self.n = 0

    def bits(self, v, n):
        for i in range(n):
            self.cur |= ((v >> i) & 1) << self.n
            self.n += 1
            if self.n == 8:
                self.out.append(self.cur)
                self.cur = 0
                self.n = 0

    def code(self, v, n):
        for i in range(n - 1, -1, -1):
            self.bits((v >> i) & 1, 1)

    def align(self):
        if self.n:
            self.out.append(self.cur)
            self.cur = 0
            self.n = 0

    def done(self):
        self.align()
        return bytes(self.out)


def fixed_literal(w, byte):
    """RFC 1951 §3.2.6's fixed literal/length code."""
    if byte < 144:
        w.code(0x30 + byte, 8)
    else:
        w.code(0x190 + byte - 144, 9)


def wrap(deflate, payload, cmf=0x78, flg=0x01, adler=None):
    a = zlib.adler32(payload) & 0xFFFFFFFF if adler is None else adler
    return bytes([cmf, flg]) + deflate + a.to_bytes(4, "big")


def build(d):
    """Every case, as (name, stream, expected-output-or-None)."""
    cases = []

    def good(name, payload, level=-1):
        cases.append((name, zlib.compress(payload, level), payload))

    def raw(name, stream, payload):
        cases.append((name, stream, payload))

    # --- ordinary streams, from zlib -----------------------------------------
    good("empty", b"")
    good("abc", b"abc")
    good("onebyte", b"\x00")
    good("allbytes", bytes(range(256)))
    good("text", b"the quick brown fox jumps over the lazy dog\n" * 300, 9)
    good("run", b"a" * 5000, 9)
    # A run whose match distance is shorter than its length, which is how
    # DEFLATE spells a repeat and the case a block copy gets wrong.
    good("overlap", b"abc" * 4000, 9)
    good("stored", os.urandom(1000), 0)
    # Over 65535 octets, so it is several stored blocks and not one.
    good("stored-multi", b"x" * 200000, 0)
    good("incompressible", os.urandom(65536), 9)
    good("big", (b"".join(b"line %d of a file that compresses\n" % i
                          for i in range(60000))), 6)

    # --- hand-built, because zlib will not emit them -------------------------
    w = BitWriter()
    w.bits(1, 1)          # final block
    w.bits(1, 2)          # fixed Huffman
    for b in b"fixed-huffman block, literals only":
        fixed_literal(w, b)
    w.code(0, 7)          # end of block
    raw("fixed", wrap(w.done(), b"fixed-huffman block, literals only"),
        b"fixed-huffman block, literals only")

    # A fixed block that also uses a match: literal 'a', then length 3
    # distance 1, which repeats it.
    w = BitWriter()
    w.bits(1, 1)
    w.bits(1, 2)
    fixed_literal(w, ord("a"))
    w.code(0x100 + 257 - 256, 7)   # length symbol 257 -> length 3, 7 bits
    w.code(0, 5)                   # distance symbol 0 -> distance 1
    w.code(0, 7)
    raw("fixed-match", wrap(w.done(), b"aaaa"), b"aaaa")

    # A stored block, hand-built so its header is visible.
    w = BitWriter()
    w.bits(1, 1)
    w.bits(0, 2)
    w.align()
    body = b"stored by hand"
    w.out += len(body).to_bytes(2, "little")
    w.out += (len(body) ^ 0xFFFF).to_bytes(2, "little")
    w.out += body
    raw("stored-byhand", wrap(w.done(), body), body)

    # --- refusals ------------------------------------------------------------
    raw("bad-empty", b"", None)
    raw("bad-onebyte", b"\x78", None)
    raw("bad-header", b"\x00\x00\x01\x00", None)
    # FDICT set, and FCHECK fixed up so the header itself is well formed.
    cmf, flg = 0x78, 0x20
    flg |= (31 - (cmf * 256 + flg) % 31) % 31
    raw("bad-preset", bytes([cmf, flg]) + zlib.compress(b"hi")[2:], None)

    ok = zlib.compress(b"the quick brown fox jumps over the lazy dog\n" * 30, 9)
    for cut in (3, 8, len(ok) // 2, len(ok) - 5, len(ok) - 1):
        raw("bad-cut%d" % cut, ok[:cut], None)
    # A flipped bit in the middle of the compressed body: either the stream
    # stops making sense, or it decodes to something else and the Adler-32
    # catches it. Both are refusals, and zlib agrees.
    flipped = bytearray(ok)
    flipped[len(ok) // 2] ^= 0x55
    raw("bad-flip", bytes(flipped), None)
    # A good stream with a mangled check value.
    raw("bad-adler", ok[:-4] + b"\x00\x00\x00\x00", None)

    # Reserved block type 3.
    w = BitWriter()
    w.bits(1, 1)
    w.bits(3, 2)
    raw("bad-blocktype", wrap(w.done(), b""), None)

    # A stored block whose NLEN is not the complement of its LEN.
    w = BitWriter()
    w.bits(1, 1)
    w.bits(0, 2)
    w.align()
    w.out += (4).to_bytes(2, "little")
    w.out += (0).to_bytes(2, "little")
    w.out += b"abcd"
    raw("bad-nlen", wrap(w.done(), b"abcd"), None)

    # A stored block that claims more octets than are there.
    w = BitWriter()
    w.bits(1, 1)
    w.bits(0, 2)
    w.align()
    w.out += (100).to_bytes(2, "little")
    w.out += (100 ^ 0xFFFF).to_bytes(2, "little")
    w.out += b"abcd"
    raw("bad-storedshort", wrap(w.done(), b""), None)

    # A dynamic block header claiming more literal/length codes than exist.
    w = BitWriter()
    w.bits(1, 1)
    w.bits(2, 2)
    w.bits(31, 5)     # HLIT  -> 288 codes, past the 286 that have meaning
    w.bits(29, 5)
    w.bits(15, 4)
    raw("bad-hlit", wrap(w.done(), b""), None)

    return cases


def main():
    d = sys.argv[1]
    os.makedirs(d, exist_ok=True)
    for old in os.listdir(d):
        os.remove(os.path.join(d, old))
    lines = []
    for name, stream, payload in build(d):
        with open(os.path.join(d, name + ".z"), "wb") as f:
            f.write(stream)
        try:
            got = zlib.decompress(stream)
        except Exception:
            got = None
        if payload is not None and got != payload:
            raise SystemExit("fixture %s: zlib does not agree with itself" % name)
        if got is None:
            lines.append("%s err" % name)
        else:
            lines.append("%s ok %d %s" % (name, len(got), hashlib.sha1(got).hexdigest()))
    lines.sort()
    print("\n".join(lines))


if __name__ == "__main__":
    main()
