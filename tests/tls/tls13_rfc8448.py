"""RFC 8448 (TLS 1.3 example traces), parsed from the RFC's own text.

`tls13_rfc8448.txt` is the RFC as published by the IETF. `fields()` returns,
in document order, every `name (N octets): hex...` item with the section it is
in and the `{client}`/`{server}` step it belongs to, so the oracles can quote
the RFC's published values instead of retyping them.
"""
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))


def _clean_lines():
    out = []
    for line in open(os.path.join(HERE, "tls13_rfc8448.txt"), encoding="utf-8"):
        line = line.rstrip("\n")
        if line.startswith("Thomson ") or line.startswith("RFC 8448 ") or "\f" in line:
            continue
        out.append(line)
    return out


FIELD = re.compile(r"^\s+([A-Za-z][A-Za-z0-9 ]*?) \((\d+) octets\):\s*(.*)$")
STEP = re.compile(r"^   \{(client|server)\}  (.*)$")
SECTION = re.compile(r"^(\d+)\.  (.*)$")
HEXLINE = re.compile(r"^\s+((?:[0-9a-f]{2} ?)+)$")


class Field:
    def __init__(self, section, step, name, size, value):
        self.section = section
        self.step = step
        self.name = name
        self.size = size
        self.value = value


def fields():
    lines = _clean_lines()
    result = []
    section = 0
    step = ""
    i = 0
    while i < len(lines):
        line = lines[i]
        m = SECTION.match(line)
        if m and 3 <= int(m.group(1)) <= 4:
            section = int(m.group(1))
        m = STEP.match(line)
        if m:
            step = m.group(2)
            i += 1
            while step.endswith(("same as", "(same as client", "(same as server")) or (
                step.count("(") > step.count(")")
            ):
                step += " " + lines[i].strip()
                i += 1
            continue
        m = FIELD.match(line)
        if m and section:
            name, size, rest = m.group(1), int(m.group(2)), m.group(3)
            hexs = rest.strip()
            i += 1
            while i < len(lines):
                j = i
                while j < len(lines) and lines[j].strip() == "":
                    j += 1
                hm = HEXLINE.match(lines[j]) if j < len(lines) else None
                if not hm:
                    break
                hexs += " " + hm.group(1).strip()
                i = j + 1
            if size == 0 or hexs.strip() == "(empty)":
                value = b""
            else:
                value = bytes.fromhex(hexs.replace(" ", ""))
                assert len(value) == size, (section, step, name, size, len(value))
            result.append(Field(section, step, name, size, value))
            continue
        i += 1
    return result


def find(section, step_contains, name, occurrence=0):
    """The `occurrence`th field called `name` in a step containing the text."""
    hits = [
        f
        for f in fields()
        if f.section == section and step_contains in f.step and f.name == name
    ]
    if occurrence >= len(hits):
        raise KeyError((section, step_contains, name, occurrence, len(hits)))
    return hits[occurrence].value


if __name__ == "__main__":
    for f in fields():
        if f.section == 3 or f.section == 4:
            print(f.section, "|", f.step[:60], "|", f.name, f.size)
