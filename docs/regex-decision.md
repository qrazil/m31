# Regex: the decision

Status: **scoping, then implemented in this change.** This is `lib/text/regex.m31`
-- a from-scratch regular expression engine, in the language, with no
runtime support beyond what `text.m31` and `unicode.m31` already lean on
(`s.chars()`, `str.from_chars`, `List`, `Map`-free). It exists because the
stdlib survey flagged a `regexp`-lite module as a gap present in nearly
every mainstream standard library (Go's `regexp`, Python's `re`, Rust's
`regex` crate), and this project has already shown, with the crypto suite in
`lib/` (`ed25519`, `x25519`, `chacha20poly1305`, `sha256`, `sha512`), that it
writes serious from-scratch algorithmic code in-language rather than reaching
for a primitive.

**This is a library, not a language feature.** Nothing about the compiler,
the IR, or the runtime changes. The one non-`lib/` edit is the one-line
registration every embedded module needs in `src/stdlib.rs`, the same entry
`math` and `sort` have.

---

## 1. Scope: what this needs to do, and no more

The goal is a small, well-understood, genuinely linear-time engine -- Go's
`regexp` package's shape, not PCRE's. Go's own documentation states the
reason plainly, and it is the reason this module follows it: **backtracking
engines are worst-case exponential in input length**, and the feature that
causes it -- backreferences, which make the language non-regular -- is not
needed by the overwhelming majority of real uses (validating a field,
pulling a token out of a line, splitting on a delimiter that isn't a fixed
string). RE2's own design note, and Russ Cox's "Regular Expression Matching
Can Be Simple And Fast" (2007), make the same case with the same evidence:
a Thompson NFA simulated breadth-first (Ken Thompson, 1968; the submatch-
tracking version is Rob Pike's, used in Plan 9's and Go's `regexp`) matches
in `O(n*m)` time, `n` the input length and `m` the compiled program size,
with **no input on which it can blow up** -- the property a backtracking
engine cannot offer no matter how it's tuned.

**v0 ships:**

  - literals, over Unicode scalar values (code points), not bytes -- see §2
  - `.`, matching any single code point (see §2a for why it includes `\n`)
  - character classes `[abc]`, `[a-z]`, negation `[^...]`, and the
    shorthand classes `\d \w \s` (positive only -- see §2b)
  - anchors `^` and `$`, string-start/string-end only -- no multiline mode,
    because there is no flags mechanism to turn one on (see §2c)
  - quantifiers `* + ?`, greedy, plus bounded `{m}`, `{m,}`, `{m,n}`
  - alternation `|`
  - grouping `(...)` with capture, numbered left to right by `(`
  - `is_match`, `find` (leftmost match, unanchored search), `find_all`
    (non-overlapping, left to right)

**Explicitly out of scope for v0, each a named, separate gap, not an
oversight:**

  - **Backreferences** (`\1` inside the pattern) and **lookahead/lookbehind**
    (`(?=...)`, `(?!...)`, `(?<=...)`, `(?<!...)`). This is the central
    decision, so it gets its own paragraph next, rather than a bullet.
  - **Non-greedy quantifiers** (`*?`, `+?`, `??`). Additive later: they need
    one more bit per quantifier (which branch of a `Split` is tried first)
    and nothing else changes in the engine. Left out because the task is
    "ship a minimal, correct engine", and every construct added is one more
    thing the freeze has to carry forever.
  - **Non-capturing groups** (`(?:...)`) and **named groups** (`(?P<name>..)`).
    A capturing group is the only group this engine has; the pure-grouping
    case (precedence without capture) still works, it just allocates a
    capture slot nobody reads. Additive later, and genuinely additive --
    today's numbering does not have to change, because `(?:...)` consumes no
    number by definition.
  - **Unicode character classes** (`\p{L}`, `\p{Greek}`, POSIX classes like
    `[:alpha:]`). `\d \w \s` are ASCII, matching `lib/text.m31`'s own
    `is_space` and the language's own "ASCII for built-ins, Unicode is the
    `unicode` module's job" line (docs/text-decision.md §7, §8). A pattern
    that needs a Unicode property class composes one from `[a-zA-Z...]`
    ranges today, or waits for `unicode` to grow the tables this would need
    to borrow from it anyway.
  - **Flags of any kind** -- no case-insensitive match, no multiline mode, no
    dot-matches-newline toggle, no verbose/extended mode. There is no
    variadic function and no default-argument-shaped way to bolt flags onto
    `compile` without it starting to look like `regex.compile(pattern,
    flags...)`, which `docs/reference.md` §9 rules out outright. A
    case-insensitive match is `[Aa][Bb][Cc]` today, or a second `compile`
    call with the pattern's cases expanded by the caller -- both exist
    without adding anything here. If demand is real, the honest answer is a
    second constructor, `regex.compile_ci(pattern)`, not a hidden parameter.
  - **`replace` and `split` by pattern.** `text.replace` already does
    fixed-string replacement; a regex-driven version is a real, separate
    function (it has to decide what `$1` means in the replacement text,
    which is its own small design) and does not block `is_match` / `find` /
    `find_all`, which is what every other user of a pattern needs. Left for
    a follow-up once there is a caller.
  - **Compiling from `bytes`.** A pattern and a subject are both `str` --
    valid UTF-8 -- consistent with `docs/text-decision.md`'s whole argument
    that text work happens in code points, not octets. A program matching
    against raw bytes decodes first (`b.utf8()`), as it would for any other
    text operation.

### Backreferences and lookaround: out, and why that is not a compromise

Go's `regexp` package made exactly this cut, and its own documentation says
why: "this package supports leftmost-first matching semantics... this
corresponds to Perl's and PCRE's except that (1) there is no support for
backreferences and (2) full Unicode support is available". Russ Cox's
articles (<https://swtch.com/~rsc/regexp/regexp1.html> and part 3,
<https://swtch.com/~rsc/regexp/regexp3.html>, which is specifically about
backreferences) lay out the actual complexity-theoretic reason: **a regular
expression with backreferences is not a regular language any more** --
matching one is NP-hard in general (equivalent to a 3-SAT-shaped search over
which prior match each backreference assumes), and every backtracking engine
(PCRE, Perl, Python's `re`, every "regex" in JavaScript) pays for that by
being worst-case exponential even on backreference-free patterns, because
the *engine* backtracks whether or not a given pattern uses the feature. A
Thompson-NFA engine cannot implement backreferences at all without reverting
to backtracking for the whole pattern -- the two features are not
independent toggles, they are two different engines. Lookahead and
lookbehind are a smaller version of the same problem: they are implementable
without backtracking (RE2 added nothing for them because the restriction
that defeats them is the same one that defeats backreferences being fast),
and a *correct, efficient* zero-width-assertion mechanism is real added
design (what can appear inside one, how it interacts with capture groups) --
not a free feature, and not needed by `is_match`/`find`/`find_all`'s actual
callers. The decision, concretely: **this engine guarantees `O(n*m)` time on
every input, for every pattern it accepts, with no caveat** -- the same
guarantee Go ships -- and the price is a feature almost no caller needs and
that every engine offering it pays for globally.

## 2. Matching is over code points, not bytes

`docs/text-decision.md` already decided this for the rest of the language:
sizes and offsets are bytes, but *text-level work* -- comparing, composing,
iterating what a program means by "character" -- is in code points, and a
code point is an `int` (`s.chars()`, `str.from_chars`). Pattern matching is
text-level work, squarely: `.` means "one character", not "one byte of a
UTF-8 sequence", and `[a-z]` is a comparison between scalar values. So a
`Regex` compiles its pattern's code points, matches against the subject's
code points (`s.chars()`), and a `Match`'s capture positions and substrings
are code-point based too -- `Group.start`/`Group.end` are offsets into
`s.chars()`, and `Group.text` is `str.from_chars` of the matched slice. This
sidesteps the byte/code-point offset question `substr` has to answer (an
offset that lands inside a character traps) entirely: a code-point slice of
a `List<int>` is always a valid place to cut, by construction, with no
boundary to get wrong.

The cost is stated at the line, the way `const`'s copy is (§4.1): `s.chars()`
is one allocation and one UTF-8 decode pass over the whole subject, paid
once per `find`/`find_all`/`is_match` call, not once per character compared.
For ASCII text -- the common case for log lines, config values, identifiers
-- this is indistinguishable from scanning bytes; for a subject with
multi-byte text it is the honest cost of comparing code points rather than
code units, the same trade Rust's `.chars()` makes.

### 2a. `.` matches any code point, newline included

Perl, Python and Go's `regexp` all default `.` to "any character except
`\n`", with a flag (`(?s)`, `re.DOTALL`) to include it. That default exists
*because* those engines have a flags mechanism already paid for by
backreferences and other per-match state, and the asymmetry (one meaning of
`.` needs a flag, the other is the default) is itself a thing to remember.
This engine has no flags (§1), so the choice is "`.` always excludes `\n`,
with no way to turn that off" or "`.` always includes it, with no way to
turn that on" -- and the second is strictly more useful with nothing to
compensate for the lack of a flag: a pattern that wants to stop at a line
boundary explicitly says `[^\n]` or anchors with `^`/`$` (which already mean
string-start/string-end, not line-start/line-end -- see §2c), and nothing
is unreachable. One rule, stated once, with no hidden mode a reader has to
know is or isn't active -- in the same spirit as this language refusing
augmented assignment over "which spelling is canonical" (`docs/reference.md`
§9).

### 2b. Shorthand classes are ASCII, and `\D \W \S` are not supported inside `[...]`

`\d` is `[0-9]`; `\w` is `[A-Za-z0-9_]`; `\s` is the same six bytes
`lib/text.m31`'s `is_space` treats as space -- space, tab, `\n`, `\r`, `\f`,
`\v` -- reused on purpose rather than redefined, so two modules never
disagree about what whitespace is (the consistency argument
`docs/text-decision.md` already made for `trim` and `split_whitespace`).
Used bare (`\d+`) or inside a class (`[\d_]`, "a digit or an underscore"),
they contribute their ranges to the surrounding match or class directly.

`\D`, `\W` and `\S` -- the negated forms -- work standalone (`\D` is its own
one-shorthand class, negated). **They are refused inside `[...]`** --
`[\D\s]` is a compile error, not a silent wrong answer. The reason is that a
character class here is one set of ranges plus one negation bit applied to
the *whole* class (§3 below); unioning a positive range list with an
already-negated sub-class is a different, harder data structure (an
expression tree of set operations, not a flat range table), for a
combination -- "not a digit, OR is whitespace" nested inside a class -- that
real patterns essentially never need and that is easy to get wrong
silently. `[\D]` alone, with nothing else in the brackets, has an honest
non-bracket spelling (`\D`) and gains nothing from the brackets.

### 2c. `^` and `$` are string anchors, not line anchors, because there is no multiline mode

Same reasoning as §2a: multiline mode (where `^`/`$` also match after/before
every `\n`) is a flag, there are no flags, and the single meaning that
survives is the one that cannot be recovered another way -- a program that
wants per-line anchoring already has `text.lines(s)` (`docs/text-decision.md`)
and can match each line.

## 3. The engine: a Thompson NFA, Pike's VM, no AST

The compiler is a single recursive-descent pass over the pattern's code
points that emits instructions directly -- there is no separate
pattern-AST type, because nothing here needs to walk the pattern twice. Each
production (`alt`, `concat`, `repeat`, `atom`) returns a `Frag`: a
self-contained `List<Inst>` with its own internal jumps addressed from 0, so
two fragments compose by copying one into the other and shifting its
addresses by where it landed -- the construction Thompson's original 1968
algorithm uses, and the one every NFA-from-regex compiler since has reused
(Cox's articles call this "dangling arrows" patched after the fact; this
module's version patches a `Split`'s or `Jmp`'s int target after both sides
of it are known, which is the same idea without the extra indirection a
patch-list needs in a language with no pointers to patch -- see
`frag_append`'s and `star`'s comments in the source for the exact mechanism).

The instruction set is the textbook seven: `Char`, `Any`, `Class` (an index
into a table of range-plus-negation definitions, built once per `compile`),
`Jmp`, `Split` (two targets, in priority order -- the first is preferred,
which is what makes `*` `+` `?` greedy and alternation left-biased), `Save`
(records the current position into a capture slot -- slot 0/1 are the whole
match, slots `2n`/`2n+1` are group `n`), and `Match`.

Matching is Pike's VM (Rob Pike's submatch-tracking extension of Thompson's
simulation, as used in Plan 9's and Go's `regexp`): two thread lists,
`clist` for the current code-point position and `nlist` for the next, each
thread a `(pc, saved)` pair. Epsilon transitions (`Jmp`, `Split`, `Save`,
and the zero-width `Bol`/`Eol` assertions `^`/`$` compile to) are followed
immediately, with a per-step "already added" bitmap keyed by `pc` so a
pattern like `(a*)*` cannot add the same thread twice and loop -- this bound
is exactly what keeps the simulation to `O(m)` threads per code point, and
therefore `O(n*m)` overall, REGARDLESS of the pattern's shape. Threads in a
list are kept in priority order (the order `Split`'s branches were taken),
so when a thread reaches `Match`, every lower-priority thread still in
`clist` is abandoned (it cannot win against a higher-priority match) while
every already-promoted, higher-priority thread in `nlist` keeps running --
exactly the rule that makes a greedy quantifier prefer the longest match
without backtracking to find it.

**Unanchored search is the standard trick, not a loop over start
positions.** `compile` wraps the real pattern as

    pc0: Split(pc1, skip)        -- try matching here first (higher priority)
    pc1: Save(0) <pattern> Save(1) Match
    skip: Any
          Jmp(pc0)                -- only reached if trying here cannot work

so one pass of the simulation, seeded once, finds the leftmost match over
the whole subject -- the same program handles `is_match`, `find` and (by
re-seeding from where the previous match ended) `find_all`, and a pattern
that begins with `^` still only matches at the true start, because `Bol`
checks the absolute code-point position, not where the search was seeded.

**Capture arrays are copy-on-write by construction, not by a defensive
`clone` at every branch.** A thread's `saved` is a `List<int>` (reference
type, §3.2); the *only* instruction that writes to one is `Save`, and `Save`
always `clone`s before writing -- so a `saved` array, once built, is never
mutated again, and two threads may freely share one without a race between
them. This is why `addthread`'s `Split` case hands the SAME `saved`
reference to both branches rather than cloning defensively: the invariant
that makes that safe is written next to it in the source.

**Bounded repeats are expansion, not a counted loop in the VM.** `{m,n}`
compiles to `m` mandatory copies of the atom's fragment followed by `n - m`
copies of `atom?` (each independently optional, which -- simulated
breadth-first rather than backtracked -- gives exactly the greedy "as many
as fit, up to n" semantics without a sixteenth instruction type); `{m,}` is
`m` copies followed by a `*`. This is RE2's own translation. A compile-time
cap (`MAX_REPEAT = 1000`, checked on `m` and `n` before expansion) exists
only to bound how large one bound can make the compiled program -- it is a
memory guard on `compile`, unrelated to the matching engine's time
complexity, which stays `O(n*m)` for whatever `m` compiling produced.

## 4. Compile errors are a payload-free enum, per `docs/stdlib-decision.md`

`compile(pattern)` returns `Result<Regex, CompileError>`. `CompileError` is
payload-free (`docs/errors-decision.md` §4: the position and the pattern are
context the caller already has, since it has the `pattern` string it passed)
and lists the nine ways a pattern can fail to parse: unbalanced parens,
an unterminated `[...]`, a quantifier with nothing before it (`*abc`,
`a**`), an unknown escape (`\q`), a pattern ending in a bare `\`, a
malformed `{...}`, a `{m,n}` past `MAX_REPEAT`, a reversed range (`[z-a]`),
and an empty class (`[]`, `[^]` is still empty under this engine's rule that
a leading `]` is not special -- see below). Matching never fails once a
`Regex` exists -- `is_match`/`find`/`find_all` always terminate with an
answer, by construction (§3) -- so there is exactly one place an `E`
is needed, and this is it.

**A literal `]` inside a class must be escaped (`\]`); there is no POSIX
"a `]` right after `[` or `[^` is literal" special case.** That convention
exists in Perl/PCRE for compatibility with old `ed`/`grep` bracket
expressions; Go's `regexp` does not carry it either, and dropping it removes
a parsing special case for a spelling (`[]abc]`) that reads as a mistake to
anyone who has not memorized the rule.

**A brace that is not a well-formed `{m}`/`{m,}`/`{m,n}` is a compile
error, not a literal `{`.** PCRE and Perl fall back to treating a malformed
`{...}` as literal text, which means the parser has to speculatively parse
a bound, fail, and backtrack to "oh, that was just a `{`" -- exactly the
kind of implicit, context-dependent rule this language's own grammar
refuses elsewhere (`docs/reference.md` §9's stance on `+=`: "the objection
is the absence of an arbiter between two legal spellings"). Here `{` always
introduces a bound; a literal `{` is `\{`.

## 5. Testing discipline

Same rule as the rest of this codebase: **an independent oracle, not this
engine checked against itself, wherever one exists.** Python's `re` module
is the oracle for every construct the two engines share exactly -- literals,
`.`, classes, anchors, greedy quantifiers, alternation, capturing groups --
because none of those constructs' *meaning* depends on backtracking vs.
simulation; the two algorithms are required to agree on every pattern that
doesn't use a backreference or lookaround, which is all of v0's surface.
`corpus/modules/stdlib-regex/oracle.py` computes the expected output with
`re` the way `corpus/modules/stdlib-text/oracle.py` does with `str`.

The one property Python's `re` cannot demonstrate is the whole reason this
design doc exists: that there is no input on which this engine is slow.
`corpus/modules/stdlib-regex-perf` compiles the classic pathological
pattern -- `a?a?a?a?a?a?a?a?a?a?a?a?a?a?a?a?a?a?a?a?aaaaaaaaaaaaaaaaaaaa`
(20 optional `a`s followed by 20 mandatory ones), which forces a naive
backtracking engine through on the order of 2^20 branch points deciding
whether each `a?` should consume -- against inputs it matches and inputs
one character short of matching (the case that makes a *backtracking*
engine work hardest, since it must exhaust every combination before
concluding failure). Both run to completion as fast as every other test in
the corpus, which is the demonstration: not a timer, but the absence of
`run.sh` needing one. `corpus/modules/stdlib-regex-errors` is the
`CompileError` enumeration, one pattern per variant.

## Open

  - Non-greedy quantifiers, non-capturing and named groups, Unicode property
    classes, `replace`/`split` by pattern, and flags of any kind (§1) --
    each additive, none blocking a caller of `is_match`/`find`/`find_all`
    today.
  - `find_all`'s current cost is `O(n*m)` for a single match and, because it
    re-seeds the whole unanchored search from the end of the previous match
    rather than continuing one pass, `O(n^2*m)` worst case across many
    matches in one long subject -- still polynomial, nowhere near the
    backtracking engines this module exists to avoid, but not the
    single-pass `O(n*m)` a `find_all`-aware driver could give it. Worth
    revisiting if a real caller matches many times over long text.
  - Whether `compile`'s one-line registration in `src/stdlib.rs` is the
    pattern every future `lib/` module should follow, or whether that file
    wants a lookup table instead of a `match` arm per module -- unrelated to
    this module, not decided here.
