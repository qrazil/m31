# Project layout: the decision

Status: **decided, phase 1 prototyped** (branch `design/project-layout`,
`src/modules.rs`). Written in the same discipline as
`docs/modules-decision.md` and `docs/remote-imports-decision.md`: this
changes load-bearing compiler surface (module resolution), so the shape is
settled once.

The problem, in the user's words: *"Folder structure for files (everything
in root)."* `gitui` has 59 entries at its repo root — 18 modules, 2 entry
points, 18 `t_*.m31` harnesses, 9 `test_*.sh`, 4 Python oracles, build
scripts, docs. `tui`, `markdown` and `httpserver` are smaller but the same
shape. This is not sloppiness; it is forced. `docs/modules-decision.md`
deferred "nested directories and how a program's source set is named", and
until now an `import x;` resolves to exactly one place — `x.m31` beside the
**entry file** — so a module cannot live anywhere but next to the program
that uses it, and a program cannot live anywhere but next to its modules.

---

## 1. What forces the flat layout today (the root cause)

In `Loader` (`src/modules.rs`) every import is resolved at
`entry_dir.join(name + ".m31")`, and a module's **identity is its basename**
(`lib.f`, `lib#Type`, `modules.contains(name)` in lower/mono/privacy).
Three consequences ripple out into the sibling repos:

1. Modules, entry points and tests must be in one directory. `t_*.m31`
   harnesses are entry files, so they sit beside the modules they test.
2. A multi-file dependency cannot be consumed in place. `gitui` imports
   a dozen `tui*` files that live in another checkout; `build-gitui.sh`,
   `test_gitui.sh`'s `build_tui` and `test_hunks.sh` each **copy** the
   needed files into a staging directory and compile from there. The
   copy list is hand-maintained in three places and silently rots when
   `tui` grows an import.
3. Module names are globally unique, and the stdlib is in the same
   namespace, so `tui` prefixes everything (`tuitext`, because `text` is
   stdlib; `tuibuf`, `tuigeom`, ...). The prefix is a manual namespace.

Any answer has to fix (1) and (2); (3) is a bonus we should not
over-promise (see section 3).

## 2. The options

**A. Search path / `-I` flags / `M31_PATH`.** Rejected. It makes the
meaning of `import x;` depend on invocation, reintroduces the
"works on my machine" class that `deps.lock` just removed, and every
build script grows a flag that must match the test scripts and CI.

**B. Per-directory imports (`import x;` = beside the importing file).**
Rejected. Looks natural, but identity is the basename, so the same file
reached from two directories has two different spellings, and moving a
file changes every import *in it*. Relative imports also make
`m31c fmt` and moves harder to reason about.

**C. A package/manifest system (name, version, `[modules]` table).**
Rejected for now. `deps` + `deps.lock` already is the manifest, and a
second one would have to be kept consistent with it.

**D. Directory-qualified imports, resolved from the project root
(chosen).** `import ui.widgets.button;` means `<root>/ui/widgets/button.m31`.
The root is the nearest ancestor of the entry file's directory that
contains a `deps` file; with none, it is the entry's directory (so every
existing program is unchanged). Dependencies are addressed by their name
as the first segment: `import tui.geom;` means
`.m31-deps/tui/geom.m31`.

Why D: it is the smallest thing that fixes (1) and (2); the spelling of an
import is the same in every file (path from root, never "where am I"); it
reuses the marker the dependency system already has; it needs no new
syntax beyond `.` in an import, which was already the qualifier
separator elsewhere; and it keeps identity = last segment, so *nothing
downstream of the loader changes* (lower, mono, privacy, fmt output of
bodies, C emission).

## 3. The model, exactly

**Syntax.** `import a.b.c;` — one or more identifiers separated by `.`.
Every segment must be a valid identifier (so `my-dir` is not importable;
rename the directory). The module's **name** is the last segment, `c`,
and that is how it is referred to in the file: `c.f(...)`, `c.Type`.
There are no aliases. Importing `a.c` and `b.c` into one file is an error
(they would both be `c`).

**Resolution of `import p1.p2...pn;` found in any file:**

1. One segment (`import x;`): `<root>/x.m31`, else the embedded stdlib
   (`lib/x.m31`), else a declared single-file dependency `x` (the v0
   remote-imports rule). Unchanged from today apart from "root" instead
   of "entry dir".
2. More than one segment, first segment `d` declared in `deps`:
   `<root>/.m31-deps/d/p2/.../pn.m31`. Inside a dependency, "root" is the
   dependency's own checkout, so *its* imports (`import style;`,
   `import widgets.label;`) resolve against `.m31-deps/d/`, not against
   the consumer. This is what makes a multi-file dependency work with no
   copying.
3. More than one segment, otherwise: `<root>/p1/.../pn.m31`.
4. If `d` is both a directory under the root **and** a name in `deps`,
   the import is an error naming both; rename one. No silent precedence.
5. A dotted import never falls through to the stdlib and never to a
   single-file dependency: `import ui.math;` with no `ui/math.m31` is
   "cannot find module `ui.math`: no `ui/math.m31` under the project
   root". It does not quietly give you `lib/math.m31`.

**Collisions.** Identity is still the last segment, so two *different*
files that would both be `helper` (`a/helper.m31`, `b/helper.m31`) are
an error at the second import: "module `helper` is both a/helper.m31 and
b/helper.m31: module names are globally unique ... rename one". A dotted
file whose name equals a stdlib module (`ui/math.m31` vs `lib/math.m31`)
is the same error with the stdlib wording, exactly as `math.m31` beside
the entry is today ("collides rather than overrides"). A one-segment
`import math;` is untouched.

**What "globally unique" becomes.** In phase 1, nothing: it is still
true, now per *program* (entry + everything it reaches), and the error
now says which two paths clash instead of just "duplicate". This is
deliberate — it is the whole reason the prototype touches only the
loader. It means directories give you **organization, not namespacing**:
`tui` still needs `tuitext` while it shares a program with stdlib `text`,
and `ui/button.m31` and `forms/button.m31` still cannot meet. We
considered doing the namespacing now and rejected it as a separate
decision with a real cost.

*Phase 2 (not built): wrapped identity.* Make the identity the full path
(`ui.widgets.button`), keep the last segment as the in-file qualifier
only when unambiguous. It would let `tui` drop its prefix and let two
`button`s coexist. The cost is exactly: `lib#Type` interning, the
`modules.contains(name)` checks in lower/mono/privacy, and C symbol
mangling all key on a bare name today and would key on a path; error
text and `fmt` need no change. It is a mechanical but wide compiler
change (estimate 2–3 days with the gates re-run) and a **breaking
symbol rename** for any emitted C that FFI-imports names. Do it only
when a second consumer actually hits a collision.

**Composition with remote imports.** `deps` / `deps.lock` / the
`.m31-deps/<name>/` cache are unchanged. What changes is that a
dependency is no longer limited to one file `<name>.m31`: the whole repo
is checked out under `.m31-deps/<name>/` and addressed by path. The old
single-file form (`import name;` where the dep repo has `name.m31` at
its root) still works. One known limit stays: a dependency's own `deps`
are not followed transitively (v0 rule); a consumer must list them.

**Why `build-gitui.sh`'s staging hack disappears.** The hack exists
because the loader cannot see another checkout. With
`deps: tui <url> <ref>` + `deps.lock`, `m31c` fetches/pins `tui` itself
and `import tui.tuiapp;` resolves into the cache; `tui`'s own imports
resolve against `tui`'s root. No `TUI_ROOT`, no copy lists, and
`TUI_REF` in `test.yml` is replaced by the lock line (bumped by deleting that
line from `deps.lock`, the existing update mechanism).

**Tool changes (all done in the prototype unless noted).**
- `m31c fmt` prints `import a.b.c;` back verbatim (`Import::dotted`).
- `gates.sh` "formatter preserves meaning" staged only `*.m31` of a case
  directory flat; for `corpus/modules/*` it now copies the case
  directory recursively (minus `.m31-deps`). The idempotence glob also
  covers `corpus/modules/*/*/*.m31` and one level deeper.
- `run.sh` needed no code change: it already compiles
  `corpus/modules/*/main.m31` and compares to `main.out`/`main.err`;
  the new cases are just directories.
- Parser: duplicate-name and self-import messages mention the dotted
  path.
- Not done: a `m31c fmt <dir>` recursive mode. Each repo's
  `for f in *.m31` check becomes `find . -name '*.m31' -not -path
  './.m31-deps/*'`; a recursive `fmt --check` flag is a small follow-up.

## 4. The standard layout

One rule: **the root holds what a consumer imports and what a user reads;
everything else has a home.**

```
<repo>/
  deps  deps.lock          # the root marker; may be empty (see 7)
  <modules>.m31            # library modules a consumer imports (libraries)
  <entry>.m31              # or: cmd/<entry>.m31 when several programs
  tests/                   # t_*.m31 harnesses, *.sh drivers, fixtures
    oracles/               # python/other reference implementations
  scripts/                 # build.sh, test.sh, bench scripts
  docs/                    # design.md, FRICTION.md, benchmarks
  README.md
  .github/workflows/
```

- **Libraries** (`tui`) keep their modules at the root, because a
  consumer's path *is* the repo's layout: `import tui.tuiapp;` is stable,
  `import tui.src.tuiapp;` is not a spelling anyone wants. Only
  non-module files leave the root.
- **Applications** (`gitui`, `markdown`, `httpserver`) may group modules
  by domain once they pass ~10 files; nothing depends on their paths.
- Scripts start with `cd "$(dirname "$0")/.."` so they run from the repo
  root, which is where the `deps` marker lives. Test entries in `tests/`
  find the root by walking up, so they import modules with root-relative
  names (`import object;` or `import core.object;`), not `../`.

### gitui, before and after

Before: 59 entries, flat (18 modules, `git.m31`, `gitui.m31`, 18 `t_*.m31`,
9 `test_*.sh`, `test.sh`, 4 `oracle_*.py` + `pty_e2e.py`, `build*.sh`,
`compare.sh`, docs).

After (phase A: no import text changes except for moves of tests and the
tui dependency):

```
gitui/
  deps                     # tui https://github.com/qrazil/tui <ref>
  deps.lock
  git.m31  gitui.m31       # the two entry points
  gitclient.m31 gitconfig.m31 gitignore.m31 gitlog.m31 hunks.m31 index.m31
  object.m31 pack.m31 packwrite.m31 patch.m31 refs.m31 repo.m31 sha1.m31
  status.m31 checkout.m31 httpfetch.m31 httppush.m31 zlib.m31     # 18 modules
  tests/
    t_*.m31 (18)  test_*.sh (9)  test.sh  pty_e2e.py
    oracles/  oracle_inflate.py oracle_inflate_at.py oracle_object.py oracle_sha1.py
  scripts/  build.sh  build-gitui.sh  compare.sh
  docs/     design.md  FRICTION.md
  README.md  .github/
```

Root drops from 59 entries to ~25. Phase B (optional, at the owner's
taste) groups the 18 modules into `core/` (sha1, zlib, object, pack,
packwrite, refs, repo, index), `work/` (status, gitignore, gitconfig,
checkout, patch, hunks), `net/` (httpfetch, httppush, gitclient, gitlog)
and changes each import to `import core.object;` — a mechanical rewrite
that `m31c` itself can check, since a wrong path is a compile error.

### The others

- **tui** (library): modules stay at the root; `bench.m31`, `browse.m31`,
  `demo.m31`, `tests.m31`+`tests.out` go to `examples/` and `tests/`;
  `build.sh`/`check.sh` to `scripts/`; `FRICTION.md` to `docs/`. Gains
  an empty `deps` marker (or, if it ever needs one, real lines).
- **markdown** (the `term-markdown` repo): modules (`blocks`, `doc`,
  `inlines`, `render`) at the root with `main.m31`; `corpus.py`,
  `fuzz.py`, `reference.py` to `tests/oracles/`; the 48-file `tests/`
  corpus stays; `build.sh`, `test.sh` to `scripts/`.
- **httpserver**: `main.m31` at the root; `BENCHMARK.md`, `SCALING.md`,
  `bench_raw/`, `scaling_raw/`, `goserver/`, `greenthread_probe.c` to
  `bench/` (they are comparison material, not the program);
  `build*.sh` to `scripts/`.
- **m31 monorepo**: `lib/` stays flat — it *is* the embedded stdlib and
  names there are the global reserved set. `apps/{git,tui,markdown,
  httpserver,ssh}` mirror the repo layouts above; `apps/git` stops
  needing a copy of `tui` by importing `tui.*` from a sibling directory
  (the monorepo root gets a `deps`-less marker plus a path rule — see
  open question 3).

## 5. The migration plan

Order matters: the compiler change is backward compatible, so no repo is
broken by it landing first.

1. **Compiler (m31)**, ~0.5 day beyond the prototype: review, merge,
   document in `docs/reference.md`, cut a release. Gate: `./gates.sh`
   green (80–90 min). Until a repo bumps `M31_REF` to that release it
   cannot use the new imports, which is the only coupling.
2. **tui** (~0.5 day): move non-modules; add `deps` marker; `test.yml`
   and `build.sh` `cd` to the root; fmt check via `find`. No consumer
   breakage: the module paths do not move.
3. **markdown, httpserver** (~0.5 day and ~0.25 day): moves as above;
   update `README` build lines, the scripts' `cd`, `.gitignore`
   (`markdown` binary path), and `test.yml`/`release.yml` paths for the
   build script and artifact.
4. **gitui** (~1–1.5 days, the one with real content): bump `M31_REF`;
   add `deps`/`deps.lock` for tui; change `import tuiapp;` etc. to
   `import tui.tuiapp;` (a `sed` over ~6 files); delete the staging
   code from `build-gitui.sh`, `test_gitui.sh`'s `build_tui` and
   `test_hunks.sh`; move tests/oracles/scripts; update `test.yml` (drop
   the "Fetch the pinned qrazil/tui commit" step and `TUI_REF`; `m31c`
   fetches it) and `release.yml` build script path. Verify with the
   full `test.sh` against real `git` as today.
5. **Monorepo apps/** (~0.5 day): mirror 2–4; keeps the extractions and
   monorepo in step.
6. **Phase B / Phase 2**, only on demand (grouping gitui's modules:
   ~0.5 day; wrapped identity: 2–3 days).

Total for phase 1 across everything: roughly 3.5–4 working days, of which
the compiler is already prototyped.

## 6. Prototype status

Implemented in `src/ast.rs` (`Import.path`), `parser.rs` (dotted
imports), `fmt.rs`, `deps.rs` (`resolve_package`, `package_root`,
`declares`) and `modules.rs` (`find_root`, `Site`, `locate`, the
collision/not-found messages); `gates.sh` updated as in section 3.
Corpus cases under `corpus/modules/dir-import-*`: basic nesting
(3 levels), missing file, stdlib collision, stdlib shadow, name
collision across directories, same-name in one file, and the package
cases (a committed fixture bare repo consumed as `tui.geom`, with its
own nested `widgets.label`, a missing package file, and the
directory-vs-dependency ambiguity). Rust unit tests cover root
discovery (marker above the entry, none, `.git` boundary, nearest
wins, imports resolve from the root not the importer).

## 7. Open questions

1. **The marker name.** I reused `deps` because it already exists and
   was already "the project file". A repo with no dependencies needs an
   empty `deps` just to say "this is the root", which is odd. Options:
   keep it, or add an explicit empty-allowed `m31.toml`-style marker
   later. Recommendation: keep `deps`; revisit if it confuses anyone.
2. **Phase 2 timing** (wrapped identity): wait for a concrete collision,
   or schedule it so `tui` can drop its prefix?
3. **Monorepo sibling imports.** Within `m31/`, `apps/git` wants `tui`
   from `apps/tui`. Either the monorepo gets a `deps` line pointing
   at a path (`tui ../apps/tui` — works with git paths today but needs
   a ref), or apps/git stays on a copy until the split is complete.
4. **Directory name grammar.** Segments must be identifiers, so
   `term-markdown/` as a *dependency directory* or `my-dir/` cannot be
   imported. Fine for now; say so in `reference.md`.
5. **Transitive deps.** Still v0 (consumer lists them). Revisit when a
   dependency has its own `deps`.
6. **`m31c fmt --check -r`.** Worth adding so each repo's CI does not
   need a `find`.
