#!/usr/bin/env bash
# Build the interactive client (`gitui.m31`) into a real executable.
#
#   ./apps/git/build-gitui.sh              -> ./ourgitui
#   ./apps/git/build-gitui.sh -o mygitui   -> ./mygitui
#
# `gitui.m31`'s entry point lives in `apps/git/`, and imports both this
# directory's own plumbing (`gitclient`, `gitlog`, `status`, `gitignore`,
# `index`, `object`, `pack`, `refs`, `repo`, `sha1`, `zlib`, `hunks`) and several
# `apps/tui/` widgets (`tuiapp`, `tuioutline`, `tuijump`, `tuimenu`,
# `tuifooter`, `tuidiffview`, and what those pull in). This compiler resolves
# every `import` against the ENTRY file's own directory only
# (`docs/modules-decision.md` §1: one flat namespace, no search path) -- so a
# program built straight out of `apps/git` can never see `apps/tui`'s files
# sitting next door, and `apps/tui`'s own files must not be duplicated
# permanently into `apps/git` either, since that leaves two copies of the
# same library to drift apart the moment one of them is edited.
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

cp apps/tui/tuiapp.m31 apps/tui/tuibuf.m31 apps/tui/tuidiff.m31 \
    apps/tui/tuidiffview.m31 apps/tui/tuifooter.m31 apps/tui/tuigeom.m31 \
    apps/tui/tuijump.m31 apps/tui/tuimenu.m31 apps/tui/tuioutline.m31 \
    apps/tui/tuiscroll.m31 apps/tui/tuistyle.m31 apps/tui/tuitext.m31 \
    apps/tui/tuiwidget.m31 \
    apps/git/repo.m31 apps/git/sha1.m31 apps/git/zlib.m31 apps/git/pack.m31 apps/git/object.m31 \
    apps/git/refs.m31 apps/git/index.m31 apps/git/gitignore.m31 apps/git/status.m31 \
    apps/git/gitlog.m31 apps/git/hunks.m31 apps/git/gitclient.m31 apps/git/gitui.m31 \
    "$stage/"

"$LANGC" --emit-c "$stage/gitui.m31" -o "$stage/gitui.c"
"$CC" -O2 -pthread -I runtime -o "$out" "$stage/gitui.c" runtime/rt.c
echo "$out"
