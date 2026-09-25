# apps/tui — a terminal user interface library

A buffer of grapheme clusters, a diffing renderer, ratatui's constraint
layout, width-aware text and five widgets. Nothing here touches a terminal:
everything is computation against an in-memory buffer, and the output of a
frame is one `bytes`. That is what makes it testable, and it is the whole
design.

This is **stage 1 of three**. Stage 2 adds the widgets and the styling layer
listed at the bottom; stage 3 adds the terminal layer (`lib/term.src`, being
written separately) and the application loop that joins them.

    bash apps/tui/check.sh                    # tests, then the benchmark
    ./build.sh apps/tui/demo.src -o /tmp/d    # and run /tmp/d in a terminal

**This library is not part of the standard library.** The compiler knows
nothing about it, nothing was added to `src/stdlib.rs`, and nothing was put
in `lib/`. It is an ordinary library that happens to live in this
repository — and the awkwardness of that is written up in `FRICTION.md` §1,
because the language has no way yet for a program to depend on a library
outside `lib/`. Today a program that uses this must live in this directory.

---

## The modules

Seven, in dependency order. They are all prefixed `tui` because module names
are globally unique and every unprefixed name a UI library wants is either
taken by `lib/` or likely to collide with a user's own file — see
`FRICTION.md` §1.

| module | what is in it |
|---|---|
| `tuistyle` | `Colour`, `Style`, `Depth`, the attribute bits, and the SGR escapes |
| `tuigeom`  | `Rect`, `Constraint`, `Layout`, and the constraint solver |
| `tuitext`  | `Align`, `wrap`, `truncate`, `pad`, `fit` — all in terminal columns |
| `tuibuf`   | `Cell` and `Buffer` |
| `tuidiff`  | `full`, `diff`, and the small control sequences |
| `tuiwidget`| the `Widget` interface, `Block` and `Paragraph` |
| `tuilists` | `ListView` and `Table` |

Each file's header comment is its reference documentation.

---

## The shape of a program

```c
import tuibuf;
import tuidiff;
import tuigeom;
import tuilists;
import tuistyle;
import tuitext;
import tuiwidget;

tuibuf.Buffer screen = tuibuf.Buffer.sized(80, 24);

List<tuigeom.Rect> band = tuigeom.rows(screen.area(),
    [tuigeom.Constraint.Length(3), tuigeom.Constraint.Fill(1)]);

tuiwidget.Block panel = tuiwidget.Block(title: "files",
    borders: tuiwidget.Borders.Rounded);
panel.render(screen, band[1]);
tuilists.ListView(names, selected: 3, highlight_symbol: "> ")
    .render(screen, panel.inner(band[1]));

bytes frame = tuidiff.full(screen, tuistyle.Depth.Ansi256);   // one write
```

A widget is handed the rectangle it draws in; it holds no position of its
own. A block and its content are two widgets, composed by the caller with
`block.inner(area)` — there is no `.block(..)` on every widget, so every
widget stays independent of `Block` and a user's widget is no harder to use
than a shipped one.

---

## A cell is a grapheme cluster

Not a code point. `é` written as `e` + U+0301 is one cell, `👍🏽` is one cell,
a flag is one cell.

A cluster two columns wide — a CJK ideograph, an emoji — occupies **two**
cells: the cluster, and a *continuation* whose symbol is empty and whose
width is 0. The renderer never writes a continuation, and every write keeps
the invariant that a continuation has a two-wide cell immediately to its
left. Overwriting either half of a pair turns the other into a space. That
is where terminal libraries have bugs, and it is checked from both
directions in `tests.src`.

---

## The layout rules

`tuigeom.resolve(total, constraints)` answers one size per constraint, and
**the sizes always sum to exactly `total` and none is negative** — so the
parts tile the whole with no gap and no overlap. That invariant is checked
against 2 000 random constraint sets from a seeded generator, horizontally
and vertically, in `tests.src`.

Each constraint states a preferred size, clamped into `0..total`:
`Length(v)` is `v`, `Percentage(p)` is `total * p / 100`, `Ratio(a, b)` is
`total * a / b`, `Min(v)` and `Max(v)` are `v`, and `Fill(w)` is 0.

If they come to **less** than the total, the surplus goes to the `Fill`s by
weight; failing that to the `Min`s equally; failing that to the last
constraint. If they come to **more**, the deficit is taken back in five
rounds — `Fill`, then `Max`, then `Percentage` and `Ratio`, then `Min`, then
`Length` — and within a round it is shared in proportion to what each
segment has to give. Both directions use largest-remainder rounding, in
whole cells, with ties to the earlier segment.

The full statement, including why `[Max(30), Max(30)]` in 100 cells gives 30
and 70, is the header comment of `tuigeom.src`.

---

## What the renderer decides

`tuidiff.diff(prev, next, depth)` writes a cell when its symbol, style or
width changed. Between two changed cells it either jumps (`\x1b[nC`) or
walks (writes the unchanged cells again), whichever is fewer bytes — and
only walks when the run is already in the pen's style. Row changes are
always an absolute `\x1b[r;cH`. Styles are written as differences, so
stopping an underline is `\x1b[24m` rather than a reset and a repaint. A run
of plain blanks to the end of a row becomes `\x1b[K`.

Colour depth is a value you pass in — `Ansi16`, `Ansi256`, `TrueColour` —
and a colour too rich for the depth is converted to the nearest entry of the
smaller palette. Detection belongs to stage 3.

---

## Performance

A 200×50 frame — 10 000 cells, a full-screen terminal — built with
`gcc -O2`, mean of 200 runs:

| | time | bytes |
|---|---|---|
| paint a screenful of widgets | 1.60 ms | |
| `full` (every cell) | 0.62 ms | 12 533 |
| `diff` after one cell changed | 0.46 ms | 21 |
| `diff` with nothing changed | 0.47 ms | 0 |
| **paint + diff, the per-keystroke cost** | **2.10 ms** | |

The 60-frames-a-second budget is 16.67 ms, so a complete frame is about
**8× inside it**, and a frame in which nothing much moved is 21 bytes on the
wire. Run `bash apps/tui/check.sh` for the numbers on your own
machine.

---

## Testing without a terminal

`tests.src` prints a report — 110 assertions plus rendered frames — and
`check.sh` compares it against `tests.out`, checks for a `FAIL` line, checks
`__rc_live=0`, and checks that gcc and clang at `-O0` and `-O2` all agree.

A buffer prints itself: `print(buf)` gives the rows as text, and
`buf.frame()` puts a rule around them so trailing spaces are visible and an
expectation is readable:

```
+----------------------------------------+
|The quick brown fox    dog, and keeps on|
|jumps over the lazy    going for quite a|
|dog, and keeps on      while after that.|
+----------------------------------------+
```

`tuidiff.show(bytes)` does the same for escape sequences:
`\e[1;1H\e[1;4mab\e[22mcd\e[0m`.

Covered: a wide character at the right edge, both halves of a wide pair
overwritten, an emoji with a modifier, combining marks with and without a
base, an empty rectangle, a rectangle smaller than its border, a list longer
than its view, a table whose columns do not fit, every border style, all
three colour depths, and a widget written outside the library in each of the
three forms a one-method interface accepts.

---

## Left for stage 2 and stage 3

**Stage 2:** the styling layer (a `Theme`, styled spans inside one line),
`Gauge`, `Sparkline`, `BarChart`, `Chart`, `Scrollbar`, `Tabs`, `Viewport`,
`TextInput`, `Spinner`, mouse events, and a `Frame`/`Terminal` abstraction
over the double buffer.

**Stage 3:** `lib/term.src` — raw mode, key decoding, window size, one-write
flush, read-with-timeout — the capability detection that chooses a `Depth`,
and the application loop that joins the two.
