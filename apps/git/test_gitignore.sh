#!/usr/bin/env bash
# `.gitignore` filtering (`apps/git/gitignore.src`), checked against real
# `git` in disposable fixtures -- the same philosophy `test_write.sh` already
# uses for the rest of the write path (its own header).
#
# Sourced from `test.sh`, after `test_write.sh` -- shares its shell, `$WORK`,
# `$LANGC`, `build`, `note`/`bad` and the `pass`/`fail` counters, and reuses
# the already-built `t_status` binary rather than building a second one.
#
# Every comparison here uses `git status --short --untracked-files=all`, not
# plain `--short`: the default grouping collapses an entirely-untracked
# directory into one `?? dir/` line, which `status.src`'s own module header
# already says this program does not attempt to match (a pre-existing,
# separate gap, not something this task touches) -- `-uall` lists every
# individual untracked file instead, which is what `status.status()` itself
# reports and what these fixtures need to compare against, file for file.

if [ ! -x "$WORK/t_status" ]; then
    build t_status
fi

check_gitignore() {
    local label=$1
    local fixdir=$2
    if [ ! -x "$WORK/t_status" ]; then
        bad "$label" "t_status did not build"
        return
    fi
    "$WORK/t_status" "$fixdir/.git" "$fixdir" >"$WORK/gi.got" 2>"$WORK/gi.err"
    git -C "$fixdir" status --short --untracked-files=all >"$WORK/gi.want"
    if cmp -s "$WORK/gi.got" "$WORK/gi.want"; then
        note "$label"
    else
        bad "$label" "$(diff "$WORK/gi.got" "$WORK/gi.want")" "$(cat "$WORK/gi.err")"
    fi
}

# --- a flat .gitignore: a wildcard, a directory-only pattern, a negation ----

gi1="$WORK/gi_flat"
mkdir -p "$gi1/keepme"
(
    set -e
    cd "$gi1"
    git init -q -b main .
    git config user.email f@example.com
    git config user.name 'Gitignore Tester'
    printf 'root\n' >root.txt
    printf '*.log\nbuild/\n!important.log\n' >.gitignore
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
    printf 'log data\n' >debug.log
    printf 'kept\n' >keepme/a.log
    printf 'not ignored\n' >keepme/normal.txt
    mkdir -p build
    printf 'artifact\n' >build/out.bin
    printf 'important\n' >important.log
    printf 'brand new\n' >newfile.txt
) >"$WORK/gi_flat.log" 2>&1 || bad "gitignore flat fixture" "$(tail -5 "$WORK/gi_flat.log")"
check_gitignore "gitignore: flat *.log / dir-only / negation matches 'git status --short -uall'" "$gi1"

# --- nested .gitignore files, a deeper one overriding a shallower one -------

gi2="$WORK/gi_nested"
mkdir -p "$gi2/src/sub"
(
    set -e
    cd "$gi2"
    git init -q -b main .
    git config user.email f@example.com
    git config user.name 'Gitignore Tester'
    printf '*.tmp\nvendor/\n' >.gitignore
    printf 'root\n' >root.txt
    printf '!*.tmp\n' >src/.gitignore
    printf 'tracked\n' >src/keep.txt
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
    printf 'root tmp, should stay ignored\n' >a.tmp
    printf 'vendor file, should stay ignored\n' >vendor/lib.txt
    printf 'nested override should re-include this\n' >src/keep.tmp
    printf 'nested override should also reach here, any depth under src\n' >src/sub/deep.tmp
) >"$WORK/gi_nested.log" 2>&1 || bad "gitignore nested fixture" "$(tail -5 "$WORK/gi_nested.log")"
check_gitignore "gitignore: a nested .gitignore overrides its parent, at any depth below it" "$gi2"

# --- the exception: negation cannot re-include a path inside an excluded ----
# directory -- git never descends into one to look for a pattern that would.
# The sibling `reincluded_dir/` shows the DIFFERENT case that does work:
# negating the directory pattern ITSELF, rather than a path inside it.

gi3="$WORK/gi_exception"
mkdir -p "$gi3"
(
    set -e
    cd "$gi3"
    git init -q -b main .
    git config user.email f@example.com
    git config user.name 'Gitignore Tester'
    printf 'excluded_dir/\n!excluded_dir/keep.txt\nreincluded_dir/\n!reincluded_dir/\n' >.gitignore
    printf 'root\n' >root.txt
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
    mkdir -p excluded_dir reincluded_dir
    printf 'negation must NOT bring this back\n' >excluded_dir/keep.txt
    printf 'plain excluded\n' >excluded_dir/other.txt
    printf 'directory itself was re-included, so this must show up\n' >reincluded_dir/file.txt
) >"$WORK/gi_exception.log" 2>&1 || bad "gitignore exception fixture" "$(tail -5 "$WORK/gi_exception.log")"
check_gitignore "gitignore: a negation cannot re-include a file inside an excluded directory" "$gi3"

# --- .git/info/exclude, and its precedence under a .gitignore --------------

gi4="$WORK/gi_exclude"
mkdir -p "$gi4"
(
    set -e
    cd "$gi4"
    git init -q -b main .
    git config user.email f@example.com
    git config user.name 'Gitignore Tester'
    printf 'root\n' >root.txt
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
    printf '*.secret\nlocalnotes/\n' >.git/info/exclude
    printf '!keepthis.secret\n' >.gitignore
    printf 'plain excluded via info/exclude\n' >a.secret
    printf 'gitignore negation overrides info/exclude\n' >keepthis.secret
    mkdir -p localnotes
    printf 'excluded via info/exclude dir pattern\n' >localnotes/todo.txt
) >"$WORK/gi_exclude.log" 2>&1 || bad "gitignore info/exclude fixture" "$(tail -5 "$WORK/gi_exclude.log")"
check_gitignore "gitignore: .git/info/exclude applies, and a .gitignore can override it" "$gi4"

# --- a realistic mix, including the "naive negation" trap ------------------
#
# `node_modules/.gitignore` physically exists and tries `!pkg/index.js` --
# real git never even reads it, because it never descends into an excluded
# directory to look for a nested `.gitignore` in the first place. A matcher
# that discovers ignore files by walking the whole tree (this one does, by
# design -- see `gitignore.src`'s own header) has to apply the same
# ancestor-exclusion short-circuit to get this right regardless.

gi5="$WORK/gi_mixed"
mkdir -p "$gi5/docs/sub"
(
    set -e
    cd "$gi5"
    git init -q -b main .
    git config user.email f@example.com
    git config user.name 'Gitignore Tester'
    printf 'root\n' >root.txt
    printf '*.log\nnode_modules/\nbuild/\n!build/keep.txt\ndocs/**/draft.md\n' >.gitignore
    printf 'docs readme\n' >docs/readme.md
    printf '!important.log\n' >docs/.gitignore
    git add -A
    GIT_AUTHOR_DATE='1700000000 +0000' GIT_COMMITTER_DATE='1700000000 +0000' \
        git commit -q -m first
    printf '*.bak\n' >.git/info/exclude
    printf 'ignored via *.log\n' >app.log
    printf 'ignored via info/exclude\n' >notes.bak
    mkdir -p node_modules/pkg build docs/sub
    printf '!pkg/index.js\n' >node_modules/.gitignore
    printf 'a naive negation might wrongly re-include this -- it must not\n' >node_modules/pkg/index.js
    printf 'plain excluded dir content\n' >build/artifact.bin
    printf 'negation attempt from root .gitignore, must NOT be re-included\n' >build/keep.txt
    printf 'matches docs/**/draft.md with zero intermediate dirs\n' >docs/draft.md
    printf 'matches docs/**/draft.md with one intermediate dir\n' >docs/sub/draft.md
    printf 're-included by nested docs/.gitignore\n' >docs/important.log
    printf 're-included by nested docs/.gitignore at any depth below docs\n' >docs/sub/important.log
    printf 'plain untracked file, nothing ignores it\n' >readme_new.txt
) >"$WORK/gi_mixed.log" 2>&1 || bad "gitignore mixed fixture" "$(tail -5 "$WORK/gi_mixed.log")"
check_gitignore "gitignore: a realistic mix, including the naive-negation-inside-an-excluded-dir trap" "$gi5"
