#!/usr/bin/env bash
# Build the interactive client (`gitui.src`) into a real executable.
#
#   ./apps/git/build-gitui.sh              -> ./ourgitui
#   ./apps/git/build-gitui.sh -o mygitui   -> ./mygitui
#
# `gitui.src`'s entry point lives in `apps/git/`, and imports both this
# directory's own plumbing (`gitclient`, `gitlog`, `status`, `gitignore`,
# `index`, `object`, `refs`, `repo`, `sha1`, `zlib`) and several `apps/tui/` widgets
# (`tuiapp`, `tuioutline`, `tuijump`, `tuimenu`, `tuifooter`, and what those
# pull in). This compiler resolves every `import` against the ENTRY file's
# own directory only (`docs/modules-decision.md` §1: one flat namespace, no
# search path) -- so a program built straight out of `apps/git` can never
# see `apps/tui`'s files sitting next door, and `apps/tui`'s own files must
# not be duplicated permanently into `apps/git` either, since that leaves two
# copies of the same library to drift apart the moment one of them is
# edited.
#
# The fix is `apps/git/test_hunks.sh`'s own `build_tui`, generalised into a
# real build script rather than something that only runs inside a test: copy
# every file this program's own module graph needs -- both halves -- into a
# throwaway staging directory, compile the entry from there, and throw the
# staging directory away. The repository keeps exactly one copy of every
# `apps/tui` file, living only in `apps/tui/`.
set -euo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh

out=ourgitui
[ "${1:-}" = "-o" ] && out=${2:?-o needs a name}

LANGC=${LANGC:-./target/debug/$LANG_BIN}
CC=${CC:-cc}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

cp apps/tui/tuiapp.src apps/tui/tuibuf.src apps/tui/tuidiff.src \
    apps/tui/tuifooter.src apps/tui/tuigeom.src apps/tui/tuijump.src \
    apps/tui/tuimenu.src apps/tui/tuioutline.src apps/tui/tuiscroll.src \
    apps/tui/tuistyle.src apps/tui/tuitext.src apps/tui/tuiwidget.src \
    apps/git/repo.src apps/git/sha1.src apps/git/zlib.src apps/git/object.src \
    apps/git/refs.src apps/git/index.src apps/git/gitignore.src apps/git/status.src \
    apps/git/gitlog.src apps/git/gitclient.src apps/git/gitui.src \
    "$stage/"

"$LANGC" --emit-c "$stage/gitui.src" -o "$stage/gitui.c"
"$CC" -O2 -pthread -I runtime -o "$out" "$stage/gitui.c" runtime/rt.c
echo "$out"
