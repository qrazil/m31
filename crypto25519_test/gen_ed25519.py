import os, random, json, sys
OUT = sys.argv[1] if len(sys.argv) > 1 else '.'
_R = random.Random(0xED25519 ^ 0xBADC0FFEE)
import nacl.signing as nsig

def bytes_lit(b):
    return "[" + ", ".join(str(x) for x in b) + "]"

src = ["import ed25519;", ""]
want = []

# --- RFC 8032 7.1 test vectors, re-verified against pynacl earlier in this
# session and cached in ed25519_vectors.json ---
vectors = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "ed25519_vectors.json")))
for name, (sk, pk, msg, sigv) in vectors.items():
    idx = len(want)
    seed = bytes.fromhex(sk)
    msgb = bytes.fromhex(msg)
    src.append(f"bytes seed{idx} = {bytes_lit(seed)};")
    if len(msgb) == 0:
        src.append(f"bytes msg{idx} = [];")
    else:
        src.append(f"bytes msg{idx} = {bytes_lit(msgb)};")
    src.append(f'print("rfc_pk_{idx} " + ed25519.public_key(seed{idx}).hex());')
    src.append(f'print("rfc_sig_{idx} " + ed25519.sign(seed{idx}, msg{idx}).hex());')
    src.append(f'print("rfc_verify_{idx} " + ed25519.verify(ed25519.public_key(seed{idx}), msg{idx}, ed25519.sign(seed{idx}, msg{idx})).to_str());')
    want.append(f"rfc_pk_{idx} " + pk)
    want.append(f"rfc_sig_{idx} " + sigv)
    want.append(f"rfc_verify_{idx} true")

# --- random differential vs pynacl: seed, message length, sign, verify -----
random.seed(0xBADC0FFEE & 0xFFFFFFFF)
N = 300
bad_precheck = 0
for i in range(N):
    seed = _R.randbytes(32)
    msg = _R.randbytes(random.randrange(0, 500))
    s = nsig.SigningKey(seed)
    pk = s.verify_key.encode()
    sig = s.sign(msg).signature
    idx = len(want)
    src.append(f"bytes seed{idx} = {bytes_lit(seed)};")
    src.append(f"bytes msg{idx} = {bytes_lit(msg)};")
    src.append(f'print("pk_{idx} " + ed25519.public_key(seed{idx}).hex());')
    src.append(f'print("sig_{idx} " + ed25519.sign(seed{idx}, msg{idx}).hex());')
    want.append(f"pk_{idx} " + pk.hex())
    want.append(f"sig_{idx} " + sig.hex())

# --- verify: accept genuine signatures, reject tampered ones ---------------
for i in range(100):
    seed = _R.randbytes(32)
    msg = _R.randbytes(random.randrange(0, 200))
    s = nsig.SigningKey(seed)
    pk = s.verify_key.encode()
    sig = s.sign(msg).signature
    idx = len(want)
    src.append(f"bytes vseed{idx} = {bytes_lit(seed)};")
    src.append(f"bytes vpk{idx} = {bytes_lit(pk)};")
    src.append(f"bytes vmsg{idx} = {bytes_lit(msg)};")
    src.append(f"bytes vsig{idx} = {bytes_lit(sig)};")
    src.append(f'print("verify_ok_{idx} " + ed25519.verify(vpk{idx}, vmsg{idx}, vsig{idx}).to_str());')
    want.append(f"verify_ok_{idx} true")

    # tamper: flip a bit in the message (skip if message is empty)
    if len(msg) > 0:
        tampered = bytearray(msg)
        tampered[0] ^= 0x01
        idx2 = len(want)
        src.append(f"bytes tmsg{idx2} = {bytes_lit(bytes(tampered))};")
        src.append(f'print("verify_tamper_msg_{idx2} " + ed25519.verify(vpk{idx}, tmsg{idx2}, vsig{idx}).to_str());')
        want.append(f"verify_tamper_msg_{idx2} false")

    # tamper: flip a bit in the signature
    tsig = bytearray(sig)
    tsig[10] ^= 0x01
    idx3 = len(want)
    src.append(f"bytes tsig{idx3} = {bytes_lit(bytes(tsig))};")
    src.append(f'print("verify_tamper_sig_{idx3} " + ed25519.verify(vpk{idx}, vmsg{idx}, tsig{idx3}).to_str());')
    want.append(f"verify_tamper_sig_{idx3} false")

    # tamper: wrong public key (use a different random key)
    other_pk = nsig.SigningKey(_R.randbytes(32)).verify_key.encode()
    idx4 = len(want)
    src.append(f"bytes opk{idx4} = {bytes_lit(other_pk)};")
    src.append(f'print("verify_wrong_key_{idx4} " + ed25519.verify(opk{idx4}, vmsg{idx}, vsig{idx}).to_str());')
    want.append(f"verify_wrong_key_{idx4} false")

# --- non-canonical S: S = L, L+1, etc. must be rejected ---------------------
L = 2**252 + 27742317777372353535851937790883648493
seed = _R.randbytes(32)
s = nsig.SigningKey(seed)
pk = s.verify_key.encode()
msg = b"canonical S test"
sig = s.sign(msg).signature
for bad_s in [L, L + 1, 2**255]:
    idx = len(want)
    bad_sig = sig[:32] + (bad_s % (1 << 256)).to_bytes(32, 'little')
    src.append(f"bytes npk{idx} = {bytes_lit(pk)};")
    src.append(f"bytes nmsg{idx} = {bytes_lit(msg)};")
    src.append(f"bytes nsig{idx} = {bytes_lit(bad_sig)};")
    src.append(f'print("noncanon_S_{idx} " + ed25519.verify(npk{idx}, nmsg{idx}, nsig{idx}).to_str());')
    want.append(f"noncanon_S_{idx} false")

# --- malformed lengths ------------------------------------------------------
idx = len(want)
src.append(f"bytes shortpk = {bytes_lit(pk)};")
src.append(f"bytes shortmsg = {bytes_lit(msg)};")
src.append(f"bytes shortsig = {bytes_lit(sig[:63])};")
src.append(f'print("short_sig_{idx} " + ed25519.verify(shortpk, shortmsg, shortsig).to_str());')
want.append(f"short_sig_{idx} false")

open(os.path.join(OUT, "t_ed25519.m31"), "w").write("\n".join(src) + "\n")
open(os.path.join(OUT, "t_ed25519.want"), "w").write("\n".join(want) + "\n")
print("wrote", len(want), "cases,", len(src), "src lines")
