#!/usr/bin/env python3
"""Oracle for `zlib.inflate_at`/`zlib.decompress_at`: several independent
streams concatenated back to back in one buffer, each one decoded starting
exactly where the previous stream's reported `used` said the next one
begins -- a packfile's own shape, but with none of `apps/git/pack.m31`'s
framing (no idx, no object headers, no deltas) around it, so a bug in the
mid-offset codec and a bug in the packfile format reader cannot hide one
behind the other.

    python3 apps/git/oracle_inflate_at.py <dir>

Writes `<dir>/streams.zlib` (RFC 1950 zlib streams back to back, for
`zlib.decompress_at`) and `<dir>/streams.raw` (raw RFC 1951 DEFLATE streams
back to back, for `zlib.inflate_at`), built from the same payloads at
different compression levels and block shapes -- empty, tiny, a run long
enough to need more than one stored block at level 0, incompressible, and
the full byte range -- and prints one line per stream, in the order it
appears in the buffer:

    <kind> <index> <start> ok <used> <sha1-of-the-payload>

`t_inflate_at.m31` walks each buffer with `decompress_at`/`inflate_at`,
advancing by the `used` it gets back each time, and prints the same line
shape for `t.sh` to diff against this.
"""
import hashlib
import os
import sys
import zlib

PAYLOADS_AND_LEVELS = [
    (b"", -1),
    (b"a", -1),
    (b"hello, world! " * 3 + b"\n", 9),
    (os.urandom(37), 6),
    (b"the quick brown fox jumps over the lazy dog\n" * 50, 9),
    (bytes(range(256)) * 4, 1),
    # Over 65535 octets at level 0: more than one stored block, still one
    # DEFLATE stream that only ends at its own end-of-block/BFINAL.
    (b"x" * 70000, 0),
    (os.urandom(5000), 0),
]


def main():
    d = sys.argv[1]
    os.makedirs(d, exist_ok=True)
    zlib_buf = bytearray()
    raw_buf = bytearray()
    zlib_lines = []
    raw_lines = []
    for i, (payload, level) in enumerate(PAYLOADS_AND_LEVELS):
        digest = hashlib.sha1(payload).hexdigest()

        start_z = len(zlib_buf)
        stream_z = zlib.compress(payload, level)
        assert zlib.decompress(stream_z) == payload
        zlib_buf += stream_z
        zlib_lines.append("zlib %d %d ok %d %s" % (i, start_z, len(stream_z), digest))

        start_r = len(raw_buf)
        co = zlib.compressobj(level, zlib.DEFLATED, -15)
        stream_r = co.compress(payload) + co.flush()
        raw_buf += stream_r
        raw_lines.append("raw %d %d ok %d %s" % (i, start_r, len(stream_r), digest))

    # The language program reads the whole `streams.zlib` buffer to the end
    # before it starts on `streams.raw` (two separate walks, not interleaved),
    # so the lines here are in that same order -- both sides walk a buffer
    # start to finish, and the fixture files are what is actually interleaved
    # on disk between the two kinds, not this list.
    lines = zlib_lines + raw_lines

    with open(os.path.join(d, "streams.zlib"), "wb") as f:
        f.write(bytes(zlib_buf))
    with open(os.path.join(d, "streams.raw"), "wb") as f:
        f.write(bytes(raw_buf))
    print("\n".join(lines))


if __name__ == "__main__":
    main()
