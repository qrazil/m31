import os, sys
OUT = sys.argv[1] if len(sys.argv) > 1 else '.'
import random

L = 2**252 + 27742317777372353535851937790883648493

def bytes_lit(b):
    return "[" + ", ".join(str(x) for x in b) + "]"

random.seed(99887766)
src = ["import scalar25519;", ""]
want = []

# reduce64 over random 64-byte inputs, and edge cases
edge64 = [0, 1, L - 1, L, L + 1, 2 * L, (1 << 512) - 1, (1 << 256)]
vals64 = list(edge64) + [random.randrange(0, 1 << 512) for _ in range(200)]
for idx, n in enumerate(vals64):
    b = (n % (1 << 512)).to_bytes(64, 'little')
    src.append(f"bytes r{idx} = {bytes_lit(b)};")
    src.append(f'print("reduce64_{idx} " + scalar25519.to_bytes(scalar25519.reduce64(r{idx})).hex());')
    want.append(f"reduce64_{idx} " + (n % L).to_bytes(32, 'little').hex())

# muladd over random and edge scalars already < L
edgeL = [0, 1, 2, L - 1, L - 2]
scalars = list(edgeL) + [random.randrange(0, L) for _ in range(200)]
triples = []
for i in range(0, len(scalars) - 2, 3):
    triples.append((scalars[i], scalars[i+1], scalars[i+2]))

for idx, (k, a, r) in enumerate(triples):
    kb = k.to_bytes(32, 'little')
    ab = a.to_bytes(32, 'little')
    rb = r.to_bytes(32, 'little')
    src.append(f"bytes mk{idx} = {bytes_lit(kb)};")
    src.append(f"bytes ma{idx} = {bytes_lit(ab)};")
    src.append(f"bytes mr{idx} = {bytes_lit(rb)};")
    src.append(f"Array<int> sk{idx} = scalar25519.from_bytes(mk{idx});")
    src.append(f"Array<int> sa{idx} = scalar25519.from_bytes(ma{idx});")
    src.append(f"Array<int> sr{idx} = scalar25519.from_bytes(mr{idx});")
    src.append(f'print("muladd_{idx} " + scalar25519.to_bytes(scalar25519.muladd(sk{idx}, sa{idx}, sr{idx})).hex());')
    want.append(f"muladd_{idx} " + ((k*a + r) % L).to_bytes(32, 'little').hex())

# is_reduced / from_bytes round trip and canonicity checks
check_vals = [0, 1, L - 1, L, L + 1, (1 << 255), (1 << 256) - 1]
for idx, n in enumerate(check_vals):
    b = (n % (1 << 256)).to_bytes(32, 'little')
    src.append(f"bytes cb{idx} = {bytes_lit(b)};")
    src.append(f"Array<int> cs{idx} = scalar25519.from_bytes(cb{idx});")
    src.append(f'print("isred_{idx} " + scalar25519.is_reduced(cs{idx}).to_str());')
    want.append(f"isred_{idx} " + ("true" if n < L else "false"))

open(os.path.join(OUT, "t_scalar25519.m31"), "w").write("\n".join(src) + "\n")
open(os.path.join(OUT, "t_scalar25519.want"), "w").write("\n".join(want) + "\n")
print("wrote", len(want), "cases")
