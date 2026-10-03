#!/usr/bin/env python3
"""The expected output of main.m31, computed with CPython's `re`.

    python3 oracle.py | diff - main.out

Every construct in main.m31 is one Python's `re` and this engine are
REQUIRED to agree on: literals, `.`, classes, anchors, greedy quantifiers,
alternation, and capturing groups all mean exactly the same thing whether
the implementation backtracks (CPython's `re`, via its C `sre` engine) or
simulates a Thompson NFA breadth-first (this module) -- none of v0's syntax
is a construct whose MEANING depends on which algorithm computes it. That is
the whole of `docs/regex-decision.md`'s argument for leaving backreferences
and lookaround out (both genuinely do change meaning by algorithm, which is
why they're cut), so `re` is a real, independent oracle for everything else
here, not a coincidence of two implementations agreeing by chance.

One offset note: Python's match objects report positions in CODE POINTS,
because a Python 3 `str` already is a sequence of code points -- there is no
separate "chars()" step the way there is here. So `m.start()`/`m.end()`
need no conversion to compare against this engine's `Group.start`/`.end`,
which are code-point offsets for the same reason (docs/regex-decision.md
§2).
"""

out = []


def p(x):
    out.append(str(x) if not isinstance(x, bool) else ("true" if x else "false"))


def show(m):
    p(m.group(0) if m else "(no match)")


import re

# --- literals -----------------------------------------------------------------
p(bool(re.search("abc", "xxabcxx")))
p(bool(re.search("abc", "xxabxx")))
show(re.search("abc", "xxabcxx"))
m = re.search("abc", "xxabcxx")
p(m.start())
p(m.end())

# --- `.` ------------------------------------------------------------------------
p(bool(re.search("a.c", "abc")))
p(bool(re.search("a.c", "a\nc", re.DOTALL)))  # this engine has no flags; DOTALL is its only `.`

# --- character classes ------------------------------------------------------
show(re.search("[abc]+", "xxabccbaxx"))
show(re.search("[a-z]+", "ABCdefGHI"))
show(re.search("[^0-9]+", "abc123"))
p(bool(re.search(r"[\]a-]", "]")))
p(bool(re.search(r"[\]a-]", "-")))
p(bool(re.search(r"[\]a-]", "b")))

# --- anchors ------------------------------------------------------------------
p(bool(re.search("^abc", "abcxyz")))
p(bool(re.search("^abc", "xabcxyz")))
p(bool(re.search("xyz$", "abcxyz")))
p(bool(re.search("xyz$", "abcxyzq")))
p(bool(re.search("^abc$", "abc")))
p(bool(re.search("^abc$", "abcd")))

# --- quantifiers --------------------------------------------------------------
show(re.search("a.*b", "a123b456b"))
p(bool(re.search("a+", "")))
p(bool(re.search("a?", "")))
show(re.search("a*", "bbb"))

show(re.search("a{2,4}", "aaaaaa"))
show(re.search("a{3}", "aaaaaa"))
show(re.search("a{2,}", "aaaaaa"))

# --- alternation ----------------------------------------------------------------
p(bool(re.search("cat|dog", "I have a dog")))
p(bool(re.search("cat|dog", "I have a bird")))

# --- grouping and capture ------------------------------------------------------
m = re.search(r"(\d+)-(\d+)", "call 123-456 now")
p(m.group(0))
p(m.group(1))
p(m.group(2))

m = re.search("((a)(b))c", "abc")
p(m.group(1))
p(m.group(2))
p(m.group(3))

m = re.search("(a)+", "aaa")
p(m.group(1))
p(m.start(1))
p(m.end(1))

m = re.search("(a)|(b)", "b")
p("none" if m.group(1) is None else "some " + m.group(1))
p("none" if m.group(2) is None else "some " + m.group(2))

p(re.compile("(a)(b)(c)").groups)
p(re.compile("abc").groups)

# --- shorthand classes ----------------------------------------------------------
p(bool(re.search(r"\d+", "abc123")))
p(bool(re.search(r"\w+", "_abc123")))
p(bool(re.search(r"\s+", "a b")))
p(bool(re.search(r"\D+", "abc")))
p(bool(re.search(r"\D+", "123")))
p(bool(re.search(r"\W+", "!!")))
p(bool(re.search(r"\S+", "x")))

# --- escaped metacharacters ---------------------------------------------------
p(bool(re.search(r"a\.b", "a.b")))
p(bool(re.search(r"a\.b", "axb")))
p(bool(re.search(r"a\{3\}", "a{3}")))
p(bool(re.search(r"a\{3\}", "aaa")))

# --- edges ----------------------------------------------------------------------
p(bool(re.search("", "")))
p(bool(re.search("", "xx")))
p(bool(re.search("abc", "")))

# --- find_all ---------------------------------------------------------------------
for x in re.findall(r"\d+", "a1 b22 c333"):
    p(x)

p(len(re.findall("a*", "baaab")))

# --- Unicode: Python's offsets are already code points -------------------------
m = re.search("é+", "café")
p(m.group(0))
p(m.start())
p(m.end())
p(bool(re.search("caf[eé]", "café")))
p(bool(re.search("caf[eé]", "cafe")))
for x in re.findall("[ée]", "café résumé"):
    p(x)

print("\n".join(out))
