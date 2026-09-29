# The interactive client — design, locked 2026-09-28

Written before stage 2 of `apps/tui` exists, so the widget list there is
built toward this rather than guessed at afterward. Not implementation —
`apps/git` is stage 1 (read-only plumbing) and this waits on `apps/tui`
stage 2/3 per its own README. Recorded now so the direction survives the
gap.

## The comparison this came from

Terminal git clients split into two families:

  - **Panel tools** — lazygit, gitui, tig. Fixed panes (files, branches,
    commits) you switch between, each showing one slice of the repository.
  - **Magit**, structurally different: one continuous, collapsible document.
    Untracked files, unstaged changes, staged changes, stashes, recent
    commits — all sections in a single scrollable view, expanded and
    collapsed in place, rather than screens you switch between. Staging is
    cursor position, not a mode: put point on a file, a hunk, or a line and
    stage exactly that. Every action with options — commit, rebase, push —
    opens a small menu of the real git flags as togglable switches, with the
    literal command shown before it runs.

Magit's one real cost is discoverability: it leans on `?`-summoned help and
users tolerating terse text. The panel tools do better here, with an
always-visible key-hint footer.

This client takes Magit's structure and the panel tools' surface manners,
not a copy of either.

## Locked

  - **One primary view**: the collapsible document, not fixed panels. This
    is the surface the client is actually driven from.
  - **One optional side panel, a jump list — corrected 2026-09-28, this is
    not a filesystem tree.** It is a flat, navigable index of whatever the
    main document currently contains as entries: log commits, changed
    files, branches, stashes — not paths on disk. It toggles, shown or
    hidden by one key, never a fixed pane competing with the main view for
    width. It works two ways: selecting an entry jumps the main document to
    it, and scrolling the main document highlights the corresponding entry
    here — so it is as much a "where am I" indicator as a way in. Closer to
    a synced table of contents than a directory browser.
  - **Cursor-addressable staging**: line, hunk, file, or section, one key,
    uniform — whatever is under the cursor is what gets staged. No separate
    staging mode.
  - **Drill-down as the universal verb**: Enter on a commit, a stash, or a
    branch expands it in place (its diff, its log) rather than switching to
    a different screen. Recursive, with a back-stack.
  - **Base commands always visible at the bottom** — the panel tools' key
    footer, kept because Magit's willingness to make users learn keys first
    is its one real weakness. This shows the small, fixed set: stage, unstage,
    commit, push, pull, diff, quit — the things done on every visit.
  - **A which-key-style popup for everything else.** Base commands are
    always on screen; a command with its own options (commit, rebase, log
    filtering, branch operations) is reached by a prefix key that opens an
    overlay of what the *next* key does — labelled, not memorised — and for
    a command with real git flags behind it, that overlay is the transient
    menu: togglable switches, a live preview of the literal command, one key
    to run it. Depth is: footer shows the base verbs; pressing one that has
    follow-ups shows the which-key overlay of them; a follow-up with flags
    opens the transient. Nothing is ever hidden behind a key with no visible
    hint of what it does.

## What this needs from `apps/tui`

Flagging these because they are library primitives, not client-specific
widgets, and belong in stage 2's scope rather than being built once inside
the git client and stuck there:

  - a **collapsible outline widget** for the primary document (nested
    sections, expand and collapse);
  - a **synced jump list**: a flat, selectable list bound to the outline
    widget's own entries, where selecting a row scrolls the document to it
    and scrolling the document highlights the corresponding row — two-way,
    not just a static index;
  - a **which-key overlay**: a small popup keyed by "what does the next
    keypress do", built from a list of (key, label) pairs;
  - a **transient menu**: the which-key overlay's sibling — togglable
    switches, a rendered command preview line, one key to confirm — built on
    top of the same overlay primitive rather than as a separate component;
  - a **diff view with an addressable cursor** down to the hunk and the
    line, since staging depends on knowing exactly what the cursor is over;
  - a **persistent footer region**, always reserved space, independent of
    whatever the main view is currently showing.

None of this is scoped or estimated yet — this is the shape, not the plan.
Revisit once `apps/tui` stage 2 lands and `lib/term.src` exists.

## The dependency chain to an actual client, 2026-09-28

Checked before starting: `lib/term.src` already exists (raw mode, key
decoding, window size, one-write flush, read-with-timeout — everything
`apps/tui`'s own README implies is still pending under "being written
separately", which is now stale). What's actually missing splits into three
independent pieces, plus one that depends on all of them:

  - **The `io`/`net`/`term` errno gap** `docs/errors-decision.md` §4 records
    B leaving open — orthogonal to everything else here, done in parallel.
  - **`apps/git` has no write path at all.** No `.git/index` (read or
    write), no object writing (blobs, trees, commits), no ref writing, no
    working-tree diff. Status, staging and committing all need this, and
    none of it is TUI work — it's plumbing this design has been silently
    assuming exists.
  - **`apps/tui` stage 2** (styling layer) plus the primitives this design
    names (outline, jump list, which-key/transient, addressable diff
    cursor, footer) plus stage 3's actual remaining piece — the application
    loop joining the renderer to the *existing* `lib/term.src`, since
    `term.src` itself is done.
  - **The client itself** depends on both of the above and cannot start
    before they land — a second wave, not this one.

Deliberately not in this wave, each already a named, separate gap rather
than a silent omission: packfiles (`apps/git/README.md`'s own stage 2 —
neither `oro` nor this repo needs it, since neither has ever been packed);
`.gitignore`; the dashboard-shaped widgets from `apps/tui`'s original
stage-2 list (`Gauge`, `Sparkline`, `BarChart`, `Chart`) and mouse support,
none of which a keyboard-driven, document-shaped client needs.

## Starting the client, 2026-09-28

Checked before starting: no line-diff algorithm exists anywhere in this
codebase. `apps/tui`'s `DiffView` only renders a `Hunk`/`Line` sequence —
nothing computes one from two texts. That's load-bearing for hunk-level
staging, Magit's whole interaction model, so it's a foundational piece on
its own rather than something the client builds inline.

Split into two tracks:

  - **The diff algorithm** — a line-based edit script (Myers or similar),
    grouped into hunks with context, matching `tuidiffview`'s existing
    `Hunk`/`Line` shape, plus git's own binary-file heuristic (a NUL byte
    in the first several KB) so a binary diff reports as one rather than
    garbling. Self-contained; the client doesn't need it to get started.
  - **The client** — the document (outline sections for untracked,
    unstaged, staged, recent commits, populated from `status.status()` and
    the existing log/object read side), whole-file stage/unstage (already
    buildable from `index.src`/`object.src`), commit via `$EDITOR` for the
    message (matching real git's own fallback when `-m` isn't given, and
    sidestepping a dependency on `TextInput`, which `apps/tui` deferred for
    lack of a caller — this is that caller, later, not now), and the loop
    wiring the outline, the jump list, the footer and the which-key overlay
    together per the locked design above.

**Hunk-level diff display and staging wait on the diff-algorithm track**
and land as a follow-up once both exist — the client's first cut stages
whole files only, which is also where Magit itself starts before hunk
granularity.

**Also out of scope for this wave, each a separate, real gap:** push/pull
(no git network protocol — smart HTTP or SSH — exists in this codebase at
all); branch switching or checkout (no "write the working tree from a
tree" primitive exists); rebase or merge; packfiles (already tracked).

**Safety, same as the write-path work:** every test runs against a
disposable fixture built and destroyed by the test itself, with real `git`
as the oracle — never a real repository, including this one.

## Going remote, 2026-09-29: packfiles, then HTTP fetch, SSH held separately

Three things were named together as "make this tool complete" — packfiles,
smart HTTP, and SSH — and they are not the same size or the same kind of
risk, so they don't start together.

**Packfiles first, on their own.** Nothing else can produce anything usable
without them: a `clone`, a `fetch` and a `push` all traffic in packfiles,
and this already matters with no network involved at all — 6 of the 31
repositories on this project's own orogit server, and any repository a real
`git clone` ever produced, are unreadable by this tool today for exactly
this reason (`apps/git/README.md`'s own long-standing table). Fully
independent of HTTP or SSH, and fully oracle-testable against real `git` in
disposable fixtures, the same discipline as everything else here. Reading
only for this pass -- writing a packfile (needed for `push`) is a named,
separate follow-up, the same shape as the index/object/ref read-before-write
split the rest of `apps/git` already went through.

**Smart HTTP next, fetch/clone only, no push yet.** Builds on packfile
reading (a fetched pack is only useful once something can unpack it) but its
own wire protocol -- pkt-line framing, ref advertisement, want/have
negotiation -- is independently buildable against `lib/http.src`, which
already exists. A freshly fetched pack is unpacked straight into loose
objects using the object-writing path that already exists, rather than also
building a packfile indexer in the same pass -- keeping a fetched pack as a
pack (what real git does, for space and time on a large repository) is a
later optimisation, not a correctness requirement.

**SSH is held, not simply sequenced after.** It is not a bigger version of
the same task -- it needs a real cryptographic transport (key exchange, host
verification, a cipher, a MAC) and public-key auth *before* the git protocol
even starts, and a subtle bug there is a vulnerability, not a wrong diff:
categorically different from "byte-for-byte matches real git." It is also
the lowest-value of the three right now -- this project's own orogit
deployment is Gitea, which serves HTTP remotes fine, so smart HTTP alone
reaches GitHub, GitLab and this project's own server with no SSH at all. It
gets its own scoping pass, the same way `docs/concurrency-decision.md`
exists as its own document, rather than riding in as a third parallel track.

## What this deliberately does not decide yet

  - Exact keybindings (mnemonic, one key per base verb, is the only
    constraint fixed so far).
  - Colour/theme (waits on `apps/tui`'s styling layer).
  - Whether jump-list entries carry a status marker (modified, untracked,
    ahead/behind) or are plain labels — a real decision, deferred rather
    than defaulted.
  - Whether the jump list is always one flat list or nests when the document
    does (a file entry nested under the commit that touches it, say).
  - How undo/redo of staging works, if at all — Magit leans on Emacs' undo,
    which has no equivalent here.
