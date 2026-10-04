#!/usr/bin/env python3
"""The other half of `t_ssh_wire.m31`: `lib/ssh.m31`'s exchange hash and key
derivation, computed independently through Python's own `hashlib`, not
through anything `lib/ssh.m31` itself does. `test.sh` diffs the two.

This is the one place a plain SHA-256 oracle earns its keep before any live
`sshd` exists at all (`docs/ssh-decision.md` §4): it isolates the exchange
hash's byte-order and `mpint`-encoding rules (RFC 8731 §3.1, RFC 4251 §5)
from the live handshake entirely, so a mistake there shows up here, in a
pure function, rather than only as "the live fixture never decrypts
anything."

    python3 apps/ssh/oracle_ssh_wire.py
"""
import hashlib

out = []


def ssh_string(b):
    return len(b).to_bytes(4, "big") + b


def to_mpint(be_value):
    if all(b == 0 for b in be_value):
        return ssh_string(b"")
    trimmed = be_value.lstrip(b"\x00")
    if trimmed[0] & 0x80:
        trimmed = b"\x00" + trimmed
    return ssh_string(trimmed)


def exchange_hash(v_c, v_s, i_c, i_s, k_s, q_c, q_s, k_mpint):
    buf = b"".join(
        [ssh_string(v_c), ssh_string(v_s), ssh_string(i_c), ssh_string(i_s),
         ssh_string(k_s), ssh_string(q_c), ssh_string(q_s), k_mpint]
    )
    return hashlib.sha256(buf).digest()


def derive64(k_mpint, h, letter, session_id):
    k1 = hashlib.sha256(k_mpint + h + letter.encode("ascii") + session_id).digest()
    k2 = hashlib.sha256(k_mpint + h + k1).digest()
    return k1 + k2


# --- a handful of fixed, hand-built cases -------------------------------------
# Every field below is arbitrary but fixed test data, not real key material --
# the point is exercising the exact concatenation order and the mpint
# sign-bit rule, not a real handshake.

V_C = b"SSH-2.0-m31_0.1"
V_S = b"SSH-2.0-OpenSSH_9.6"
I_C = bytes(range(40))
I_S = bytes(range(40, 80))
K_S = b"\x00\x00\x00\x0bssh-ed25519" + ssh_string(bytes(range(100, 132)))
Q_C = bytes(range(1, 33))
Q_S = bytes(range(33, 65))

# A shared secret with the top bit of its first byte SET -- the ordinary-
# handshake case `lib/ssh.m31`'s own header calls out, not an edge case.
K_HIGH_BIT = bytes([0x80] + list(range(1, 32)))
# One with the top bit clear, and a leading zero byte to be trimmed.
K_LOW_BIT = bytes([0x00, 0x7F] + list(range(2, 32)))
# All zero -- RFC 4251 §5's own special case, the empty-string mpint.
K_ZERO = bytes(32)

for label, k_raw in [("highbit", K_HIGH_BIT), ("lowbit", K_LOW_BIT), ("zero", K_ZERO)]:
    k_mpint = to_mpint(k_raw)
    h = exchange_hash(V_C, V_S, I_C, I_S, K_S, Q_C, Q_S, k_mpint)
    out.append(f"H-{label} " + h.hex())
    session_id = h
    c2s = derive64(k_mpint, h, "C", session_id)
    s2c = derive64(k_mpint, h, "D", session_id)
    out.append(f"derive64-C-{label} " + c2s.hex())
    out.append(f"derive64-D-{label} " + s2c.hex())

print("\n".join(out))
