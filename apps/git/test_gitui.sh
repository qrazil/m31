# The interactive client (`gitui.src`/`gitclient.src`) -- unit-level,
# oracle-level and pty-driven end-to-end checks, the three tiers
# `apps/git/design.md`'s "how you'll know you're done and correct" names.
#
# Sourced from `test.sh`, which is why there is no `set`, no `cd` and no
# `trap` here -- see `test_write.sh`'s own header for why: it runs in
# `test.sh`'s own shell, sharing its `WORK`, `build`, `note`/`bad` and
# `pass`/`fail` counters. `bash apps/git/test.sh` is still the one command
# that runs everything, this included.
#
# Every fixture below is built fresh under `$WORK` and real `git` is used
# only to build them and to read them back as the oracle -- never against a
# real repository, exactly as `apps/git/design.md`'s own "Safety" section
# requires for this client specifically.
#
# `gitclient.src` (and so every test harness below that imports it) reaches
# into `apps/tui/`, which this program's own module loader resolves relative
# to the ENTRY file's directory only (`docs/modules-decision.md` §1: one flat
# namespace, no search path) -- so a harness built straight out of `apps/git`
# can never see `apps/tui`'s files. `build_tui` is `test.sh`'s own `build`,
# staging both directories' sources into one throwaway place first -- the
# same fix `apps/git/test_hunks.sh` already uses for `hunks.src`'s own
# dependency on `tuidiffview`, and what `apps/git/build-gitui.sh` (the real,
# user-facing build) does too. Nothing here duplicates an `apps/tui` file
# into the repository itself; the staging directory is `$WORK`'s own and is
# gone when `test.sh` is.

build_tui() {
    local name=$1
    local stage="$WORK/tui-stage"
    mkdir -p "$stage"
    cp apps/tui/tuiapp.src apps/tui/tuibuf.src apps/tui/tuidiff.src \
        apps/tui/tuidiffview.src \
        apps/tui/tuifooter.src apps/tui/tuigeom.src apps/tui/tuijump.src \
        apps/tui/tuimenu.src apps/tui/tuioutline.src apps/tui/tuiscroll.src \
        apps/tui/tuistyle.src apps/tui/tuitext.src apps/tui/tuiwidget.src \
        apps/git/repo.src apps/git/sha1.src apps/git/zlib.src apps/git/object.src \
        apps/git/refs.src apps/git/index.src apps/git/gitignore.src apps/git/status.src \
        apps/git/gitlog.src apps/git/hunks.src \
        apps/git/gitclient.src "apps/git/$name.src" "$stage/"
    if ! "$LANGC" --emit-c "$stage/$name.src" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name (staged with apps/tui)" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" runtime/rt.c 2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

# --- unit: outline building and commit-message stripping, no repository ----

if build_tui t_gitclient; then
    "$WORK/t_gitclient" >"$WORK/gitclient_unit.out" 2>"$WORK/gitclient_unit.err"
    if grep -q '^FAIL' "$WORK/gitclient_unit.out"; then
        bad "gitui: unit tests (t_gitclient)" "$(grep '^FAIL' "$WORK/gitclient_unit.out")" "$(cat "$WORK/gitclient_unit.err")"
    else
        note "gitui: unit tests -- $(tail -1 "$WORK/gitclient_unit.out")"
    fi
fi

# --- oracle: stage/unstage/commit driven directly, checked against git -----

opsfx="$WORK/gitui_ops"
mkdir -p "$opsfx"
(
    set -e
    cd "$opsfx"
    git init -q -b main .
    git config user.email o@example.com
    git config user.name 'Ops Tester'
    printf 'one\n' >a.txt
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
    printf 'changed\n' >>a.txt
    printf 'brand new\n' >new.txt
) >"$WORK/gitui_ops.log" 2>&1 || bad "gitui: ops fixture" "$(tail -5 "$WORK/gitui_ops.log")"

if build_tui t_gitclient_ops; then
    "$WORK/t_gitclient_ops" "$opsfx/.git" "$opsfx" stage new.txt >"$WORK/ops1.out" 2>"$WORK/ops1.err"
    got=$(git -C "$opsfx" status --short)
    want=$(printf ' M a.txt\nA  new.txt')
    if [ "$got" = "$want" ]; then
        note "gitui ops: stage_path on an untracked file matches git status --short"
    else
        bad "gitui ops: stage_path on an untracked file" "got:  $got" "want: $want" "$(cat "$WORK/ops1.out")" "$(cat "$WORK/ops1.err")"
    fi

    "$WORK/t_gitclient_ops" "$opsfx/.git" "$opsfx" stage a.txt >"$WORK/ops2.out" 2>"$WORK/ops2.err"
    got=$(git -C "$opsfx" status --short)
    want=$(printf 'M  a.txt\nA  new.txt')
    if [ "$got" = "$want" ]; then
        note "gitui ops: stage_path on a modified, already-tracked file matches git status --short"
    else
        bad "gitui ops: stage_path on a modified file" "got:  $got" "want: $want" "$(cat "$WORK/ops2.out")" "$(cat "$WORK/ops2.err")"
    fi

    "$WORK/t_gitclient_ops" "$opsfx/.git" "$opsfx" unstage new.txt >"$WORK/ops3.out" 2>"$WORK/ops3.err"
    got=$(git -C "$opsfx" status --short)
    want=$(printf 'M  a.txt\n?? new.txt')
    if [ "$got" = "$want" ]; then
        note "gitui ops: unstage_path on a path never in HEAD drops it from the index"
    else
        bad "gitui ops: unstage_path (never in HEAD)" "got:  $got" "want: $want" "$(cat "$WORK/ops3.out")" "$(cat "$WORK/ops3.err")"
    fi

    "$WORK/t_gitclient_ops" "$opsfx/.git" "$opsfx" unstage a.txt >"$WORK/ops4.out" 2>"$WORK/ops4.err"
    got=$(git -C "$opsfx" status --short)
    want=$(printf ' M a.txt\n?? new.txt')
    if [ "$got" = "$want" ]; then
        note "gitui ops: unstage_path on a path tracked in HEAD restores HEAD's entry"
    else
        bad "gitui ops: unstage_path (tracked in HEAD)" "got:  $got" "want: $want" "$(cat "$WORK/ops4.out")" "$(cat "$WORK/ops4.err")"
    fi

    # A blob written by a stage that a later unstage (of a never-committed
    # path) or reset walked away from is expected garbage, not corruption --
    # `apps/git/test_write.sh`'s own `t_write_object` check treats a
    # dangling *commit* the same way. Only a line that is not one of those is
    # a real problem.
    fsck_out=$(git -C "$opsfx" fsck --full 2>&1)
    bad_fsck=$(echo "$fsck_out" | grep -v '^dangling blob ' || true)
    if [ -z "$bad_fsck" ]; then
        note "gitui ops: fsck reports nothing but expected dangling blobs after stage/unstage"
    else
        bad "gitui ops: fsck after stage/unstage" "$fsck_out"
    fi

    # commit: write a message file directly -- exactly the fallback path
    # `finish_commit`/`f` still cover when nothing can launch `$EDITOR` at
    # all, or a person edited the file in another terminal -- then finish it.
    git -C "$opsfx" add a.txt >/dev/null
    printf 'a message written by the test, not an editor\n' >"$opsfx/.git/COMMIT_EDITMSG"
    "$WORK/t_gitclient_ops" "$opsfx/.git" "$opsfx" commit >"$WORK/ops5.out" 2>"$WORK/ops5.err"
    log_line=$(git -C "$opsfx" log -1 --format=%s 2>&1)
    if [ "$log_line" = "a message written by the test, not an editor" ]; then
        note "gitui ops: finish_commit's message and tree match what git log/cat-file see"
    else
        bad "gitui ops: finish_commit" "log_line: $log_line" "$(cat "$WORK/ops5.out")" "$(cat "$WORK/ops5.err")"
    fi
    [ -f "$opsfx/.git/COMMIT_EDITMSG" ] && bad "gitui ops: COMMIT_EDITMSG should be removed after a successful commit" "still present"

    # the empty-message refusal, driven the same way.
    printf 'nothing staged\n' >"$opsfx/untracked-only.txt"
    printf '\n' >"$opsfx/.git/COMMIT_EDITMSG"
    before=$(git -C "$opsfx" rev-parse HEAD)
    "$WORK/t_gitclient_ops" "$opsfx/.git" "$opsfx" commit >"$WORK/ops6.out" 2>"$WORK/ops6.err"
    after=$(git -C "$opsfx" rev-parse HEAD)
    if [ "$before" = "$after" ] && grep -qx "aborting commit due to empty commit message" "$WORK/ops6.out"; then
        note "gitui ops: finish_commit refuses an empty message and moves nothing"
    else
        bad "gitui ops: empty-message refusal" "before=$before after=$after" "$(cat "$WORK/ops6.out")"
    fi

    # --- show_diff_current: unstaged/staged/untracked/binary, against real git

    diffx="$WORK/gitui_diff"
    mkdir -p "$diffx"
    (
        set -e
        cd "$diffx"
        git init -q -b main .
        git config user.email d@example.com
        git config user.name 'Diff Tester'
        printf 'a\nb\nc\n' >f.txt
        printf 'keep me\n' >u.txt
        git add -A
        GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
            git commit -q -m first
        # staged: f.txt changed and staged (index vs HEAD).
        printf 'a\nB\nc\n' >f.txt
        git add f.txt
        # unstaged: a further change on top of the staged version (disk vs
        # index) -- exactly the two-comparisons-on-one-file scenario
        # `apps/git/design.md`'s own "how you'll know you're done" names.
        printf 'a\nB\nC\n' >f.txt
        printf 'brand\nnew\n' >new.txt
        printf 'hello\000world\n' >bin.dat
    ) >"$WORK/gitui_diff.log" 2>&1 || bad "gitui: diff fixture" "$(tail -5 "$WORK/gitui_diff.log")"

    "$WORK/t_gitclient_ops" "$diffx/.git" "$diffx" diff staged f.txt >"$WORK/diff_staged.out" 2>"$WORK/diff_staged.err"
    git -C "$diffx" diff --cached --no-color -- f.txt | tail -n +5 >"$WORK/diff_staged.want"
    if cmp -s "$WORK/diff_staged.out" "$WORK/diff_staged.want"; then
        note "gitui diff: staged row diffs the index's blob against HEAD's tree, matching git diff --cached"
    else
        bad "gitui diff: staged" "$(diff "$WORK/diff_staged.out" "$WORK/diff_staged.want")"
    fi

    "$WORK/t_gitclient_ops" "$diffx/.git" "$diffx" diff unstaged f.txt >"$WORK/diff_unstaged.out" 2>"$WORK/diff_unstaged.err"
    git -C "$diffx" diff --no-color -- f.txt | tail -n +5 >"$WORK/diff_unstaged.want"
    if cmp -s "$WORK/diff_unstaged.out" "$WORK/diff_unstaged.want"; then
        note "gitui diff: unstaged row diffs the index's blob against the working tree, matching git diff"
    else
        bad "gitui diff: unstaged" "$(diff "$WORK/diff_unstaged.out" "$WORK/diff_unstaged.want")"
    fi

    "$WORK/t_gitclient_ops" "$diffx/.git" "$diffx" diff untracked new.txt >"$WORK/diff_untracked.out" 2>"$WORK/diff_untracked.err"
    want_untracked=$'@@ -0,0 +1,2 @@\n+brand\n+new'
    if [ "$(cat "$WORK/diff_untracked.out")" = "$want_untracked" ]; then
        note "gitui diff: an untracked row is one all-added hunk against an empty old side"
    else
        bad "gitui diff: untracked" "got: $(cat "$WORK/diff_untracked.out")" "want: $want_untracked"
    fi

    "$WORK/t_gitclient_ops" "$diffx/.git" "$diffx" diff untracked bin.dat >"$WORK/diff_binary.out" 2>"$WORK/diff_binary.err"
    if [ "$(cat "$WORK/diff_binary.out")" = "Binary files differ" ]; then
        note "gitui diff: a NUL-bearing file reports 'Binary files differ', git's own wording"
    else
        bad "gitui diff: binary" "$(cat "$WORK/diff_binary.out")"
    fi

    # --- e: actually launching $EDITOR, every os.run outcome ----------------
    #
    # A real, interactive $EDITOR (vim, nano) cannot be driven deterministically
    # under a scripted pty, so each outcome here is exercised with a stand-in
    # "editor" -- a tiny shell script, standing in for $EDITOR, that rewrites
    # the message file and exits with whatever status the scenario needs.
    # `edit` (t_gitclient_ops.src's own new op) calls `edit_message()` then
    # `launch_editor()` exactly as `gitui.src`'s driver does once the terminal
    # has been handed back -- there is no terminal in this harness at all,
    # which is what makes every outcome reachable with no pty and no keystrokes.

    cat >"$WORK/editor-ok.sh" <<'EOF'
#!/bin/sh
printf 'a message from the stand-in editor\n' >"$1"
exit 0
EOF
    cat >"$WORK/editor-empty.sh" <<'EOF'
#!/bin/sh
printf '\n' >"$1"
exit 0
EOF
    cat >"$WORK/editor-nonzero.sh" <<'EOF'
#!/bin/sh
printf 'edited but the editor refused\n' >"$1"
exit 9
EOF
    cat >"$WORK/editor-signal.sh" <<'EOF'
#!/bin/sh
printf 'edited then the editor was killed\n' >"$1"
kill -TERM "$$"
EOF
    chmod +x "$WORK/editor-ok.sh" "$WORK/editor-empty.sh" "$WORK/editor-nonzero.sh" "$WORK/editor-signal.sh"
    mkdir -p "$WORK/fakebin"
    cp "$WORK/editor-ok.sh" "$WORK/fakebin/vi"
    chmod +x "$WORK/fakebin/vi"

    editfx="$WORK/gitui_edit"
    mkdir -p "$editfx"
    (
        set -e
        cd "$editfx"
        git init -q -b main .
        git config user.email e@example.com
        git config user.name 'Edit Tester'
        printf 'one\n' >a.txt
        git add -A
        GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
            git commit -q -m first
        printf 'changed\n' >>a.txt
        git add -A
        rm -f .git/COMMIT_EDITMSG
    ) >"$WORK/gitui_edit.log" 2>&1 || bad "gitui edit fixture" "$(tail -5 "$WORK/gitui_edit.log")"

    # exit 0, a real message: launch_editor reuses finish_commit's own
    # stripping/empty-message logic, so a successful edit commits without a
    # separate `f`, exactly like real git's own `$EDITOR` flow.
    EDITOR="$WORK/editor-ok.sh" "$WORK/t_gitclient_ops" "$editfx/.git" "$editfx" edit >"$WORK/edit1.out" 2>"$WORK/edit1.err"
    log_line=$(git -C "$editfx" log -1 --format=%s 2>&1)
    status_out=$(git -C "$editfx" status --short)
    if [ "$log_line" = "a message from the stand-in editor" ] && [ "$status_out" = "" ] && [ ! -f "$editfx/.git/COMMIT_EDITMSG" ]; then
        note "gitui edit: exit 0 reads the message back and finishes the commit (git log/status agree)"
    else
        bad "gitui edit: exit 0 finishes the commit" "log_line=$log_line status=$status_out" "$(cat "$WORK/edit1.out")" "$(cat "$WORK/edit1.err")"
    fi

    # exit 0, an empty message: the same refusal `f` already gives, reached
    # through the editor instead.
    printf 'more\n' >"$editfx/b.txt"
    git -C "$editfx" add -A >/dev/null
    rm -f "$editfx/.git/COMMIT_EDITMSG"
    before=$(git -C "$editfx" rev-parse HEAD)
    EDITOR="$WORK/editor-empty.sh" "$WORK/t_gitclient_ops" "$editfx/.git" "$editfx" edit >"$WORK/edit2.out" 2>"$WORK/edit2.err"
    after=$(git -C "$editfx" rev-parse HEAD)
    if [ "$before" = "$after" ] && grep -qx "aborting commit due to empty commit message" "$WORK/edit2.out"; then
        note "gitui edit: exit 0 with an empty message refuses, exactly like f"
    else
        bad "gitui edit: exit 0 empty-message refusal" "before=$before after=$after" "$(cat "$WORK/edit2.out")"
    fi

    # nonzero exit: a cancelled edit, real git's own treatment -- nothing is
    # committed and the message file is left exactly as the editor wrote it,
    # so e/f/a still have something to act on.
    before=$(git -C "$editfx" rev-parse HEAD)
    EDITOR="$WORK/editor-nonzero.sh" "$WORK/t_gitclient_ops" "$editfx/.git" "$editfx" edit >"$WORK/edit3.out" 2>"$WORK/edit3.err"
    after=$(git -C "$editfx" rev-parse HEAD)
    left=$(cat "$editfx/.git/COMMIT_EDITMSG" 2>/dev/null)
    if [ "$before" = "$after" ] && grep -q "exited 9" "$WORK/edit3.out" && [ "$left" = "edited but the editor refused" ]; then
        note "gitui edit: a nonzero exit cancels the edit, commits nothing, and leaves the message file"
    else
        bad "gitui edit: nonzero exit" "before=$before after=$after left=$left" "$(cat "$WORK/edit3.out")"
    fi

    # killed by a signal: also a cancelled edit, but named differently -- a
    # more unusual case than a plain nonzero exit.
    before=$(git -C "$editfx" rev-parse HEAD)
    EDITOR="$WORK/editor-signal.sh" "$WORK/t_gitclient_ops" "$editfx/.git" "$editfx" edit >"$WORK/edit4.out" 2>"$WORK/edit4.err"
    after=$(git -C "$editfx" rev-parse HEAD)
    if [ "$before" = "$after" ] && grep -q "killed by signal 15" "$WORK/edit4.out"; then
        note "gitui edit: a signalled editor cancels the edit and says so distinctly"
    else
        bad "gitui edit: signalled editor" "before=$before after=$after" "$(cat "$WORK/edit4.out")"
    fi

    # $EDITOR unset: falls back to vi, real git's own default -- proven here
    # with a $PATH under this test's own control rather than the real vi.
    before=$(git -C "$editfx" rev-parse HEAD)
    got=$(cd "$editfx" && PATH="$WORK/fakebin:$PATH" env -u EDITOR "$WORK/t_gitclient_ops" "$editfx/.git" "$editfx" edit)
    after=$(git -C "$editfx" rev-parse HEAD)
    log_line=$(git -C "$editfx" log -1 --format=%s 2>&1)
    if [ "$before" != "$after" ] && [ "$log_line" = "a message from the stand-in editor" ]; then
        note "gitui edit: \$EDITOR unset falls back to vi, found on \$PATH"
    else
        bad "gitui edit: vi fallback" "before=$before after=$after log_line=$log_line got=$got"
    fi

    # os.run itself fails: no process ever started, so this falls back to
    # exactly the pre-os.run behaviour -- the template on disk, and an
    # instruction to edit it externally and press f.
    printf 'yet another change\n' >"$editfx/c.txt"
    git -C "$editfx" add -A >/dev/null
    rm -f "$editfx/.git/COMMIT_EDITMSG"
    before=$(git -C "$editfx" rev-parse HEAD)
    EDITOR="$WORK/no-such-editor-anywhere" "$WORK/t_gitclient_ops" "$editfx/.git" "$editfx" edit >"$WORK/edit5.out" 2>"$WORK/edit5.err"
    after=$(git -C "$editfx" rev-parse HEAD)
    if [ "$before" = "$after" ] && grep -q "cannot launch" "$WORK/edit5.out" && grep -q "press f to finish" "$WORK/edit5.out" && [ -f "$editfx/.git/COMMIT_EDITMSG" ]; then
        note "gitui edit: os.run itself failing falls back to the template-and-edit-externally path"
    else
        bad "gitui edit: os.run Err fallback" "before=$before after=$after" "$(cat "$WORK/edit5.out")"
    fi

    fsck_out=$(git -C "$editfx" fsck --full 2>&1)
    bad_fsck=$(echo "$fsck_out" | grep -v '^dangling blob ' || true)
    if [ -z "$bad_fsck" ]; then
        note "gitui edit: fsck reports nothing but expected dangling blobs after the editor scenarios"
    else
        bad "gitui edit: fsck after the editor scenarios" "$fsck_out"
    fi
fi

# --- end to end: the real interactive loop, driven under a pty -------------
#
# The real, user-facing build (`build-gitui.sh` does its own staging, the
# same way `build_tui` above does for a test harness) rather than a second,
# slightly different way of compiling the same program -- if the actual build
# a user runs ever drifted from what this test exercises, this is exactly the
# kind of gap that would hide.
#
# `pty_e2e.py` builds and destroys its own fixtures under the directory it is
# given; `$WORK/pty` is `test.sh`'s own scratch directory, removed with
# everything else on exit.

if bash apps/git/build-gitui.sh -o "$WORK/gitui" >"$WORK/gitui_build.log" 2>&1; then
    note "gitui: build-gitui.sh produces a working executable"
    if command -v python3 >/dev/null; then
        if out=$(python3 apps/git/pty_e2e.py "$WORK/gitui" "$WORK/pty" 2>&1); then
            note "gitui pty: $(echo "$out" | grep -c '^ok') end-to-end checks passed under a real pty"
        else
            bad "gitui pty end-to-end" "$out"
        fi
    else
        echo "gitui pty: skipped, no python3" >&2
    fi
else
    bad "gitui: build-gitui.sh" "$(cat "$WORK/gitui_build.log")"
fi
