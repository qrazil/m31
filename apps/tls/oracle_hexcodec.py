#!/usr/bin/env python3
"""The other half of `t_hexcodec.m31`: the same lines, from Python.

`bytes.fromhex` skips whitespace and `hexcodec.decode` refuses it, so the
digits are checked here first, strictly, in the same order the module does:
a bad character before an odd length.
"""
import re


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


def outcome(text):
    raw = text.encode("utf-8")
    if re.fullmatch(rb"[0-9a-fA-F]*", raw) is None:
        return "error digit"
    if len(raw) % 2 != 0:
        return "error length"
    return "ok " + bytes.fromhex(text).hex()


print("empty [" + b"".hex() + "]")
for size in range(41):
    data = pattern(size, 37, size)
    text = data.hex()
    print("encode %d %s" % (size, text))
    print("roundtrip %d %s" % (size, outcome(text)))

every = pattern(256, 1, 0)
print("every-octet " + every.hex())
print("every-octet-roundtrip " + outcome(every.hex()))

for text in ["", "00", "ff", "FF", "aB", "Ab", "0123456789abcdef", "0123456789ABCDEF", "DeadBEEF", "deadbeef", "00FF10"]:
    print("decode [%s] %s" % (text, outcome(text)))

for text in ["0", "abc", "12345", "zz", "0g", "g0", "G1", "0x00", " 00", "00 ", "0 0", "ab\n", "-1", "+1", "a:", "@A", "`a", "/0", "abz", "z", "é1", "00é"]:
    print("refuse [%s] %s" % (text, outcome(text)))

print("literal " + bytes.fromhex("00ff10").hex())
print("literal-empty " + str(len(b"".hex())))
print("literal-upper " + bytes.fromhex("ABCDEF").hex())
