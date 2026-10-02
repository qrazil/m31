#!/usr/bin/env bash
# Every program the corpus runs, built with AddressSanitizer and
# UndefinedBehaviorSanitizer and run once.
#
# Why this exists: a use-after-free in argument lowering (a borrowed field
# read, freed by the callee while still in use) passed every other check.
# The freed block was reused, the program printed the wrong number, and yet
# gcc and clang agreed at -O0 and -O2, the C compiled without a warning and
# the refcount ended at zero -- the oracle compares builds with each other,
# and all four were wrong the same way. Only a memory checker sees it.
#
# Channels are immortal by design (docs/concurrency-decision.md), so
# LeakSanitizer is told to ignore what rt_alloc_immortal and rt_chan_new
# hand out. Anything else it reports is a real leak.
#
# clang only: this host's gcc has no sanitizer runtime installed. If clang
# cannot link a sanitized binary at all, the check is skipped and says so,
# rather than passing silently or failing for a reason that is not a bug.
set -uo pipefail
cd "$(dirname "$0")"
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

SAN="-fsanitize=address,undefined -fno-sanitize-recover=all -fno-omit-frame-pointer"
printf 'leak:rt_alloc_immortal\nleak:rt_chan_new\n' >"$WORK/lsan.supp"
export LSAN_OPTIONS="suppressions=$WORK/lsan.supp:print_suppressions=0"
export ASAN_OPTIONS="detect_leaks=1"

echo 'int main(void){return 0;}' >"$WORK/probe.c"
if ! clang $SAN "$WORK/probe.c" -o "$WORK/probe" 2>/dev/null; then
    echo "skipped: clang cannot link a sanitized program here"
    exit 0
fi

bad=0
checked=0
# A program is one core test, or one module test directory with a main.src
# and no main.err (a program that is meant not to compile has nothing to run).
programs=(corpus/core/*."$LANG_EXT")
for d in corpus/modules/*/; do
    [ -e "$d/main.$LANG_EXT" ] && [ ! -e "$d/main.err" ] && programs+=("$d/main.$LANG_EXT")
done

for src in "${programs[@]}"; do
    stem=${src%.$LANG_EXT}
    if ! "$LANGC" --emit-c "$src" -o "$WORK/p.c" 2>/dev/null; then
        echo "does not compile: $src"
        bad=1
        continue
    fi
    if ! clang -O1 -g $SAN -ffp-contract=off -I runtime "$WORK/p.c" \
            runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
            -lpthread -o "$WORK/p" 2>"$WORK/cc"; then
        echo "sanitized build failed: $src: $(head -1 "$WORK/cc")"
        bad=1
        continue
    fi
    stdin="$PWD/$stem.in"
    [ -e "$stdin" ] || stdin=/dev/null
    argv=()
    [ -e "$stem.args" ] && mapfile -t argv <"$stem.args"
    rundir=$PWD
    if [ -e "$stem.setup" ]; then
        rundir=$(mktemp -d -p "$WORK")
        (cd "$rundir" && bash "$OLDPWD/$stem.setup") >/dev/null 2>&1
    fi
    (cd "$rundir" && "$WORK/p" "${argv[@]}" <"$stdin") >"$WORK/out" 2>&1
    checked=$((checked + 1))
    if grep -qE 'ERROR: (AddressSanitizer|LeakSanitizer)|runtime error:' "$WORK/out"; then
        echo "sanitizer: $src"
        grep -E 'ERROR:|runtime error:|^    #[0-3] ' "$WORK/out" | head -6 | sed 's/^/    /'
        bad=1
    fi
done
echo "checked $checked programs"
exit $bad
