# Per-app repo split: the decision

> **Update:** `git`, `tui`, `markdown` and `httpserver` have been extracted to
> github.com/qrazil/{gitui,tui,term-markdown,httpserver} and removed from this
> monorepo (releases there). `apps/ssh` remains; the text below is the original
> scoping and is kept as history.

Status: **scoping**. Nothing here is implemented — no repo created, no
directory removed, no CI written. This document exists for the same reason
`docs/ssh-decision.md` and `docs/remote-imports-decision.md` do: this changes
how `apps/` relates to the rest of the project, and getting the shape right
once beats discovering it mid-extraction.

---

## 1. Scope: what this is, and what it isn't yet

The end state: each app under `apps/` (`git`, `httpserver`, `markdown`, `ssh`,
`tui`) eventually lives in its own git repository, with its own CI pipeline
that builds it for each architecture the runtime supports, and is removed
from this monorepo once that's proven out. `apps/git` and `apps/ssh` are
explicitly **not** in scope for actual extraction yet — git's SSH remote path
isn't built, and SSH is at Phase 2 of 4. This document scopes the
*mechanism*, piloted on an app that's actually ready, so the mechanism is
proven before it's pointed at the two that aren't.

**Not decided here:** which order `git`/`ssh`/`tui` follow once ready, or
whether `tui` (a library, not a standalone program) gets the same treatment
as a shape with a `main.m31` — see §6.

---

## 2. Pilot candidate: `apps/httpserver`

Surveyed all three eligible apps (`markdown`, `tui`, `httpserver` — `git`/`ssh`
excluded per §1):

| | imports | cross-app coupling | own build.sh/test.sh | size |
|---|---|---|---|---|
| `markdown` | `args`, `blocks`, `html`, `io`, `os`, `render`, `doc`, `inlines`, `math`, `text` — all embedded stdlib + its own files | none found | yes | ~1,900 lines across 5 files |
| `tui` | embedded stdlib only | **none outgoing**, but `apps/git` depends on *it* (below) | `check.sh` | ~5,600 lines across 13 files |
| `httpserver` | `args`, `http`, `net`, `io`, `os` — all embedded stdlib | none | yes (`build.sh`, `build_scaling.sh`, `test.sh`) | 150 lines, one file (`main.m31`) |

**`apps/tui` is disqualified as the pilot, not just a second choice.**
`apps/git/build-gitui.sh` and `apps/git/test_hunks.sh`/`test_gitui.sh`
directly `cp` eleven of `tui`'s own `.m31` files into a staging directory
before compiling `gitui.m31` — a real, load-bearing, one-way dependency
(confirmed: `grep` found zero references the other direction). Extracting
`tui` first would force an immediate decision about how `apps/git` consumes
it (the `deps`/`deps.lock` remote-import mechanism, most likely) while
`apps/git` itself is explicitly not ready to be touched per §1. That's a
second, coupled migration riding on the first one — exactly the kind of
scope creep a pilot should avoid.

**`apps/httpserver` is the pilot.** It's the smallest (150 lines, one file),
has zero cross-app coupling in either direction, already has its own
build/test scripts, and — not incidental — it's the app this very session
spent the most time hardening tonight (the green-thread dip investigation,
the listen-backlog fix): it's in the best-tested state of any app in the
tree right now. `markdown` is a reasonable second choice (also zero
coupling) if `httpserver`'s migration surfaces something `markdown`-specific
that needs a second data point before `git`/`ssh`/`tui` follow.

---

## 3. What moves, what stays, and why `deps`/`deps.lock` doesn't apply here

**Moves:** `apps/httpserver/`'s own files — `main.m31`, `build.sh`,
`build_scaling.sh`, `test.sh`, `greenthread_probe.c`, `README.md`,
`BENCHMARK.md`, `SCALING.md`. (`goserver`, `httpserver`,
`httpserver_scaling`, `bench_raw`, `scaling_raw` are build
artifacts/comparison binaries, not source — confirm none are meant to be
tracked before the move.)

**Stays:** everything else — the compiler (`src/`), the runtime
(`runtime/`), the embedded standard library (`lib/`).

**`deps`/`deps.lock` (`docs/remote-imports-decision.md`) is not the
mechanism this migration needs, and that's worth being explicit about
rather than assuming it applies by default.** `deps` resolves a *remote m31
library* — another repo's `<name>.m31`, pulled by the compiler at build
time. `httpserver` imports nothing like that: every one of its imports
(`args`, `http`, `net`, `io`, `os`) is embedded *inside the `m31c` binary
itself* (`src/stdlib.rs`), resolved with no network access regardless of
which repository the importing program lives in. A standalone `httpserver`
repo needs exactly the same `m31c` binary this repo builds — not an m31-level
dependency at all, but a **tooling** dependency, same shape as "this repo
needs a Rust toolchain" or "this app needs `cc`." `deps`/`deps.lock` would
only become relevant the day `httpserver` (or any extracted app) starts
importing a *third* m31 library that isn't embedded stdlib — not a concern
this migration creates.

So "how does the new repo get `m31c`" is a CI-pipeline question (§5), not an
application-dependency one.

---

## 4. Git history: preserve it, with a caveat

**Recommendation: preserve `apps/httpserver`'s history into the new repo**,
not a fresh single-commit import. This matches the standing discipline in
this project (keeping a reverted experiment's own commit as "a good,
transparent engineering record" rather than squashing it away) — history
here is treated as documentation, not clutter. `apps/httpserver` has 10
commits (`git log --oneline -- apps/httpserver/`), spanning its creation,
the Go-benchmark comparison, the scaling test, and the dip-investigation
fixes — a real record of *why* the code looks the way it does.

**Caveat, found while checking, not assumed:** neither `git subtree` nor
`git-filter-repo` is available on this machine today. `git subtree` isn't
present as a git subcommand here at all; `git-filter-repo` (the modern,
Git-project-recommended tool for exactly this split) isn't installed either,
but it's a single `pip install git-filter-repo` away. The actual extraction
command, once installed:

```
git clone <this-repo> httpserver-extract
cd httpserver-extract
git filter-repo --path apps/httpserver/ --path-rename apps/httpserver/:
```

This rewrites the clone to contain *only* `apps/httpserver`'s history, with
paths rebased to the new repo's root. The original monorepo is untouched by
this — `filter-repo` operates on the clone, not the source.

---

## 5. CI pipeline for the new, standalone repo

**m31c acquisition** — two real options, a genuine judgment call, not
decided here (see §7):

- **(a) Build from source, pinned to a commit/tag of this repo.** The new
  repo's workflow clones a pinned ref of this repo and runs `cargo build
  --release`. Slower per run, but testing against an exact, auditable
  compiler version.
- **(b) Download a released binary.** `.github/workflows/release.yml`
  already publishes per-architecture tarballs attached to GitHub Releases
  for pushed `v*` tags; the new repo's CI downloads a specific tag's asset.
  Faster, but a new m31 feature isn't usable until a tag is cut.

**Target architectures — checked, not assumed, and this surfaced a real,
separate finding:** the runtime fully supports Linux x86-64, Linux aarch64,
and macOS/BSD (kqueue reactor) today. **But no CI workflow in this repo has
ever actually built an *app* on aarch64 or macOS** — `gates.sh` never
touches `apps/`, and `.github/workflows/release.yml`'s own `build-macos` job
is still `if: false`, with a comment asserting the kqueue gap is open —
which is now stale; `runtime/reactor_kqueue.c` already exists. A new
`httpserver` repo's CI claiming to build for aarch64/macOS would be the
**first time that's ever been attempted**, on either platform, for any app.

**Concrete workflow outline** (three jobs, matching this repo's own
`release.yml` shape): `test-linux-x86_64` / `test-linux-aarch64` (new
ground) / `test-macos` (new ground), each: checkout, obtain m31c, build,
run the app's own `test.sh`; then a `release` job on a pushed tag only,
packaging each platform's binary.

`build.sh`/`test.sh` need a small path adjustment once flattened to the new
repo's root (today's paths are `apps/httpserver/`-prefixed) — mechanical,
not a design question.

---

## 6. The cleanup step, once the pilot is proven

Once `httpserver`'s standalone repo has a green CI run on a real push:

1. **Remove `apps/httpserver/` from this repo** in its own commit, not
   bundled with anything else.
2. **Edit `.github/workflows/release.yml`'s `build-linux-x86_64` job** —
   worth a second look first: `httpserver` is notably **already absent**
   from the "Build every app" step today (only `git`/`markdown`/`tui`/the
   `ssh` self-tests are built there). Whether that's deliberate or a gap
   independent of this whole initiative changes what this step needs to do
   — see §7.
3. **`NOTES.txt`'s generation logic** — if `httpserver` is ever added to a
   release bundle before extraction, its removal should leave a line
   pointing at the new repo's URL, not silently drop it.

---

## 6a. Version coordination across repos (decided 2026-10-04)

Resolved while scoping, not left open: the new repo's CI downloads a
**released m31c binary** (not a build-from-source pin) — faster, and it
exercises the actual release artifact a real user would get. This has a
direct, recurring consequence worth stating plainly rather than discovering
at the next version bump: **every extracted app's CI pins a specific m31
release tag, so every future m31 release that an app should pick up needs
that app's own workflow file updated to point at the new tag.** This is not
a one-time migration cost, it is an ongoing one, paid once per m31 release
per extracted app.

Concretely, when `v0.2.0` of m31 ships after `httpserver` (and later
`markdown`/`tui`/`git`/`ssh`) have been extracted: each extracted repo's
`.github/workflows/*.yml` has a line naming `v0.1.0` (or whatever it was
pinned to) that must become `v0.2.0` before that repo's CI builds against
the new release. Three ways to handle this, not decided here:

- **Manual, per release.** Whoever cuts the new m31 tag also opens a bump PR
  (or pushes a bump commit, if they also own the sub-repo) against every
  known extracted repo. Simplest, no new tooling, but easy to forget as the
  number of extracted repos grows — exactly the kind of cross-repo
  bookkeeping that silently drifts.
- **A small propagation script/workflow in the m31 repo itself**, run as
  part of (or right after) cutting a release: iterate a maintained list of
  extracted-app repos and open a bump PR against each (needs a
  cross-repo-capable token, since the default `GITHUB_TOKEN` a workflow gets
  is scoped to the repo it runs in).
- **Track `latest` instead of a pinned tag** in each app's own CI. Removes
  the bump step entirely, but reopens the exact trade-off `v0.1.0`'s own
  release-vs-source-pin decision (§5) already weighed in favor of pinning:
  an app's CI could then start failing from an m31 change the app's own
  maintainer never asked for or reviewed.

This needs a real decision before the second m31 release ships, not before
the first extraction — `v0.1.0` existing is `httpserver`'s own prerequisite
(§5), and this is what happens the next time that version number needs to
move.

---

## 7. Open questions — not resolved here, by design

- **(a) vs. (b) in §5** for `m31c` acquisition — a real trade-off
  (latest-vs-pinned, speed-vs-source-of-truth) deliberately not picked here.
- **`release.yml`'s macOS job is stale** (`if: false`, citing a kqueue gap
  that's since closed). A separate, real finding, independent of this
  initiative — worth its own fix regardless of whether `httpserver` ever
  moves.
- **`httpserver` is already absent from `release.yml`'s own app-bundling
  step.** Intentional, or an oversight? Changes what §6 step 2 means.
- **Whether `apps/tui`, being a library with no single `main.m31`, gets the
  identical treatment** once `apps/git` is ready to consume it as a remote
  import, or needs a different split pattern (a versioned m31 library an
  app's `deps` file names, rather than a program with a shipped binary) —
  downstream of `git` being ready, not resolved here.
- **Repository ownership/naming/org** for the new repos.
- **CI secrets/credentials** for a new repo's own releases — assumed to
  carry over by the same mechanism as this repo's `release.yml`, not
  independently verified.
