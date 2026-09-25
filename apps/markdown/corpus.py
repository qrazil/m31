#!/usr/bin/env python3
"""Write the markdown test INPUTS.

    python3 apps/markdown/corpus.py apps/markdown/tests
    python3 apps/markdown/reference.py apps/markdown/tests   # the expectations

The .md files are checked in, so this only has to run when a case is added or
changed. It exists because two of the inputs cannot survive an editor: the
ones with meaningful TRAILING SPACES (a hard line break) and the one with CRLF
endings. `{SP}` stands for a space and is substituted on the way out.

A case with a `.hand` file beside it is one the oracle cannot judge -- GFM
tables, which it does not implement, and `divergences.md`, which is a list of
the places this program deliberately parts company with CommonMark. Those
expectations are written and checked by hand.
"""
import os, sys

D = sys.argv[1]
os.makedirs(D, exist_ok=True)

CASES = {

"headings.md": """\
# one
## two
### three
#### four
##### five
###### six
####### seven hashes is a paragraph
#no space is a paragraph

## closing hashes ##
## trailing hash kept#
#

Setext level one
================

Setext level two
----------------

   # three spaces is still a heading
    # four spaces is code
""",

"paragraphs.md": """\
One paragraph
spread over
three lines.

Another, after a blank line.


Two blank lines is still one break.

   leading spaces are dropped{SP}{SP}{SP}
and trailing ones break the line.
""",

"emphasis.md": """\
*single star* and _single underscore_.

**double star** and __double underscore__.

***all three*** and ___three underscores___.

a*b*c intraword star works.

a_b_c intraword underscore does not.

**nested *emphasis* inside strong**

*nested **strong** inside emphasis*
""",

"emphasis-awkward.md": """\
*unterminated emphasis runs to the end

closing without opening*

**mismatched* count

*mismatched** count

****four stars****

*****five*****

a * b * c with spaces is literal

*a**b*

**a*b**

foo_bar_baz

foo*bar*baz

_ leading space _

snake_case_word and another_one_here
""",

"code-spans.md": """\
A `simple` span.

Two backticks: ``a ` b``.

Three: ```x `` y```.

Unmatched ` backtick stays literal.

`` ` `` is a single backtick.

`  padded  ` keeps inner spaces, strips one each side.

`a
b` joins lines with a space.

`<b>&amp;</b>` is escaped, not markup.

Empty `` span.
""",

"fences.md": """\
```
plain fence
```

```python
def f(x):
    return x * 2
```

~~~
tilde fence with ``` inside
~~~

````
a fence of four holds ``` three
````

```   ruby  extra info words
x
```

```
unclosed fence runs to the end
""",

"indented-code.md": """\
A paragraph.

    indented code
    second line

      deeper line kept as is

    after a blank line

Back to a paragraph.

	tab indented code
""",

"lists-simple.md": """\
- one
- two
- three

* star markers
* again

+ plus markers
+ again

- item with
  a continuation line
- item with

  a second paragraph
""",

"lists-nested.md": """\
- a
  - b
    - c
- d

1. first
   - bullet inside
   - another
2. second
   1. nested ordered
   2. more

- mixed
+ markers start a new list
* each time

- outer
  1. inner ordered
  2. two
- outer again
""",

"lists-loose.md": """\
- tight one
- tight two

- loose one

- loose two

* item

  with two paragraphs
* second item

1. ordered loose

2. second

- tight again
- still tight

- - -

- a list after a break
""",

"ordered.md": """\
1. one
2. two
3. three

5. starts at five
6. six

1) paren delimiter
2) again

10. ten
11. eleven
""",

"quotes.md": """\
> a quote
> over two lines

> lazy continuation
carries on here

> # heading in a quote
>
> - list in a quote
> - second

> > nested
> > quote

> quote

not the quote
""",

"links.md": """\
[plain](http://example.com)

[with title](http://example.com "the title")

[single quoted title](/url 'title')

[paren title](/url (title))

[angle dest](</url with spaces>)

[balanced (parens) in url](/foo(bar)baz)

[empty dest]()

[nested [brackets] inside](/u)

[code `]` inside](/u)

[unclosed](/u

[not a link] (space before paren)

[escaped \\] bracket](/u)

[emphasis *inside*](/u)

[url with ampersand](/a?b=1&c=2)

[non-ascii](/café/中文)
""",

"images.md": """\
![alt text](/img.png)

![alt with title](/img.png "a title")

![](/empty-alt.png)

![*emphasis* in alt](/i.png)

![`code` in alt](/i.png)

An ![inline](/i.png) image in a paragraph.
""",

"autolinks.md": """\
<http://example.com>

<https://example.com/a?b=1&c=2>

<mailto:someone@example.com>

<someone@example.com>

<ftp://ftp.example.com/x>

<a@b>

a <http://x.y> b
""",

"breaks.md": """\
Two spaces{SP}{SP}
make a hard break.

A backslash\\
also makes one.

One space{SP}
makes a soft break.

Three spaces{SP}{SP}{SP}
also break.

Trailing spaces at the end of a paragraph{SP}{SP}{SP}
""",

"escapes.md": """\
\\* not emphasis \\*

\\# not a heading

\\[not a link\\](/u)

\\\\ a literal backslash

\\` not a code span \\`

\\a is not an escape

Ampersand & angle < > and quote " and apostrophe '
""",

"thematic.md": """\
---

***

___

- - -

* * *

_____________

   ---

Not a break:

--

a---
""",

"unicode.md": """\
Café naïve résumé.

中文标题测试 with **粗体**.

Emoji: \U0001f44d\U0001f3fd \U0001f469‍\U0001f469‍\U0001f467 \U0001f1ef\U0001f1f5

# Привет, мир

- リスト項目
- أخر

`código` and *énfasis*.

[リンク](/日本語)
""",

"mixed.md": """\
# Project

A short **description** with a [link](https://example.com/path?a=1&b=2).

## Install

```sh
git clone https://example.com/repo.git
cd repo && make
```

## Usage

1. Run `make`.
2. Then:

   ```
   ./prog --help
   ```

3. Read the output.

> **Note**
> This is a block quote with `code` and *emphasis*.

### Notes

- A list item with a very long line that goes on and on and does not wrap in
  the source, and continues here.
- Another item
  - nested
    - deeper

---

See <https://example.com> for more.
""",
}

HAND = {
"divergences.md": """\
Raw HTML is not in the subset, so a block of it is an escaped paragraph:

<div class="note">
  <p>hello</p>
</div>

<notascheme>

<not a link>

Inline raw HTML is escaped too: a <span>span</span> and a <br> break.

Entity references are not decoded: &amp; &copy; &#35; &#x2764;

Link reference definitions are not supported, so this is a paragraph:

[ref]: /url "title"

and [this][ref] is not a link, nor is [this] on its own.
""",

"tables.md": """\
| a | b |
| - | - |
| 1 | 2 |

| left | centre | right | none |
|:-----|:------:|------:|------|
| 1    | 2      | 3     | 4    |

| one | two |
| --- | --- |
| short |
| too | many | cells |

| esc \\| pipe | x |
| --- | --- |
| a | b |

| inline **bold** | `code` |
| --- | --- |
| [link](/u) | *em* |

a | b
--- | ---
1 | 2

| not a table |
| because no delimiter row |
""",
}

for name, body in list(CASES.items()) + list(HAND.items()):
    with open(os.path.join(D, name), "w", encoding="utf-8", newline="") as f:
        f.write(body.replace("{SP}", " "))

for name in HAND:
    open(os.path.join(D, name[:-3] + ".hand"), "w").close()

# CRLF endings, written as bytes: a hard break (two spaces before the CR), a
# list and a fence, so the \r has to be stripped in three different places.
crlf = ("# CRLF document\r\n"
        "\r\n"
        "A paragraph  \r\n"
        "with a hard break.\r\n"
        "\r\n"
        "- one\r\n"
        "- two\r\n"
        "\r\n"
        "```\r\n"
        "code line\r\n"
        "```\r\n")
with open(os.path.join(D, "crlf.md"), "wb") as f:
    f.write(crlf.encode())

print("wrote", len(CASES) + len(HAND) + 1, "inputs to", D)
