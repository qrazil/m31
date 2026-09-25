# `apps/git` — a terminal git client, stage 1

Read-only git plumbing, written entirely in this language: SHA-1, zlib
inflate, the loose-object format, refs, and five commands over `lib/args`.
Nothing here is a binding to anything; the only C in the program is the
runtime every program links.

```
cargo build
bash apps/git/test.sh                    # the built-in fixtures
bash apps/git/test.sh <repo> [<repo>…]   # those, and each repository named

./build.sh apps/git/git.src -o ourgit
./ourgit -log --max 5
```

| file | what it is |
|---|---|
| `sha1.src` | SHA-1 (FIPS 180-4), incremental and one-shot |
| `zlib.src` | DEFLATE inflate (RFC 1951) and the zlib wrapper (RFC 1950), with Adler-32 |
| `object.src` | loose objects: the header, the SHA-1 check, trees, commits, tags |
| `refs.src` | HEAD, `refs/**`, `packed-refs`, symbolic refs, `rev-parse`'s DWIM |
| `repo.src` | where the files are: `.git` as a file, and a linked worktree's `commondir` |
| `git.src` | the CLI |
| `t_*.src` | test programs, each printing what a Python oracle prints |
| `oracle_*.py` | the oracles: `hashlib`, `zlib`, and a from-scratch format reader |
| `compare.sh` | every command beside the real `git`, compared octet for octet |
| `test.sh` | all of the above |
| `FRICTION.md` | **the other half of this**: what the language made hard, and what it made easy |

## What works

`cat-file --type/--size/--pretty`, `ls-tree`, `log [--max N] [<rev>]`,
`rev-parse` and `refs`, on a working tree, a bare repository or a linked
worktree.

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
  - **The index.** `.git/index` is never opened, so no `status`, no `diff`
    against the working tree, no `ls-files`.
  - **Writing.** Nothing here creates or changes an object, a ref or a file
    in a repository.
  - **The commit graph, bitmaps, alternates, replace refs, shallow clones,
    submodules, SHA-256 repositories.** All ignored; a SHA-256 repository
    would be refused by the length check rather than misread.
  - **`git log`'s other orderings.** The walk is git's date-ordered queue.
    `--topo-order`, `--reverse`, path limiting and `--graph` are not there.
