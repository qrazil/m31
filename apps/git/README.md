# `apps/git` — a terminal git client

Git plumbing and an interactive client, written entirely in this language:
SHA-1, zlib inflate, the loose-object format, refs, the index, working-tree
status, and both a read-only CLI (`git.src`) and a `tuiapp.Loop`-driven
interactive client (`gitui.src`) over `apps/tui`. Nothing here is a binding
to anything; the only C in the program is the runtime every program links.

```
cargo build
bash apps/git/test.sh                    # the built-in fixtures
bash apps/git/test.sh <repo> [<repo>…]   # those, and each repository named

./build.sh apps/git/git.src -o ourgit
./ourgit -log --max 5

./apps/git/build-gitui.sh -o ourgitui    # not build.sh -- see build-gitui.sh
./ourgitui                               # run from a repository's own top level
```

| file | what it is |
|---|---|
| `sha1.src` | SHA-1 (FIPS 180-4), incremental and one-shot |
| `zlib.src` | DEFLATE inflate (RFC 1951) and the zlib wrapper (RFC 1950), with Adler-32 |
| `object.src` | loose objects: the header, the SHA-1 check, trees, commits, tags |
| `refs.src` | HEAD, `refs/**`, `packed-refs`, symbolic refs, `rev-parse`'s DWIM |
| `repo.src` | where the files are: `.git` as a file, and a linked worktree's `commondir` |
| `git.src` | the read-only CLI |
| `index.src` | `.git/index`: read, write, a fresh entry from `fs.stat` |
| `status.src` | working-tree status: staged, unstaged, untracked |
| `hunks.src` | `lib/diff.src`'s edit script, grouped into `apps/tui/tuidiffview.Hunk`/`Line` with context |
| `gitlog.src` | the commit-history walk, shared by `git.src -log` and `gitui.src` |
| `gitclient.src` | the interactive client's state and logic (no top-level statements, so it is importable and testable) |
| `gitui.src` | the interactive client's thin driver: parses a path, runs `tuiapp.Loop` |
| `build-gitui.sh` | builds `gitui.src`: this compiler resolves every `import` against the entry file's own directory (`src/modules.rs`'s `load`), not a search path, so `gitui.src`'s `apps/tui/` dependencies are staged into a temporary directory at build time rather than copied into this one -- see the script's own header |
| `t_*.src` | test programs, each printing what a Python oracle prints, or asserting against its own expectations |
| `oracle_*.py` | the oracles: `hashlib`, `zlib`, and a from-scratch format reader |
| `pty_e2e.py` | drives `ourgitui` under a real pty against disposable fixtures, real `git` as the oracle |
| `compare.sh` | every command beside the real `git`, compared octet for octet |
| `test.sh` | all of the above (sources `test_write.sh`, `test_hunks.sh` and `test_gitui.sh`) |
| `FRICTION.md` | **the other half of this**: what the language made hard, and what it made easy |

## What works

Read-only: `cat-file --type/--size/--pretty`, `ls-tree`, `log [--max N]
[<rev>]`, `rev-parse` and `refs`, on a working tree, a bare repository or a
linked worktree.

The write path: `.git/index` (read and write), loose object writing (blob,
tree, commit), ref writing (`update`, `update_symbolic`, compare-and-swap),
and working-tree status, all checked against real git in disposable
fixtures (`test_write.sh`).

The interactive client (`gitui.src`, `apps/git/design.md`'s locked design):
a collapsible outline of untracked files, unstaged changes, staged changes
and recent commits; whole-file staging and unstaging (`s`/`u`); a commit,
via a message file at `COMMIT_EDITMSG` read back and refused if empty
(`c` opens a which-key overlay: `e` launches `$EDITOR` (`vi` if unset) on
the message file, falling back to writing the template and naming the path
if no editor can be launched at all; `f` finishes; `a` aborts -- see
`gitclient.src`'s own header, "launching `$EDITOR`, and the terminal handoff
that takes", for how the terminal is handed to the editor and back); a
persistent footer of the base commands; and a synced jump list toggled with
`J`. Hunk-level diff display and staging, push/pull, checkout and rebase are
each a named, deliberate gap in `apps/git/design.md`, not an oversight here.

Everything is checked against something that is not this program:

| check | oracle | scale |
|---|---|---|
| SHA-1 | Python `hashlib` | empty, `abc`, the 448-bit vector, a million `a` whole and in seven chunk sizes, every length 0…200, 8 MiB of random octets — 213 digests |
| inflate | Python `zlib` | 29 streams: stored, fixed, dynamic, multi-block, overlapping matches, incompressible, 2.2 MB, plus 15 corrupt inputs that must all be refused |
| objects | a from-scratch Python reader | every loose object in each repository named: 95 828 canonical lines over 8 801 objects across the four used here, with paths, names, emails and messages compared as hex or digests so no text decoding is in the loop |
| commands | the real `git` | 1 388 invocations compared byte for byte across those four repositories |

Both `log` and `cat-file -p` reproduce git's output exactly, which is less
obvious than it sounds — see `git.src` on tab expansion in commit messages,
on trailing-whitespace trimming, and on C-style path quoting.

Throughput, `cc -O2` on x86-64, best of several runs on a busy machine:
SHA-1 **38–45 MB/s**, inflate **71–102 MB/s** of output, and **10 MB/s** of
object content end to end (open, inflate, verify the SHA-1, parse).
`FRICTION.md` §5 takes the SHA-1 figure apart, because it is a language
datapoint and not a git one.

## What a real repository needs that this cannot do

**Packfiles.** That is the whole of it, and how much it costs depends
entirely on how the repository got there.

Counted by walking from every ref with loose objects alone, at the time of
writing:

| repository | objects reachable from its refs | loose | in packs |
|---|---|---|---|
| `workspace/oro` | 3 847 | **100%** | 0 |
| `workspace/lang` (this one) | 4 050 | **100%** | 0 |
| `apps/orogit`, 25 of 31 repos | 276 | **100%** | 0 |
| `apps/orogit`, the other 6 | — | **0%** | everything |
| `git clone --no-local` of `oro` | 0 of the 17 the walk asked for | **0%** | everything |
| `git clone` of `oro` over the filesystem | 3 847 | **100%** | 0 (hardlinked) |

So the answer is not "mostly unusable" and it is not "fine", it is a sharp
split:

  - **A repository that was written to locally is entirely loose.** Neither
    `oro` nor `lang` has ever been packed — `git gc` runs on its own schedule
    and neither has hit it — so stage 1 reads all of both, completely, and
    `log` walks their whole histories. Twenty-five of the thirty-one
    repositories on the orogit server are the same, because they were pushed
    into and never repacked.
  - **A repository that arrived over a network is entirely packed.** A real
    `git clone` writes one packfile and not one loose object, so stage 1 sees
    a HEAD pointing at an object that is not there and can do *nothing at
    all* — not one commit, not one blob. The six orogit repositories that
    were imported from Gitea are in exactly this state.

The line is not "old objects are packed and new ones are loose": it is
"objects this machine wrote are loose, objects that arrived in a pack are
packed, until something repacks". Stage 1 is therefore a usable tool on a
repository you have been committing to and a useless one on a fresh clone,
with very little in between. `apps/git/test.sh` is pointed at repositories of
the first kind on purpose, and the fixture it builds for itself is one.

Reading a packfile needs the `.idx` fanout, the pack's own object encoding,
and — the real work — `OBJ_OFS_DELTA` and `OBJ_REF_DELTA`, which are a
copy/insert delta format applied on top of a base object that may itself be a
delta. It also wants the inflate in `zlib.src` to run from a mid-file offset
without being handed the rest of the file, which is a change to that module's
shape rather than an addition to it. That is stage 2.

## Smaller things this does not do

  - **The revision grammar.** `HEAD~3`, `main^2`, `v1^{tree}`, `@{upstream}`,
    `:/message`. `rev-parse` takes a ref, a full object name or an
    unambiguous prefix. `refs.peel` follows an annotated tag to its commit,
    because `log v1` needs it.
  - **Configuration.** No `.git/config` is read at all, so no `.mailmap`, no
    `core.abbrev` (seven digits, fixed), no `log.decorate`, no
    `core.quotePath` (on, as it is by default), no colour, no pager, no
    `i18n.logOutputEncoding`.
  - **Hunk-level diff against the working tree.** `status.src` reports
    whole-file staged/unstaged/untracked; `hunks.src` can compute a
    line-level diff between any two texts, but nothing yet wires the two
    together into a `diff`-shaped view of the working tree, or `ls-files`.
  - **Writing beyond what `gitui.src` does.** The write path (index,
    objects, refs) is real and checked against real `git`, but there is no
    standalone write-side CLI — only the interactive client and the tests
    exercise it today.
  - **The commit graph, bitmaps, alternates, replace refs, shallow clones,
    submodules, SHA-256 repositories.** All ignored; a SHA-256 repository
    would be refused by the length check rather than misread.
  - **`git log`'s other orderings.** The walk is git's date-ordered queue.
    `--topo-order`, `--reverse`, path limiting and `--graph` are not there.
