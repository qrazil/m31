import os, hashlib

p = 2**255 - 19
def inv(x): return pow(x, p-2, p)

# ---------- X25519 (RFC 7748) ----------
A24 = 121665  # (486662-2)/4

def decodeLittleEndian(b):
    return int.from_bytes(b, 'little')

def decodeUCoordinate(u_bytes):
    u = bytearray(u_bytes)
    u[31] &= 0x7f
    return decodeLittleEndian(bytes(u)) % p

def decodeScalar25519(k_bytes):
    k = bytearray(k_bytes)
    k[0] &= 248
    k[31] &= 127
    k[31] |= 64
    return decodeLittleEndian(bytes(k))

def encodeUCoordinate(u):
    return (u % p).to_bytes(32, 'little')

def x25519(k_bytes, u_bytes):
    k = decodeScalar25519(k_bytes)
    u = decodeUCoordinate(u_bytes)
    x1 = u
    x2, z2 = 1, 0
    x3, z3 = u, 1
    swap = 0
    for t in range(254, -1, -1):
        k_t = (k >> t) & 1
        swap ^= k_t
        if swap:
            x2, x3 = x3, x2
            z2, z3 = z3, z2
        swap = k_t
        A = (x2 + z2) % p
        AA = (A*A) % p
        B = (x2 - z2) % p
        BB = (B*B) % p
        E = (AA - BB) % p
        C = (x3 + z3) % p
        D = (x3 - z3) % p
        DA = (D*A) % p
        CB = (C*B) % p
        x3 = (DA+CB)**2 % p
        z3 = (x1 * (DA-CB)**2) % p
        x2 = (AA*BB) % p
        z2 = (E * (AA + A24*E)) % p
    if swap:
        x2, x3 = x3, x2
        z2, z3 = z3, z2
    return encodeUCoordinate((x2 * inv(z2)) % p)

BASE_U = (9).to_bytes(32,'little')

def x25519_base(k_bytes):
    return x25519(k_bytes, BASE_U)

# quick self test vs pynacl
from nacl.public import PrivateKey
import nacl.bindings as bnd

k = os.urandom(32)
mine = x25519_base(k)
theirs = bnd.crypto_scalarmult_base(k)
assert mine == theirs, (mine.hex(), theirs.hex())
print("x25519 base OK")

k2 = os.urandom(32)
u2 = os.urandom(32)
mine2 = x25519(k2, u2)
theirs2 = bnd.crypto_scalarmult(k2, u2)
assert mine2 == theirs2
print("x25519 general OK")

