#!/usr/bin/env bash
# Every command of `apps/git/git.src`, beside the real `git`, on a real
# repository, compared octet for octet.
#
#   bash apps/git/compare.sh <our-binary> <repo> [workdir]
#
# `git` is only ever READ from here: `cat-file`, `ls-tree`, `log`,
# `rev-parse`, `show-ref`. Nothing in this script writes to a repository.
#
# Two flags are pinned on git's side, and `git.src`'s header says why:
# `core.abbrev=7`, because git derives an abbreviation length from how many
# objects a repository has, and `log.decorate=false`, because a repository
# may have turned decoration on in its config and this program has no config
# at all.
set -uo pipefail

OURS=$1
REPO=$2
WORK=${3:-$(mktemp -d)}
mkdir -p "$WORK"
G=(git -C "$REPO" -c core.abbrev=7 -c log.decorate=false --no-pager)
bad=0
n=0

check() { # check <label> <our-output-file> <git-output-file>
    n=$((n + 1))
    if ! cmp -s "$2" "$3"; then
        printf '\033[31mdiffer\033[0m %s\n' "$1"
        diff <(head -40 "$2") <(head -40 "$3") | head -14 | sed 's/^/       /'
        bad=$((bad + 1))
    fi
}

ours() { ( cd "$REPO" && "$OURS" --git-dir "$(cd "$REPO" && git rev-parse --absolute-git-dir)" "$@" ); }

# --- rev-parse ----------------------------------------------------------------

"${G[@]}" show-ref | awk '{print $2}' | sort > "$WORK/refnames"
while read -r name; do
    ours -rev-parse "$name" > "$WORK/a" 2>&1
    "${G[@]}" rev-parse "$name" > "$WORK/b" 2>&1
    check "rev-parse $name" "$WORK/a" "$WORK/b"
done < "$WORK/refnames"

for name in HEAD; do
    ours -rev-parse "$name" > "$WORK/a" 2>&1
    "${G[@]}" rev-parse "$name" > "$WORK/b" 2>&1
    check "rev-parse $name" "$WORK/a" "$WORK/b"
done

# A short name, and the shortest unambiguous prefix git itself would print.
head_id=$("${G[@]}" rev-parse HEAD)
for len in 7 10 40; do
    short=${head_id:0:$len}
    ours -rev-parse "$short" > "$WORK/a" 2>&1
    "${G[@]}" rev-parse "$short" > "$WORK/b" 2>&1
    check "rev-parse ${len}-digit prefix" "$WORK/a" "$WORK/b"
done

# --- show-ref -----------------------------------------------------------------

ours -refs > "$WORK/a" 2>&1
"${G[@]}" show-ref | sort -k2 > "$WORK/b"
check "refs" "$WORK/a" "$WORK/b"

# --- cat-file and ls-tree, over a sample of the store -------------------------
#
# Every loose object if there are few, and a spread of them if there are
# thousands: the first, the last and every Nth, so the sample is the same on
# every run and covers all four types.

common=$(cd "$REPO" && git rev-parse --path-format=absolute --git-common-dir)
python3 - "$common" "$WORK/sample" <<'PY'
import os, sys
od = os.path.join(sys.argv[1], "objects")
names = []
for two in sorted(os.listdir(od)):
    if len(two) == 2 and all(c in "0123456789abcdef" for c in two):
        for rest in sorted(os.listdir(os.path.join(od, two))):
            if len(rest) == 38:
                names.append(two + rest)
step = max(1, len(names) // 120)
with open(sys.argv[2], "w") as f:
    f.write("\n".join(names[::step]))
    if names:
        f.write("\n" + names[-1] + "\n")
PY

while read -r oid; do
    [ -n "$oid" ] || continue
    ours -cat-file --type "$oid" > "$WORK/a" 2>&1
    "${G[@]}" cat-file -t "$oid" > "$WORK/b" 2>&1
    check "cat-file -t $oid" "$WORK/a" "$WORK/b"

    ours -cat-file --size "$oid" > "$WORK/a" 2>&1
    "${G[@]}" cat-file -s "$oid" > "$WORK/b" 2>&1
    check "cat-file -s $oid" "$WORK/a" "$WORK/b"

    ours -cat-file "$oid" > "$WORK/a" 2>&1
    "${G[@]}" cat-file -p "$oid" > "$WORK/b" 2>&1
    check "cat-file -p $oid" "$WORK/a" "$WORK/b"

    if [ "$("${G[@]}" cat-file -t "$oid")" = tree ]; then
        ours -ls-tree "$oid" > "$WORK/a" 2>&1
        "${G[@]}" ls-tree "$oid" > "$WORK/b" 2>&1
        check "ls-tree $oid" "$WORK/a" "$WORK/b"
    fi
done < "$WORK/sample"

# `ls-tree HEAD` -- a commit, not a tree, which git accepts and so must this.
ours -ls-tree HEAD > "$WORK/a" 2>&1
"${G[@]}" ls-tree HEAD > "$WORK/b" 2>&1
check "ls-tree HEAD" "$WORK/a" "$WORK/b"

# --- log ----------------------------------------------------------------------

for max in 1 2 5 50 400; do
    ours -log --max "$max" > "$WORK/a" 2>&1
    "${G[@]}" log --max-count="$max" > "$WORK/b" 2>&1
    check "log --max $max" "$WORK/a" "$WORK/b"
done

ours -log > "$WORK/a" 2>&1
"${G[@]}" log > "$WORK/b" 2>&1
check "log (whole history)" "$WORK/a" "$WORK/b"

# From every branch tip as well, so a merge is reached from more than one side.
while read -r name; do
    case $name in refs/heads/*) ;; *) continue ;; esac
    ours -log --max 30 "$name" > "$WORK/a" 2>&1
    "${G[@]}" log --max-count=30 "$name" > "$WORK/b" 2>&1
    check "log $name" "$WORK/a" "$WORK/b"
done < "$WORK/refnames"

# --- refusals -----------------------------------------------------------------
#
# The MESSAGE is this program's own, so only the status is compared: a name
# that is not there must fail, and must not trap.

for junk in nosuchref 0000000000000000000000000000000000000000 zz ../../etc/passwd; do
    n=$((n + 1))
    if ours -rev-parse "$junk" >/dev/null 2>&1; then
        printf '\033[31mdiffer\033[0m rev-parse %s succeeded and should not have\n' "$junk"
        bad=$((bad + 1))
    fi
done

if [ $bad -eq 0 ]; then
    echo "     $n comparisons against git, all identical"
fi
exit $bad
