#!/usr/bin/env python3
"""Cases and expected output for `lib/crypto/ed25519.m31`'s signer (`t_tlsserver_ed25519.m31`).

    python3 tlsserver_ed25519_oracle.py CASES_OUT EXPECTED_OUT [COUNT]

CASES_OUT holds `name|seed_hex|message_hex` lines; EXPECTED_OUT holds the line the
m31 program must print for each: `name public_key signature`.

Ed25519 signatures are deterministic, so the match is byte for byte. Two references:
the RFC 8032 section 7.1 vectors, asserted below against both the published
text and the `cryptography` package, and COUNT random seeds with random message
lengths (0 to 300 octets) signed by `cryptography` (OpenSSL).
"""
import random
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ed25519

RFC8032 = [
    ("rfc8032_test1",
     "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60", "",
     "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
     "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"),
    ("rfc8032_test2",
     "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb", "72",
     "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
     "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"),
]


def reference(seed, message):
    key = ed25519.Ed25519PrivateKey.from_private_bytes(seed)
    public = key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
    return public, key.sign(message)


def main():
    cases_path, expected_path = sys.argv[1], sys.argv[2]
    count = int(sys.argv[3]) if len(sys.argv) > 3 else 1000
    cases, expected = [], []
    for name, seed, message, public, signature in RFC8032:
        got_public, got_signature = reference(bytes.fromhex(seed), bytes.fromhex(message))
        assert got_public.hex() == public and got_signature.hex() == signature, name
        cases.append("%s|%s|%s" % (name, seed, message))
        expected.append("%s %s %s" % (name, public, signature))
    generator = random.Random(8032)
    for index in range(count):
        seed = bytes(generator.getrandbits(8) for _ in range(32))
        message = bytes(generator.getrandbits(8) for _ in range(generator.choice([0, 1, 31, 32, 33, 64, 100, generator.randrange(0, 301)])))
        public, signature = reference(seed, message)
        name = "random_%d" % index
        cases.append("%s|%s|%s" % (name, seed.hex(), message.hex()))
        expected.append("%s %s %s" % (name, public.hex(), signature.hex()))
    with open(cases_path, "w") as handle:
        handle.write("\n".join(cases) + "\n")
    with open(expected_path, "w") as handle:
        handle.write("\n".join(expected) + "\n")
    print("wrote %d cases" % len(cases))


if __name__ == "__main__":
    main()
