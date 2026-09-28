# apps/tui — a terminal user interface library

A buffer of grapheme clusters, a diffing renderer, ratatui's constraint
layout, width-aware text, a styling layer, thirteen widgets, and the
application loop that joins all of it to a real terminal. Nothing below the
loop touches a terminal: everything else is computation against an
in-memory buffer, and the output of a frame is one `bytes`. That is what
makes it testable, and it is the whole design.

Stage 1 was the buffer, the layout solver, width-aware text and five
widgets. **Stage 2 and stage 3 are both done**: the styling layer (`Theme`,
styled spans), the primitives `apps/git/design.md` names (a collapsible
outline, a synced jump list, a which-key overlay and the transient menu
built on it, an addressable diff view, a persistent footer, plus `Viewport`
and `Scrollbar` in support of the first two), and `tuiapp.Loop`, which reads
real keys through `lib/term.src` and redraws through this library's own
renderer. See "Left deliberately undone" below for what stage 2's original
wishlist dropped, and why.

    bash apps/tui/check.sh                       # tests, then the benchmark
    ./build.sh apps/tui/demo.src -o /tmp/d       # a static screen; run /tmp/d
    ./build.sh apps/tui/browse.src -o /tmp/b     # an interactive one; run /tmp/b

**This library is not part of the standard library.** The compiler knows
nothing about it, nothing was added to `src/stdlib.rs`, and nothing was put
in `lib/`. It is an ordinary library that happens to live in this
repository — and the awkwardness of that is written up in `FRICTION.md` §1,
because the language has no way yet for a program to depend on a library
outside `lib/`. Today a program that uses this must live in this directory.

---

## The modules

Fourteen, in dependency order. They are all prefixed `tui` because module
names are globally unique and every unprefixed name a UI library wants is
either taken by `lib/` or likely to collide with a user's own file — see
`FRICTION.md` §1. `tuiapp` is the one exception worth naming: it is the only
module that imports `lib/term.src` and touches raw mode, a real read and a
real write; every other module below it is still pure computation against a
buffer.

| module | what is in it |
|---|---|
| `tuistyle` | `Colour`, `Style`, `Depth`, the attribute bits, the SGR escapes, and `Theme` |
| `tuigeom`  | `Rect`, `Constraint`, `Layout`, and the constraint solver |
| `tuitext`  | `Align`, `wrap`, `truncate`, `pad`, `fit` — all in terminal columns |
| `tuibuf`   | `Cell` and `Buffer` |
| `tuidiff`  | `full`, `diff`, and the small control sequences |
| `tuiwidget`| the `Widget` interface, `Block`, `Paragraph`, `Span` and `Line` |
| `tuilists` | `ListView` and `Table` |
| `tuiscroll`| `Viewport` (a scroll offset and its clamping/reveal math) and `Scrollbar` |
| `tuioutline`| `Node`, `Outline` — the collapsible primary-view document |
| `tuijump`  | `Entry`, `JumpList` — a flat index synced two ways to an `Outline` |
| `tuimenu`  | `Popup`, `WhichKey`, `Switch`, `Transient` — one base, two overlays |
| `tuidiffview`| `Kind`, `Line`, `Hunk`, `DiffView` — a diff with a cursor addressable to the line |
| `tuifooter`| `split`, `Footer` — a screen band reserved regardless of the main view |
| `tuiapp`   | `Action`, `Handler`, `Loop` — the application loop, over `lib/term.src` |

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
smaller palette. Detection belongs to whatever a program built on this does
with `$TERM` and terminfo; `lib/term.src` deliberately leaves it out too,
for the same reason (its own header, "What is left out, and where it
belongs").

---

## The styling layer

`Theme` (`tuistyle.src`) is a plain struct of `Style` fields, one per role —
`normal`, `dim`, `title`, `accent`, `highlight`, `selected`, `error`,
`added`, `removed`, `border` — each with a sensible default, so `Theme()` is
a working plain theme exactly as `Style()` is a working plain style.
Re-skinning a program built on this is replacing one `Theme` value rather
than hunting down every `Style(...)` a widget was built with; there is no
registry and no lookup by name, because a struct field is what this
language's interfaces already give for free.

`Span` and `Line` (`tuiwidget.src`) are ratatui's `Span`/`Line` *concept*
through this library's own idiom rather than its API: a `Span` is a run of
text in one `Style`, and a `Line` is a `List<Span>` with a `render` method —
a `Widget`, exactly like `Block` and `Paragraph`, composing with `tuigeom`
the same way, rather than a separate "text buffer" type of its own.

```c
tuiwidget.Line([tuiwidget.Span("staged: ", style: th.dim),
    tuiwidget.Span("3 files", style: th.accent)])
    .render(buf, area);
```

---

## The primitives `apps/git/design.md` names

Six library primitives the interactive git client's design is built around,
none of them git-specific:

  - **`tuioutline.Outline`** — nested, collapsible sections. A node's
    children are rows only while every ancestor down to it is expanded;
    folding a section removes its whole subtree from the flattened list in
    one step. Every node carries an `id`, the caller's own key, because
    position is not a stable address once a fold changes every row number
    below it.
  - **`tuijump.JumpList`** — a flat, selectable index of an `Outline`'s
    current rows, synced two ways: `sync_from` rebuilds it from the
    outline's flattened rows, `follow` highlights the entry matching wherever
    the outline's cursor now is, and `activate` moves the outline's cursor
    to whatever is selected here. Matching is always by `id`, never by row
    number, which is what keeps the two in step regardless of what is
    currently folded.
  - **`tuimenu.WhichKey`** and **`tuimenu.Transient`** — a popup of
    `(key, label)` pairs, and its sibling: the same popup with togglable
    `Switch`es and a live-rendered command preview. Both are built on one
    private `Popup` (a titled, sized-and-centred box of text rows) rather
    than as two unrelated components.
  - **`tuidiffview.DiffView`** — a diff with a cursor addressable down to
    the hunk *and* the line: `at()` answers a `Row` naming a hunk's header
    or one particular line of one particular hunk, which is the granularity
    "stage whatever is under the cursor" needs.
  - **`tuifooter`** — `split(area, height)` cuts a fixed band off the
    bottom of the screen for a `Footer` (an ordinary `Line`) to draw into
    every frame, regardless of what the main view currently is.
  - **`tuiscroll.Viewport`** and **`tuiscroll.Scrollbar`** — the scroll
    offset and clamping math `Outline` and `DiffView` both need, and the bar
    that shows it, pulled out once rather than written a third and fourth
    time. `ListView` and `Table` keep their own inline copy, untouched.

---

## The application loop

`tuiapp.Loop` is the one module that touches a real terminal: raw mode via
`lib/term.src`'s `raw()`, a read with a timeout via its `Reader`, a frame
via this library's own diffing renderer, a flush via its `Writer`. It is
the loop `examples/keys.src` hand-writes, pulled out so a program built on
this library does not write it twice.

```c
tuiapp.Loop().run(view, on_event);
```

`view` is an ordinary `tuiwidget.Widget` — no new drawing interface exists,
because `Widget` already is "draw yourself into a rectangle of a buffer",
which is the whole of what a frame needs. `on_event` is a `tuiapp.Handler`,
a one-method interface in the same idiom `Widget` and every callback in this
language already use: `Action handle(term.Event ev)`, answering `Continue`,
`Redraw` or `Quit`. The loop itself decides *when* to redraw — after a
`Redraw`, or when `term.size()` says the window changed, since there is no
resize event — and whether that redraw is a full repaint or a diff against
the previous frame.

`apps/tui/browse.src` is a runnable proof: a small interactive file browser
built ONLY from widgets that existed before this loop (`ListView`,
`Block`, `Paragraph`) — j/k or the arrows move, Enter descends into a
directory, u or Backspace goes back up, q or Escape quits, and the terminal
is always restored on the way out, trap included, because that is what
`term.raw()`'s `Session` is for.

    ./build.sh apps/tui/browse.src -o /tmp/browse && /tmp/browse

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

`tests.src` prints a report — 163 assertions plus rendered frames — and
`check.sh` compares it against `tests.out`, checks for a `FAIL` line, checks
`__rc_live=0`, and checks that gcc and clang at `-O0` and `-O2` all agree.
Every widget and primitive added for stage 2 and stage 3 is in there,
against synthetic buffers and synthetic `term.Event`s with no terminal
anywhere near it — `tuiapp.Loop.run` is the one function in this library
that cannot be (it opens raw mode and blocks on a real read), so its own
pure pieces are tested instead (`resized`, and a `Handler` written outside
the library dispatched directly, the same proof `Widget` gets below) and
the loop itself is proven by `browse.src` against a real terminal.

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

## Left deliberately undone

Stage 2's original wishlist named a few things not built here, each a
named, separate omission rather than a silent one:

  - **`Gauge`, `Sparkline`, `BarChart`, `Chart`** — dashboard widgets. Nothing
    in a text-document-shaped client (`apps/git/design.md`'s own frame, and
    the only concrete client this library is being built toward so far)
    has a role for one, and adding a chart widget on spec, with nothing to
    render it TO, is exactly the guessing this library's design has avoided
    elsewhere.
  - **`Tabs`** — the locked design is one continuous, collapsible document
    with a jump list beside it, not a set of screens to switch between; a
    tabs widget would be built for a shape this library is not taking.
  - **Mouse events** — the locked design is keyboard-driven throughout.
    `lib/term.src` already decodes SGR mouse reports (`Event.Mouse`) for
    whatever does want them; there is simply nothing here that reads one.
  - **`TextInput`** — no named client need yet. Nothing in
    `apps/git/design.md` asks for free-text entry (a commit message is the
    likely first caller, and it is not designed yet); adding it now would be
    guessing at a shape.
  - **`Spinner`** — lowest priority on the original list, and nothing asked
    for it in the course of building the six primitives above, so it did
    not fall out for free and was not built speculatively.
  - **A separate `Frame`/`Terminal` abstraction over the double buffer** —
    `tuiapp.Loop` ended up owning the previous/next buffer pair and the
    decision between `tuidiff.full` and `tuidiff.diff` directly, which is
    the whole of what that abstraction would have been; a second type
    wrapping the same two fields would have had no job left to do.

`lib/term.src` itself is not on this list: it already existed, complete and
tested, before this work started — see the module table above, and do not
believe an older copy of this README that says otherwise.
