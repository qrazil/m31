import os, random, re, sys
OUT = sys.argv[1] if len(sys.argv) > 1 else '.'
_R = random.Random(0xC0FFEE25519)
import nacl.bindings as bnd

def bytes_lit(b):
    return "[" + ", ".join(str(x) for x in b) + "]"

# --- pull the RFC 7748 vectors straight from the cached RFC text, never by
# hand: manual retyping of 64-hex-digit strings has already proven error
# prone once in this session. ---
text = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "rfc7748.txt")).read()
start = text.index("Two types of tests are provided")
end = text.index("6.  Diffie-Hellman", start)
section = text[start:end]

def hx(s):
    return re.sub(r'[^0-9a-fA-F]', '', s)

m = re.search(r"X25519:\s*Input scalar:(.*?)Input scalar as a number.*?Input u-coordinate:(.*?)Input u-coordinate as a number.*?Output u-coordinate:(.*?)Input scalar:", section, re.S)
k1_raw, u1_raw, o1_raw = hx(m.group(1)), hx(m.group(2)), hx(m.group(3))
rest = section[m.end("__dummy__") if False else 0:]
m2 = re.search(r"Input scalar:(.*?)Input scalar as a number.*?Input u-coordinate:(.*?)Input u-coordinate as a number.*?Output u-coordinate:(.*?)X448:", section, re.S)
# second match: find the SECOND occurrence directly
all_inputs = re.findall(r"Input scalar:(.*?)Input scalar as a number", section, re.S)
all_u = re.findall(r"Input u-coordinate:(.*?)Input u-coordinate as a number", section, re.S)
all_out = re.findall(r"Output u-coordinate:(.*?)(?:Input scalar:|X448:)", section, re.S)
ks = [hx(x)[:64] for x in all_inputs[:2]]
us = [hx(x)[:64] for x in all_u[:2]]
outs = [hx(x)[:64] for x in all_out[:2]]
for k,u,o in zip(ks,us,outs):
    assert len(k)==64 and len(u)==64 and len(o)==64, (len(k),len(u),len(o))
    got = bnd.crypto_scalarmult(bytes.fromhex(k), bytes.fromhex(u)).hex()
    assert got == o, (got, o)
print("RFC7748 5.2 pair vectors re-extracted and verified:", ks, us, outs)

# Section 6.1 Alice/Bob -- also extracted from text, not retyped.
sec = text.index("6.1.  Curve25519", end)
sec_end = text.index("6.2.  Curve448", sec)
s61 = text[sec:sec_end]
def field(label, nxt):
    mm = re.search(re.escape(label) + r"(.*?)" + re.escape(nxt), s61, re.S)
    return hx(mm.group(1))
alice_priv = field("Alice's private key, a:", "Alice's public key")
alice_pub = field("Alice's public key, X25519(a, 9):", "Bob's private key")
bob_priv = field("Bob's private key, b:", "Bob's public key")
bob_pub = field("Bob's public key, X25519(b, 9):", "Their shared secret")
shared = hx(s61[s61.index("Their shared secret, K:"):].split("Their shared secret, K:")[1][:200])
alice_priv, alice_pub, bob_priv, bob_pub, shared = (x[:64] for x in (alice_priv, alice_pub, bob_priv, bob_pub, shared))
assert bnd.crypto_scalarmult_base(bytes.fromhex(alice_priv)).hex() == alice_pub
assert bnd.crypto_scalarmult_base(bytes.fromhex(bob_priv)).hex() == bob_pub
assert bnd.crypto_scalarmult(bytes.fromhex(alice_priv), bytes.fromhex(bob_pub)).hex() == shared
assert bnd.crypto_scalarmult(bytes.fromhex(bob_priv), bytes.fromhex(alice_pub)).hex() == shared
print("RFC7748 6.1 Alice/Bob vector re-extracted and verified")

src = []
want = []
src.append("import x25519;")
src.append("")

def add_case(tag, k, u):
    idx = len(want)
    src.append(f"bytes k{idx} = {bytes_lit(k)};")
    src.append(f"bytes u{idx} = {bytes_lit(u)};")
    src.append(f'print("{tag}{idx} " + x25519.x25519(k{idx}, u{idx}).hex());')
    out = bnd.crypto_scalarmult(bytes(k), bytes(u))
    want.append(f"{tag}{idx} " + out.hex())

for k,u,o in zip(ks,us,outs):
    add_case("rfc", bytes.fromhex(k), bytes.fromhex(u))

src.append("bytes alice_priv = " + bytes_lit(bytes.fromhex(alice_priv)) + ";")
src.append("bytes bob_priv = " + bytes_lit(bytes.fromhex(bob_priv)) + ";")
src.append('print("alice_pub " + x25519.base_point(alice_priv).hex());')
src.append('print("bob_pub " + x25519.base_point(bob_priv).hex());')
src.append("bytes bob_pub_dec = " + bytes_lit(bytes.fromhex(bob_pub)) + ";")
src.append("bytes alice_pub_dec = " + bytes_lit(bytes.fromhex(alice_pub)) + ";")
src.append('print("shared_a " + x25519.x25519(alice_priv, bob_pub_dec).hex());')
src.append('print("shared_b " + x25519.x25519(bob_priv, alice_pub_dec).hex());')

want.append("alice_pub " + alice_pub)
want.append("bob_pub " + bob_pub)
want.append("shared_a " + shared)
want.append("shared_b " + shared)

random.seed(0xC0FFEE)
p = 2**255 - 19

# Pure random inputs: real 32-byte randomness essentially never lands on a
# low-order point (probability astronomically small), so these are checked
# against pynacl's libsodium binding, the strongest independent oracle.
for i in range(300):
    k = _R.randbytes(32)
    u = _R.randbytes(32)
    add_case("rnd", k, u)

# RFC 7748 boundary / non-canonical u-coordinates: 0, 1, small-order points
# and values >= p that RFC 7748 §5 explicitly requires accepting ("MUST
# accept non-canonical values and process them as if they had been reduced
# modulo the field prime"). libsodium's crypto_scalarmult DELIBERATELY
# refuses several of these (it blacklists known low-order inputs and an
# all-zero result) as a safety policy on top of the bare RFC function, so it
# is not usable as the oracle here. The independent oracle for this group is
# `proto.x25519` (this same file's sibling `proto.py`) -- a from-scratch,
# big-integer implementation of RFC 7748 §5's own pseudocode, itself
# separately checked against pynacl for generic inputs and against every
# official RFC 7748 test vector earlier in this file's session.
from proto_x25519_bigint import x25519 as proto_x25519
edge_u = [0, 1, 2, 9, p - 1, p, p + 1, (1 << 255) - 1, (1 << 256) - 1, 486662, 19]
for ev in edge_u:
    k = _R.randbytes(32)
    u = (ev % (1 << 256)).to_bytes(32, 'little')
    idx = len(want)
    src.append(f"bytes ek{idx} = {bytes_lit(k)};")
    src.append(f"bytes eu{idx} = {bytes_lit(u)};")
    src.append(f'print("edge{idx} " + x25519.x25519(ek{idx}, eu{idx}).hex());')
    want.append(f"edge{idx} " + proto_x25519(k, u).hex())

for i in range(30):
    a = _R.randbytes(32)
    b = _R.randbytes(32)
    idx = len(want)
    src.append(f"bytes ca{idx} = {bytes_lit(a)};")
    src.append(f"bytes cb{idx} = {bytes_lit(b)};")
    src.append(f"bytes capub{idx} = x25519.base_point(ca{idx});")
    src.append(f"bytes cbpub{idx} = x25519.base_point(cb{idx});")
    src.append(f'print("chain{idx} " + x25519.x25519(ca{idx}, cbpub{idx}).hex());')
    src.append(f'print("chain2_{idx} " + x25519.x25519(cb{idx}, capub{idx}).hex());')
    Ka = bnd.crypto_scalarmult_base(a)
    Kb = bnd.crypto_scalarmult_base(b)
    shared1 = bnd.crypto_scalarmult(a, Kb)
    shared2 = bnd.crypto_scalarmult(b, Ka)
    assert shared1 == shared2
    want.append(f"chain{idx} " + shared1.hex())
    want.append(f"chain2_{idx} " + shared2.hex())

open(os.path.join(OUT, "t_x25519.src"), "w").write("\n".join(src) + "\n")
open(os.path.join(OUT, "t_x25519.want"), "w").write("\n".join(want) + "\n")
print("wrote", len(want), "cases,", len(src), "src lines")
