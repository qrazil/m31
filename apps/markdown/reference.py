#!/usr/bin/env python3
"""Regenerate the expected HTML from a reference implementation.

    pip install commonmark
    python3 apps/markdown/reference.py apps/markdown/tests

The oracle is Python `commonmark`, a line-by-line port of cmark, which is the
CommonMark spec's own reference implementation. A `foo.md` with a `foo.hand`
beside it is skipped: its `foo.html` is hand-written, because it tests
something the oracle does not implement (GFM tables) or a place where this
program deliberately differs.

ONE normalisation is applied to the oracle's output, and it is narrow enough
to write out in full:

    '  ->  &#x27;

cmark escapes four characters in text (`&`, `<`, `>`, `"`) and leaves the
apostrophe alone. `lib/html.escape` in this repository escapes all five, on
purpose and with no flag to turn it off (see lib/html.src, and FRICTION.md
"lib/html.escape is stricter than cmark"). `&#x27;` is exactly what cmark's
own href escaper emits for an apostrophe, so the two agree everywhere but in
text and in a title attribute. Rather than write a second escaper in the app
and diverge from the standard library, the oracle is brought to the app's
spelling here, in one line, in the open.
"""
import glob
import os
import sys

import commonmark


def normalise(html: str) -> str:
    return html.replace("'", "&#x27;")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    d = sys.argv[1]
    n = 0
    for md in sorted(glob.glob(os.path.join(d, "*.md"))):
        if os.path.exists(md[:-3] + ".hand"):
            continue
        with open(md, "rb") as f:
            src = f.read().decode("utf-8")
        out = normalise(commonmark.commonmark(src))
        with open(md[:-3] + ".html", "w", encoding="utf-8", newline="\n") as f:
            f.write(out)
        n += 1
    print(f"wrote {n} expectations from commonmark into {d}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
