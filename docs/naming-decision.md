# Naming — the decision

Written before any rename, like `docs/modules-decision.md`. The prompt was
"every variable needs proper names". This records what *proper* means well
enough to be checked by a program, what the code looks like today, what the
rule costs, and the order to pay it in. Nothing in `lib/`, `apps/` or `corpus/`
has been touched; `m31c lint` (§6) only reports.

Numbers below come from `m31c lint`, which walks the parsed AST, and from
one regex survey (§2) taken before the lint existed. Where they disagree the
lint is right: the regex survey cannot see lambda parameters, fields or
function names. Everything quoted from other languages' guidance is from
memory and was **not** re-checked this session.

---

## 1. The question

`reference.md` §1.3 constrains an identifier to "a letter or `_`, then
letters, digits or `_`" and says nothing else. There is no naming guidance
anywhere in `docs/`; `readability-plan.md` even warns that renames invalidate
decision records that quote old names, and defers the whole subject until the
in-flight apps land. So today the only rule is habit, and habit produced `e`
as the name of a thing in 178 places in `apps/` alone.

Two questions, then: **what is the rule**, and **can a tool hold the line**.
A convention nothing enforces decays to the survey in §2 within a year.

---

## 2. What the code does today

### Casing is already a convention — and a clean one

| | snake_case values and functions | PascalCase types and variants | SCREAMING consts |
|---|---|---|---|
| `lib/`, `apps/`, the four extracted repos | **0 violations** | **0 violations** | **0 violations** |

Every violation of casing in the tree is in `corpus/` (24 outside file names),
and each is a test: `017-lowercase-types.m31` declares `type point` on
purpose, `None_` and `Ok_` are variants dodging the prelude. So §3.1 below
writes down what everyone already does, and the lint can enforce it with no
migration at all.

### Length is the problem

Identifier lengths, from the regex survey (declaration sites only: locals,
parameters, `case` payloads, loop variables):

| Repo | Files | Bindings | ≤ 2 chars | of those, 1 char |
|---|---|---|---|---|
| `lib/` | 29 | 3096 | 46% | 1085 |
| `apps/` | 58 | 3468 | 46% | 1164 |
| `examples/` | 3 | 101 | 63% | 58 |
| `corpus/` | 731 | 3917 | 68% | 2300 |
| gitui | 29 | 1840 | 42% | 523 |
| tui | 18 | 1103 | 50% | 424 |
| term-markdown | 5 | 422 | 54% | 177 |
| httpserver | 1 | 17 | 53% | 7 |

Almost half of all bindings in production code are one or two letters. Split
by what they bind, the share that is short:

| Binding kind | `lib/` | `apps/` | gitui |
|---|---|---|---|
| local | 40% | 36% | 29% |
| parameter | 47% | 42% | 31% |
| `case` payload | **75%** | **86%** | **83%** |
| loop variable | **85%** | **81%** | 78% |

The pattern is the finding. Locals and parameters are short about as often as
not; a `case` payload or a loop variable is short **four times in five**.
These are exactly the bindings the type-first syntax makes cheapest to name
badly: `case Err(io.Error e)` already says *what* it is on the left, so the
writer feels the name has nothing left to add.

Most common short names and what they bind (from the lint, default mode):

- **`e`** — almost always a `case Err(T e)` payload. 59 in `lib/`, 178 in
  `apps/`, 138 in gitui. In `gitui/index.m31:184` it is not even used.
- **`n`, `b`, `c`, `s`, `a`** — an int, a `bytes`, a char or byte, a `str`,
  the first of two symmetric operands. 100+ each in `lib/` for the first four.
- **`i`** — a counting loop (64 in `lib/`, 94 in `apps/`); the one short name
  that is a real convention.
- **`at`, `fd`, `ev`, `gd`, `cm`, `pc`** — two-letter and made up.
- **`buf`, `idx`, `len`, `dir`, `src`, `prev`, `cur`, `pos`, `msg`, `tmp`** —
  long enough to pass a length rule and still abbreviations. 63 `buf` across
  `lib/`, `apps/`, gitui and tui (19 in `lib/`).

Numbered clones (`t0`, `r2`, `h1`, `h2`) are 101 findings in `lib/` — mostly
the crypto modules transcribing RFC variable names — and 37 in `apps/`.

### The language already pushes toward long names

Shadowing is an error (`reference.md` §4.1): *a name means one thing for the
whole region a reader can see it in.* That removes Go's escape hatch. Go can
afford `e` in every `if err != nil` block because the next block's `e` is a
different, shadowing `e`; here two `match`es in one function cannot both bind
`e`. The code's answer was `e2`, `r2`, `h2` — numbered short names, which is
the worst of both.

Functions are small (median 9 lines, p90 36, max 128, over `lib/` and
`apps/git/`), so Go's argument is not empty here; §4 takes it up.

---

## 3. What is decided

**A name says what a thing *is* in the program — its role — not what type it
has and not where it sits. Spelled out, in snake_case, three letters or more.**

That is the whole rule. The rest is what it means for each kind of name.

### 3.1 By kind

| Kind | Case | Length | Notes |
|---|---|---|---|
| local, parameter, field | `snake_case` | ≥ 3 | the role: `line_count`, not `n` |
| `case` payload | `snake_case` | ≥ 3 | §3.4 |
| loop variable | `snake_case` | ≥ 3 | counting loop may use `i`/`j`, §3.2 |
| lambda parameter | `snake_case` | ≥ 3 | same as a parameter |
| function, method | `snake_case` | ≥ 3 | a verb or a noun, whichever reads at the call site |
| type, interface | `PascalCase` | any | the lint accepts any ASCII letters and digits after the first capital |
| enum variant | `PascalCase` | any | `Ok`, `Err`, `Io` stay as they are |
| type parameter | `PascalCase` | any | `T` is fine; `Key` and `Value` are better when there are two |
| constant (`const` at top level) | `SCREAMING_SNAKE_CASE` | ≥ 3 | `ROUND_CONSTANTS`, not `K` |
| module | `snake_case` | any | file basename; already required to be an identifier (`modules-decision.md` §1) |

Names starting `__` are the standard library's seam to C
(`docs/stdlib-seam.md`) and are outside this document.

### 3.2 The allowlist

Strict by default; four exceptions, each of which earns its place:

| Exception | Where | Why |
|---|---|---|
| `i`, `j` | the counter of `for (T i in a .. b)` only | the one universal short name, and the counter *belongs to the loop* (`reference.md` §5.5) so its scope is exactly one block. `for (T i in xs)` is **not** exempt: that binds an element, which has a role |
| `id` | anywhere | a word in its own right: nobody writes "identifier" out |
| `x`, `y` | any `int`/`float` binding | coordinates have no better name, and `column` for `x` is wrong the moment the axis is not a text column |
| `at`, `by`, `eq`, `of`, `ok`, `up` | function names only | two-letter English words that read as words at the call site: `grid.at(row, col)`, `Point.of(1, 2)`, `a.eq(b)` |

Not on the list, deliberately: `w`/`h` (`width`/`height` cost four letters
each), `k`/`v` (`key`/`value`), `n` (`count`, `size`, `length` — the
container method is `size()`), `s` (`text`, `line`, `name`), `e`.

The lint's `--allow` takes a comma-separated list for anything beyond this,
and `--strict` drops the four rows above. Anyone adding a name to a project's
`--allow` is doing what Go calls an exception; it should show up in the
commit that adds it.

### 3.3 Abbreviations

**Spell it out.** Length ≥ 3 is the mechanical floor, but `idx`, `buf` and
`tmp` clear it and say less than their word, so the lint carries a closed
table of the common ones and suggests the expansion:

`idx→index  cnt→count  tmp→temp  res/ret→result  val→value  msg→message
cfg→config  ctx→context  num→number  pos→position  arr→array  cur→current
prev→previous  src→source  dst→destination  buf→buffer  ptr→pointer
elem→element  args→arguments  param→parameter  tok→token  err→error
dir→directory  len→length  conn→connection  req→request
resp→response`

The table is a list of *observed* abbreviations, not a dictionary — no tool
can decide that `fd` is cryptic and `url` is not, and this document will not
pretend one can. It grows by pull request when a new one turns up in review.

`len` is the questionable entry and is listed anyway. The language has a
method called `size()`, so `len` does not echo the stdlib; and "length" is not
longer in any way that matters. `id` is the exception because "identifier"
*is* longer in a way that matters: nobody writes it.

Established initialisms that are the full name of the thing — `url`, `http`,
`csv`, `utf8`, `sha256` — are fine as words and the table does not list them.

### 3.4 `case` payloads

The biggest single source of short names (365 of the 1561 findings in
`apps/`, 259 of 783 in gitui). Three rules:

1. **Name the role, never the variant.** `Err(io.Error error)`, not `e`.
   `Ok(bytes data)`, not `b`, not `ok`, not `value`.
2. **`Err` payloads are `error`** unless the function handles two kinds of
   error, in which case they are named for the kind: `parse_error`,
   `io_error`. The lint suggests `error` for any short `Err` payload, which
   is the one case where the right name is mechanical.
3. **A named type suggests its own name.** `Ok(Config c)` → `config`;
   `Some(tuibuf.Cell c)` → `cell`. The lint offers the snake-cased type name
   whenever the payload is a user type, and does not for `int`, `str` or a
   container, where the type says nothing about the role.

An unused payload is still named. That is a language gap, not a naming
question: there is no discard (`case Err(_)`), so a handler that ignores the
error must still bind it (`gitui/index.m31:184`). Open question 2.

### 3.5 Loop variables

`for (int i in 0 .. n)` — `i`, and `j` inside it, are allowed. Everything else
is named for what it is: `for (str path in paths)`, `for (int row in 0 .. height)`.
A counter that *means* something should say it: `row` and `column` beat
`i` and `j` in a grid, and `for (int limb in 0 .. 16)` beats `k` in the
crypto code, but the exception stays because forcing `index` onto a
subscript loop produces `a[index]` and a reader who has stopped reading it.

### 3.6 Lambda parameters

Same rule as any parameter. The types are already written
(`closures-decision.md`), so a short name saves nothing: `(Point left, Point right) => left.x - right.x`.
`left`/`right` is the name for symmetric operands of a comparison or a
binary function (`math.min`, `sort.by`); `first`/`second` where order
matters.

### 3.7 Shadowing

**No new rule.** The language already refuses it (§4.1), so naming has the
guarantee Go's convention has to hope for: a name in scope is unique. The
lint therefore need not check shadowing and does not. The one consequence is
for authors: because a nested block cannot reuse a name, a name that is
precise enough for the whole function is better than one that is only right
in the block it was written for. That argues *for* §3.1, and is the reason
`e2` is not a fix.

---

## 4. Alternatives considered

**Go's: short names in short scopes.** The Code Review Comments guidance is
that "the further from its declaration that a name is used, the more
descriptive the name must be", with `i` for an index and `c` for a count as
examples; Dave Cheney's *Practical Go* is the fuller argument. It is a
reasonable rule and half of it is kept here (`i`/`j`, `id`). Rejected as the
rule, for four reasons:

1. **It is not checkable.** "Short scope" is a judgment about distance
   between a declaration and its last use, and a lint for it is a flow
   analysis with a threshold that someone has to defend. A rule a tool
   cannot hold is a rule the survey in §2 shows drifting.
2. **It assumes shadowing.** Go reuses `err` and `e` safely because the inner
   one shadows. Here that is a compile error.
3. **The type is on the left, not the right.** In Go, `c` is read next to
   `c := x.Count()`, and the call explains it. In m31 the declaration is
   `int c = ...` — the type is the *only* explanation at the declaration, and
   it says "int", not "count of what".
4. **The user asked for strict.** "Every variable needs proper names." The
   migration cost is not a reason to soften (the project prefers the enforced rule over cheap migration).

**Rust and Python: descriptive, by convention.** Both say "descriptive" and
neither enforces it — rustc has no length lint; Clippy's `min_ident_chars` is
in its off-by-default *restriction* group, and `many_single_char_names` is a
pedantic one. That is the evidence that the rule is easy to write down and
hard to hold, and why this document ships a lint with it.

**A length rule alone.** Cheapest, and §3.3's table exists because it is not
enough: `buf` and `idx` pass it. Taken, but with the table.

**A larger floor (4 or 5).** Rejected. `row`, `key`, `path`, `size` are
names nobody wants to lengthen, and a floor of 4 turns `key` into `key_`.
Three is the shortest length at which a *word* fits.

**An empty allowlist.** Considered seriously; it is `--strict`, which the lint
supports and gates may choose. Rejected as the default because `for (int i in
0 .. n)` with `index` would be rule-following that makes code worse, and a
rule that makes code worse gets disabled.

**Per-module overrides** (let the crypto code keep RFC letters). Not taken;
open question 1.

**Casing options.** camelCase for values and functions is the Java/Go-private
shape; the code is **0%** camelCase, so choosing it would be a migration with
no user. The choice was made by the tree.

---

## 5. Before and after

Real code. The "after" is what the rule asks for; it was written by hand and
has not been compiled.

**`gitui/gitui.m31:64-79`** — payloads and lambda parameters:

```c
// before
match (loop.run((tuibuf.Buffer buf, tuigeom.Rect area) => state.draw(buf, area), (term.Event ev) => state.handle(ev))) {
    case Ok(int n): { ... }
    case Err(term.Error e): {
        io.eprint("gitui: " + e.to_str());

// after
match (loop.run((tuibuf.Buffer buffer, tuigeom.Rect area) => state.draw(buffer, area), (term.Event event) => state.handle(event))) {
    case Ok(int exit_code): { ... }
    case Err(term.Error error): {
        io.eprint("gitui: " + error.to_str());
```

`n` here is an `int` with no mechanical suggestion; the author has to say what
it is (it is unused — Open question 2).

**`gitui/index.m31:176-190`** — a payload that is read and one that is not:

```c
// before                              // after
case Ok(bytes b): {                    case Ok(bytes contents): {
    data = b;                              data = contents;
}                                      }
case Err(io.Error e): {                case Err(io.Error error): {
```

**`lib/field25519.m31:117-121`** — parameter and loop variable:

```c
// before
pub Array<int> from_bytes(bytes b) {
    for (int k in 0 .. 16) {
        out[k] = b[2 * k] | b[2 * k + 1] << 8;

// after
pub Array<int> from_bytes(bytes encoded) {
    for (int limb in 0 .. 16) {
        out[limb] = encoded[2 * limb] | encoded[2 * limb + 1] << 8;
```

The `k` → `limb` rename is the case for dropping the exception of §3.5 and
the case against it: the new line reads better and is eight characters longer.

**`lib/math.m31:509`, `lib/sort.m31:20`** — symmetric operands:

```c
pub int min(int a, int b)                       // → min(int left, int right)
sort.by(ps, (Point a, Point b) => a.x - b.x)    // → (Point left, Point right) => left.x - right.x
```

**`lib/sha256.m31:181`, `lib/http.m31:230`** — constants:

```c
const Array<int> K = [...]     // → ROUND_CONSTANTS
const int LF = 10;             // → LINE_FEED   (CR → CARRIAGE_RETURN, SP → SPACE)
```

---

## 6. Enforcement: `m31c lint`

```
m31c lint [--allow NAME[,NAME..]] [--strict] [--summary] <file|dir>...
```

Parse-only (the same path as `m31c fmt`), so it runs on code that does not
typecheck and on a repo whose imports are not on disk — which is how the four
extracted repos were measured. Directories are walked for `*.m31`, sorted.

Report mode only. One line per finding on **stdout**:

```
gitui/gitui.m31:76:29: match binding `e` is too short; a name needs 3 or more characters (try `error`)
gitui/gitui.m31:67:36: lambda parameter `buf` is an abbreviation (try `buffer`)
```

The column is the name's, not the declaration's. Exit **0** clean, **1** any
finding or any file that does not parse, **2** bad usage. A summary line goes
to stderr; `--summary` adds per-kind counts and the twelve most common names.

It checks: casing for every kind, the three-character floor for values and
for function and constant names, the abbreviation table, and the exceptions of
§3.2. It does not check that a name is *good* — `data` and `thing` pass —
and cannot. One finding per name at most: three findings on `e` is noise.

Why in the compiler and not a script: the hard part is knowing a name's *kind*
(loop counter or element, payload or local, `Err` or `Ok`), and the parser
already has that. A regex script (the §2 survey) cannot see lambda parameters
or fields and misreads `for (x in xs)`. The cost is `src/lint.rs`, about 690
lines of tree walk and CLI, plus 90 of unit tests, and no new dependency.

**Not wired into `gates.sh`** — by instruction, and rightly: it reports
~5,900 findings in the monorepo today (3,100 outside `corpus/`). The wiring is the last step of §7, not the first.

### Violation counts

`m31c lint` default mode, files that parse. Module-name findings are
excluded from `corpus/` (506: the test programs are named `001-hello.m31` and
are never imported; the lint would flag the dash). 44 files in `corpus/` do not
parse — they are the negative tests.

| Repo | Findings (default) | `--strict` | Local | Param | Field | Payload | Loop | Lambda | Fn / const |
|---|---|---|---|---|---|---|---|---|---|
| `lib/` | 1453 | 1544 | 677 | 550 | 44 | 123 | 50 | 0 | 9 |
| `apps/` | 1561 | 1744 | 691 | 356 | 23 | 365 | 115 | 5 | 6 |
| `examples/` | 62 | 66 | 14 | 13 | 2 | 27 | 5 | 0 | 1 |
| `corpus/` | 2801 | 3080 | — | — | — | — | — | — | — |
| **monorepo** | **3076** (5877 with corpus) | | | | | | | | |
| gitui | 783 | 878 | 293 | 129 | 10 | 259 | 88 | 2 | 2 |
| tui | 546 | 636 | 275 | 154 | 6 | 59 | 46 | 3 | 3 |
| term-markdown | 234 | 239 | 127 | 77 | 7 | 15 | 7 | 0 | 1 |
| httpserver | 9 | 9 | 0 | 2 | 0 | 7 | 0 | 0 | 0 |
| **extracted** | **1572** | | | | | | | | |

Casing findings in everything but `corpus/`: **0**, except 1 in `examples/`.
The four exceptions of §3.2 are worth 91 findings in `lib/`, 183 in `apps/`, 95
in gitui and 90 in tui — between 6% (`lib/`) and 10% (`apps/`) of each
total; the rule is not close to being decided by the allowlist.

---

## 7. Migration

The rule is strict and the tree is not, so this is a long mechanical rename.
Rough size is findings plus *lines touched* — code lines (comments and string
literals stripped) containing a flagged name as a word, an upper bound since
the same letter in two functions counts once per line:

| Repo | Findings | Code lines touched | of code lines |
|---|---|---|---|
| `lib/` | 1453 | ~4,100 | 32% |
| `apps/` | 1561 | ~4,700 | 35% |
| gitui | 783 | ~2,200 | 29% |
| tui | 546 | ~1,800 | 42% |
| term-markdown | 234 | ~770 | 49% |
| httpserver | 9 | ~18 | 25% |
| `examples/` | 62 | ~150 | 32% |
| `corpus/` | 2801 | ~6,000 | 35% |

### Order

1. **Wait.** Do not start while the in-flight gitui/tui branches are open:
   a rename over every other line is a merge conflict with each of them.
   Land the feature branches first, then migrate from one clean master.
2. **`httpserver`** (1 file, 9 findings). The pilot: proves the process in one
   commit and finds what the lint gets wrong.
3. **`term-markdown`** (5 files, 234), then **`tui`** (18, 546), then
   **`gitui`** (31, 783). Smallest-to-largest, and gitui last because it
   imports the other two: each repo's public names (`pub` functions, fields)
   change only in its own migration, never two at once.
4. **`lib/`** (29 files, 1453). After the apps, because renaming a `pub`
   parameter is free (arguments are positional; named arguments exist only
   for parameters with defaults) while renaming a `pub` *field* or a named
   default is a call-site change in every app — do those in the same commit
   as their callers. Crypto modules (`field25519`, `ed25519`, `sha256`, …)
   last within `lib/`, and only after Open question 1.
5. **`apps/`** (58 files, 1561) in the same sweep as `lib/` where they share
   fields.
6. **`corpus/`** last, by hand where a test pins the *name* (an error
   message quoting `e`) and by tool where it does not. Its `.err` and `.out`
   files compare byte-for-byte, so a renamed parameter that appears in a
   diagnostic changes the expected file in the same commit.
7. **Wire `m31c lint` into `gates.sh`**, with an empty `--allow`, the day the
   last repo is clean. Wiring it earlier makes every gate run fail.

### How, safely

- **One file per commit, formatter-clean**: `m31c fmt --check` on the file
  before the commit. A rename can change line width, and `fmt` reflows.
- **Tests green per commit.** The corpus is the oracle; a pure rename must
  change no `.out`, and any that changes is a name that leaked into
  behaviour (a diagnostic or a printed field name) and gets reviewed.
- **Rename by the lint's output, not by sed.** `e` is a payload in one function
  and the `e` in `"e"` or in a comment in the next. The `file:line:col` of
  each finding is the declaration; the rename is that name to the end of its
  region, which is the whole function (shadowing is banned, §3.7).
- **Never rename a name a decision record quotes** without updating the record
  in the same commit (`readability-plan.md`'s caution).
- **Do not rename for suggestion's sake.** The suggestions are a starting
  point; `Ok(int n)` has none because only the author knows what the int is.

---

## 8. Decisions on the open questions (locked)

0. **Full names.** The user's standing preference is a spelled-out word over a
   clipped one: `connection`, not `conn`; `request`, not `req`. The table in
   §3.3 is how the lint holds the line, and grows whenever review finds another.


1. **RFC-transcribed names are renamed.** `A`, `K`, `x1`, `t0` become role
   names (`ROUND_CONSTANTS`, `limb`, ...), with the RFC's name in a trailing
   comment so the code still greps against the standard. There is no
   per-file escape hatch: every file would use it.
2. **A discard binding is added to the language.** `case Err(_)` binds
   nothing and names nothing. It is a compiler change, tracked separately;
   until it lands an unused payload is still named (`error`).
3. **`len` is an abbreviation.** It becomes `length`.
4. **`at`, `by`, `eq`, `of`, `ok`, `up` stay**, for function names only.
   As *binding* names they remain flagged.
5. **Casing is snake_case, not camelCase.** Values, functions and modules
   are `snake_case`; types and variants are `PascalCase`. The case of a name
   tells a reader what it is: `Ip.eq(other)` is a type's method and
   `field25519.eq(a, b)` is a module's function, and only the case separates
   them. camelCase would erase that difference and make the long role names
   this rule asks for harder to read.
6. **Module names.** Five shapes, all distinguishable at a glance:

   | Shape | Example | Kind |
   |---|---|---|
   | lowercase, one word | `io`, `net`, `sha256` | stdlib module (`lib/`) |
   | `PREFIX_name` | `TUI_text`, `GIT_refs` | any other module |
   | `PascalCase` | `TuiText` | type or variant |
   | `SCREAMING_SNAKE` | `ROUND_CONSTANTS` | constant |
   | `snake_case` | `form_button` | value or function |

   A non-stdlib module name is an upper-case prefix (the project, or an
   established acronym of it), an underscore, and a lowercase descriptive
   part: `TUI_text`, never a bare `button` or `text`. The prefix cannot be
   confused with a constant (those have no lowercase part) or a type (no
   underscore). The stdlib stays lowercase because it is the language's own
   namespace. The compiler already accepts upper-case module names and
   already refuses a value that shares a module's name. Renaming a module
   renames its file and every import of it, so it is a breaking change for
   consumers and is done once per repo, in the migration of that repo.
