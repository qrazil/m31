#!/usr/bin/env python3
"""The other half of `t_hmac.m31`: the same lines, from Python's hmac/hashlib.

`test.sh` diffs the two outputs. Nothing here may read the language's answer.
The RFC 4231 cases are also asserted against the published digests below, so
the oracle itself is checked against the standard, not only against the
harness.
"""
import hashlib
import hmac


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


def mac(name, key, message):
    return hmac.new(key, message, name).hexdigest()


# RFC 4231 §4: published HMAC-SHA-256 digests, asserted against hashlib.
RFC4231_SHA256 = {
    1: "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
    2: "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
    3: "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
    4: "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
    6: "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
    7: "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
}
CASES = {
    1: (b"\x0b" * 20, b"Hi There"),
    2: (b"Jefe", b"what do ya want for nothing?"),
    3: (b"\xaa" * 20, b"\xdd" * 50),
    4: (pattern(25, 1, 1), b"\xcd" * 50),
    6: (b"\xaa" * 131, b"Test Using Larger Than Block-Size Key - Hash Key First"),
    7: (b"\xaa" * 131, b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm."),
}

out = []
for number, (key, data) in CASES.items():
    got = mac("sha256", key, data)
    assert got == RFC4231_SHA256[number], "RFC 4231 case %d disagrees with hashlib" % number
    out.append("rfc4231-%d %s" % (number, got))

MESSAGE_SIZES = [0, 1, 55, 56, 63, 64, 65, 111, 112, 127, 128, 129, 200, 1000]
KEY_SIZES = [0, 1, 20, 32, 63, 64, 65, 100, 128, 129, 200]

for key_size in KEY_SIZES:
    for message_size in MESSAGE_SIZES:
        key = pattern(key_size, 11, 5)
        message = pattern(message_size, 7, 13)
        out.append("sha256 key%d msg%d %s" % (key_size, message_size, mac("sha256", key, message)))

print("\n".join(out))
