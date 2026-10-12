#!/usr/bin/env python3
"""Oracle for t_tls13_schedule.m31: RFC 8446 §7 from hashlib, hmac and the
`cryptography` package's HKDF-Expand, never from the code under test.

  oracle_tls13_schedule.py --args   the hex arguments t_tls13_schedule takes
  oracle_tls13_schedule.py          the lines it must print

Every value RFC 8448 publishes is asserted against this script's own result
before anything is printed, so the script is itself checked against the RFC.
"""
import hashlib
import hmac
import struct
import sys

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.kdf.hkdf import HKDFExpand

import tls13_rfc8448 as rfc


class Suite:
    def __init__(self, tag, code, name, hash_name, key_size):
        self.tag = tag
        self.code = code
        self.name = name
        self.hash_name = hash_name
        self.key_size = key_size
        self.size = hashlib.new(hash_name).digest_size
        self.zeros = bytes(self.size)
        self.crypto_hash = {"sha256": hashes.SHA256, "sha384": hashes.SHA384}[hash_name]

    def hash(self, data):
        return hashlib.new(self.hash_name, data).digest()

    def mac(self, key, data):
        return hmac.new(key, data, self.hash_name).digest()

    def extract(self, salt, ikm):
        return self.mac(salt if salt else self.zeros, ikm)

    def expand_label(self, secret, label, context, length):
        full = b"tls13 " + label.encode()
        info = struct.pack(">H", length) + bytes([len(full)]) + full + bytes([len(context)]) + context
        return HKDFExpand(self.crypto_hash(), length, info).derive(secret)

    def derive_secret(self, secret, label, transcript_hash):
        return self.expand_label(secret, label, transcript_hash, self.size)


SUITES = [
    Suite("aes128", 0x1301, "TLS_AES_128_GCM_SHA256", "sha256", 16),
    Suite("chacha", 0x1303, "TLS_CHACHA20_POLY1305_SHA256", "sha256", 32),
    Suite("aes256", 0x1302, "TLS_AES_256_GCM_SHA384", "sha384", 32),
]

# --- RFC 8448 inputs -----------------------------------------------------------
shared_secret = rfc.find(3, 'extract secret "handshake"', "IKM")
psk = rfc.find(4, 'extract secret "early"', "IKM")
client_hello = rfc.find(3, "construct a ClientHello", "ClientHello")
server_hello = rfc.find(3, "construct a ServerHello", "ServerHello")
encrypted_extensions = rfc.find(3, "construct an EncryptedExtensions", "EncryptedExtensions")
certificate = rfc.find(3, "construct a Certificate handshake", "Certificate")
certificate_verify = rfc.find(3, "construct a CertificateVerify", "CertificateVerify")
server_finished = rfc.find(3, "construct a Finished", "Finished", 0)
client_finished = rfc.find(3, "construct a Finished", "Finished", 1)
psk_client_hello = rfc.find(4, "send handshake record", "payload", 0)
assert len(psk_client_hello) == 512

if "--args" in sys.argv:
    print(
        " ".join(
            v.hex()
            for v in (
                shared_secret,
                psk,
                client_hello,
                server_hello,
                encrypted_extensions,
                certificate,
                certificate_verify,
                server_finished,
                client_finished,
                psk_client_hello,
            )
        )
    )
    sys.exit(0)

lines = []
CURRENT = None


def show(name, value):
    lines.append(f"{CURRENT.tag}.{name} {value.hex()}")


def check(name, got, section, step, field, occurrence=0):
    # RFC 8448 is a SHA-256 trace: its secrets are the same for every
    # SHA-256 suite; only the AES-128 one also shares its key length.
    if CURRENT.hash_name != "sha256":
        return
    want = rfc.find(section, step, field, occurrence)
    assert got == want, f"{name}: {got.hex()} != RFC 8448 {want.hex()}"


def keys(name, traffic_secret, rfc_step=None, rfc_occurrence=0):
    S = CURRENT
    key = S.expand_label(traffic_secret, "key", b"", S.key_size)
    iv = S.expand_label(traffic_secret, "iv", b"", 12)
    if rfc_step:
        if S.key_size == 16:
            check(name + ".key", key, 3, rfc_step, "key expanded", rfc_occurrence)
        check(name + ".iv", iv, 3, rfc_step, "iv expanded", rfc_occurrence)
    show(name + ".key", key)
    show(name + ".iv", iv)
    show(name + ".pair_key", key)
    show(name + ".pair_iv", iv)


def scenario(S):
    global CURRENT
    CURRENT = S
    empty = S.hash(b"")
    show("empty_hash", empty)

    early = S.extract(b"", S.zeros)
    check("early", early, 3, 'extract secret "early"', "secret")
    show("early", early)
    derived = S.derive_secret(early, "derived", empty)
    check("derived", derived, 3, 'derive secret for handshake "tls13 derived"', "expanded")
    handshake = S.extract(derived, shared_secret)
    check("handshake", handshake, 3, 'extract secret "handshake"', "secret")
    show("handshake", handshake)

    messages = client_hello + server_hello
    hello_hash = S.hash(messages)
    check("hello_hash", hello_hash, 3, 'derive secret "tls13 c hs traffic"', "hash")
    show("hello_hash", hello_hash)
    show("hello_hash_again", hello_hash)
    client_handshake = S.derive_secret(handshake, "c hs traffic", hello_hash)
    server_handshake = S.derive_secret(handshake, "s hs traffic", hello_hash)
    check("client_handshake_traffic", client_handshake, 3, 'derive secret "tls13 c hs traffic"', "expanded")
    check("server_handshake_traffic", server_handshake, 3, 'derive secret "tls13 s hs traffic"', "expanded")
    show("client_handshake_traffic", client_handshake)
    show("server_handshake_traffic", server_handshake)
    # The RFC's "derive read traffic keys for handshake data" is the server reading
    # what the client wrote: the client handshake keys.
    keys("client_handshake", client_handshake, "derive read traffic keys for handshake data")
    keys("server_handshake", server_handshake, "derive write traffic keys for handshake data")
    client_finished_key = S.expand_label(client_handshake, "finished", b"", S.size)
    server_finished_key = S.expand_label(server_handshake, "finished", b"", S.size)
    check("client_finished_key", client_finished_key, 3, "calculate finished", "expanded", 1)
    check("server_finished_key", server_finished_key, 3, "calculate finished", "expanded", 0)
    show("client_finished_key", client_finished_key)
    show("server_finished_key", server_finished_key)

    messages += encrypted_extensions + certificate + certificate_verify
    verify_hash = S.hash(messages)
    show("verify_hash", verify_hash)
    server_verify = S.mac(server_finished_key, verify_hash)
    check("server_verify_data", server_verify, 3, "calculate finished", "finished", 0)
    if S.hash_name == "sha256":
        assert server_finished == bytes([0x14, 0, 0, S.size]) + server_verify
    show("server_verify_data", server_verify)
    messages += server_finished
    finished_hash = S.hash(messages)
    check("finished_hash", finished_hash, 3, 'derive secret "tls13 c ap traffic"', "hash")
    show("finished_hash", finished_hash)
    client_verify = S.mac(client_finished_key, finished_hash)
    check("client_verify_data", client_verify, 3, "calculate finished", "finished", 1)
    if S.hash_name == "sha256":
        assert client_finished == bytes([0x14, 0, 0, S.size]) + client_verify
    show("client_verify_data", client_verify)

    master_salt = S.derive_secret(handshake, "derived", empty)
    master = S.extract(master_salt, S.zeros)
    check("master", master, 3, 'extract secret "master"', "secret")
    show("master", master)
    client_application = S.derive_secret(master, "c ap traffic", finished_hash)
    server_application = S.derive_secret(master, "s ap traffic", finished_hash)
    check("client_application_traffic", client_application, 3, 'derive secret "tls13 c ap traffic"', "expanded")
    check("server_application_traffic", server_application, 3, 'derive secret "tls13 s ap traffic"', "expanded")
    show("client_application_traffic", client_application)
    show("server_application_traffic", server_application)
    exporter = S.derive_secret(master, "exp master", finished_hash)
    check("exporter_master", exporter, 3, 'derive secret "tls13 exp master"', "expanded")
    show("exporter_master", exporter)
    keys("client_application", client_application, "derive write traffic keys for application data", 1)
    keys("server_application", server_application, "derive write traffic keys for application data", 0)

    messages += client_finished
    resumption = S.derive_secret(master, "res master", S.hash(messages))
    check("resumption_master", resumption, 3, 'derive secret "tls13 res master"', "expanded")
    show("resumption_master", resumption)

    client_next = S.expand_label(client_application, "traffic upd", b"", S.size)
    server_next = S.expand_label(server_application, "traffic upd", b"", S.size)
    show("client_application_next", client_next)
    show("server_application_next", server_next)
    show("client_application_next2", S.expand_label(client_next, "traffic upd", b"", S.size))
    keys("client_application_next", client_next)

    restarted = bytes([0xFE, 0, 0, S.size]) + S.hash(client_hello)
    show("retry_restarted", S.hash(restarted))
    show("retry_final", S.hash(restarted + server_hello + psk_client_hello))

    early_psk = S.extract(b"", psk)
    check("psk_early", early_psk, 4, 'extract secret "early"', "secret")
    show("psk_early", early_psk)
    binder_resumption = S.derive_secret(early_psk, "res binder", empty)
    check("psk_binder_key_resumption", binder_resumption, 4, "calculate PSK binder", "PRK")
    show("psk_binder_key_resumption", binder_resumption)
    show("psk_binder_key_external", S.derive_secret(early_psk, "ext binder", empty))
    client_early = S.derive_secret(early_psk, "c e traffic", S.hash(psk_client_hello))
    check("psk_client_early_traffic", client_early, 4, 'derive secret "tls13 c e traffic"', "expanded")
    show("psk_client_early_traffic", client_early)




for suite in SUITES:
    scenario(suite)
lines.append("codes 4865 4866 4867")
lines.append("sizes 32 48 16 32 32")
for code in (0x1301, 0x1302, 0x1303, 0x1304, 0x1305, 0, 0xC02F):
    found = [s.name for s in SUITES if s.code == code]
    lines.append(f"suite {code} {found[0] if found else 'none'}")

print("\n".join(lines))
