#!/usr/bin/env bash
# Every check for apps/git, against an oracle that is not this program.
#
#   bash apps/git/test.sh               the built-in fixtures
#   bash apps/git/test.sh <repo> ...    those, and each repository named
#
# The oracles are Python's `hashlib` and `zlib` for the two codecs, a
# from-scratch Python reader of the loose-object format for the layer above
# them, and the real `git` for the commands. Nothing here compares this
# program with itself.
#
# `git` is used READ-ONLY throughout, except inside the scratch fixture
# repositories this script builds under its own temporary directory.
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."

LANGC=./target/debug/langc
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

build() {
    local name=$1
    if ! "$LANGC" --emit-c "apps/git/$name.src" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" runtime/rt.c 2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

# --- house style ---------------------------------------------------------------
#
# `gates.sh` formats and re-checks `lib/`, `corpus/` and `examples/` and does
# not look at `apps/`, so this source would drift out of the house layout with
# nothing to notice. The same check, here, over this directory.

if out=$(for f in apps/git/*.src; do "$LANGC" fmt --check "$f" || echo "$f"; done 2>&1) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: langc fmt apps/git/<file>.src)" "$out"
fi

# --- SHA-1 --------------------------------------------------------------------

python3 - "$WORK/random.bin" <<'PY'
import random, sys
random.seed(1234)
open(sys.argv[1], "wb").write(random.randbytes(8 * 1024 * 1024))
PY

if build t_sha1; then
    "$WORK/t_sha1" "$WORK/random.bin" >"$WORK/sha1.got" 2>"$WORK/sha1.time"
    python3 apps/git/oracle_sha1.py "$WORK/random.bin" >"$WORK/sha1.want"
    if cmp -s "$WORK/sha1.got" "$WORK/sha1.want"; then
        note "sha1: $(wc -l <"$WORK/sha1.got") digests match hashlib"
    else
        bad "sha1" "$(diff "$WORK/sha1.got" "$WORK/sha1.want" | head -8)"
    fi
    sed 's/^/     /' "$WORK/sha1.time"
fi

# --- inflate ------------------------------------------------------------------

if build t_inflate; then
    python3 apps/git/oracle_inflate.py "$WORK/z" >"$WORK/z.want"
    "$WORK/t_inflate" "$WORK/z" >"$WORK/z.got" 2>"$WORK/z.time"
    if cmp -s "$WORK/z.got" "$WORK/z.want"; then
        note "inflate: $(wc -l <"$WORK/z.got") streams match zlib, refusals included"
    else
        bad "inflate" "$(diff "$WORK/z.got" "$WORK/z.want" | head -12)"
    fi
    grep '^inflate:' "$WORK/z.time" | sed 's/^/     /'
fi

# --- the fixture repository ----------------------------------------------------
#
# Everything the format can do that this program has to get right, in one
# history: an empty file, a hundred kilobytes of incompressible data, a
# filename that is not UTF-8, a filename with a tab and one with a quote, an
# executable, a symlink, a subdirectory, two commits in the same second, a
# merge with two parents, an author whose name is not ASCII, zones east and
# west and `-0000`, an annotated tag and a lightweight one, a symbolic ref, a
# detached-looking branch, packed refs, a tab in a commit message, and a
# message with blank lines at both ends.

fixture="$WORK/fixture"
mkdir -p "$fixture"
(
    set -e
    cd "$fixture"
    git init -q -b master .
    git config user.email t@example.com
    git config user.name 'A U Thor'
    mkdir -p sub/deeper
    printf 'hello\n' >a.txt
    : >empty.txt
    head -c 120000 /dev/urandom >sub/blob.bin
    printf 'x\ty\n' >"$(printf 'sub/l\xe9gume.txt')"
    printf 'q\n' >"$(printf 'sub/we\tird.txt')"
    printf 'q\n' >'sub/quo"te.txt'
    printf 'q\n' >'sub/back\slash.txt'
    printf 'exe\n' >run.sh
    chmod +x run.sh
    ln -s a.txt link.txt
    printf 'deep\n' >sub/deeper/d.txt
    git add -A
    printf 'first\n\na body, with a blank line above it\nand a tab:\there\n' >"$WORK/msg1"
    GIT_AUTHOR_DATE='1700000000 +0530' GIT_COMMITTER_DATE='1700000001 -0800' \
        git -c 'user.name=Ünïcødé Näme' -c user.email='u@exämple.test' \
        commit -q -F "$WORK/msg1"
    printf 'second\n' >>a.txt
    git add -A
    GIT_AUTHOR_DATE='1700000100 -0000' GIT_COMMITTER_DATE='1700000101 +0000' \
        git commit -qm second
    printf 'third\n' >>a.txt
    git add -A
    GIT_AUTHOR_DATE='1600000000 +0100' GIT_COMMITTER_DATE='1750000000 +0100' \
        git commit -qm 'authored long before it was committed'
    # Two commits whose committer timestamps are equal, so the walk's
    # tie-breaking is exercised rather than assumed.
    git checkout -q -b side HEAD~2
    printf 'branchy\n' >b.txt
    git add -A
    GIT_AUTHOR_DATE='1700000200 +0200' GIT_COMMITTER_DATE='1700000300 +0000' \
        git commit -qm 'on a side branch'
    git checkout -q master
    printf 'more\n' >c.txt
    git add -A
    GIT_AUTHOR_DATE='1700000200 +0200' GIT_COMMITTER_DATE='1700000300 +0000' \
        git commit -qm 'the same second as the side branch'
    GIT_AUTHOR_DATE='1700000400 +0000' GIT_COMMITTER_DATE='1700000400 +0000' \
        git merge -q --no-ff side -m 'a merge, with two parents'
    printf '\n\ntrailing blanks below\n\n\n' >"$WORK/msg2"
    git commit -q --allow-empty --cleanup=verbatim -F "$WORK/msg2" \
        --date='1700000500 +0000'
    GIT_COMMITTER_DATE='1700000600 +0000' git tag -a v1 -m 'an annotated tag
with a body'
    git tag lightweight HEAD~2
    git update-ref refs/heads/detachable HEAD~1
    git symbolic-ref refs/heads/aliased refs/heads/side
    git pack-refs --all
    # A loose ref after packing, so the loose-wins-over-packed rule is live.
    git update-ref refs/heads/loose-one HEAD~1
) >"$WORK/fixture.log" 2>&1 || { bad "fixture repository" "$(tail -5 "$WORK/fixture.log")"; }
[ -d "$fixture/.git" ] && note "fixture repository built"

repos=("$fixture" "$@")

# --- objects, against the Python reader ----------------------------------------

if build t_object; then
    for repo in "${repos[@]}"; do
        common=$(cd "$repo" && git rev-parse --path-format=absolute --git-common-dir)
        "$WORK/t_object" "$common" >"$WORK/obj.got" 2>"$WORK/obj.err"
        python3 apps/git/oracle_object.py "$common" >"$WORK/obj.want" 2>"$WORK/obj.oracle"
        if cmp -s "$WORK/obj.got" "$WORK/obj.want"; then
            note "objects: $(wc -l <"$WORK/obj.got") lines match the Python reader on $repo"
        else
            bad "objects on $repo" "$(diff "$WORK/obj.got" "$WORK/obj.want" | head -12)" \
                "$(head -3 "$WORK/obj.oracle")"
        fi
        sed 's/^/     /' "$WORK/obj.err"
    done
fi

# --- the commands, against the real git ----------------------------------------

if build git; then
    for repo in "${repos[@]}"; do
        if out=$(bash apps/git/compare.sh "$WORK/git" "$repo" "$WORK/cmp" 2>&1); then
            note "commands on $repo: ${out## }"
        else
            bad "commands on $repo" "$out"
        fi
    done
fi

# --- the write path: .git/index, loose objects, refs, status -------------------
#
# `test_write.sh` shares this script's shell, `$WORK`, `$LANGC`, `build`,
# `note`/`bad` and the `pass`/`fail` counters, and builds its own disposable
# fixtures under `$WORK` -- see its own header.

source apps/git/test_write.sh

# --- the line diff, against real diff -u and git diff --------------------------

source apps/git/test_hunks.sh

# --- the interactive client: unit, oracle and pty-driven end-to-end --------
#
# `test_gitui.sh` shares this script's shell the same way `test_write.sh`
# does -- see its own header.

source apps/git/test_gitui.sh

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/git checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/git checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
