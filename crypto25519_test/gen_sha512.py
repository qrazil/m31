import os, sys
OUT = sys.argv[1] if len(sys.argv) > 1 else '.'
import hashlib, random

src = []
want = []
src.append("import sha512;")
src.append("")

# every length 0..300, deterministic pattern (matches t_sha1.src's approach)
src.append("bytes pattern = [];")
src.append("int n = 0;")
src.append("while (n <= 300) {")
src.append('    print("len" + n.to_str() + " " + sha512.hex(pattern));')
src.append("    pattern.push(n * 7 + 13 & 0xFF);")
src.append("    n = n + 1;")
src.append("}")

pat = bytearray()
for n in range(301):
    want.append(f"len{n} " + hashlib.sha512(bytes(pat)).hexdigest())
    pat.append((n * 7 + 13) & 0xFF)

# a big buffer, chunked at various sizes crossing the 128-byte block boundary
src.append("")
src.append("bytes big = [];")
src.append("int m = 0;")
src.append("while (m < 5000) {")
src.append("    big.push(m * 3 + 1 & 0xFF);")
src.append("    m = m + 1;")
src.append("}")
src.append('print("big-whole " + sha512.hex(big));')
src.append("const Array<int> CHUNKS = [1, 7, 63, 127, 128, 129, 255, 1000, 4096];")
src.append("for (int size in CHUNKS) {")
src.append("    sha512.Hasher h = sha512.Hasher();")
src.append("    int at = 0;")
src.append("    while (at < big.size()) {")
src.append("        int end = at + size;")
src.append("        if (end > big.size()) { end = big.size(); }")
src.append("        h.update(big.substr(at, end));")
src.append("        at = end;")
src.append("    }")
src.append('    print("big-by-" + size.to_str() + " " + h.digest().hex());')
src.append("}")

bigbuf = bytes((m * 3 + 1) & 0xFF for m in range(5000))
want.append("big-whole " + hashlib.sha512(bigbuf).hexdigest())
for size in (1, 7, 63, 127, 128, 129, 255, 1000, 4096):
    want.append(f"big-by-{size} " + hashlib.sha512(bigbuf).hexdigest())

open(os.path.join(OUT, "t_sha512.src"), "w").write("\n".join(src) + "\n")
open(os.path.join(OUT, "t_sha512.want"), "w").write("\n".join(want) + "\n")
print("wrote", len(want), "cases")
