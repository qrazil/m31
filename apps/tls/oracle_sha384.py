#!/usr/bin/env python3
"""The other half of `t_sha384.m31`: the same lines, from Python's hashlib.

`test.sh` diffs the two outputs. Nothing here may read the language's answer,
which is the whole point -- an oracle that has seen the result is a
tautology. The SHA-256 twin of this file is `apps/ssh/oracle_sha256.py`.
"""
import hashlib


def h(b):
    return hashlib.sha384(b).hexdigest()


# FIPS 180-4 / RFC 6234 published SHA-384 digests, asserted against hashlib.
assert h(b"abc") == "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"
assert h(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu") == "09330c33f71147e83d192fc782cd1b4753111b173b3b05d22fa08086e3b0f712fcc7c71a557e2db966c3e9fa91746039"
assert h(b"") == "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b"

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
