#!/usr/bin/env bash
# Heap watch costs a default build nothing (docs/heap-watch-design.md,
# "Builds"): compiled without -DRT_HEAPWATCH, the runtime and every program
# are the same machine code as in a tree from before heap watch.
#
# This compares the two, instruction by instruction, with the addresses of
# static data and the names of symbols that only sit next to it taken out
# (a TypeInfo has the fields heap watch reads in every build, so .rodata
# moves; code does not):
#
#   bench/heapwatch/identity.sh <baseline-tree> [program.m31 ...]
#
# <baseline-tree> is a checkout (git worktree or git archive) of the commit
# before heap watch, with its compiler built (cargo build); this tree's
# compiler must be built too. With no programs, the corpus, the examples and
# the benchmarks. Exit status 1 if anything differs.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
BASE=${1:?usage: identity.sh <baseline-tree> [program.m31 ...]}
shift
. ./config.sh
NEW=$PWD
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
CC=${CC:-cc}

cat > "$W/norm.sed" <<'EOF'
s/^[0-9a-f]+ <(.*)>:/<\1>:/
s/^ *[0-9a-f]+:\t/\t/
s/^ *[0-9a-f]+:\s+(R_)/\t\1/
s/\.rodata(\+0x[0-9a-f]+)?/.rodata/
s/<(ti|hwf)_[A-Za-z0-9_]+(\+0x[0-9a-f]+)?>/<ti>/
s/^(\t(call|jmp|j[a-z]+) +[0-9a-f]+) <.*>$/\1/
EOF
norm() { objdump -dr --no-show-raw-insn "$1" | grep -v "file format" | sed -E -f "$W/norm.sed"; }

bad=0
for opt in -O0 -O2; do
    for f in rt scheduler; do
        for t in base new; do
            tree=$BASE; [ $t = new ] && tree=$NEW
            "$CC" $opt -I "$tree/runtime" -c "$tree/runtime/$f.c" -o "$W/$t-$f.o" || exit 1
        done
        if cmp -s <(norm "$W/base-$f.o") <(norm "$W/new-$f.o"); then
            echo "runtime/$f.c $opt: identical"
        else
            echo "runtime/$f.c $opt: DIFFERS"
            bad=1
        fi
    done
done

if [ $# -eq 0 ]; then
    set -- corpus/*/*.$LANG_EXT examples/*.$LANG_EXT bench/*/*.$LANG_EXT
fi
n=0 same=0
for src in "$@"; do
    "$BASE/target/debug/$LANG_BIN" -emit-c "$src" --out "$W/base.c" 2>/dev/null || continue
    "$NEW/target/debug/$LANG_BIN" -emit-c "$src" --out "$W/new.c" 2>/dev/null \
        || { echo "$src: compiles with the baseline and not here"; bad=1; continue; }
    for opt in -O0 -O2; do
        "$CC" $opt -w -I "$BASE/runtime" -c "$W/base.c" -o "$W/base.o" 2>/dev/null || continue 2
        "$CC" $opt -w -I "$NEW/runtime" -c "$W/new.c" -o "$W/new.o" 2>/dev/null \
            || { echo "$src $opt: the C does not compile here"; bad=1; continue 2; }
        n=$((n + 1))
        if cmp -s <(norm "$W/base.o") <(norm "$W/new.o"); then
            same=$((same + 1))
        else
            echo "DIFFERS: $src $opt"
            bad=1
        fi
    done
done
echo "programs x opt levels: $n compared, $same identical"
exit $bad
