#!/usr/bin/env python3
"""Drives `t_ssh_keyfiles` over `ssh-keygen`-made keys and checks every
claim against OpenSSH and OpenSSL:

  * the public blob m31 derived from the (decrypted) key equals what
    `ssh-keygen -y` prints for it (with the passphrase);
  * every signature verifies under that public key with `cryptography`
    (Ed25519; ECDSA P-256 with SHA-256; RSA PKCS#1 v1.5 with SHA-256 and
    SHA-512 for rsa-sha2-256/512);
  * the comment round-trips;
  * a wrong passphrase is refused as such, and an encrypted key given no
    passphrase is `Encrypted`.

Usage: oracle_sshkeyfiles.py BIN WORKDIR
"""
import base64
import os
import subprocess
import sys

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec, ed25519, padding, rsa, utils

BIN, WORK = sys.argv[1], sys.argv[2]
PASS = "correct horse battery staple éè " + "x" * 90  # non-ASCII and longer than 72 bytes
MESSAGE = b"m31ssh keyfile test message"
failures = []
checks = 0


def check(cond, label):
    global checks
    checks += 1
    if not cond:
        failures.append(label)


def keygen(name, type_args, comment, cipher=None, rounds=None, passphrase=""):
    path = os.path.join(WORK, name)
    cmd = ["ssh-keygen", "-q", "-f", path, "-C", comment] + type_args + ["-N", passphrase]
    if cipher:
        cmd += ["-Z", cipher]
    if rounds:
        cmd += ["-a", str(rounds)]
    subprocess.run(cmd, check=True, capture_output=True)
    os.chmod(path, 0o600)
    return path


def read_u32(b, o):
    return int.from_bytes(b[o:o + 4], "big"), o + 4


def fields(blob):
    out, o = [], 0
    while o < len(blob):
        n, o = read_u32(blob, o)
        out.append(blob[o:o + n])
        o += n
    return out


def verify(algorithm, public_blob, sig_blob):
    parts = fields(public_blob)
    name, data = fields(sig_blob)
    if name.decode() != algorithm:
        return False
    try:
        if parts[0] == b"ssh-ed25519":
            ed25519.Ed25519PublicKey.from_public_bytes(parts[1]).verify(data, MESSAGE)
        elif parts[0] == b"ecdsa-sha2-nistp256":
            key = ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), parts[2])
            r_raw, s_raw = fields(data)
            sig = utils.encode_dss_signature(int.from_bytes(r_raw, "big"), int.from_bytes(s_raw, "big"))
            key.verify(sig, MESSAGE, ec.ECDSA(hashes.SHA256()))
        elif parts[0] == b"ssh-rsa":
            e, n = int.from_bytes(parts[1], "big"), int.from_bytes(parts[2], "big")
            key = rsa.RSAPublicNumbers(e, n).public_key()
            h = hashes.SHA256() if algorithm == "rsa-sha2-256" else hashes.SHA512()
            key.verify(data, MESSAGE, padding.PKCS1v15(), h)
        else:
            return False
    except InvalidSignature:
        return False
    return True


cases = []
types = [("ed25519", ["-t", "ed25519"]), ("ecdsa", ["-t", "ecdsa", "-b", "256"]), ("rsa2048", ["-t", "rsa", "-b", "2048"])]
for label, targs in types:
    cases.append((label + "-plain", keygen(label + "_plain", targs, "plain ünicode comment"), ""))
    for cipher in ("aes256-ctr", "aes128-ctr", "aes256-gcm@openssh.com", "chacha20-poly1305@openssh.com"):
        short = cipher.split("@")[0]
        cases.append(("%s-%s" % (label, short),
                      keygen("%s_%s" % (label, short), targs, "enc %s %s" % (label, short), cipher=cipher, rounds=4, passphrase=PASS),
                      PASS))
cases.append(("rsa3072-aes256-ctr-default-rounds", keygen("rsa3072", ["-t", "rsa", "-b", "3072"], "r3072", passphrase="pw"), "pw"))
cases.append(("ed25519-empty-comment-rounds-1", keygen("ed_r1", ["-t", "ed25519"], "", cipher="aes256-ctr", rounds=1, passphrase="x"), "x"))

for label, path, passphrase in cases:
    p = subprocess.run([BIN, passphrase or "-", path], capture_output=True, text=True, timeout=300)
    check(p.returncode == 0, "%s: t_ssh_keyfiles exited %d: %s" % (label, p.returncode, p.stderr[-200:]))
    lines = [l for l in p.stdout.split("\n") if l]
    keyline = [l for l in lines if l.startswith(path + " key ")]
    check(len(keyline) == 1 and not any(" ERROR " in l for l in lines), "%s: no errors (%s)" % (label, lines[:3]))
    if not keyline:
        continue
    _, _, algorithm, blob_hex, *comment = keyline[0].split(" ", 4)[0:5]
    comment = keyline[0].split(" ", 4)[4] if len(keyline[0].split(" ", 4)) > 4 else ""
    blob = bytes.fromhex(blob_hex)
    y = subprocess.run(["ssh-keygen", "-y", "-P", passphrase, "-f", path], capture_output=True, text=True)
    want = base64.b64decode(y.stdout.split()[1])
    check(blob == want, "%s: public key equals ssh-keygen -y" % label)
    pub = open(path + ".pub").read().strip()
    check(comment == (pub.split(" ", 2)[2] if len(pub.split(" ", 2)) > 2 else ""), "%s: comment round trip (%r)" % (label, comment))
    sigs = [l.split(" ") for l in lines if l.startswith(path + " sig ")]
    expect = {"ssh-ed25519": ["ssh-ed25519"], "ecdsa-sha2-nistp256": ["ecdsa-sha2-nistp256"], "ssh-rsa": ["rsa-sha2-512", "rsa-sha2-256"]}[algorithm]
    check([s[2] for s in sigs] == expect, "%s: signature algorithms %s" % (label, [s[2] for s in sigs]))
    for s in sigs:
        check(verify(s[2], blob, bytes.fromhex(s[3])), "%s: %s signature verifies under OpenSSL" % (label, s[2]))
    if passphrase:
        check(any(l == path + " wrong-passphrase-refused" for l in lines), "%s: wrong passphrase refused as WrongPassphrase" % label)

if failures:
    for f in failures[:20]:
        print("FAIL " + f)
    sys.exit(1)
print("%d checks, 0 failures" % checks)
