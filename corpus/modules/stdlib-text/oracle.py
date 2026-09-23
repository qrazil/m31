#!/usr/bin/env python3
"""The expected output of main.src, computed with CPython.

    python3 oracle.py | diff - main.out

Every line CPython can answer is CPython's own answer -- `str.replace`,
`str.splitlines`, `str.split`, `str.strip`, `str.removeprefix`,
`str.removesuffix`, `str.rjust`, `str.ljust`. The handful where this language
deliberately answers something else are computed by `ours_*` below, each with
the reason written next to it; `main.src` marks the same four DIFFERS.

`parse_int` is the exception: CPython's `int()` is not an oracle for it,
because `int()` accepts what `str.parse_int` refuses -- surrounding
whitespace, `_` separators and non-ASCII decimal digits (`int("４２") == 42`).
The oracle for the grammar is the language's own built-in, and main.src checks
against it in the program, on every probe. CPython is still the oracle for the
VALUE of everything that does parse, which is asserted here.
"""

out = []


def p(s):
    out.append(s)


def visible(s):
    r = []
    for c in s:
        n = ord(c)
        if n < 32 or n == 127:
            r.append("\\x%02x" % n)
        elif n in (0xA0, 0x2028, 0x2029):
            r.append("\\u{%04x}" % n)
        else:
            r.append(c)
    return "".join(r)


def show(xs):
    return "%d [%s]" % (len(xs), "|".join(visible(x) for x in xs))


# --- what this language answers where CPython answers something else ---------
def ours_lines(s):
    """`\\n` ends a line, `\\r\\n` is one terminator, nothing else breaks, and
    a trailing terminator adds no empty last line."""
    parts = s.split("\n")
    res = [x[:-1] if x.endswith("\r") else x for x in parts[:-1]]
    if parts[-1] != "":
        res.append(parts[-1])
    return res


SPACE = " \t\n\r\f\v"  # runtime/rt.c is_space, and str.trim's set


def ours_split_whitespace(s):
    res, cur = [], ""
    for c in s:
        if c in SPACE:
            if cur:
                res.append(cur)
            cur = ""
        else:
            cur += c
    if cur:
        res.append(cur)
    return res


# --- replace -----------------------------------------------------------------
p("a-b-c".replace("-", "+"))
p("a-b-c".replace("-", "+", 1))
p("a-b-c".replace("-", "+", 2))
p("a-b-c".replace("-", "+", 9))
p("a-b-c")  # count: 0 -- CPython's replace(.., 0) is the string unchanged
p("abc".replace("z", "!"))
p("ab".replace("abcd", "!"))
p("[" + "abc".replace("abc", "") + "]")
p("[" + "a-b-c".replace("-", "") + "]")
p("aaa".replace("aa", "b"))
p("aaaa".replace("aa", "b"))
p("aaa".replace("aa", "b", 1))
p("a".replace("a", "aa"))
p("aaa".replace("a", "aa"))
p("x&y".replace("&", "&amp;"))
p("héllo héllo".replace("é", "e"))
p("a,b".replace(",", " — "))
p("日本語".replace("本", "ほん"))
p("é©".replace(")", "!"))
p("[" + "".replace("a", "b") + "]")

# --- lines ---------------------------------------------------------------------
for s in ["", "a", "a\n", "a\nb", "a\nb\n", "\n", "\n\n", "a\n\nb", "a\n\n",
          "a\r\nb\r\n", "a\r\n\r\nb"]:
    assert ours_lines(s) == s.splitlines(), s
    p(show(s.splitlines()))
# DIFFERS 1-3: a lone \r is not a terminator, not in the middle and not at the
# end; and \v, \f, \x1c-\x1e, \x85, U+2028 and U+2029 are not terminators at
# all. CPython's splitlines breaks on every one of them.
for s in ["a\rb", "a\r", "a\u000bb\u000cc", "a" + chr(0x2028) + "b"]:
    assert ours_lines(s) != s.splitlines(), s
    p(show(ours_lines(s)))
p(show("héllo\n日本\n".splitlines()))

# --- split_whitespace ------------------------------------------------------------
for s in ["", "   ", "a", "  a\tb  c ", "\n\ta\u000bb\u000c\r", "café 日本 x"]:
    assert ours_split_whitespace(s) == s.split(), s
    p(show(s.split()))
# DIFFERS 4: CPython's str.split() also breaks on \x1c-\x1f and on Unicode
# spaces such as U+00A0. `str.trim`'s set is the six ASCII bytes, and one
# language does not get two definitions of space.
for s in ["a\u001cb", "a" + chr(0xA0) + "b"]:
    assert ours_split_whitespace(s) != s.split(), s
    p(show(ours_split_whitespace(s)))

p("space set agrees with trim")

# --- trim_start, trim_end --------------------------------------------------------
for s in ["  a  ", "", "   ", "a"]:
    p("[" + s.lstrip(SPACE) + "]")
    p("[" + s.rstrip(SPACE) + "]")
p("[" + "    indented line   ".rstrip(SPACE) + "]")
p(str(len((chr(0xA0) + "x").lstrip(SPACE).encode())))
p(str(len(("x" + chr(0xA0)).rstrip(SPACE).encode())))

# --- strip_prefix, strip_suffix ---------------------------------------------------
p("--verbose".removeprefix("--"))
p("verbose".removeprefix("--"))
p("[" + "--".removeprefix("--") + "]")
p("[" + "-".removeprefix("--") + "]")
p("[" + "abc".removeprefix("") + "]")
p("[" + "".removeprefix("x") + "]")
p("notes.txt".removesuffix(".txt"))
p("notes.md".removesuffix(".txt"))
p("[" + ".txt".removesuffix(".txt") + "]")
p("[" + "abc".removesuffix("") + "]")
p("[" + "".removesuffix("x") + "]")
p("[" + "md".removesuffix(".txt") + "]")
p("époque".removeprefix("é"))
p("café".removesuffix("é"))
p("é".removeprefix("e"))

# --- pad_left, pad_right -----------------------------------------------------------
p("[" + "7".rjust(3) + "]")
p("[" + "7".ljust(3) + "]")
p("[" + "7".rjust(3, "0") + "]")
p("[" + "".rjust(3, ".") + "]")
p("[" + "".ljust(3, ".") + "]")
p("[" + "abc".rjust(3) + "]")
p("[" + "abcde".rjust(3) + "]")
p("[" + "abc".rjust(0) + "]")
p("[" + "abc".rjust(-5) + "]")
p("[" + "café".ljust(5) + "]")
p(str(len("café".ljust(5).encode())))
p("[" + "日本".rjust(4, "·") + "]")
p(str(len("x".rjust(4, "·").encode())))

# --- parse_int ---------------------------------------------------------------------
# The grammar: an optional sign, then one or more ASCII digits, then the end.
# CPython's int() is deliberately not used to decide acceptance -- see the
# docstring -- but it IS used to check the value of everything accepted.
LIMIT_HI = "9223372036854775807"
LIMIT_LO = "-9223372036854775808"


def parsed(s):
    body = s[1:] if s[:1] in "+-" else s
    if body == "":
        return "err no digits to parse"
    for i, ch in enumerate(body):
        if ch not in "0123456789":
            at = i + (1 if s[:1] in "+-" else 0)
            b = s.encode()[at]
            shown = "'%c'" % b if 32 <= b <= 126 else "byte 0x%02x" % b
            head = "err invalid digit %s at position %d" % (shown, at)
            if b in (32, 9, 10, 13, 12, 11):
                head += ": surrounding space is not accepted, so trim first"
            return head
    v = int(s)
    if v > int(LIMIT_HI):
        return "err the number is above %s, an int's largest value" % LIMIT_HI
    if v < int(LIMIT_LO):
        return "err the number is below %s, an int's smallest value" % LIMIT_LO
    return "ok %d" % v


PROBES = ["42", "-42", "+42", "0", "-0", "007", LIMIT_HI, LIMIT_LO,
          "9223372036854775808", "-9223372036854775809",
          "99999999999999999999999999", "", "+", "-", "4 2", " 42", "42 ",
          "42abc", "abc", "4.2", "4-2", "1_000", "0x10", "４２", "\t1", "1\n"]
for s in PROBES:
    p(parsed(s))
p("parse_int agrees with the built-in")

print("\n".join(out))
