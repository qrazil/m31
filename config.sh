# Single source of truth for the language's identity.
#
# Everything else — run.sh, the corpus file extensions, the README — reads
# these. To rename the language, run ./rename.sh; do not edit this by hand,
# because the corpus files have to move at the same time.

LANG_NAME=lang    # human-readable name
LANG_BIN=langc    # compiler binary, built to target/debug/$LANG_BIN
LANG_EXT=src      # source file extension, without the dot
