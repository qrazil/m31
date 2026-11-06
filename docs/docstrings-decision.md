# Docstrings, and the comment rule

Shipped in 0.14.0. Two changes that belong together: a documentation comment
the front end records, and a lint rule that says a comment longer than one
line is a block comment.

## Syntax

    /// One line of documentation.
    pub int twice(int value) { ... }

    /**
     * Several lines of documentation.
     *
     * A markdown paragraph, a list, an example.
     */
    pub type Point { ... }

A **doc comment** is `///` (one line) or `/** ... */` (any number of lines),
alone on its line. It is not a doc comment when it is anything else:

| written | is |
| --- | --- |
| `// text`, `/* text */` | a plain comment |
| `//// text`, `/**/`, `/*** text */` | a plain comment (the extra character opts out) |
| `int x = 1; /// text` | a plain comment (a doc comment is alone on its line) |

## What it attaches to

The next declaration, and only a declaration: a `fn` or method, a `type`,
`enum` or `interface` (and `distinct`), a **variant** of an enum, a **field**
of a type, a **method signature** of an interface, a module `const`, a
`prim`, and a module-level variable (`int counter = 0;` at the top of a
file). `pub` and `static` belong to the declaration; the doc comment goes
above them.

"Next" is strict. Between the doc comment and its declaration there may be
nothing but a plain comment on the very next line; a **blank line detaches**
it. Several `///` lines (or a `///` line and a `/** */` block) directly under
each other are one docstring, joined by newlines.

A doc comment that attaches to nothing -- at the end of a file, before a
statement, before `import`, inside a function body, before a parameter, after
the last field -- is an error at the comment:

    doc comment is not attached to a declaration

It is an error rather than a silent plain comment because a docstring that
quietly documents nothing is the failure the feature exists to prevent: the
author thinks the API is documented, and it is not.

## What the compiler does with it

Nothing. The front end records the text on the AST node (`doc:
Option<String>` on `Func`, `TypeDecl`, `EnumVariant`, field `Param`,
`ConstDecl` and the module-level `Stmt::Decl`); the type checker, monomorphiser
and back end never look at it, so a program emits the same C with or without
its docstrings. `m31c -doc` reads them back.

The text is **normalised** once, in the lexer: `///` and one following space
are dropped; for a block, `/**` and `*/` are dropped and, when every line after
the first starts with `*`, that `*` and one following space (the gutter) are
too; blocks with no gutter lose their common indent; trailing blanks and blank
lines at either end go. Markdown inside is not interpreted.

## Why `///` and `/** */`

* The repository already wrote 5,000 lines of `///` above declarations in
  `lib/`, so the spelling is the existing habit, and Rust, C#, Swift and
  Doxygen use it. A second marker would be a second habit.
* `/** */` is the same thing for more than one line, and it is what the
  lint rule asks for when a comment is longer than a line (below). One
  line gets `///`, many lines get a block: one rule, not "pick one".
* Both are **already comments** to every compiler before 0.14.0. A `///` is
  read as `//` and a `/**` as the start of a `/* */` block, so a library that
  starts to use docstrings still builds with an older `m31c` (the apps pin
  `M31_REF v0.9.0` in CI and keep building until they bump). The new
  rejection -- a detached doc comment -- only exists in 0.14.0 and later.

## Why block comments do not nest

They never did (reference section 1.2) and docstrings do not change that. A
nesting comment needs the lexer to count, and the only thing it buys is
commenting out code that already holds a block comment; `//` per line does
that. It also means `*/` ends a block always, which is what makes the
autofix below safe to reason about: a run whose text holds `*/` is the one
case it refuses.

## What the formatter does

Docstrings are comments to the formatter, so it already placed them
"directly above the item" (a comment block touching a declaration travels
with it). It keeps their text byte for byte, and a block comment's
continuation lines now keep their layout relative to the opening `/*`
instead of being flattened, so a ` * ` gutter survives at any depth. On a
file with no block comment spanning lines the output is the same as before.

## The comment rule

> a comment longer than one line must be a block comment

`m31c -lint` reports two or more **consecutive own-line `//` comment lines**,
at one indent with no blank line between, and the same for a run of `///`:

    f.m31:4:1: a comment longer than one line must be a block comment: 2
    consecutive `//` lines (try `/* ... */`)

The suggestion is `/** ... */` instead when the run sits directly above a
declaration, since a block there documents it, and always for a `///` run.
Not flagged: a single `//` or `///` line; a trailing comment after code
(`x = 1;  // why`), however many stack up; a **divider** (a line whose text
starts with three of `-`, `=`, `*`, `#`, `~`, `_`, `+`: `// ---- parsing ----`),
which is layout, not prose, and which neither joins a run nor gets absorbed
into one; and a run broken by a blank line, a change of indent, or a change
between `//` and `///`.

`m31c -lint --fix` rewrites each run in place, **line for line**:

    // a                /* a
    // b          -->    * b */

The text keeps its column, the line count does not change (so no diagnostic
or golden below it shifts), and a second run changes nothing. A `///` run
becomes `/** ... */` and keeps its normalised text. A `//` run becomes a plain
`/* ... */` -- the fixer never promotes a plain comment to a docstring on
its own, since that changes what `-doc` prints; the message suggests
`/** */` and the author decides. A run whose text contains `*/` is
reported and left (it cannot live in a block comment). `--fix` is the one
autofix `-lint` has; it only touches this rule.

## `m31c -doc`

`m31c -doc <file|dir>...` prints markdown: one `# Module` section per file and
under it each **public** constant, type (with its variants, public fields and
interface methods as a list), method and function, in source order, with the
signature in a code block and the docstring as written. Not covered: private
items, module-level variables (they cannot be `pub`), module-level docs (there
is no way to attach a docstring to a file; a doc comment before `import` is
refused), cross-links, other output formats, and checking that the docs are
true.
