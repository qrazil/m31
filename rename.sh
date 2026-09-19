#!/usr/bin/env bash
# Rename the language. Rewrites config.sh and moves every corpus file.
#
#   ./rename.sh <name> [binary] [extension]
#
# Defaults: binary = <name>c, extension = first three letters of <name>.
set -euo pipefail
cd "$(dirname "$0")"
. ./config.sh

[ $# -ge 1 ] || { echo "usage: $0 <name> [binary] [extension]" >&2; exit 1; }

new_name=$1
new_bin=${2:-${new_name}c}
new_ext=${3:-${new_name:0:3}}

echo "  name: $LANG_NAME -> $new_name"
echo "binary: $LANG_BIN  -> $new_bin"
echo "   ext: .$LANG_EXT -> .$new_ext"

if [ "$LANG_EXT" != "$new_ext" ]; then
  # Move sources and their twin companions. Expected-output files (.out, .err)
  # keep their own extensions and follow the basename automatically.
  find corpus -name "*.$LANG_EXT" -print0 | while IFS= read -r -d '' f; do
    git mv "$f" "${f%.$LANG_EXT}.$new_ext" 2>/dev/null || mv "$f" "${f%.$LANG_EXT}.$new_ext"
  done
  # Expected diagnostics quote the source path, so they go stale on a rename.
  find corpus/errors -name '*.err' -print0 2>/dev/null \
    | xargs -0 -r sed -i "s/\.$LANG_EXT:/.$new_ext:/g"
fi

cat > config.sh <<EOF
# Single source of truth for the language's identity.
#
# Everything else — run.sh, the corpus file extensions, the README — reads
# these. To rename the language, run ./rename.sh; do not edit this by hand,
# because the corpus files have to move at the same time.

LANG_NAME=$new_name
LANG_BIN=$new_bin
LANG_EXT=$new_ext
EOF

echo "done. config.sh rewritten, corpus renamed."
