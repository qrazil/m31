#!/usr/bin/env bash
# The write path -- `.git/index`, loose objects, refs, working-tree status --
# checked against real `git` in disposable fixtures, the same philosophy
# `test.sh` already uses for the read side (`apps/git/README.md`'s table).
#
# Sourced from `test.sh`, which is why there is no `set`, no `cd` and no
# `trap` here: it runs in `test.sh`'s own shell, after `test.sh` has already
# `cd`'d to the repository root and built `$LANGC`, and shares its `WORK`
# scratch directory, its `build`/`note`/`bad` functions, and its `pass`/`fail`
# counters. `bash apps/git/test.sh` is still the one command that runs
# everything.
#
# Every fixture below is built fresh under `$WORK` (a `mktemp -d` `test.sh`
# already made and will `rm -rf` on exit) and never anything else. `git` is
# used to build fixtures and to read them back for comparison -- never
# against a real repository, exactly as `test.sh`'s own header promises.

# --- the object/index/refs fixture -------------------------------------------

wfx="$WORK/wfixture"
mkdir -p "$wfx"
(
    set -e
    cd "$wfx"
    git init -q -b main .
    git config user.email w@example.com
    git config user.name 'Write Tester'
    printf 'one\n' >a.txt
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
) >"$WORK/wfixture.log" 2>&1 || bad "write-path fixture" "$(tail -5 "$WORK/wfixture.log")"
[ -d "$wfx/.git" ] && note "write-path fixture built"
head_id=$(git -C "$wfx" rev-parse HEAD)

# --- objects: write_blob / write_tree / write_commit -------------------------

if build t_write_object; then
    if "$WORK/t_write_object" "$wfx/.git" >"$WORK/wobj.out" 2>"$WORK/wobj.err"; then
        blob_id=$(sed -n '1p' "$WORK/wobj.out")
        tree_id=$(sed -n '2p' "$WORK/wobj.out")
        commit_id=$(sed -n '3p' "$WORK/wobj.out")
        ok=1
        [ "$(git -C "$wfx" cat-file -t "$blob_id")" = "blob" ] || ok=0
        [ "$(git -C "$wfx" cat-file -p "$blob_id")" = "hello from the write path" ] || ok=0
        [ "$(git -C "$wfx" cat-file -t "$tree_id")" = "tree" ] || ok=0
        want_ls=$(printf '100644 blob %s\tnew.txt' "$blob_id")
        [ "$(git -C "$wfx" ls-tree "$tree_id")" = "$want_ls" ] || ok=0
        [ "$(git -C "$wfx" cat-file -t "$commit_id")" = "commit" ] || ok=0
        git -C "$wfx" cat-file -p "$commit_id" | grep -qx "tree $tree_id" || ok=0
        fsck_out=$(git -C "$wfx" fsck --full 2>&1)
        # The only thing `fsck` should say is that the commit we just wrote
        # is unreachable from any ref -- expected, since nothing points at it
        # yet -- and nothing about corruption.
        echo "$fsck_out" | grep -qv "^dangling commit $commit_id$" && [ -n "$fsck_out" ] && ok=0
        if [ "$ok" = 1 ]; then
            note "object writing: blob/tree/commit agree with real git cat-file/ls-tree, fsck clean"
        else
            bad "object writing" "blob=$blob_id tree=$tree_id commit=$commit_id" "fsck: $fsck_out" "$(cat "$WORK/wobj.out")"
        fi
    else
        bad "t_write_object" "$(cat "$WORK/wobj.err")"
    fi
fi

# --- the index: read, matching `git ls-files --stage` exactly ----------------

if build t_index_read; then
    "$WORK/t_index_read" "$wfx/.git" >"$WORK/ixread.got" 2>"$WORK/ixread.err"
    git -C "$wfx" ls-files --stage >"$WORK/ixread.want"
    if cmp -s "$WORK/ixread.got" "$WORK/ixread.want"; then
        note "index reading matches 'git ls-files --stage' byte for byte"
    else
        bad "index reading" "$(diff "$WORK/ixread.got" "$WORK/ixread.want")" "$(cat "$WORK/ixread.err")"
    fi
fi

# --- the index: write, dropping every extension real git added --------------
#
# Re-encoding the same entries must leave git's own view of them unchanged --
# `ls-files --stage` identical, `fsck` unbothered -- even though the bytes on
# disk are no longer git's own (no cache-tree extension, a fresh checksum).

if build t_index_rewrite; then
    before=$(git -C "$wfx" ls-files --stage)
    if "$WORK/t_index_rewrite" "$wfx/.git" >"$WORK/ixrw.out" 2>"$WORK/ixrw.err"; then
        after=$(git -C "$wfx" ls-files --stage)
        if [ "$before" = "$after" ]; then
            note "index writing: re-encoded index still matches 'git ls-files --stage'"
        else
            bad "index writing" "before: $before" "after:  $after"
        fi
    else
        bad "t_index_rewrite" "$(cat "$WORK/ixrw.err")"
    fi
fi

# --- the index: stage a brand new file ---------------------------------------

if build t_index_stage; then
    printf 'brand new content\n' >"$wfx/staged.txt"
    want_blob=$(git -C "$wfx" hash-object "$wfx/staged.txt")
    if "$WORK/t_index_stage" "$wfx/.git" "$wfx" staged.txt >"$WORK/ixstage.out" 2>"$WORK/ixstage.err"; then
        got_blob=$(awk '{print $2}' "$WORK/ixstage.out")
        stage_line=$(git -C "$wfx" ls-files --stage -- staged.txt)
        want_line=$(printf '100644 %s 0\tstaged.txt' "$want_blob")
        # `git status` must see it as cleanly staged, with no need to
        # re-hash: the mtime/size this wrote has to be one `stat(2)` itself
        # would produce, or git's own shortcut would not trust it either.
        status_line=$(git -C "$wfx" status --short -- staged.txt)
        if [ "$got_blob" = "$want_blob" ] && [ "$stage_line" = "$want_line" ] && [ "$status_line" = "A  staged.txt" ]; then
            note "index staging: new entry matches git's own hash, stage line and clean status"
        else
            bad "index staging" "got_blob=$got_blob want_blob=$want_blob" "stage: $stage_line" "status: $status_line"
        fi
    else
        bad "t_index_stage" "$(cat "$WORK/ixstage.err")"
    fi
fi

# --- refs: update, update_symbolic, and compare-and-swap ---------------------

if build t_write_refs; then
    "$WORK/t_write_refs" "$wfx/.git" "$commit_id" "$head_id" >"$WORK/wrefs.out" 2>"$WORK/wrefs.err"
    ok=1
    grep -qx "update-plain ok" "$WORK/wrefs.out" || ok=0
    grep -qx "symbolic-head ok" "$WORK/wrefs.out" || ok=0
    grep -q "^cas-wrong err" "$WORK/wrefs.out" || ok=0
    grep -qx "cas-right ok" "$WORK/wrefs.out" || ok=0
    grep -qx "detached-head ok" "$WORK/wrefs.out" || ok=0
    grep -qx "cas-create ok" "$WORK/wrefs.out" || ok=0
    [ "$(git -C "$wfx" rev-parse refs/heads/newbranch)" = "$head_id" ] || ok=0
    [ "$(git -C "$wfx" rev-parse refs/heads/fresh)" = "$commit_id" ] || ok=0
    [ "$(cat "$wfx/.git/HEAD")" = "$commit_id" ] || ok=0
    if [ "$ok" = 1 ]; then
        note "ref writing: update/update_symbolic/compare-and-swap all agree with real git"
    else
        bad "ref writing" "$(cat "$WORK/wrefs.out")" "$(cat "$WORK/wrefs.err")"
    fi
fi

# --- status: a mixed working tree, against 'git status --short' -------------

sfx="$WORK/sfixture"
mkdir -p "$sfx"
(
    set -e
    cd "$sfx"
    git init -q -b main .
    git config user.email s@example.com
    git config user.name 'Status Tester'
    printf 'unchanged\n' >keep.txt
    printf 'original\n' >modme.txt
    printf 'will be deleted\n' >delme.txt
    printf 'v1\n' >stagedmod.txt
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
    printf 'changed on disk\n' >modme.txt
    rm delme.txt
    printf 'v2 staged\n' >stagedmod.txt
    git add stagedmod.txt
    printf 'new and staged\n' >addedstaged.txt
    git add addedstaged.txt
    printf 'never added\n' >untracked.txt
    ln -s keep.txt link.txt
) >"$WORK/sfixture.log" 2>&1 || bad "status fixture" "$(tail -5 "$WORK/sfixture.log")"
[ -d "$sfx/.git" ] && note "status fixture built"

if build t_status; then
    "$WORK/t_status" "$sfx/.git" "$sfx" >"$WORK/status.got" 2>"$WORK/status.err"
    git -C "$sfx" status --short >"$WORK/status.want"
    if cmp -s "$WORK/status.got" "$WORK/status.want"; then
        note "status: staged/unstaged/untracked match 'git status --short' exactly"
    else
        bad "status" "$(diff "$WORK/status.got" "$WORK/status.want")" "$(cat "$WORK/status.err")"
    fi
fi

# --- status: an unborn branch -- nothing committed yet ------------------------

ufx="$WORK/ufixture"
mkdir -p "$ufx"
(
    set -e
    cd "$ufx"
    git init -q -b main .
    printf 'fresh\n' >f.txt
    git add -A
) >"$WORK/ufixture.log" 2>&1 || bad "unborn-branch fixture" "$(tail -5 "$WORK/ufixture.log")"
if [ -x "$WORK/t_status" ]; then
    "$WORK/t_status" "$ufx/.git" "$ufx" >"$WORK/ustatus.got" 2>"$WORK/ustatus.err"
    git -C "$ufx" status --short >"$WORK/ustatus.want"
    if cmp -s "$WORK/ustatus.got" "$WORK/ustatus.want"; then
        note "status on an unborn branch matches 'git status --short'"
    else
        bad "status (unborn branch)" "$(diff "$WORK/ustatus.got" "$WORK/ustatus.want")" "$(cat "$WORK/ustatus.err")"
    fi
fi
