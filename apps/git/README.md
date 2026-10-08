# `apps/git` — a terminal git client

Git plumbing and an interactive client, written entirely in this language:
SHA-1, zlib inflate, the loose-object format, refs, the index, working-tree
status, and both a read-only CLI (`git.m31`) and a `tuiapp.Loop`-driven
interactive client (`gitui.m31`) over `apps/tui`. Nothing here is a binding
to anything; the only C in the program is the runtime every program links.

```
cargo build
bash apps/git/test.sh                    # the built-in fixtures
bash apps/git/test.sh <repo> [<repo>…]   # those, and each repository named

./build.sh apps/git/git.m31 -o ourgit
./ourgit -log --max 5

./apps/git/build-gitui.sh -o ourgitui    # not build.sh -- see build-gitui.sh
./ourgitui                               # run from a repository's own top level
```

| file | what it is |
|---|---|
| `sha1.m31` | SHA-1 (FIPS 180-4), incremental and one-shot |
| `zlib.m31` | DEFLATE inflate (RFC 1951) and the zlib wrapper (RFC 1950), with Adler-32, from a mid-file offset as well as from the front |
| `object.m31` | the object store: the header, the SHA-1 check, trees, commits, tags -- loose or, via `pack.m31`, packed, through the one `read` |
| `pack.m31` | packfiles: `.idx` v2, the pack's own object encoding, `OBJ_OFS_DELTA`/`OBJ_REF_DELTA` delta-chain resolution |
| `refs.m31` | HEAD, `refs/**`, `packed-refs`, symbolic refs, `rev-parse`'s DWIM |
| `repo.m31` | where the files are: `.git` as a file, and a linked worktree's `commondir` |
| `git.m31` | the read-only CLI |
| `index.m31` | `.git/index`: read, write, a fresh entry from `fs.stat` |
| `status.m31` | working-tree status: staged, unstaged, untracked |
| `hunks.m31` | `lib/diff.m31`'s edit script, grouped into `apps/tui/tuidiffview.Hunk`/`Line` with context |
| `gitlog.m31` | the commit-history walk, shared by `git.m31 -log` and `gitui.m31` |
| `gitclient.m31` | the interactive client's state and logic (no top-level statements, so it is importable and testable) |
| `gitui.m31` | the interactive client's thin driver: parses a path, runs `tuiapp.Loop` |
| `httpfetch.m31` | git's smart-HTTP protocol, v0 fetch/clone only: pkt-line framing, the ref advertisement, want/have negotiation, side-band-64k demultiplexing, and pack checksum verification, over `lib/http.m31` |
| `build-gitui.sh` | builds `gitui.m31`: this compiler resolves every `import` against the entry file's own directory (`src/modules.rs`'s `load`), not a search path, so `gitui.m31`'s `apps/tui/` dependencies are staged into a temporary directory at build time rather than copied into this one -- see the script's own header |
| `t_*.m31` | test programs, each printing what a Python oracle prints, or asserting against its own expectations |
| `oracle_*.py` | the oracles: `hashlib`, `zlib`, and a from-scratch format reader |
| `pty_e2e.py` | drives `ourgitui` under a real pty against disposable fixtures, real `git` as the oracle |
| `compare.sh` | every command beside the real `git`, compared octet for octet |
| `test.sh` | all of the above (sources `test_write.sh`, `test_gitignore.sh`, `test_hunks.sh`, `test_gitui.sh` and `test_httpfetch.sh`) |
| `FRICTION.md` | **the other half of this**: what the language made hard, and what it made easy |

## What works

Read-only: `cat-file --type/--size/--pretty`, `ls-tree`, `log [--max N]
[<rev>]`, `rev-parse` and `refs`, on a working tree, a bare repository or a
linked worktree -- **loose or packed**, transparently: `object.read` checks
the loose store first and `pack.m31`'s `.idx`/`.pack` reading second, so
every reader above it (this CLI, the interactive client's status/diff/commit
reading, `rev-parse`'s short-hash resolution) works the same way on a
repository a real `git clone` produced as on one this program has only ever
committed to itself. See "Packfiles", below, for what that took.

The write path: `.git/index` (read and write), loose object writing (blob,
tree, commit), ref writing (`update`, `update_symbolic`, compare-and-swap),
and working-tree status, all checked against real git in disposable
fixtures (`test_write.sh`).

The interactive client (`gitui.m31`, `apps/git/design.md`'s locked design):
a collapsible outline of untracked files, unstaged changes, staged changes
and recent commits; whole-file staging and unstaging (`s`/`u`); a commit,
via a message file at `COMMIT_EDITMSG` read back and refused if empty
(`c` opens a which-key overlay: `e` launches `$EDITOR` (`vi` if unset) on
the message file, falling back to writing the template and naming the path
if no editor can be launched at all; `f` finishes; `a` aborts -- see
`gitclient.m31`'s own header, "launching `$EDITOR`, and the terminal handoff
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
obvious than it sounds — see `git.m31` on tab expansion in commit messages,
on trailing-whitespace trimming, and on C-style path quoting.

Throughput, `cc -O2` on x86-64, best of several runs on a busy machine:
SHA-1 **38–45 MB/s**, inflate **71–102 MB/s** of output, and **10 MB/s** of
object content end to end (open, inflate, verify the SHA-1, parse).
`FRICTION.md` §5 takes the SHA-1 figure apart, because it is a language
datapoint and not a git one.

## Packfiles

This used to be the whole of what a real repository needed that this could
not do, and how much it cost depended entirely on how the repository got
there -- both still true of the numbers below, which describe the
repositories this project actually has lying around, not this program's own
ability to read them any more. `apps/git/design.md`'s "Going remote" names
packfiles as the first, independent piece of that larger plan (reading only
-- writing one, for `push`, is later, separate work), and it has landed:
`pack.m31` reads the `.idx` (format v2; v1 is refused, not guessed at, since
nothing still writes it), the packfile's own variable-length object headers,
and resolves an `OBJ_OFS_DELTA`/`OBJ_REF_DELTA` chain of either kind (or a
mix) down to a real commit/tree/blob/tag, iteratively rather than
recursively so a real chain cannot blow the stack. `zlib.m31` grew the
matching primitive, `inflate_at`/`decompress_at`: decompress one DEFLATE or
zlib stream starting at an offset inside a much larger buffer, and report how
many input octets it consumed, so a packfile's objects are read one at a time
without ever copying the pack to get to the next one.

`object.read` -- and so `log`, `cat-file`, `ls-tree`, `rev-parse`'s short
names and the interactive client's own reading -- checks the loose store
first and a repository's packs second, with nothing above `object.m31`
changed to make that true. Checked the same way everything else here is:
`apps/git/oracle_object.py`'s from-scratch reader grew its own independent
`.idx`/pack/delta implementation (Python's `zlib.decompressobj`, fed from an
offset, stands in for `inflate_at`) and walks every object of a fixture
`git repack -ad` packs into real `OBJ_OFS_DELTA` chains and, separately,
`git pack-objects --no-delta-base-offset` packs into real `OBJ_REF_DELTA`
ones instead; `apps/git/compare.sh`'s full command comparison against real
`git` runs against both packed fixtures exactly as it runs against the loose
one; and `apps/git/oracle_inflate_at.py` checks the mid-offset codec on its
own, isolated from the packfile format around it.

Counted by walking from every ref with loose objects alone, at the time of
writing (a measurement of these repositories' own history, unrelated to
what this program can now read):

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
packed, until something repacks". That split used to mean this program was a
usable tool on a repository you have been committing to and a useless one on
a fresh clone, with very little in between; `pack.m31` is what closes that
gap. `apps/git/test.sh`'s own built-in fixture is still an all-loose one on
purpose (a repository this program itself commits to, same as `oro` and
`lang`), and it now builds two packed fixtures alongside it -- one repacked
with real `OBJ_OFS_DELTA` chains, one with real `OBJ_REF_DELTA` ones -- so
every command comparison in this file's own test suite runs against a packed
repository as well as a loose one.

Packfile *writing* remains out of scope here -- it is what `push` needs, and
`apps/git/design.md`'s "Going remote" holds it as later, separate work, the
same read-before-write split the index/object/ref write path already went
through.

## Smart-HTTP fetch (`httpfetch.m31`): a verified pack on disk, and no further

`httpfetch.m31` speaks enough of git's smart-HTTP protocol -- v0 only, over
`lib/http.m31` -- to fetch a real packfile from a real server: the ref
advertisement (`GET .../info/refs?service=git-upload-pack`, refusing a
"dumb HTTP" answer rather than misparsing it), want/have negotiation (a
`clone`'s empty `have` list and a `fetch`'s non-empty one are both tested),
side-band-64k demultiplexing, and the pack's own trailing SHA-1 checked
before anything is written to disk. Tested against a real `git http-backend`
run as a genuine CGI script (`test_httpfetch.sh`), not a hand-rolled
stand-in: the ref advertisement matches `git ls-remote` byte for byte, a
full clone's pack is byte-for-byte the server's own (repacked, so genuinely
delta-compressed) pack, a `fetch` against a repository that already has
history gets a visibly smaller pack, and both are accepted by real
`git index-pack --stdin`. A truncated or corrupted transfer -- checked both
as hand-corrupted bytes with no server involved, and as a genuinely
truncated response over the wire -- is refused cleanly, never written.

**This is exactly as far as it goes: verified bytes on disk, not a usable
repository.** `httpfetch.m31` does not unpack anything it fetches -- no
`.idx`, no delta resolution, no loose objects written -- because doing that
needs the same `OBJ_OFS_DELTA`/`OBJ_REF_DELTA` machinery stage 2 (above) is
for, and duplicating an incomplete piece of that here was explicitly out of
scope. Turning a fetched pack into a repository this program can `log` or
`cat-file` is therefore stage 2's own follow-up, not a gap in this file.
Push, SSH and protocol v2 are named, separate gaps, not oversights: v0 is
universally supported as a fallback even where v2 is preferred, and
`https://` URLs work as they are: `lib/https.m31` (over `lib/tls.m31`) verifies the server's
certificate against the system roots before sending a byte, and there is no
way to turn that off.

## Smaller things this does not do

  - **The revision grammar.** `HEAD~3`, `main^2`, `v1^{tree}`, `@{upstream}`,
    `:/message`. `rev-parse` takes a ref, a full object name or an
    unambiguous prefix. `refs.peel` follows an annotated tag to its commit,
    because `log v1` needs it.
  - **Configuration.** No `.git/config` is read at all, so no `.mailmap`, no
    `core.abbrev` (seven digits, fixed), no `log.decorate`, no
    `core.quotePath` (on, as it is by default), no colour, no pager, no
    `i18n.logOutputEncoding`.
  - **Hunk-level diff against the working tree.** `status.m31` reports
    whole-file staged/unstaged/untracked; `hunks.m31` can compute a
    line-level diff between any two texts, but nothing yet wires the two
    together into a `diff`-shaped view of the working tree, or `ls-files`.
  - **Writing beyond what `gitui.m31` does.** The write path (index,
    objects, refs) is real and checked against real `git`, but there is no
    standalone write-side CLI — only the interactive client and the tests
    exercise it today.
  - **The commit graph, bitmaps, alternates, replace refs, shallow clones,
    submodules, SHA-256 repositories.** All ignored; a SHA-256 repository
    would be refused by the length check rather than misread.
  - **`git log`'s other orderings.** The walk is git's date-ordered queue.
    `--topo-order`, `--reverse`, path limiting and `--graph` are not there.
