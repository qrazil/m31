#!/usr/bin/env python3
"""A loose-object AND packfile reader written from the format, not from
`git`.

    python3 apps/git/oracle_object.py <repo-or-gitdir>

prints one canonical line per object -- loose or packed -- and, for a tree,
one more per entry, for `t_object.m31` to be diffed against. It uses only
`zlib` and `hashlib` (never `git` itself, and never `apps/git/pack.m31`), so
it agrees with the language program only if both read the format correctly;
it is not `git` wearing a hat, and it is not this program checking itself.

Every field that can hold arbitrary octets -- a path in a tree, an author's
name, a commit message -- is printed as hex or as its SHA-1, never as text.
A repository with a Latin-1 filename in it should compare exactly as cleanly
as one without, and a comparison that goes through a decoder is a comparison
of the decoders.

--- packfiles, from scratch -----------------------------------------------

`.idx` v2 only (fanout, sorted 20-octet names, CRC32 -- unused here, a 4-byte
offset per object with the large-offset table for the MSB-set case), the
packfile's own variable-length type+size header, `OBJ_OFS_DELTA` (a backward
offset from the object's own header, `+1`-folded 7-bit groups) and
`OBJ_REF_DELTA` (a base named by id, looked up in the same `.idx` first and
falling back to a loose object), and the copy/insert delta format applied
against the resolved base -- independently of, and in general shaped
differently from, `apps/git/pack.m31`'s own version of all of this. Python's
`zlib.decompressobj` does the mid-offset inflate: fed the tail of the pack
buffer from a start offset, `.unused_data` after it returns is what is left
over once the stream's own end is reached, so `len(fed) - len(unused_data)`
is exactly the "how many input octets did that consume" `zlib.inflate_at`
answers in the language program -- the same primitive, a different
implementation of it.
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


# --- packfiles, entirely independent of apps/git/pack.m31 -------------------

def loose_names(od):
    names = []
    for two in os.listdir(od):
        if len(two) == 2 and all(c in "0123456789abcdef" for c in two):
            for rest in os.listdir(os.path.join(od, two)):
                if len(rest) == 38:
                    names.append(two + rest)
    return names


def loose_plain(od, oid):
    """`oid`'s loose plaintext, or `None` if it is not loose."""
    path = os.path.join(od, oid[:2], oid[2:])
    if not os.path.exists(path):
        return None
    with open(path, "rb") as f:
        return zlib.decompress(f.read())


def find_packs(od):
    """`[(idx_path, pack_path), ...]` for every matched `.idx`/`.pack` pair
    under `od/pack`, sorted by name."""
    packdir = os.path.join(od, "pack")
    if not os.path.isdir(packdir):
        return []
    out = []
    for name in sorted(os.listdir(packdir)):
        if name.endswith(".idx"):
            base = name[:-4]
            packpath = os.path.join(packdir, base + ".pack")
            if os.path.isfile(packpath):
                out.append((os.path.join(packdir, name), packpath))
    return out


class Idx:
    """A parsed `.idx`, format v2 only -- v1 (no magic) is refused, the same
    choice `apps/git/pack.m31` makes and for the same reason (the module
    header there says it)."""

    def __init__(self, data):
        assert data[:4] == b"\xff\x74\x4f\x63", "not a v2 .idx (no v2 magic -- v1 is refused, not guessed at)"
        version = int.from_bytes(data[4:8], "big")
        assert version == 2, "unsupported .idx version %d" % version
        self.data = data
        self.fanout = [int.from_bytes(data[8 + 4 * i:12 + 4 * i], "big") for i in range(256)]
        self.n = self.fanout[255]
        self.names_off = 8 + 1024
        self.crc_off = self.names_off + 20 * self.n
        self.off_off = self.crc_off + 4 * self.n
        self.large_off = self.off_off + 4 * self.n

    def all_ids(self):
        return [self.data[self.names_off + 20 * i:self.names_off + 20 * i + 20].hex()
                for i in range(self.n)]

    def offset_of(self, id20):
        first = id20[0]
        lo = self.fanout[first - 1] if first > 0 else 0
        hi = self.fanout[first]
        while lo < hi:
            mid = (lo + hi) // 2
            got = self.data[self.names_off + mid * 20:self.names_off + mid * 20 + 20]
            if got == id20:
                return self._offset_at(mid)
            elif got < id20:
                lo = mid + 1
            else:
                hi = mid
        return None

    def _offset_at(self, i):
        v = int.from_bytes(self.data[self.off_off + i * 4:self.off_off + i * 4 + 4], "big")
        if v & 0x80000000:
            li = v & 0x7FFFFFFF
            return int.from_bytes(self.data[self.large_off + li * 8:self.large_off + li * 8 + 8], "big")
        return v


TYPE_NAMES = {1: "commit", 2: "tree", 3: "blob", 4: "tag"}


def read_obj_header(pack, offset):
    """(type, declared-size, header-length)."""
    c = pack[offset]
    kind = (c >> 4) & 0x7
    size = c & 0xF
    shift = 4
    i = offset + 1
    while c & 0x80:
        c = pack[i]
        i += 1
        size |= (c & 0x7F) << shift
        shift += 7
    return kind, size, i - offset


def inflate_at(pack, start):
    """(payload, input-octets-consumed) -- the same pairing `zlib.inflate_at`
    answers in the language program, from Python's own `zlib`."""
    d = zlib.decompressobj()
    out = d.decompress(pack[start:])
    out += d.flush()
    consumed = len(pack) - start - len(d.unused_data)
    return out, consumed


def read_delta_varint(delta, pos):
    shift = 0
    value = 0
    while True:
        c = delta[pos]
        pos += 1
        value |= (c & 0x7F) << shift
        shift += 7
        if not (c & 0x80):
            break
    return value, pos


def apply_delta(base, delta):
    base_size, pos = read_delta_varint(delta, 0)
    result_size, pos = read_delta_varint(delta, pos)
    assert base_size == len(base), "delta base-size disagrees with the actual base"
    out = bytearray()
    n = len(delta)
    while pos < n:
        c = delta[pos]
        pos += 1
        if c & 0x80:
            offset = 0
            size = 0
            if c & 0x01:
                offset |= delta[pos]
                pos += 1
            if c & 0x02:
                offset |= delta[pos] << 8
                pos += 1
            if c & 0x04:
                offset |= delta[pos] << 16
                pos += 1
            if c & 0x08:
                offset |= delta[pos] << 24
                pos += 1
            if c & 0x10:
                size |= delta[pos]
                pos += 1
            if c & 0x20:
                size |= delta[pos] << 8
                pos += 1
            if c & 0x40:
                size |= delta[pos] << 16
                pos += 1
            if size == 0:
                size = 0x10000
            out += base[offset:offset + size]
        elif c != 0:
            out += delta[pos:pos + c]
            pos += c
        else:
            raise AssertionError("delta opcode 0 is reserved")
    assert len(out) == result_size, "delta result-size disagrees with what it produced"
    return bytes(out)


def resolve_offset(idxes, packs_bytes, pack_i, offset, loose_od):
    """(kind_word, content) at `offset` in `packs_bytes[pack_i]`, walking an
    OFS_DELTA/REF_DELTA chain down iteratively and applying it back up --
    `apps/git/pack.m31`'s own `resolve_offset`, independently written."""
    pack = packs_bytes[pack_i]
    idx = idxes[pack_i]
    chain = []
    cur = offset
    base_kind = base_content = None
    steps = 0
    while base_kind is None:
        steps += 1
        assert steps < 10000, "delta chain implausibly deep or cyclic"
        kind, size, hdr_len = read_obj_header(pack, cur)
        if kind in TYPE_NAMES:
            content, _ = inflate_at(pack, cur + hdr_len)
            assert len(content) == size
            base_kind, base_content = TYPE_NAMES[kind], content
            break
        elif kind == 6:  # OBJ_OFS_DELTA
            p = cur + hdr_len
            c = pack[p]
            p += 1
            value = c & 0x7F
            while c & 0x80:
                c = pack[p]
                p += 1
                value = ((value + 1) << 7) | (c & 0x7F)
            base_off = cur - value
            assert 0 <= base_off < cur, "OBJ_OFS_DELTA offset out of range"
            delta, _ = inflate_at(pack, p)
            chain.append(delta)
            cur = base_off
        elif kind == 7:  # OBJ_REF_DELTA
            base_sha = pack[cur + hdr_len:cur + hdr_len + 20]
            delta, _ = inflate_at(pack, cur + hdr_len + 20)
            chain.append(delta)
            here = idx.offset_of(base_sha)
            if here is not None:
                cur = here
            else:
                base_kind, base_content = resolve_id(idxes, packs_bytes, base_sha.hex(), loose_od)
                break
        else:
            raise AssertionError("unknown pack object type %d" % kind)
    content = base_content
    for delta in reversed(chain):
        content = apply_delta(content, delta)
    return base_kind, content


def resolve_id(idxes, packs_bytes, oid, loose_od):
    """`(kind, content)` for `oid`, loose first (if `loose_od` is given, for
    an OBJ_REF_DELTA base a thin pack could leave outside every pack here),
    then each pack's `.idx` in turn."""
    if loose_od is not None:
        plain = loose_plain(loose_od, oid)
        if plain is not None:
            nul = plain.index(b"\0")
            kind, size = plain[:nul].split(b" ")
            return kind.decode("ascii"), plain[nul + 1:]
    id20 = bytes.fromhex(oid)
    for i, idx in enumerate(idxes):
        off = idx.offset_of(id20)
        if off is not None:
            return resolve_offset(idxes, packs_bytes, i, off, loose_od)
    raise AssertionError("object %s named by no known pack, nor loose" % oid)


def main():
    gd = gitdir(sys.argv[1])
    od = os.path.join(gd, "objects")
    names = set(loose_names(od))

    packs = find_packs(od)
    idxes = []
    packs_bytes = []
    for idx_path, pack_path in packs:
        with open(idx_path, "rb") as f:
            idx = Idx(f.read())
        with open(pack_path, "rb") as f:
            pack_bytes = f.read()
        assert pack_bytes[:4] == b"PACK"
        idxes.append(idx)
        packs_bytes.append(pack_bytes)
        names.update(idx.all_ids())

    names = sorted(names)

    out = []
    for oid in names:
        plain = loose_plain(od, oid)
        if plain is not None:
            assert hashlib.sha1(plain).hexdigest() == oid, oid
            nul = plain.index(b"\0")
            kind, size = plain[:nul].split(b" ")
            content = plain[nul + 1:]
            assert len(content) == int(size), oid
            kind = kind.decode("ascii")
        else:
            kind, content = resolve_id(idxes, packs_bytes, oid, od)
            assert hashlib.sha1(("%s %d\0" % (kind, len(content))).encode("ascii") + content).hexdigest() == oid, oid
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
