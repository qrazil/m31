"""A hostile or quirky sftp v3 subsystem for tests/ssh/test_m31sftp.py.

Run by sshd as `Subsystem sftp python3 -I fake_sftp_server.py SCENARIO`
(shortread, esc, names, one, loop). Adapted from the independent audit harness.
"""
import sys, os, struct
scen = sys.argv[1]
inp, out = sys.stdin.buffer, sys.stdout.buffer
def rd(n):
    b = b""
    while len(b) < n:
        c = inp.read(n - len(b))
        if not c: sys.exit(0)
        b += c
    return b
def send(t, body):
    p = bytes([t]) + body
    out.write(struct.pack(">I", len(p)) + p); out.flush()
def s(b):
    if isinstance(b, str): b = b.encode()
    return struct.pack(">I", len(b)) + b
def attrs(size=None, perm=None):
    f = 0; b = b""
    if size is not None: f |= 1; b += struct.pack(">Q", size)
    if perm is not None: f |= 4; b += struct.pack(">I", perm)
    return struct.pack(">I", f) + b
def status(i, code, msg=""):
    send(101, struct.pack(">II", i, code) + s(msg) + s(""))
DATA = bytes((i * 7) & 255 for i in range(100000))
ESC = "\x1b]0;PWNED-TITLE\x07\x1b[2J"
dirnames = {"loop": ["d"], "names": ["good", "..", "a/b", "", ".", "x\x00y", "../../escape"], "esc": ["f" + ESC + "ile"], "one": ["a.txt"]}
handles = {}
while True:
    hdr = inp.read(4)
    if len(hdr) < 4: break
    n = struct.unpack(">I", hdr)[0]
    body = rd(n)
    t = body[0]
    if t == 1:
        send(2, struct.pack(">I", 3)); continue
    i = struct.unpack(">I", body[1:5])[0]
    rest = body[5:]
    def getstr(b, o=0):
        l = struct.unpack(">I", b[o:o+4])[0]; return b[o+4:o+4+l], o+4+l
    if t == 16:   # realpath
        p, _ = getstr(rest)
        path = p.decode(errors="replace")
        if scen == "esc": path = "/home/" + ESC + "x"
        send(104, struct.pack(">II", i, 1) + s(path) + s(path) + attrs())
    elif t in (17, 7):  # stat
        p, _ = getstr(rest); p = p.decode(errors="replace")
        isdir = p.endswith("/d") or p.endswith("dir") or p == "/home/" + ESC + "x"
        if scen in dirnames and (p.endswith("/d") or p.endswith("dir")):
            send(105, struct.pack(">I", i) + attrs(0, 0o040755))
        elif scen in ("names","esc","one") and "/" in p and not p.endswith("dir"):
            send(105, struct.pack(">I", i) + attrs(len(DATA), 0o100644))
        else:
            send(105, struct.pack(">I", i) + attrs(100000, 0o100644))
    elif t == 11:  # opendir
        handles[b"H1"] = 0; send(102, struct.pack(">I", i) + s(b"H1"))
    elif t == 12:  # readdir
        h, _ = getstr(rest)
        if handles.get(b"H1") == 0:
            handles[b"H1"] = 1
            names = dirnames.get(scen, ["x"])
            b = struct.pack(">I", len(names))
            for nm in names:
                b += s(nm.encode("latin1")) + s(nm.encode("latin1")) + attrs(10, 0o100644)
            send(104, struct.pack(">I", i) + b)
        else:
            status(i, 1, "EOF")
    elif t == 3:   # open
        send(102, struct.pack(">I", i) + s(b"F1"))
    elif t == 5:   # read
        h, o = getstr(rest)
        off, ln = struct.unpack(">QI", rest[o:o+12])
        if scen == "shortread": ln = min(ln, 4096)
        if off >= len(DATA): status(i, 1, "EOF")
        else: send(103, struct.pack(">I", i) + s(DATA[off:off+ln]))
    elif t == 4:
        status(i, 0, "")
    elif t == 13 or t == 14 or t == 15 or t == 9:
        status(i, 4, "denied " + (ESC if scen == "esc" else ""))
    else:
        status(i, 8, "op unsupported " + (ESC if scen == "esc" else ""))
