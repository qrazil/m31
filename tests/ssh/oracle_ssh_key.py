#!/usr/bin/env python3
"""Oracle for t_ssh_key.m31's real-key half: the `cryptography` package
(OpenSSL) reads each unencrypted `ssh-keygen` key and prints its public key,
comment and signature over a fixed message. Ed25519 signatures are
deterministic, so t_ssh_key must print exactly the same lines.

usage: oracle_ssh_key.py <encrypted key> <plain key>...
(the encrypted key is only there to keep the argument lists identical; the
comment comes from the `.pub` file next to each key, which ssh-keygen writes).
"""
import sys

from cryptography.hazmat.primitives import serialization

print("encrypted refused: the private key is passphrase-protected; only unencrypted keys are supported (ssh-keygen -p -N \"\" -f <copy of the key> removes the passphrase)")
for i, path in enumerate(sys.argv[2:]):
    with open(path, "rb") as f:
        key = serialization.load_ssh_private_key(f.read(), password=None)
    pub = key.public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw
    )
    with open(path + ".pub") as f:
        fields = f.read().split(None, 2)
    comment = fields[2].strip() if len(fields) > 2 else ""
    print("key %d" % i)
    print("pub " + pub.hex())
    print("comment " + comment)
    print("sig " + key.sign(b"oracle message").hex())
