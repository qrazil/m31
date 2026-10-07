# Remote imports: the decision

Status: **scoping, v0**. Nothing here is implemented yet. Written before
building, same discipline as `docs/modules-decision.md` and
`docs/ssh-decision.md` — this changes load-bearing compiler surface
(`src/modules.rs`'s module resolution), so the shape gets decided once
rather than discovered mid-implementation.

`docs/modules-decision.md` deliberately deferred this: "Nested directories
and how a program's source set is named... answering it badly now would be
worse than answering it later." Today an `import x;` resolves to exactly one
place — `x.m31` beside the importing file, or the embedded standard library
(`src/modules.rs`'s `Loader::visit`) — with no search path, no project
manifest, no network access of any kind. This document is that deferred
answer's first slice: pulling a library from a remote git repository.

---

## 1. The comparison, and why none of the three obvious answers fit whole

**Cargo** (crates.io registry, `Cargo.toml` + `Cargo.lock`, a real version
resolver): the registry alone is enormous infrastructure to build and run
— an index, a tarball host, a checksum database, an account/publish system
— to build before a single library can be pulled down. Wrong order of
investment for a language whose own compiler is still one file in, one file
out.

**npm**: decentralized-ish in theory, but `node_modules` nesting exists
specifically to duplicate incompatible versions rather than resolve them,
and the ecosystem's own history (the 2016 `left-pad` incident, repeated
typosquatting campaigns, `postinstall` scripts running arbitrary code on
install) is a demonstrated cost of "anyone can publish anything under any
name" with no stronger identity than a string. Not a foundation to build on
without the reasoning for why those specific failures don't recur.

**pip**: weak by the same comparison — `requirements.txt` is not a real
lockfile (no transitive pins, no hashes, by default), and PyPI's own
resolver was known to produce inconsistent installs for years before a
2020 rewrite.

**Go modules** fit best, for a reason specific to this feature: an import
path can *be* a git location directly (`import "github.com/user/repo"`),
needing no central registry at all — just a URL and a ref. That is exactly
the shape of "pull a library from a remote git repo," which is the feature
asked for. The real cost Go paid for shipping the naive version of this
first — no `go.sum`, no checksum database, mutable tags trusted at face
value — was years of supply-chain exposure before `go.sum` and the
`sum.golang.org` checksum proxy landed. The lesson taken here is not "don't
use this model," it's **"the lockfile is not a phase 2."**

**What makes this easier here than it was for Go:** a git commit hash is
already a content-addressed commitment to a whole tree (modulo SHA-1's
known weakening, which is exactly why git itself is moving to SHA-256) —
unlike a registry tarball, which needs a *separate* hash computed and
trusted on top of it (npm's `integrity` field, `Cargo.lock`'s checksums,
`go.sum`'s own line-per-module-version). So the lockfile here can be as
dumb as *(name, resolved commit)* with no checksum database to invent, and
still get the reproducibility guarantee for free.

**Decision: Go's resolution shape (a git URL as the location, no central
registry) + a lockfile from day one (Cargo's actual discipline, not Go's
original gap), with a dumb lockfile format that git's own content-addressing
makes possible.**

---

## 2. Scope: what v0 needs to do, and no more

One remote, pinned, single-file library, fetched and cached locally,
resolved the same way a local import already is.

**Two new files, beside the entry file** (the one existing search location
local imports already use — `Loader.dir` in `src/modules.rs` — not a new
"project root" concept):

- **`deps`** — intent. One line per remote import: `name url ref`
  (whitespace-separated; `#`-prefixed and blank lines ignored). `ref` is
  anything `git checkout` accepts — branch, tag, or commit.
  *Since the project-layout work (docs/project-layout-decision.md):* the file
  also starts with a required header, `name <project>` and `version
  <x.y.z>`, and a line `name path <dir>` names a directory used in place
  instead of a git repository. Everything below is about the `name url ref`
  form.
- **`deps.lock`** — reality, pinned. One line per resolved import: `name
  commit-sha`. Written automatically the first time `name` resolves.
  **Authoritative once present**: the compiler trusts a locked commit and
  never silently re-resolves `ref` against it. Deleting `name`'s line (or
  the whole file) is how an update is requested — no `--update` flag in
  v0, since there is only one thing update could mean yet.

**Cache**: `.m31-deps/<name>/`, beside the entry file — a real `git`
checkout, at the locked commit. Project-local, not a global
`~/.cargo`-style shared cache: this build has no other environment-
dependent state today (`docs/stdlib-seam.md`'s "one binary... the standard
library not changing that" reasoning extends naturally here), and a shared
cache is exactly the kind of state `GOPATH`-era Go spent "a decade of
issues" on (same doc). Revisit if duplicated clones across sibling projects
on one machine actually become a measured cost.

**Fetch mechanism**: shell out to the system `git` via
`std::process::Command` — `git clone`, `git checkout`, `git rev-parse
HEAD`. Not a `git2`-style linked library: `Cargo.toml`'s `[dependencies]`
block is enforced empty by `gates.sh`'s "no dependencies" gate (it counts
`Cargo.lock` packages and fails above one), and shelling out is pure
`std`. This also means `git` itself becomes a new, explicit, documented
requirement for compiling a program that uses a remote import — not needed
at all for a program that doesn't.

**Module shape**: a remote dependency is **one file**, `<name>.m31` at the
repo's root — the same "a file is a module, its name is the basename" rule
(`docs/modules-decision.md` §1) that local imports already follow, not a
new nested-package layout to design. A library wanting to ship more than
one module ships more than one remote import, each its own repo (or its
own `deps` line pointed at a different ref of the same repo, if a mono-repo
layout is wanted later) — not decided further than that here.

**Resolution flow**, the one new fallback added to `Loader::visit`
(`src/modules.rs`), between "check the embedded stdlib" and "error: cannot
find module": if `name.m31` is not beside the entry file, and a `deps`
file beside it has a line for `name`, resolve it there instead:

1. `deps.lock` has a commit for `name`?
   - `.m31-deps/<name>/` exists and its checked-out commit (`git rev-parse
     HEAD` inside it) matches? Use `.m31-deps/<name>/<name>.m31` directly
     — **no network access**.
   - Otherwise (missing or mismatched — a fresh checkout, an interrupted
     previous fetch, a hand-edited cache): `git clone` fresh, `git
     checkout` the **locked commit** (never `ref` — the lock wins), verify,
     proceed. Still no re-resolution of `ref` itself.
2. No entry in `deps.lock` yet: `git clone <url>` into `.m31-deps/<name>/`,
   `git checkout <ref>`, `git rev-parse HEAD` to get the resolved commit,
   append `name commit-sha` to `deps.lock`, proceed.
3. Once the file is on disk, it is handed to the existing `self.parse(...)`
   exactly like a local import — everything downstream of "here is a path
   and its text" is unchanged.

**Errors, each a clear diagnostic, not a panic:** `git` not found on
`PATH`; `deps` malformed (wrong field count on a non-comment, non-blank
line); clone or checkout failure (git's own stderr surfaced); a `deps.lock`
entry naming a commit `.m31-deps/<name>/` cannot be made to check out to
(e.g. the remote history was rewritten and the commit is gone).

---

## 3. Explicitly out of scope for v0, each a named, separate gap

- **Version ranges / a real resolver.** No semver, no minimal-version
  selection across a dependency graph. `deps` names one exact ref per
  import; `deps.lock` pins one exact commit. If two remote dependencies
  ever need compatible-but-different versions of a third, v0 cannot
  express that — it can express only "one remote repo, one pinned commit,"
  the same granularity Go had before modules existed at all.
- **Transitive remote dependencies.** A remote dependency's own `deps`
  file, if it has one, is not read in v0. A remote library may only use
  the embedded standard library and whatever it ships in its own one file.
- **Multi-file remote packages.** One file per remote import, per §2.
- **A shared/global cache.** Per-project only, per §2's reasoning.
- **Integrity beyond git's own commit addressing.** No separate signature
  scheme, no maintainer-identity verification beyond "this is the commit
  this URL's history contains." A compromised upstream repo that force-
  pushes a tag before it is first locked is not defended against here —
  the same trust boundary `go.mod` has before `go.sum`'s checksum database
  is consulted, named rather than silently assumed away.
- **An update command/flag.** Delete the `deps.lock` line by hand in v0.

## Open

- Whether `.m31-deps/` needs its own `.gitignore` entry convention
  documented for programs that use this (almost certainly yes, deferred to
  implementation).
- Whether a repo-root `<name>.m31` constraint (§2) is too strict once a
  real remote library wants to exist — revisit once one does, not before.
- How this interacts with `apps/git`'s own eventual capabilities (this
  project already has an in-progress git client/server, `docs/orogit`-
  adjacent work) — plausibly `git` the *subprocess* dependency above could
  eventually be this project's own `apps/git` instead of the system
  binary, but that is a separate follow-up, not a v0 blocker.
