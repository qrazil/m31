#!/usr/bin/env python3
"""The other half of `t_sha256.src`: the same lines, from Python's hashlib.

`test.sh` diffs the two outputs. Nothing here may read the language's answer,
which is the whole point -- an oracle that has seen the result is a
tautology. The SHA-1 twin of this file is `apps/git/oracle_sha1.py`.
"""
import hashlib


def h(b):
    return hashlib.sha256(b).hexdigest()


out = []
out.append("empty " + h(b""))
out.append("abc " + h(b"abc"))
out.append("448 " + h(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"))
out.append("896 " + h(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"))

pattern = bytearray()
for n in range(261):
    out.append("len%d %s" % (n, h(bytes(pattern))))
    pattern.append((n * 7 + 13) & 0xFF)

million = b"a" * 1_000_000
out.append("million " + h(million))
for size in (1, 3, 63, 64, 65, 1000, 4096):
    # Chunking cannot change a digest, so the expectation is the same string;
    # it is printed per chunk size because the language side computes it per
    # chunk size and a diff has to line up.
    out.append("million-by-%d %s" % (size, h(million)))

print("\n".join(out))
