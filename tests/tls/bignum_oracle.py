#!/usr/bin/env python3
"""Cases and expected answers for `lib/crypto/bignum.m31`, from Python's own
arbitrary-precision integers -- not from this project's code.

    python3 bignum_oracle.py CASES_OUT EXPECTED_OUT

CASES_OUT is read by `t_bignum.m31`, one case per line:

    op|modulus_hex|a_hex|b_hex        op is add sub mul pow inv cmp bits

(`mul` is a*b mod m through to_montgomery / montgomery_multiply /
from_montgomery; `pow` is a**b mod m; `inv` ignores b; `bits` ignores both m and b
beyond sizing.) EXPECTED_OUT has the line `index answer` the m31 program must
print for each case, results as fixed-width big-endian hex of the modulus's size.
"""
import random
import sys

rng = random.Random(24)


def is_prime(n):
    if n < 2:
        return False
    for small in (2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37):
        if n % small == 0:
            return n == small
    d, r = n - 1, 0
    while d % 2 == 0:
        d //= 2
        r += 1
    for _ in range(24):
        a = rng.randrange(2, n - 1)
        x = pow(a, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(r - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True


def random_prime(bits):
    while True:
        candidate = rng.getrandbits(bits) | (1 << (bits - 1)) | 1
        if is_prime(candidate):
            return candidate


P256 = 0xFFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF
N256 = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551
P384 = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFFFF0000000000000000FFFFFFFF
N384 = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFC7634D81F4372DDF581A0DB248B0A77AECEC196ACCC52973

PRIMES = [P256, N256, P384, N384, 2**255 - 19, 0xFFFFFD, 0xFFFFFFFB, random_prime(55), random_prime(160),
          random_prime(264), random_prime(520)]
COMPOSITES = [3 * 5 * 7 * 11 * 13, (2**127 - 1) * (2**89 - 1), 2**521 - 1 + 2 * 3 * 7 * 2**200, 0xFFFFFF - 1 | 1]
for i, c in enumerate(COMPOSITES):
    COMPOSITES[i] = c | 1


def size_of(m):
    return (m.bit_length() + 7) // 8


def edge_values(m):
    values = {0, 1, 2, 3, m - 1, m - 2, m // 2, m // 2 + 1}
    for k in range(1, m.bit_length() // 24 + 1):
        for delta in (-1, 0, 1):
            v = (1 << (24 * k)) + delta
            if 0 <= v < m:
                values.add(v)
    values.add((1 << (m.bit_length() - 1)) % m)
    values.add(((1 << m.bit_length()) - 1) % m)
    return sorted(v for v in values if 0 <= v < m)


cases = []
answers = []


def emit(op, m, a, b, answer):
    width = size_of(m)
    cases.append(f"{op}|{m:0{2 * width}x}|{a:0{2 * width}x}|{b:0{2 * width}x}")
    answers.append(answer)


def hexed(m, v):
    return f"{v:0{2 * size_of(m)}x}"


for m in PRIMES + COMPOSITES:
    edges = edge_values(m)
    randoms = [rng.randrange(m) for _ in range(6)]
    pool = edges + randoms
    pairs = [(a, b) for a in edges for b in edges][:150]
    pairs += [(rng.randrange(m), rng.randrange(m)) for _ in range(25)]
    pairs += [(rng.choice(pool), rng.choice(pool)) for _ in range(25)]
    for a, b in pairs:
        emit("add", m, a, b, hexed(m, (a + b) % m))
        emit("sub", m, a, b, hexed(m, (a - b) % m))
        emit("mul", m, a, b, hexed(m, a * b % m))
    for a in pool:
        for e in (0, 1, 2, rng.getrandbits(m.bit_length()), m - 2, m - 1):
            emit("pow", m, a, e % (1 << (24 * ((size_of(m) * 8 + 23) // 24))), hexed(m, pow(a, e, m)))
    if m in PRIMES:
        for a in pool:
            emit("inv", m, a, 0, hexed(m, pow(a, -1, m) if a else 0))
    # compare and bit_length work on any value that fits the width
    top = 1 << (8 * size_of(m))
    wide = [0, 1, top - 1, top - 2, m, m - 1, m + 1, rng.randrange(top), rng.randrange(top), 1 << (24 * (m.bit_length() // 24))]
    wide = [v % top for v in wide]
    for a in wide:
        emit("bits", m, a, 0, str(a.bit_length()))
        for b in wide[:6]:
            emit("cmp", m, a, b, str((a > b) - (a < b)))

with open(sys.argv[1], "w") as out:
    out.write("\n".join(cases) + "\n")
with open(sys.argv[2], "w") as out:
    out.write("".join(f"{i} {answer}\n" for i, answer in enumerate(answers)))
print(f"{len(cases)} cases", file=sys.stderr)
