#!/usr/bin/env bash
# `hunks.src`: the generic edit script from `lib/diff.src`, grouped into
# `apps/tui/tuidiffview.Hunk`/`Line` with context -- checked against real
# `diff -u` (hunk boundaries and line classification) and real `git diff`
# (the binary-file case, since git's own NUL heuristic is what
# `lib/diff.src`'s `is_binary` matches).
#
# Sourced from `test.sh`, on the same terms as `test_write.sh`: no `set`, no
# `cd`, no `trap` here, and `$WORK`, `$LANGC`, `note`/`bad` and the
# `pass`/`fail` counters are all `test.sh`'s.
#
# `hunks.src` imports `apps/tui/tuidiffview`, which this program's own
# module loader resolves relative to the ENTRY file's directory only
# (docs/modules-decision.md §1: one flat namespace, no search path) -- so a
# program built straight out of `apps/git` can never see `apps/tui`'s files.
# `build_tui` below is the version of `test.sh`'s own `build` that stages
# both directories' sources into one place first, the same fix
# `gates.sh`'s formatter check already uses for a module and the sibling
# files it needs (`cp "$(dirname "$f")"/*.$LANG_EXT`).

build_tui() {
    local name=$1
    local stage="$WORK/tui-stage"
    mkdir -p "$stage"
    cp apps/tui/tuibuf.src apps/tui/tuigeom.src apps/tui/tuiscroll.src \
        apps/tui/tuistyle.src apps/tui/tuitext.src apps/tui/tuidiffview.src \
        apps/git/hunks.src "apps/git/$name.src" "$stage/"
    if ! "$LANGC" --emit-c "$stage/$name.src" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name (staged with apps/tui)" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" \
           runtime/rt.c runtime/scheduler.c runtime/reactor.c "$RT_CTX_ASM" \
           2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

if build_tui t_hunks; then
    t_hunks="$WORK/t_hunks"
    hdir="$WORK/hunks-fixtures"
    mkdir -p "$hdir"

    # Each pair is compared two ways: `t_hunks`'s own output against real
    # `diff -u`'s body (its `---`/`+++` file-header lines stripped, and any
    # `\ No newline at end of file` marker dropped -- `tuidiffview.Line` has
    # no way to carry that annotation, only the text of a line, so this is
    # the one place the two are allowed to differ; the ROUND-TRIP property in
    # `corpus/modules/stdlib-diff` is what actually proves the no-trailing-
    # newline case is handled correctly).
    oracle_case() {
        local name=$1 old=$2 new=$3
        local got want
        got=$("$t_hunks" "$old" "$new")
        want=$(diff -u "$old" "$new" | tail -n +3 | grep -v '^\\ No newline')
        if [ "$got" = "$want" ]; then
            note "hunks vs diff -u: $name"
        else
            bad "hunks vs diff -u: $name" "$(diff <(echo "$got") <(echo "$want"))"
        fi
    }

    mk() { printf '%b' "$2" >"$hdir/$1"; }

    mk a.txt 'a\nb\nc\n'
    mk b.txt 'a\nb\nc\n'
    oracle_case "no change" "$hdir/a.txt" "$hdir/b.txt"

    mk a.txt 'a\nb\n'
    mk b.txt 'a\nx\ny\nz\nb\n'
    oracle_case "pure addition" "$hdir/a.txt" "$hdir/b.txt"

    mk a.txt 'a\nx\ny\nz\nb\n'
    mk b.txt 'a\nb\n'
    oracle_case "pure deletion" "$hdir/a.txt" "$hdir/b.txt"

    mk a.txt '1\n2\n3\n4\n5\n6\n7\n8\n9\n'
    mk b.txt '1\n2\n3\nX\n5\n6\n7\n8\n9\n'
    oracle_case "change with context both sides" "$hdir/a.txt" "$hdir/b.txt"

    mk a.txt 'a\nb\nc\nd\ne\n'
    mk b.txt 'Z\nb\nc\nd\ne\n'
    oracle_case "change at the very start (no leading context)" "$hdir/a.txt" "$hdir/b.txt"

    mk a.txt 'a\nb\nc\nd\ne\n'
    mk b.txt 'a\nb\nc\nd\nZ\n'
    oracle_case "change at the very end (no trailing context)" "$hdir/a.txt" "$hdir/b.txt"

    : >"$hdir/empty.txt"
    mk full.txt 'a\nb\nc\n'
    oracle_case "empty old" "$hdir/empty.txt" "$hdir/full.txt"
    oracle_case "empty new" "$hdir/full.txt" "$hdir/empty.txt"
    oracle_case "both empty" "$hdir/empty.txt" "$hdir/empty.txt"

    mk a.txt 'a\nb\nc'
    mk b.txt 'a\nb\nX\n'
    oracle_case "no trailing newline, old side" "$hdir/a.txt" "$hdir/b.txt"

    mk a.txt '1\n2\n3\n4\n5\n6\n7\n'
    mk b.txt '1\nA\n3\n4\n5\nB\n7\n'
    oracle_case "adjacent changes merge into one hunk" "$hdir/a.txt" "$hdir/b.txt"

    mk a.txt '1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n'
    mk b.txt '1\nA\n3\n4\n5\n6\n7\n8\n9\nB\n11\n'
    oracle_case "distant changes stay in separate hunks" "$hdir/a.txt" "$hdir/b.txt"

    python3 - "$hdir/rand_a.txt" "$hdir/rand_b.txt" <<'PY'
import random, sys
random.seed(99)
lines = [f"line {i}" for i in range(150)]
old = list(lines)
new = list(lines)
for _ in range(12):
    i = random.randrange(len(new))
    op = random.choice(["mod", "del", "ins"])
    if op == "mod":
        new[i] = new[i] + " CHANGED"
    elif op == "del" and len(new) > 1:
        del new[i]
    else:
        new.insert(i, "INSERTED " + str(i))
open(sys.argv[1], "w").write("\n".join(old) + "\n")
open(sys.argv[2], "w").write("\n".join(new) + "\n")
PY
    oracle_case "a larger file with scattered changes" "$hdir/rand_a.txt" "$hdir/rand_b.txt"

    # --- binary detection, against real `git diff` ---------------------------
    #
    # A disposable fixture repository, built and torn down with everything
    # else under `$WORK` -- never a real repository, this one included.
    bindir="$WORK/hunks-bin-fixture"
    mkdir -p "$bindir"
    (
        set -e
        cd "$bindir"
        git init -q -b main .
        git config user.email h@example.com
        git config user.name 'Hunks Tester'
        printf 'hello\000world\n' >bin.dat
        git add -A
        GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
            git commit -q -m 'a NUL-bearing file'
        printf 'hello\000world\000more\n' >bin.dat
    ) >"$WORK/hunks-bin.log" 2>&1 || bad "binary fixture repository" "$(tail -5 "$WORK/hunks-bin.log")"

    if git -C "$bindir" diff --no-color 2>/dev/null | grep -q '^Binary files'; then
        git -C "$bindir" show HEAD:bin.dat >"$hdir/bin_old.dat"
        cp "$bindir/bin.dat" "$hdir/bin_new.dat"
        got=$("$t_hunks" "$hdir/bin_old.dat" "$hdir/bin_new.dat")
        if [ "$got" = "Binary files differ" ]; then
            note "hunks vs git diff: a NUL-bearing file is reported binary, not line-diffed"
        else
            bad "hunks vs git diff: binary file" "hunks.hunks said: $got"
        fi
    else
        bad "binary fixture: git diff did not call the fixture binary"
    fi
fi
