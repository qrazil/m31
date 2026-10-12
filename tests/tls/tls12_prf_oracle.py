#!/usr/bin/env python3
"""The other half of `tls12_prf.m31`: RFC 5246 §5's P_hash from `hmac` and
`hashlib`, the key block layout of §6.3 and RFC 5288/7905, and Finished of
§7.4.9. Nothing here may read the language's answer."""
import hashlib
import hmac

SUITES = [
    (0xCCA9, "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256", hashlib.sha256, 32, 12),
    (0xCCA8, "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256", hashlib.sha256, 32, 12),
    (0xC02B, "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256", hashlib.sha256, 16, 4),
    (0xC02F, "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256", hashlib.sha256, 16, 4),
    (0xC02C, "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384", hashlib.sha384, 32, 4),
    (0xC030, "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384", hashlib.sha384, 32, 4),
]


def pattern(count, multiplier, offset):
    return bytes((i * multiplier + offset) & 0xFF for i in range(count))


def prf(hash_function, secret, label, seed, size):
    seed = label + seed
    out = b""
    a = seed
    while len(out) < size:
        a = hmac.new(secret, a, hash_function).digest()
        out += hmac.new(secret, a + seed, hash_function).digest()
    return out[:size]


VECTOR = ("e3f229ba727be17b8d122620557cd453c2aab21d07c3d495329b52d4e61edb5a6b301791e90d35c9c9a46b4e14baf9af0fa022f7077def17abfd3797c0564bab4fbc91666e9def9b97fce34f796789baa48082d122ee42c5a72e5a5110fff70187347b66")
vector = prf(hashlib.sha256, bytes.fromhex("9bbe436ba940f017b17652849a71db35"), b"test label",
             bytes.fromhex("a0ba9f936cda311827a6f796ffd5198c"), 100)
assert vector.hex() == VECTOR, "the oracle's PRF disagrees with the published vector"
print("vector " + vector.hex())

for code, _name, hash_function, key_size, iv_size in SUITES:
    name = str(code)
    premaster = pattern(32, 5, 1)
    session_hash = pattern(hash_function().digest_size, 3, 9)
    client_random = pattern(32, 7, 2)
    server_random = pattern(32, 11, 4)
    master = prf(hash_function, premaster, b"extended master secret", session_hash, 48)
    block = prf(hash_function, master, b"key expansion", server_random + client_random,
                2 * key_size + 2 * iv_size)
    print(name, "master", master.hex())
    print(name, "client_key", block[:key_size].hex())
    print(name, "server_key", block[key_size:2 * key_size].hex())
    print(name, "client_iv", block[2 * key_size:2 * key_size + iv_size].hex())
    print(name, "server_iv", block[2 * key_size + iv_size:].hex())
    print(name, "client_finished", prf(hash_function, master, b"client finished", session_hash, 12).hex())
    print(name, "server_finished", prf(hash_function, master, b"server finished", session_hash, 12).hex())
    print(name, "prf_long", prf(hash_function, master, b"label", client_random, 200).hex())
    print(name, "prf_empty_seed", prf(hash_function, master, b"x", b"", 1).hex())

print("signed " + (pattern(32, 1, 0) + pattern(32, 2, 0) + pattern(37, 3, 1)).hex())
names = {code: name for code, name, *_ in SUITES}
for code in [0xCCA9, 0xC02B, 0xC030, 0x1301, 0xC013, 0x009C, 0x0000]:
    print("code", code, names.get(code, "none"))
