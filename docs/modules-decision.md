# Modules — the decision

Written before building, like `docs/errors-decision.md`. Modules are the last
large piece of surface before the freeze, and everything after them — the
standard library, and self-hosting — is written *in* them.

Evidence below was gathered by reproducing behaviour locally (go1.23.8,
CPython 3.12.10, g++ 14.2.1, Temurin 21) rather than recalled, because the
restrictions here need justifying rather than asserting.

---

## 1. A file is a module, and its name is the basename

`strings.m31` is the module `strings`. No `mod` declarations, no module tree
to keep in agreement with the filesystem — the part of Rust's system people
trip over.

**Module names are therefore globally unique across a program.** Two files
named `util.m31` in different directories are an error, not two modules. This
is OCaml's model and it has a real cost, which is worth stating rather than
discovering: OCaml strips the directory the same way, and the resulting flat
namespace is exactly why dune had to invent wrapped libraries and module
aliases to escape it. Their own documentation says polluting the top-level
namespace "will make your library unusable with other libraries if there is a
module name clash".

Taken anyway, because the alternative — Go's, where the *path* locates and a
`package` clause *names* — needs a package clause in every file, and that is
the ceremony this design exists to avoid. The cost lands when a program grows
large enough to want two `util`s, and the answer then is to name them
differently, which is what you would want regardless.

### Case-insensitive collisions are an error on every platform

Two files whose basenames differ only in case are rejected on Linux as firmly
as on macOS. Go does exactly this (`case-insensitive file name collision`),
and the comment says why: "to avoid problems on case-insensitive files".

Go adopted the rule **late** and paid for it across at least eight issues,
one of which made a repository permanently unusable with "NO fix other than
mutating the commit history". Adopting it on day one is nearly free. Python
went the other way with a `PYTHONCASEOK` escape hatch, which is the shape of
a mistake being lived with.

### A filename must be a valid identifier

The filename becomes a name in the language, so the filename grammar becomes
part of the language grammar. `my-mod.m31` is an error **at discovery**, not
at the import that first mentions it — this language has no warnings, and
OCaml's `bad-module-name` being only a warning is the wrong end of that.

Python shows the seam: `import my-mod` is a `SyntaxError` while
`importlib.import_module('my-mod')` works, so the file is a legal module that
no syntax can name.

---

## 2. Private by default, `pub` to export

A declaration is visible only inside its module unless marked `pub`.

**No block form.** A `private { }` section with default-public was considered
and rejected, and a `pub { }` grouping alongside per-declaration `pub` was
rejected as a second way to do one thing — the same rule that gives one `cmp`
rather than four comparison methods.

The default is the whole decision, and it turns on the freeze. Forgetting to
mark something private makes it **exported permanently**; forgetting to mark
something public is a one-word fix the moment somebody needs it. Those are
not symmetric mistakes in a language that intends to stop changing.

Three smaller arguments point the same way. A reader knows a declaration's
visibility by looking at it, rather than scrolling up to find which section
they are in — C++'s stateful `public:`/`private:` is the counter-example.
Methods are declared outside their type here, so a block could hold
`Rect.area` while `Rect` is public, giving a public type with an invisible
method. And moving a declaration in or out of a block changes its visibility
while the diff reads as a move.

### Fields follow the same rule

A field is a declaration too, so it is private unless marked `pub`:
`type P { pub int x; int secret; }`. For a while every field of a `pub` type
was public, which is the asymmetric mistake above made for every type at
once — and it cost the library real workarounds: `io` kept a `File`'s state
in a private second type, `random.Seeded` carried a `state` field with a
comment asking callers to leave it alone, and `date.Civil` documented as a
"convention" what Oro enforces. Private fields are what make an invariant a
module's to keep.

Construction follows from it. Building a value writes every field, so from
another module it is allowed only when every field it would write is one
the caller could write anyway: a private field without a default refuses
construction, and the module provides a function that builds one; a
private field with a default blocks nothing unless it is named. Enum
variants and interface methods are not fields in this sense — they are what
the enum or the interface is — so they stay exactly as visible as it. Two
levels still, as below: no read-only fields, no `pub(get)`.

### The evidence for default-public is bad

C is the natural experiment: file scope is external by default and `static`
is its private. People forget `static`, symbols leak, and unrelated
translation units collide at link time — which is why every C style guide
says to mark everything `static` unless it must escape, and why
`-fvisibility=hidden` is standard practice for shared libraries. The
ecosystem overrode the default with a compiler flag.

Python is public with `_name` as convention only, and the standard library
has repeatedly been unable to remove "private" names people depended on.
Ruby's `private` is a stateful section marker. Java's default is
package-private — neither extreme. JavaScript was default-public until ES
modules made `export` mandatory.

No language designed in the last twenty years chose default-public.

---

## 3. Import cycles are forbidden, and the diagnostic prints the chain

`A` imports `B`, `B` imports `A` — a compile error, always.

This is the only one of the four that **cannot be added later**. Allow cycles
now and real programs grow them; forbidding them afterwards breaks working
code. Everything else here is recoverable.

### What allowing them costs, with receipts

**Java answers silently and differently depending on load order.** JLS
§12.4.2 step 3 says a recursive initialization request must "release LC and
complete normally". Two classes cross-referencing static fields:

    touch A first:  A.VALUE_FROM_B=7   B.VALUE_FROM_A=0
    touch B first:  B.VALUE_FROM_A=5   A.VALUE_FROM_B=0

Same bytecode. One field is always `0` and which one depends on load order.
No error, no warning. With `static final` the constant folding hides it
entirely.

**Python's outcome depends on which file you ran.** Two files, unchanged
between runs: `import mod_e` succeeds, `import mod_f` raises
`AttributeError: partially initialized module 'mod_f' has no attribute 'F'`.
Identical source, identical cycle; the entry point decides. Python needed two
separate fixes and a `sys.modules` fallback in `ceval.c` to make a cycle
merely *usually* work.

**C++ never mentions the cycle.** With `#pragma once` the guard truncates one
arm and the error lands in an innocent file: `'A' does not name a type`.
Unguarded, the preprocessor recurses to its limit and emits about 1380 lines.

### How it is enforced

Go's shape, which is small and worth copying: a **dedicated pass that owns
the dependency graph**, a depth-first search carrying an explicit import
stack, where reaching a node already on the stack is the cycle. Go puts this
in the driver rather than the type checker — `go/types` has no cycle check at
all — and the separation is the lesson: the graph is not the type checker's
business.

The error prints **every edge**, head repeated to close the loop:

    package a
        imports b
        imports c
        imports a: import cycle not allowed

Go's source carries a comment saying they deliberately keep the *cycle* path
rather than the shortest path. That is the useful one.

**OCaml is the counter-example, not the model.** It forbids cycles by
construction — link order is a total order — but has no cycle diagnostic at
all. The user gets `Reference to undefined global mod` at link time, naming
neither the cycle nor the chain. Banning cycles without a chain-printing
error is worse than not banning them.

**Self-import needs no special case.** C++20 states acyclicity as: a
translation unit "shall not have an interface dependency on itself", with
transitivity folded into the definition of the dependency. Self-import and an
N-cycle become one rule, and one chain printer handles both. Go's
implementation behaves the same way.

---

## 4. Imports are qualified; no wildcards, no aliases

    import strings;
    ...
    strings.trim(s)

An imported name is always written with its module. Go forces this on
purpose — "one writes `io.Reader` not `Reader`" — and the payoff is that a
package's contents need not repeat the package name.

**And only the importing file may write it.** The qualifier `strings` is
bound by the `import` line, not by the program containing a module of that
name, so a file with no `import strings;` cannot write `strings.trim(s)` at
all. Anything else makes an import a *program*-wide binding wearing a
file-wide syntax: the compiler once let a module's name outrank a field in a
file that never imported it, and the breakage arrived when a different file
added an unrelated import that happened to reach that module transitively.
A name in a file may only be changed by editing that file.

**No wildcard import.** The stated harm across three communities is the same:
a reader cannot tell where a name came from, and it compounds when an
upstream release adds a name. PEP 8 says `import *` makes it "unclear which
names are present in the namespace". Go's own review guide says a dot-import
makes programs "much harder to read because it is unclear whether a name like
`Quux` is a top-level identifier in the current package or in an imported
package". Java's version is now in a first-party spec document: JEP 494 shows
importing `java.base` and `java.desktop` then writing `List` → "Error -
Ambiguous name!".

That argument is **stronger here than anywhere else**, because this language
already bans shadowing. A wildcard import would be the last remaining way to
make a name mean something the reader cannot see.

**No aliasing**, and the flat namespace pays for itself here: module names
are globally unique, so two imports cannot collide, so there is nothing for
an alias to resolve. Go allows renaming but tells you to avoid it except for
collisions. With no collisions possible, the feature has no remaining job.

---

## 5. One entry file, declared rather than discovered

There is no `main` — top-level statements *are* the program. With several
files that becomes ambiguous, so one file is the entry point and every other
file is declarations only.

**The entry file is the one named on the command line.** A non-entry file
containing a top-level statement is a hard error.

The alternative — discovering the entry as "the only file with top-level
statements" — was rejected because a stray statement in a library file would
then silently move the program, or make it ambiguous long after the fact.
Declaring it means the same mistake is a diagnostic at the file that made it.

This also dodges both ways a `main`-based entry pays. Name-based entry either
pushes ambiguity to launch time (Java's entry class is a launch argument and
outside the spec; Cargo's "could not determine which binary to run") or gives
a late, badly-located diagnostic (Go's linker: `function main is undeclared
in the main package`, no source position; C's `crt1.o: undefined reference to
'main'`).

The diagnostic to copy is Go's for *two* mains — compile-time, naming **both
files with positions**:

    ./c.go:3:6: main redeclared in this block
    ./b.go:3:6: other declaration of main

Python reached the same rule and never enforced it: a `__main__.py`
"typically isn't fenced with an `if __name__ == '__main__'` block. Instead,
those files are kept short and import functions to execute from other
modules." That is this rule, as convention. PEP 3122 records why the dual
role is a wart — "Guido views running scripts within a package as an
anti-pattern" — and Python's *Import Traps* names the double import trap that
follows from it.

---

## Order of work

1. Discovery and naming: find the source set, basename as module name,
   reject invalid identifiers and case-insensitive collisions.
2. `import`, qualified use, and name resolution across modules.
3. `pub`, with everything else private, enforced at the use site.
4. The cycle pass: DFS with an explicit stack, printing the whole chain.
5. Entry file declared; a top-level statement anywhere else is an error.

Steps 1 and 2 are the useful half and can land first. The compiler is
currently one file in, one file out, so step 1 is also where the command-line
surface has to grow up.

---

## Deliberately not decided here

  - **Nested directories and how a program's source set is named.** Today
    the compiler takes one file. Whether it grows a project file, a directory
    convention, or a list of files on the command line is a build question,
    and answering it badly now would be worse than answering it later.
  - **Visibility finer than module-wide.** No `pub(crate)`, no friend
    modules. Two levels until something forces a third.
  - **Circular *type* references within one module.** Only imports are
    acyclic; a module is free to be as tangled inside as it likes.
