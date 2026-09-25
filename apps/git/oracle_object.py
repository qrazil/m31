#!/usr/bin/env python3
"""A loose-object reader written from the format, not from `git`.

    python3 apps/git/oracle_object.py <repo-or-gitdir>

prints one canonical line per loose object -- and, for a tree, one more per
entry -- for `t_object.src` to be diffed against. It uses only `zlib` and
`hashlib`, so it agrees with the language program only if both read the format
correctly; it is not `git` wearing a hat.

Every field that can hold arbitrary octets -- a path in a tree, an author's
name, a commit message -- is printed as hex or as its SHA-1, never as text.
A repository with a Latin-1 filename in it should compare exactly as cleanly
as one without, and a comparison that goes through a decoder is a comparison
of the decoders.
"""
import hashlib
import os
import sys
import zlib


def gitdir(path):
    d = os.path.join(path, ".git")
    if os.path.isdir(d):
        return d
    return path


def find(b, ch, at=0):
    i = b.find(ch, at)
    return i


def hx(b):
    return b.hex() if b else "-"


def split_headers(content):
    """Header lines (continuations folded) and the message after the blank."""
    lines = []
    at = 0
    n = len(content)
    while at < n:
        e = content.find(b"\n", at)
        if e < 0:
            e = n
        if e == at:
            return lines, content[at + 1:]
        line = content[at:e]
        at = e + 1
        while at < n and content[at:at + 1] == b" ":
            c = content.find(b"\n", at)
            if c < 0:
                c = n
            line += b"\n" + content[at + 1:c]
            at = c + 1
        lines.append(line)
    return lines, b""


def ident(v):
    lt = v.rfind(b"<")
    gt = v.find(b">", lt + 1) if lt >= 0 else -1
    if lt >= 0 and gt >= 0:
        end = lt
        while end > 0 and v[end - 1:end] == b" ":
            end -= 1
        name = v[:end]
        email = v[lt + 1:gt]
        rest = v[gt + 1:]
    else:
        name, email, rest = v, b"", b""
    rest = rest.lstrip(b" ")
    when, zone = 0, "+0000"
    if rest:
        parts = rest.split(b" ", 1)
        if parts[0].isdigit():
            when = int(parts[0])
        if len(parts) > 1:
            z = parts[1].strip()
            if len(z) == 5 and z[0:1] in (b"+", b"-") and z[1:].isdigit():
                zone = z.decode("ascii")
    sign = -1 if zone[0] == "-" else 1
    offset = sign * (int(zone[1:3]) * 60 + int(zone[3:5]))
    return "%s|%s|%d|%s|%d" % (hx(name), hx(email), when, zone, offset)


def entries(content):
    out = []
    at = 0
    n = len(content)
    while at < n:
        sp = content.find(b" ", at)
        nul = content.find(b"\0", sp + 1)
        mode = int(content[at:sp], 8)
        name = content[sp + 1:nul]
        oid = content[nul + 1:nul + 21].hex()
        out.append((mode, name, oid))
        at = nul + 21
    return out


def main():
    gd = gitdir(sys.argv[1])
    od = os.path.join(gd, "objects")
    names = []
    for two in os.listdir(od):
        if len(two) == 2 and all(c in "0123456789abcdef" for c in two):
            for rest in os.listdir(os.path.join(od, two)):
                if len(rest) == 38:
                    names.append(two + rest)
    names.sort()

    out = []
    for oid in names:
        with open(os.path.join(od, oid[:2], oid[2:]), "rb") as f:
            plain = zlib.decompress(f.read())
        assert hashlib.sha1(plain).hexdigest() == oid, oid
        nul = plain.index(b"\0")
        kind, size = plain[:nul].split(b" ")
        content = plain[nul + 1:]
        assert len(content) == int(size), oid
        kind = kind.decode("ascii")
        if kind == "blob":
            out.append("%s blob %d %s" % (oid, len(content), hashlib.sha1(content).hexdigest()))
        elif kind == "tree":
            es = entries(content)
            out.append("%s tree %d %d" % (oid, len(content), len(es)))
            for i, (mode, name, sub) in enumerate(es):
                out.append("%s tree-entry %d %06o %s %s" % (oid, i, mode, sub, hx(name)))
        elif kind == "commit":
            lines, msg = split_headers(content)
            tree = ""
            parents = []
            author = committer = ""
            extra = []
            for line in lines:
                k, _, v = line.partition(b" ")
                k = k.decode("ascii", "replace")
                if k == "tree" and not tree:
                    tree = v.decode("ascii")
                elif k == "parent":
                    parents.append(v.decode("ascii"))
                elif k == "author" and not author:
                    author = ident(v)
                elif k == "committer" and not committer:
                    committer = ident(v)
                else:
                    extra.append(line)
            out.append("%s commit %d tree=%s parents=%s author=%s committer=%s extra=%d/%s msg=%d/%s" % (
                oid, len(content), tree, ",".join(parents) or "-", author, committer,
                len(extra), hashlib.sha1(b"\n".join(extra)).hexdigest(),
                len(msg), hashlib.sha1(msg).hexdigest()))
        elif kind == "tag":
            lines, msg = split_headers(content)
            obj = typ = ""
            tname = b""
            tagger = "-"
            extra = []
            for line in lines:
                k, _, v = line.partition(b" ")
                k = k.decode("ascii", "replace")
                if k == "object" and not obj:
                    obj = v.decode("ascii")
                elif k == "type" and not typ:
                    typ = v.decode("ascii")
                elif k == "tag" and not tname:
                    tname = v
                elif k == "tagger" and tagger == "-":
                    tagger = ident(v)
                else:
                    extra.append(line)
            out.append("%s tag %d object=%s type=%s name=%s tagger=%s extra=%d/%s msg=%d/%s" % (
                oid, len(content), obj, typ, hx(tname), tagger,
                len(extra), hashlib.sha1(b"\n".join(extra)).hexdigest(),
                len(msg), hashlib.sha1(msg).hexdigest()))
        else:
            raise SystemExit("unknown type %r in %s" % (kind, oid))
    print("\n".join(out))


if __name__ == "__main__":
    main()
