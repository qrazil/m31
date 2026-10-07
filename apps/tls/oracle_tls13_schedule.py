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

ZEROS = bytes(32)


def sha256(data):
    return hashlib.sha256(data).digest()


def extract(salt, ikm):
    return hmac.new(salt if salt else ZEROS, ikm, hashlib.sha256).digest()


def expand_label(secret, label, context, length):
    full = b"tls13 " + label.encode()
    info = struct.pack(">H", length) + bytes([len(full)]) + full + bytes([len(context)]) + context
    return HKDFExpand(hashes.SHA256(), length, info).derive(secret)


def derive_secret(secret, label, transcript_hash):
    return expand_label(secret, label, transcript_hash, 32)


EMPTY = sha256(b"")

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


def show(name, value):
    lines.append(f"{name} {value.hex()}")


def check(name, got, section, step, field, occurrence=0):
    want = rfc.find(section, step, field, occurrence)
    assert got == want, f"{name}: {got.hex()} != RFC 8448 {want.hex()}"


def keys(name, traffic_secret, rfc_step=None, rfc_occurrence=0):
    key16 = expand_label(traffic_secret, "key", b"", 16)
    key32 = expand_label(traffic_secret, "key", b"", 32)
    iv = expand_label(traffic_secret, "iv", b"", 12)
    if rfc_step:
        check(name + ".key16", key16, 3, rfc_step, "key expanded", rfc_occurrence)
        check(name + ".iv", iv, 3, rfc_step, "iv expanded", rfc_occurrence)
    show(name + ".key16", key16)
    show(name + ".key32", key32)
    show(name + ".iv", iv)
    show(name + ".pair_key", key32)
    show(name + ".pair_iv", iv)


show("empty_hash", EMPTY)

early = extract(b"", ZEROS)
check("early", early, 3, 'extract secret "early"', "secret")
show("early", early)
derived = derive_secret(early, "derived", EMPTY)
check("derived", derived, 3, 'derive secret for handshake "tls13 derived"', "expanded")
handshake = extract(derived, shared_secret)
check("handshake", handshake, 3, 'extract secret "handshake"', "secret")
show("handshake", handshake)

messages = client_hello + server_hello
hello_hash = sha256(messages)
check("hello_hash", hello_hash, 3, 'derive secret "tls13 c hs traffic"', "hash")
show("hello_hash", hello_hash)
show("hello_hash_again", hello_hash)
client_handshake = derive_secret(handshake, "c hs traffic", hello_hash)
server_handshake = derive_secret(handshake, "s hs traffic", hello_hash)
check("client_handshake_traffic", client_handshake, 3, 'derive secret "tls13 c hs traffic"', "expanded")
check("server_handshake_traffic", server_handshake, 3, 'derive secret "tls13 s hs traffic"', "expanded")
show("client_handshake_traffic", client_handshake)
show("server_handshake_traffic", server_handshake)
# The RFC's "derive read traffic keys for handshake data" is the server reading
# what the client wrote: the client handshake keys.
keys("client_handshake", client_handshake, "derive read traffic keys for handshake data")
keys("server_handshake", server_handshake, "derive write traffic keys for handshake data")
client_finished_key = expand_label(client_handshake, "finished", b"", 32)
server_finished_key = expand_label(server_handshake, "finished", b"", 32)
check("client_finished_key", client_finished_key, 3, "calculate finished", "expanded", 1)
check("server_finished_key", server_finished_key, 3, "calculate finished", "expanded", 0)
show("client_finished_key", client_finished_key)
show("server_finished_key", server_finished_key)

messages += encrypted_extensions + certificate + certificate_verify
verify_hash = sha256(messages)
show("verify_hash", verify_hash)
server_verify = hmac.new(server_finished_key, verify_hash, hashlib.sha256).digest()
check("server_verify_data", server_verify, 3, "calculate finished", "finished", 0)
assert server_finished == b"\x14\x00\x00\x20" + server_verify
show("server_verify_data", server_verify)
messages += server_finished
finished_hash = sha256(messages)
check("finished_hash", finished_hash, 3, 'derive secret "tls13 c ap traffic"', "hash")
show("finished_hash", finished_hash)
client_verify = hmac.new(client_finished_key, finished_hash, hashlib.sha256).digest()
check("client_verify_data", client_verify, 3, "calculate finished", "finished", 1)
assert client_finished == b"\x14\x00\x00\x20" + client_verify
show("client_verify_data", client_verify)

master_salt = derive_secret(handshake, "derived", EMPTY)
master = extract(master_salt, ZEROS)
check("master", master, 3, 'extract secret "master"', "secret")
show("master", master)
client_application = derive_secret(master, "c ap traffic", finished_hash)
server_application = derive_secret(master, "s ap traffic", finished_hash)
check("client_application_traffic", client_application, 3, 'derive secret "tls13 c ap traffic"', "expanded")
check("server_application_traffic", server_application, 3, 'derive secret "tls13 s ap traffic"', "expanded")
show("client_application_traffic", client_application)
show("server_application_traffic", server_application)
exporter = derive_secret(master, "exp master", finished_hash)
check("exporter_master", exporter, 3, 'derive secret "tls13 exp master"', "expanded")
show("exporter_master", exporter)
keys("client_application", client_application, "derive write traffic keys for application data", 1)
keys("server_application", server_application, "derive write traffic keys for application data", 0)

messages += client_finished
resumption = derive_secret(master, "res master", sha256(messages))
check("resumption_master", resumption, 3, 'derive secret "tls13 res master"', "expanded")
show("resumption_master", resumption)

client_next = expand_label(client_application, "traffic upd", b"", 32)
server_next = expand_label(server_application, "traffic upd", b"", 32)
show("client_application_next", client_next)
show("server_application_next", server_next)
show("client_application_next2", expand_label(client_next, "traffic upd", b"", 32))
keys("client_application_next", client_next)

restarted = b"\xfe\x00\x00\x20" + sha256(client_hello)
show("retry_restarted", sha256(restarted))
show("retry_final", sha256(restarted + server_hello + psk_client_hello))

early_psk = extract(b"", psk)
check("psk_early", early_psk, 4, 'extract secret "early"', "secret")
show("psk_early", early_psk)
binder_resumption = derive_secret(early_psk, "res binder", EMPTY)
check("psk_binder_key_resumption", binder_resumption, 4, "calculate PSK binder", "PRK")
show("psk_binder_key_resumption", binder_resumption)
show("psk_binder_key_external", derive_secret(early_psk, "ext binder", EMPTY))
client_early = derive_secret(early_psk, "c e traffic", sha256(psk_client_hello))
check("psk_client_early_traffic", client_early, 4, 'derive secret "tls13 c e traffic"', "expanded")
show("psk_client_early_traffic", client_early)

print("\n".join(lines))
