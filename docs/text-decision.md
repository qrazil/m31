# Text — the decision

Decided **2026-09-21**. Answers the open question reference §3.10 carried
since `bytes` landed ("`str` holds arbitrary bytes"), and says what the
language does with emoji and accented characters.

In one paragraph: **a `str` is always valid UTF-8**, and `bytes` is the only
home for arbitrary octets. **Sizes and offsets stay in bytes** — `size()`,
`substr`, `index_of`, `byte_at` — because that is O(1) and what files and
sockets speak, and **an offset that is not on a character boundary traps**.
**Text-level work is in code points**, and a code point is an **`int`**:
`s.chars()` gives them, `str.from_chars(xs)` builds text back. Case mapping,
grapheme clusters and normalisation need Unicode tables and wait for a
`unicode` module, whose plan is §8.

This is Rust's model, which the author approved as the direction. The
sections below are the evidence for it and the places it was refined.

---

## 1. What the other languages did, and what it cost them

| | the unit of `string` | may it hold invalid text? | `len` counts | indexing | what goes wrong |
|---|---|---|---|---|---|
| **Go** | bytes | yes | bytes | `s[i]` is a byte; `for i, r := range s` yields runes | invalid UTF-8 flows anywhere; `range` silently turns each bad byte into U+FFFD, so two different strings can iterate identically |
| **Rust** | UTF-8 bytes | no | bytes | `&s[a..b]` by byte offset, **panics** off a char boundary; `.chars()` for code points | a byte-offset slice computed by arithmetic panics; `String::from_utf8` is fallible and `env::args()` panics on a non-UTF-8 argument |
| **Swift** | grapheme clusters (`Character`) | no | graphemes, O(n) | no integer indexing — `String.Index` only | `count` is O(n) and depends on the Unicode tables of the OS it runs on; every index is an opaque value tied to one string |
| **Python 3** | code points | lone surrogates, via `surrogateescape` (PEP 383) | code points | `s[i]` O(1) | PEP 393 stores each string as 1, 2 or 4 bytes per code point to keep O(1) indexing; the surrogates it smuggles bytes in with are text that cannot be encoded, and blow up later at `.encode()` |
| **Java / JavaScript** | UTF-16 code units | lone surrogates | code units | `s.charAt(i)` is a code unit | `"😀".length` is 2; `substring` and `split("")` cut emoji in half; `toUpperCase` of a surrogate half is a no-op that looks like it worked |

Two lessons carry weight here.

**Validity is cheapest at the door.** Go lets invalid UTF-8 into `string`,
so every consumer — `range`, `strings.ToUpper`, `json.Marshal` — has to
decide what a bad byte means, and each decides differently. Rust checks once,
where octets become text, and then nothing downstream has to. This
language's `json.stringify` had exactly Go's problem: it re-validated every
string and carried an error variant, `NotUtf8`, for text a `str` could hold.
That check and that variant are gone (§5).

**The unit of indexing should be the unit the representation has.** Python
pays in memory and representation-switching for O(1) code point indexing;
Java and JavaScript froze the wrong unit in 1995 and are still paying; Swift
is correct about what a reader calls a character and pays with O(n) counts,
opaque indices and OS-dependent answers. Bytes are what a UTF-8 string *is*.
Offsets in bytes cost nothing, agree with every file and socket, and — with
the boundary rule — cannot produce broken text.

---

## 2. `str` is always valid UTF-8

Every way to make a `str` yields valid UTF-8:

| how a `str` is made | why it is valid |
|---|---|
| a literal | the source is UTF-8 (the lexer refuses anything else), `\u{}` refuses surrogates and values past U+10FFFF, and `\x` stops at 7F (reference §1.5) |
| `b.utf8()` | strict decoder, RFC 3629: no overlong forms, no surrogates, nothing past U+10FFFF, no truncation; `None` otherwise — never U+FFFD |
| `io.read`, `io.read_line_of`, `fs.listdir` | built on `utf8()`, and an error value when it says no |
| `os.args`, `os.env` | built on `utf8()` — **changed**: the runtime used to push argv and environment values straight into a `str` (§4) |
| `+`, `concat`, `repeat`, `join` | valid strings side by side are valid |
| `trim`, `to_upper`, `to_lower` | touch only ASCII bytes, and UTF-8 never uses an ASCII byte inside a multi-byte character |
| `split`, `index_of` | a needle is valid UTF-8, so it begins with a non-continuation byte and ends a character: every match starts and ends on a boundary |
| `substr(from, to)` | **changed**: an offset inside a character traps (§3) |
| `str.from_chars(xs)` | **new**: refuses any value that is not a scalar value, then goes through `utf8()` (§6) |
| `v.to_str()`, `hex()`, float text | ASCII |

`byte_at(i)` stays, anywhere in the string: reading one byte of a character
breaks nothing, and it is how a library scans for ASCII delimiters, which
is always safe (the table's `trim` row).

The runtime comment in `runtime/rt.c` above the string functions carries the
same argument, so the next function added there is checked against it.

### Why not Go's rule

Go's rule — a string is any bytes — was this language's until today, and
reference §6.5 called it "the honest one for a type that carries bytes".
It stopped being honest when `bytes` arrived: a type for octets now exists,
so `str` holding octets is a second way to say the same thing, and the one
that cannot be trusted to be text. And the cost of Go's rule is not paid by
the program that makes the bad string; it is paid by every library that
receives one. The standard library here was already paying it (`json`).

---

## 3. Offsets stay in bytes; one inside a character traps

`s.substr(from, to)` takes byte offsets, and either one landing inside a
character **traps**:

    trap: substring offset 1 is inside a character, not on a UTF-8 boundary

This is Rust's rule for `&s[a..b]`. The alternatives, and why not:

  - **Code point offsets.** Makes `substr` O(n), and every loop that cuts at
    `index_of` quadratic — or it needs Python's representation (§1).
  - **Round to the nearest boundary.** Returns a string of a length the
    caller did not ask for, silently. A parser that is off by one would keep
    working on ASCII input and produce subtly wrong output on anything else.
  - **Return `Option<str>`.** Every cut in every program pays a `match` for
    a failure that never happens with offsets the language hands out.

The last point is why a trap is right under "traps are for bugs, errors are
values" (reference §6.6): **every offset the language gives a program is
already on a boundary** — `index_of`, `size()`, a scan for an ASCII byte
with `byte_at`. Only arithmetic on a guess lands inside a character, and
that is a bug at the call site, like an index out of range.

The one legitimate computed offset is a byte budget — "at most 255 bytes of
this name". `corpus/core/793-truncate-on-a-boundary.m31` is the idiom: back
up while the byte there is a continuation byte (`b & 192 == 128`).

The standard library cut at computed offsets on text a caller supplied in
three modules, each of which would now have trapped on input rather than on
a bug. Nearly all compared one ASCII byte through `substr(i, i + 1)`:
`date.parse` (a helper, `is_at`, now compares `byte_at`), `args` (the
sign test compares a byte; the short-option cluster check, its refusal
message and the one-character rule for `short:` now work over `chars()`),
and `json`'s literal matcher (`word`, now byte by byte). Three corpus programs manufactured
invalid text with `substr` on purpose — to feed `json` a lone lead byte and
to spell a byte-order mark — and were rewritten; the BOM is `"\u{feff}"`.

`bytes.substr` is unchanged: octets have no boundaries.

---

## 4. The process's arguments and environment

On Unix an argument or an environment value is any run of non-NUL bytes. A
`str` must be valid UTF-8. Something has to give, and the languages differ:

  - **Rust:** `env::args()` **panics** on a non-UTF-8 argument;
    `env::args_os()` gives `OsString`s. `env::var(k)` returns
    `Result<String, VarError>` with `NotPresent` and `NotUnicode(OsString)`.
  - **Go:** no question arises — a string holds anything.
  - **Python 3:** `sys.argv` decodes with `surrogateescape`, so the bytes
    survive as lone surrogates that fail later, far from their source.

Here:

| | |
|---|---|
| `os.args()` | `List<str>` — an argument that is not UTF-8 **traps**, naming it and `args_bytes()`. Rust's trade (`env::args()` panics): a `Result` here would put a `match` in every program that reads its command line, for input almost none will meet. |
| `os.args_bytes()` | `List<bytes>`, exactly what the OS passed: Rust's `args_os` |
| `os.env(name)` | `Option<str>` — `None` if unset **or** not UTF-8 |
| `os.env_bytes(name)` | `Option<bytes>`, exactly what the OS holds |

**Not a trap, unlike Rust's `args()`.** A Latin-1 file name on the command
line is the world, not a bug in the program, and "a trap is for a bug, never
for the world" (reference §6.6) is this language's rule even where Rust's is
looser. A program that must take any argument reads `args_bytes`; the
common one matches once.

**Neither dropped nor repaired.** Dropping a bad argument would move every
later index; replacing bytes with U+FFFD would hand the program a file name
that names no file — the same reason `utf8()` refuses rather than repairs.

**`env` is an `Option`, `args` a `Result`.** `env` already answers "is there
a value of this name"; a value that is not text is "no text of that name",
and a program falling back to a default when `LANG` is unset does the right
thing when it is not UTF-8. (`io.read_line` made the same trade for a line
and has since been removed; `io.read_line_of` answers a `Result` and says
`InvalidUtf8` outright, which is the better shape where there is room for
it.) `args` cannot: a list cannot leave one element out without moving the
rest, so it has to fail as a whole.

The runtime now pushes `bytes` for both (`rt_args`, `rt_env`), and
`lib/os.m31` decodes, so the runtime has no remaining way to make a `str`
that nothing checked.

---

## 5. What the guarantee let the library delete

`json` validated UTF-8 twice — once while parsing a string, once while
writing one — and `stringify` had an error variant, `json.Error.NotUtf8`,
for a `str` that was not text. With the guarantee both checks are dead code
and are removed, along with the variant and its private decoder
(`utf8_length`). `json`'s private UTF-8 encoder for `\u` escapes is replaced
by `str.from_chars([cp])`. Removing a variant is a breaking change to
`json`'s API, made now because nothing can construct it any more and an
enum that lists an impossible case misleads every `match` over it.

---

## 6. A code point is an `int`

`s.chars()` returns `List<int>`; `str.from_chars(List<int>)` builds text.

**`int`, not a new `char` type.** The language has one integer type, and a
byte is already a value of it (reference §3.10) — a code point being one
too is the same rule, not a new one. Go agrees (`rune` is an alias for
`int32`). A `char` type would need its own literal syntax (`'é'`), its own
conversions both ways, its own rules as a `Map` key, in `==`, in `print`,
in a `List`. What it buys over `int` is that it cannot hold a surrogate or
a value past U+10FFFF — and that is enforced where it matters anyway, at
`from_chars`, the one place a code point becomes text. A program that
wants the type distinction can declare `distinct int Char;` (reference
§3.6). Adding a `char` later is possible; removing one is not.

**`chars()`, a list.** Rust's name for code points. A list rather than a new
iteration form, so `for (int c in s.chars())` needs nothing the language
lacks; it costs an allocation, which is visible in the spelling. "char"
means a Unicode scalar value here, as in Rust, not what a reader sees as one
character (§8).

**No `char_count()`.** The count is `s.chars().size()`, one spelling, and
the spelling shows that it walks the string. `size()` stays bytes. Asking
for `char_count` or `length` gets a diagnostic naming both.

**`str.from_chars(xs)`, static.** A static method on the target, like
`float.from_bits`, because it is the result type the name has to say. One
code point is `str.from_chars([c])`; there is no `from_char`, and asking for
one (or `chr`) names the list form. This also answers the last sentence of
§3.10's old question — "there is no function from one `int` to a one-byte
`str`" — there is now, and it is not one byte.

**An invalid scalar value traps.** A surrogate, a negative number or
anything past U+10FFFF has no UTF-8 form:

    trap: str.from_chars: U+D800 is not a Unicode scalar value

That is the rule `bytes` already has — storing 256 traps rather than
truncating — for the same reason: the value is the program's own, like an
index. A program holding numbers from outside checks them first, as `json`
does for `\u` escapes (it refuses unpaired surrogates as a syntax error
before it builds anything). Rust's `char::from_u32` returns an `Option`
instead; the difference is that Rust's is the *only* checked conversion,
where here the check sits in the one function that builds text, and an
`Option` there would put a `match` on every correct program to catch a bug.

**`index_of` stays a byte offset**, and is always on a boundary (§3).

### Implemented in the language

Both are UTF-8 bit manipulation over what the language already reaches —
`byte_at`, `bytes.push`, `utf8()` — so they are language source,
`lib/__text.m31`, loaded like `lib/__floatfmt.m31`: only when a file
mentions `chars` or `from_chars`, and unimportable (the name begins with
`__`). `chars` decodes without validating, because the type guarantees
validity; `from_chars` checks each value and still ends in `utf8()`, so it
is not a second door into `str` that trusts its caller. The runtime gained
only the boundary check in `substr`.

---

## 7. What is still bytes or ASCII

  - `size()`, `substr`, `index_of`, `byte_at`: bytes, by design (§3).
  - `trim`: ASCII whitespace. U+00A0 and U+3000 are text.
  - `to_upper`, `to_lower`: ASCII only, as before. `"école".to_upper()` is
    `"éCOLE"`; the é is left alone.
  - `==` and sorting: bytewise. For UTF-8 that is code point order, which
    is deterministic and not what a dictionary does. `"é" == "e\u{301}"` is
    false: they are different code points, equal only after normalisation.
  - Diagnostics report columns in bytes: a caret after a non-ASCII
    character in a source line is misplaced. Not changed here.
  - `args`' help table pads by `size()`, so a non-ASCII option name or help
    text misaligns its column. Not changed here.

---

## 8. Next: a `unicode` module

Everything a reader means by "character" beyond a code point needs the
Unicode Character Database, and tables of that size want module-level
constant arrays, which another change is adding. Nothing here builds them;
this is the plan. Counts are from the UCD 16.0.0 files installed at
`/usr/share/unicode/ucd/` on the development machine, measured when this
was written; the generator will pin the version and regenerate.

| | what it needs | size |
|---|---|---|
| **Simple case mapping** — `unicode.upper(s)`, `lower(s)` one code point to one | `UnicodeData.txt` fields 12–13: 1,477 upper and 1,460 lower mappings | ~680 `(lo, hi, delta)` runs each; fewer with Go's trick of coding alternating upper/lower pairs as one run |
| **Full case mapping** — `ß` → `SS`, final sigma | `SpecialCasing.txt`: 119 entries, a few context-dependent (final sigma) and some locale-dependent (Turkish, Lithuanian) | a small list; locale rules deferred |
| **Case folding** — caseless comparison | `CaseFolding.txt`: 1,590 entries | as simple mapping, plus the full-fold list |
| **Grapheme clusters** (UAX #29) — `unicode.graphemes(s)` → `List<str>`, what a reader calls a character: `👍🏽`, `🇬🇧`, `👨‍👩‍👧`, `e` + U+0301 | `Grapheme_Cluster_Break`: 1,419 ranges, 621 once the 798 Hangul LV/LVT lines are computed from the syllable arithmetic instead; `Extended_Pictographic`: 78 merged ranges; `Indic_Conjunct_Break`: 403 | ~1,100 `(lo, hi, class)` triples, binary-searched; the rules themselves are a state machine in source |
| **Normalisation** NFC / NFD — so `"é" == "e\u{301}"` can be asked | 2,081 canonical decompositions, 934 non-zero combining classes in 393 same-class ranges, 961 primary composites after exclusions; Hangul (11,172 syllables) is arithmetic, no table | ~2,100 decomposition entries + ~960 composition pairs + ~400 class ranges |
| NFKC / NFKD | 3,832 compatibility decompositions more | deferred until something needs it |

Oracles exist for all of it in the same directory and are the test plan:
`auxiliary/GraphemeBreakTest.txt` (1,093 cases) and `NormalizationTest.txt`
(19,965 cases), plus Python's `unicodedata` and `str.upper` for case.

The names would then be: `size()` bytes, `chars().size()` code points,
`unicode.graphemes(s).size()` user-perceived characters — three counts,
each named for what it counts, none pretending to be the only one.
`str.to_upper` stays ASCII: a method on every `str` is not the place for
tables and locale questions, and a program that wants Unicode case says
`unicode.upper(s)`.
