# Writing the first real application in this language

A log of what happened building `apps/markdown` — about 1 900 lines of
markdown parser across five modules, written against `docs/reference.md` by
somebody who had not written a line of the language before, and forbidden to
change the compiler or `lib/` to make it easier. The program is the excuse;
this file is the point.

> **Four of these have since been fixed**, and this file has not been
> rewritten to hide that it asked for them — the source has been changed to
> use them, so the diff against this log is the evidence. A character literal
> (1.3, and this report's number one ask), a range `for` (1.2), a `match` arm
> that may omit its bindings (1.5, 1.12), and a formatter that keeps the
> parentheses the author wrote (1.11). The two diagnostics 3 asks for —
> "there is no three-clause `for`" and "there is no `+=`" — exist too.
> Everything else below still stands. `docs/reference.md` §1.5, §5.5, §5.6
> and §6.1 have the rules and the reasoning, including why `+=` itself was
> refused.

The headline, first, because it frames everything below:

> **The whole program compiled on the second attempt, with one error.** No
> type errors, no arity errors, no borrow or ownership errors, nothing about
> refcounting. The one error was a shadowing complaint about a local called
> `close`. Then the first run produced correct HTML for a mixed document with
> headings, nested lists, a quote, a fence and a table.
>
> It also never leaked and never trapped. `-DRC_DEBUG` reports
> `__rc_live=0` for every one of the twenty-three test inputs, including a
> recursive tree over a megabyte of source, and I never once thought about
> memory. ASan and UBSan are clean. Four builds (gcc/clang × `-O0`/`-O2`)
> agree byte for byte.

That is a very high floor. Everything below is what happened above it.

---

## 1. The language got in the way

### 1.1 `close` is unavailable as a name in every program, forever

The only compile error the whole program produced:

```
apps/markdown/inlines.m31:448:5: `close` is already a function; shadowing is
not allowed, rename one
    |
448 |     int close = close_bracket(s, open + 1);
    |     ^
```

`close` is a channel builtin (§6.6). This program has no channels, imports no
`Chan`, and could not reach one if it tried — but §4.1 says nothing shadows a
builtin *in any module*, so `print`, `concat`, `clone`, `send`, `recv`,
`close` and `trap` are seven identifiers no program may ever use as a local,
on top of the keywords. Three of them — `close`, `send` and `print` — are the
obvious names for things in any program that parses or writes anything.

What I wanted: `int close = close_bracket(...)`. What I wrote: `int shut =
close_bracket(...)`, which is worse English.

The rule is defensible — `docs/consistency-over-migration-cost`-style, one
rule with no exceptions — but the *cost* falls entirely on the three channel
builtins, which are the only ones with short common-noun names. A reader of
`int close = ...` is in no danger of thinking it is the channel primitive.

The diagnostic is otherwise excellent (it names the rule and the fix); the
one thing it does not say is that `close` is a **builtin** rather than
something in my own file or an import. I grepped the reference to find out.

### 1.2 `i = i + 1;` — seventy-seven times

There is no `+=` and no `++` (§6.1: "There is no `&=`, `<<=` or any other
augmented assignment, because there is no `+=`"), and there is no
three-clause `for` (§5.5). So every index loop is:

```c
int i = 0;
while (i < n) {
    ...
    i = i + 1;
}
```

I counted 77 lines of the form `x = x + 1;` / `x = x - 1;` in this program.
That is 4% of the source doing nothing but advancing a cursor. In a parser —
which is nothing but cursors — it is the most-repeated line by a wide margin.

Worse than the noise is the *hazard*: because the increment is the last
statement of the body rather than part of the loop header, **every `continue`
has to re-do it by hand**. `scan()` in `inlines.m31` has eleven `continue`s and
each one is preceded by an explicit assignment to `i`; getting one wrong is an
infinite loop, not a compile error. A three-clause `for` makes that class of
bug impossible. I did not ship one, but only because I was watching for it.

### 1.3 There is no character literal, so `'*'` is `42`

39 comparisons in this program are of the form `s.byte_at(i) == 42`. The
reference's argument is that there is no `char` type because there is one
integer type (§3.2a), and that is right. But a *literal* is not a type: Go
has no `char` type either and writes `'*'` for the rune 42.

What I wanted:

```c
if (c == '*') { ... }
if (c == '\n') { ... }
```

What I wrote:

```c
if (c == 42) { ... }        // '*'
if (c == 10) { ... }        // '\n'
```

The alternatives are all worse. `"*".byte_at(0)` is a call in a hot loop and
reads no better. A module constant per character (`const int STAR = 42;`)
means a declaration per character *and* burns the name for every local in the
module (§4.1, §4.5), and `if (c == STAR)` still makes the reader look it up.
So the program is full of magic numbers with comments beside them, which is
exactly the thing every other convention in this language is designed to
prevent. **This is the single biggest readability cost in the program.**

A `'x'` literal that is simply an `int` — no new type, no new rules, one
lexer rule, refusing anything that is not a single ASCII character — would
remove it. It is additive.

### 1.4 `Option<T>` loses to a `bool ok` field when you only want to test it

`marker_of(r)` answers "is there a list marker at the start of this line, and
what is it". The honest type is `Option<Marker>`. I wrote this instead:

```c
type Marker { bool ok; bool ordered; int num; int ch; int len; }
...
return Marker(false, false, 0, 0, 0);       // "no marker"
```

which is the sentinel the language is proud of having abolished, reinvented
in a struct field. I did it because `Option` has no way to get at a payload
except `match`, and there are seven call sites, of which five only want the
`.ok`:

```c
// what I would have to write, at each of seven sites
match (marker_of(r2)) {
    case Some(Marker m): { ... }
    case None: { }
}
// what I write
if (marker_of(r2).ok) { ... }
```

`?` does not help: it only exists inside a function that itself returns the
same kind (§6.3), and these are `bool` and `int` functions. `is_some()` tells
you it is there but cannot hand it over. There is no `if let`, no `match`
expression, no `or_else`, no `map`. §3.7a says "There is no `unwrap`" and
gives a good reason; the consequence is that `Option` is excellent as a value
you immediately propagate and expensive as a value you immediately test.

The same thing happened again with `Target` in `inlines.m31` (`bool ok` plus
four fields) — a link destination clause that may or may not parse.

Two small things would fix most of it without reintroducing `unwrap`: a
`match` that is an *expression*, or an `o.or_default()`-shaped extractor for
types that have one. Neither is `unwrap`.

### 1.5 An enum cannot be compared, so the natural type became a boolean

The inline parser's node is either finished HTML or an unresolved delimiter
run. The natural spelling:

```c
enum Kind { Text; Raw; Delim; }
type Node { Kind kind; ... }
...
if (n.kind == Delim) { ... }        // error
```

`==` on a user enum needs `bool Kind.eq(Kind other)` (the diagnostic says so,
clearly — see §3 below). Writing `eq` for a payload-less three-variant enum
means a 3×3 nested `match`, fifteen lines, to answer "same variant". Or an
`int Kind.rank()` and compare the ints, seven lines, which is an enum
pretending to be an int.

I restructured instead: `bool delim` on the struct, and escape text at push
time so there is no third state. The result is arguably *better* code. But it
was not a design choice, it was a workaround, and the next person will make
the same one. A payload-less variant has nothing to disagree about; derived
structural equality for enums whose variants carry only comparable types
would cost nothing and is what every language with enums does.

Relatedly, `match` has no `default` and every arm needs a brace-block, so
asking "is it this variant" always costs the whole variant list. That rule is
exactly right where it matters — see §4 — and a tax on a three-line predicate.

### 1.6 You cannot name a mandatory argument

§4.2's rule is "mandatory parameters are positional, optional ones are
named", with no choice about it. That leaves no way to write

```c
Marker(ok: false, ordered: false, num: 0, ch: 0, len: 0)
```

and so the construction reads

```c
return Marker(false, false, 0, 0, 0);
```

five times in one function, and nobody can tell which zero is `num` and which
is `len`. Giving the fields defaults does not help: a defaulted field is
*optional*, and these are not optional, they are mandatory-and-unreadable.

The `Node` type has ten fields and the constructor is
`Node(false, h, 0, 0, 0, false, false, false, "", "")`. I hid it behind two
private helpers (`html_node`, `delim_node`) which is the right thing to do
anyway — but it was forced, not chosen.

The rule's stated purpose is that "a reader should never have to count commas
to find which parameter a value lands in". Here it guarantees exactly that.
Allowing a mandatory argument to be named *optionally* would break the
one-call-one-spelling rule; requiring it past some arity would not.

### 1.7 There is no string builder, and `+` in a loop is silently quadratic

`str` is immutable and `+` allocates. So every accumulator in this program is

```c
List<str> out = [];
out.push(part);
...
return out.join("");
```

which is correct and fast and three lines of ceremony, and which the reader
has to recognise as "this is a string builder". `bytes` is the mutable
sequence, but it is octets, so building text in one means `push`ing UTF-8 by
hand and calling `utf8()` at the end, which returns an `Option` — worse.

Nothing in the language or in `lib/text` warns you that `out = out + x;`
inside a loop is O(n²). For a program whose whole job is to produce a
megabyte of output that is the one performance decision that matters, and it
is invisible. `lib/text` documents at length the functions it deliberately
omits; a `text.Builder`, or a note saying "the accumulator is
`List<str>` + `join`", would be worth more than several of them.

### 1.8 No mutable or lazily-initialised module state, and `const` cannot call anything

`href_safe(c)` asks whether a byte may appear unencoded in a URL. The right
data structure is a 128-entry lookup table. I cannot build one:

- a module `const` is computed by the compiler from a *constant expression*
  with "no call, field, index, method or construction" (§4.5), so the table
  would have to be 128 literal `true`/`false`s written out by hand;
- there is no mutable module state to build it into once (§2), and no lazy
  initialisation;
- building it in `main` and threading it through would mean an extra
  parameter on `escape_href`, `link`, `autolink`, `scan` and `render` — five
  functions — to get one table to one leaf.

So I wrote this, and it is the only place in the program where I knowingly
left performance on the floor:

```c
const str HREF_SAFE = "!#$%&'()*+,-./:;=?@_~`";

bool href_safe(int c) {
    if (alnum_byte(c)) { return true; }
    return HREF_SAFE.index_of(str.from_chars([c])).is_some();
}
```

Per byte of every URL that is a `List<int>` allocation, a `str` allocation
and a substring search. A `const Map<int, bool>` would have been legal (§4.5
allows `int` keys) and 90 entries long. A `const Array<bool>` would have been
128 literals. Neither is a thing a person writes.

What is missing is small: either a constant expression that may call a
`const`-evaluable function, or a `contains_byte` on `str`.

### 1.9 `str.from_chars([c])` is the only way to turn one code point into text

Related to 1.8 and to 1.3. There is no `from_char`, and §6.5 explains why
("which is `from_chars([c])`"). It is a defensible rule that produces an
allocation of a one-element `List<int>` and then a `str`, in a loop, to do
what `s[i]` does elsewhere. In `escape_href` I got out of it with
`dest.substr(i, i + 1)`, which is a much better spelling and only works
because I already knew the byte was ASCII.

### 1.10 Writing exactly a string to standard output is six lines

`print(s)` appends a newline and `io.eprint(s)` appends a newline — both
line writers. A program whose whole output is one already-newline-terminated
string cannot use either, so "write exactly this to standard output" is:

```c
io.File output = io.stdout();
return output.write(out.to_bytes());     // a File, a bytes copy, a Result
```

inside a `Result`-returning function, with a `match` at the call site. That is
`main.m31`'s `emit()`, six lines for the last thing the program does, and the
`to_bytes()` is a full copy of the megabyte I just built.

`io.write(path, data)` takes a `str` and does the whole job for a file. The
same function for the two standard streams — or a `File.write_str` — would
close the gap, and would take the copy with it.

### 1.11 The formatter deletes grouping parentheses

`m31c fmt` rewrote

```c
return (c >= 65 && c <= 90) || (c >= 97 && c <= 122);
if (c == 91 || (c == 33 && i + 1 < n && s.byte_at(i + 1) == 91)) {
```

as

```c
return c >= 65 && c <= 90 || c >= 97 && c <= 122;
if (c == 91 || c == 33 && i + 1 < n && s.byte_at(i + 1) == 91) {
```

Both are correct by §6.1's precedence table and both are harder to read.
§6.1 takes trouble to fix C's `x & 1 == 0` trap *because* precedence that a
reader has to recall is a hazard; the formatter then removes the parentheses
a careful author wrote so the reader would not have to. Redundant parentheses
around a mixed `&&`/`||` are information, not noise.

(`apps/` is not covered by the `gates.sh` formatter checks, so I could have
declined — but the house style is formatted source, and a rule you opt out of
is not a rule.)

### 1.12 Smaller things

- **`match` arms need a brace block even for one expression**, so a
  three-variant dispatch is eleven lines. Fine in `render.m31`, heavy in a
  predicate.
- **`break` only leaves the innermost loop and there are no labels.** I used
  `bool done` / `bool ended` sentinels in five places where a labelled break
  would have been clearer. This is a known and stated omission (§9); it is
  still the thing I reached for.
- **`for` iterates `Array`, `List` and `bytes` only** — not a `Map`, and not
  a range. A counted loop over `0 .. n` is `while` plus 1.2.
- **`text.lines(s)` drops a trailing empty line**, which was exactly what I
  wanted, and I had to read the source to be sure. That is on the docs, and
  the docs are in the source, so it took ten seconds.
- **No `Set`.** `Map<K, bool>` is the stand-in everywhere.
- **No callbacks were wanted.** See the note under §2; §6.7's design went
  entirely unexercised, which is itself a data point about what a parser
  needs.

---

## 2. Standard library functions I reached for and did not find

| Wanted | Why | Workaround |
|---|---|---|
| `s.index_of(sub, from: i)` | every scanner in this program | a byte loop with `byte_at`, of which this program has two dozen. `lib/text.m31` already confesses this one at length, and is right that the library cannot fix it — it has to be a builtin |
| `s.contains_byte(c)` / `s.find_first_of(set)` | character-class tests | `HREF_SAFE.index_of(str.from_chars([c])).is_some()` (see 1.8) |
| a string builder | building a megabyte of HTML | `List<str>` + `join("")`, everywhere |
| `unicode.is_punctuation(cp)`, `unicode.is_whitespace(cp)` | **CommonMark's emphasis rules are literally defined in terms of Unicode punctuation and whitespace** | ASCII-only classification, documented as a divergence in README. `lib/unicode` is 2 500 lines of tables and exposes `upper`, `lower`, `fold`, `graphemes`, `grapheme_count`, `width`, `nfc`, `nfd` — every derived operation and no general category query. It is the one thing a text-processing program needs from those tables, and it is the one thing not exported |
| `io.write_str` on a `File`, or `io.out(s)` | a `str` to standard output without a `bytes` copy | `io.stdout().write(s.to_bytes())` (see 1.10) |
| `Option.map` / `and_then` / `or_else` | see 1.4 | `match` |
| `text.escape_url` or similar | percent-encoding | wrote `escape_href` (30 lines), which is fair — it is cmark's table, not a general one |

Things that were exactly right and that I want on the record: `text.replace`,
`text.lines`, `text.trim_start`, `text.split_whitespace`, `math.min`,
`str.repeat`, `str.split`, `str.substr`, `str.parse_int` returning `Option`,
`str.join` on a collection, and `html.escape`. Four of `lib/text`'s ten
functions were used (`replace`, `lines`, `trim_start`, `split_whitespace`),
which is a good hit rate for a module of ten — four of the six unused ones
are padding and affix work this program does not do, and the fifth,
`text.parse_int`, loses to the builtin `str.parse_int` here because I do not
need to tell the user *why* a list number did not parse.

`lib/sort` was not used at all, and nor was any callback or lambda. A parser
is hand-rolled loops from end to end. That is worth knowing about §6.7: the
callback design may be excellent and this program had no opinion on it.

`lib/text.m31`'s "what is deliberately NOT here" section is the best piece of
standard-library documentation I have read anywhere, and it is right about
every entry.

---

## 3. Diagnostics

Only one fired during development (see 1.1), which limits the sample. So I
went and made the mistakes deliberately.

**Good, and better than most compilers:**

```
`str` has no method `byte_at_or`; it has size, substr, contains, index_of,
starts_with, ends_with, split, trim, to_upper, to_lower, repeat, byte_at,
chars and to_bytes

`Box` takes 3 positional argument(s), found 2

`==` on `Kind` needs a method `bool Kind.eq(..)`

`match` must handle every variant of `Kind`; missing Delim

`close` is already a function; shadowing is not allowed, rename one
```

Each names the rule and the fix. The `match` one names the missing variant.
The `==` one names the method to write. This is the standard the rest should
be held to.

**Two that are not:**

```
apps/.../a.m31:29:16: expected `in`, found `=`
   |
29 |     for (int j = 0; j < n; j = j + 1) {
   |                ^

apps/.../b.m31:36:8: expected an expression, found `=`
   |
36 |     n += 1;
   |        ^
```

`for (int i = 0; ...)` and `n += 1` are the two mistakes *every* programmer
coming to this language will make in their first hour, because both are in
C, Java, Go, Rust and Python and both are deliberately absent here (§5.5,
§6.1, §9). The parser knows exactly what it is looking at in both cases and
says only that a token surprised it. "There is no three-clause `for`; write
`for (T x in xs)` or a `while`" and "there is no `+=`; write `n = n + 1`"
would cost two lines of parser each. §9 exists precisely so these absences
are decisions; the decisions should reach the person who trips over them.

**One small inaccuracy:** the `str` method list above omits `parse_int`,
`parse_float` and `to_str`, all of which §6.5 documents and all of which
work. A programmer who typed `s.parse_int()`, mistyped it, and read that list
would conclude it does not exist.

**Not checked, because it never happened:** anything about ownership,
refcounting, moves or lifetimes. That is the point of §7 and it held.

---

## 4. Run time

**Nothing surprised me.** That sentence is the finding.

- No trap, ever, across several thousand fuzzed documents and a megabyte of generated
  input. The program indexes strings by computed byte offsets on every line
  of the inline parser; in C that is where the CVEs live.
- `__rc_live=0` on every input, first try, with no thought given to memory. A
  recursive `List<Block>` / `List<Item>` tree built and dropped per document.
- ASan and UBSan clean.
- gcc and clang, `-O0` and `-O2`, produce byte-identical output.
- 1 MB of markdown to 1.7 MB of HTML in **118 ms** at `-O2`. Nothing is
  accidentally quadratic, and the one thing that could have been (string
  concatenation) I avoided by hand, not because anything told me to.

  For scale, and with a caveat: the oracle, Python `commonmark`, takes **8
  minutes 54 seconds** on the same file (3m22 user, 5m28 sys — it is
  thrashing, so it is super-linear on a document this size). That is a
  statement about `commonmark.py`, not about Python and not really about this
  language either. The honest version is: a straightforwardly written parser
  in this language, with no tuning beyond "do not concatenate strings in a
  loop", handles a megabyte in the time a C program would.

The one thing worth flagging is not a surprise but an absence: **there is
almost no way to find out that a program is slow from inside the language.**
`date.now_ns()` exists and is enough to time a block by hand, but its own
doc-comment says "The wall clock can be set backwards; this is a date, not a
stopwatch" — there is no monotonic reading, and `lib/date`'s header says the
two clock functions are only squatting there until a `time` module exists.
There is no allocation counter outside `RC_DEBUG` and no profiler. Every
timing in `test.sh` is `date +%s%N` in bash around the whole process.

This cost me something real. The program had a quadratic in it — `link()`
searched forward for a matching `]` at every `[`, so 50 000 unmatched
brackets (50 KB of input, an entirely ordinary thing to be handed) took four
and a half seconds, and a megabyte of them would have taken half an hour. It
is a denial of service in anything that converts untrusted markdown. Nothing
found it: not the corpus, not the fuzzer, not the 1 MB timing, which uses
well-formed input. I found it by sitting down and writing seven adversarial
inputs on purpose, which is now a step in `test.sh`. The fix (`inlines.brackets`,
one stack pass instead of a search per bracket) took it to 7 ms.

The language cannot be blamed for a bad algorithm. But "the program is
correct and leak-free and memory-safe and orders of magnitude faster than
the reference" is exactly the state in which nobody goes looking, and there is
nothing in the toolchain that would have pointed at it. A monotonic clock —
the `time` module `lib/date`'s header is waiting for — would at least let a
program's own test suite say "this function took longer than it should".

---

## 5. Where it was better than the alternatives

These are not consolation prizes; each of them saved real work.

**Exhaustive `match` caught the change it exists to catch.** I added the
`Grid` variant to `doc.Block` for tables after `render.m31` was written. The
compiler pointed at the one `match` that had to learn about it. In Go or
Python that is a silent fall-through to the default case and a missing
`<table>` in production.

**No null.** `index_of` and `parse_int` return `Option`. I wrote no null
check in 1 900 lines and got none wrong. §1.4 above is a complaint about
`Option`'s *ergonomics*, not about the decision.

**Refcounting with no annotations at all.** A tree of mutually recursive
reference types, built by five functions, dropped at the end of a document,
zero live objects — and I wrote no `clone`, no `&`, no lifetime, no
`Rc::new`. The comparison is Rust, where this program's `List<Block>` inside
`Quote` inside `Block` would have been an afternoon of thinking about `Box`
and ownership. It is C, where it would have been an afternoon of
`free`. Here it was not a topic.

**`str` is guaranteed UTF-8, and offsets are bytes.** For a markdown parser
this is exactly the right trade and it is not close. Every offset I compute
comes from matching an ASCII byte, so it is always on a character boundary —
which is the argument §6.5 makes — and `substr` traps if I ever get that
wrong. The Unicode test case (Cyrillic, CJK, Arabic, a skin-tone emoji, a ZWJ
family sequence, a flag) passed the first time, and I wrote no encoding code.

**Modules are files, and that is all there is to it.** Five modules, `import`
by basename, forward references everywhere, no headers, no ordering, no build
file, no `mod.rs`. `doc.m31` exists only so `blocks` and `render` need not
import each other, and making it took thirty seconds.

**Privacy by default.** `doc.m31` exports six types, four constants and an
enum; everything
in `blocks.m31` except `parse` is private; I never wrote `pub` by accident
and never had to audit what I had exported.

**Trapping arithmetic and bounds checks, by default, with no opt-out.** This
program is a pile of `i + 1`, `n - 1` and `s.byte_at(i)`. The floor on a bug
here is a crash with a message. That floor is why the fuzz run is meaningful:
several thousand random documents with zero crashes means zero crashes, not
"no crash we noticed".

**`lib/args`.** Five lines gave flags, a value with a placeholder, a
positional with a default, `--help` with generated help text, and a usage
error object with a `to_str`. I read its header comment once and wrote the
command line without a second look.

The one place it surprised me is its grammar, and it is a deliberate one: a
single dash is a *command* word, so `--o` is this program's short option and
`-o page.html` is "markdown takes no commands, and got '-o'". The module
argues the case well (a repository may be called `serve`) and every program
in this repository will be consistent — but `-o` is forty years of unix habit
and the error message, while correct, does not say "did you mean `--o`?". I
had written `-o` in my own README without noticing.

**Structs are references, so `xs[i].field = v` changes the element.** The
emphasis pass is a list of nodes whose counts and flags are mutated in place
by index while the list is walked, which is exactly what cmark's algorithm
needs. In a language with value semantics for structs this is where the
`&mut` or the index-juggling goes. Here it is §3.2 working as written, and it
is the reason that pass reads like the reference implementation instead of
like a workaround.

**A collection literal takes its type from where it is written.** `return
[];` for a `List<int>`, `[-1; n]` for a filled one, `["a", "b"]` for a
parameter. One rule, one level of inference, and it removed every
`List<int>::new()`-shaped line that other languages need.

**`lib/html.escape`.** One function, no options, correct. The module comment
argues for exactly one behaviour and then has exactly one behaviour. Its only
cost is 5.1 below.

### 5.1 The one place `lib/html` cost me something

`html.escape` escapes all five of `& < > " '`; cmark escapes four and leaves
the apostrophe. So this program's output differs from the reference
implementation's by `&#x27;` wherever a text node contains an apostrophe.

Both are correct HTML. The alternative was to write a second escaper in the
app and not use the standard library's, which is worse. So the test harness
brings the *oracle* to this program's spelling with one documented
substitution (`reference.py`, and the `'` → `&#x27;` line is the only one).

I am recording it not as a complaint — `lib/html.m31` argues its case well and
"no `quote` flag" is the right call — but because it is a concrete instance
of a general thing: **a standard library that is opinionated in the right way
still costs you exactness against somebody else's opinionated implementation,
and a test harness has to have somewhere to put that.**

---

## 6. If I could change three things

1. **A character literal.** `'*'` meaning the `int` 42. One lexer rule, no
   new type, removes 39 magic numbers from this program alone. (1.3)
2. **`+=` and a three-clause `for`, or at least a diagnostic that says they
   are absent.** 77 lines and a live class of infinite-loop bug. (1.2, §3)
3. **Somewhere to put a computed table** — a `const` initialiser that may
   call a pure function, or lazily-initialised module state. (1.8)

And one I would not change: nothing about memory, ownership or lifetimes. I
wrote a recursive-tree program in a refcounted language with destructors and
never thought about either. That is the whole pitch, and it is true.
