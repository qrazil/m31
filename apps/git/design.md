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
  - **One optional side panel**, a directory-tree view, and it **toggles** —
    shown or hidden by one key, never a fixed pane competing with the main
    view for width. It is a way *in* to the main view (pick a path, jump the
    document to it), not a second thing to keep in sync with it.
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

  - a **collapsible outline / tree widget** for the primary document (nested
    sections, expand and collapse, one of which is the toggleable directory
    panel);
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

## What this deliberately does not decide yet

  - Exact keybindings (mnemonic, one key per base verb, is the only
    constraint fixed so far).
  - Colour/theme (waits on `apps/tui`'s styling layer).
  - Whether the directory panel can also show status per file (a coloured
    marker for modified/untracked) or is a plain tree — a real decision,
    deferred rather than defaulted.
  - How undo/redo of staging works, if at all — Magit leans on Emacs' undo,
    which has no equivalent here.
