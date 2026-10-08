#!/usr/bin/env python3
"""The other half of `t_consttime.m31`: the same lines, from `hmac.compare_digest`.

`test.sh` diffs the two outputs. Nothing here may read the language's answer.
"""
import hmac


def pattern(n, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(n))


def show(label, left, right):
    print("%s %d %d %s" % (label, len(left), len(right), "true" if hmac.compare_digest(left, right) else "false"))


show("empty-empty", b"", b"")
show("empty-one", b"", b"\x00")
show("one-empty", b"\x00", b"")
show("prefix", bytes([1, 2, 3]), bytes([1, 2, 3, 0]))
show("shorter", bytes([1, 2, 3, 4]), bytes([1, 2, 3]))
show("zeros-sizes", bytes(16), bytes(17))

for size in [1, 2, 15, 16, 17, 32, 33, 64]:
    base = pattern(size, 7, 3)
    show("same-%d" % size, base, pattern(size, 7, 3))
    for position in range(size):
        for bit in [0, 3, 7]:
            changed = bytearray(pattern(size, 7, 3))
            changed[position] ^= 1 << bit
            show("flip-%d-%d-%d" % (size, position, bit), base, bytes(changed))

for left in range(256):
    for right in [0, 1, left, 255 - left, 128]:
        show("pair-%d-%d" % (left, right), bytes([left]), bytes([right]))
