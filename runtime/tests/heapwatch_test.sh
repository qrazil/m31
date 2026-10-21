#!/usr/bin/env bash
# Heap watch (docs/heap-watch-design.md): the ledger, read back by the tool.
#
#   - a site that leaks one object in four is a SUSPECT once its survivors
#     outlive the site's own lifetime, with the minute clock shortened by
#     M31_HEAPWATCH_MINUTE_MS so that "10 minutes" takes a fraction of a
#     second; types and sites marked forever (M31_AGE, M31_HEAPWATCH_CONF)
#     and an object marked by heapwatch.forever are listed as forever;
#   - the ledger survives `kill -9`, and `m31c -heapwatch` reads it from the
#     dead process, finding it by path and, while it ran, by pid;
#   - a clean exit with nothing to report leaves no files; M31_HEAPWATCH=0
#     makes none at all;
#   - the count word: a retain and a release touch only the count, and a
#     retain past its 33 bits traps.
#
# Timing margins are generous (a loaded CI runner): every wait polls for
# what it needs, up to tens of seconds, and never relies on a fixed sleep.
#
#   bash runtime/tests/heapwatch_test.sh
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
if [ ! -x "$LANGC" ]; then
    echo "compiler not built: $LANGC" >&2
    exit 1
fi
WORK=$(mktemp -d)
pids=()
cleanup() {
    for p in "${pids[@]+"${pids[@]}"}"; do kill -9 "$p" 2>/dev/null; done
    rm -rf "$WORK"
}
trap cleanup EXIT
fail=0
bad() { echo "FAILED: $*"; fail=1; }

build() { # build <name> <source>
    if ! "$LANGC" -emit-c "$2" --out "$WORK/$1.c" 2>"$WORK/$1.err"; then
        echo "FAILED: compile $2: $(head -3 "$WORK/$1.err")"
        exit 1
    fi
    if ! cc -O2 -pthread -I runtime -o "$WORK/$1" "$WORK/$1.c" \
            runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/$1.cc"; then
        echo "FAILED: build $2:"
        sed 's/^/    /' "$WORK/$1.cc" | head -20
        exit 1
    fi
}

# wait_for <seconds> <command...>: poll until the command succeeds.
wait_for() {
    local limit=$1
    shift
    local end=$((SECONDS + limit))
    while [ $SECONDS -lt $end ]; do
        "$@" && return 0
        sleep 0.2
    done
    return 1
}

build age runtime/tests/heapwatch/t_age.m31
build clean runtime/tests/heapwatch/t_clean.m31

# --- the age ledger, forever marks, kill -9 --------------------------------
dir="$WORK/ledgers"
mkdir -p "$dir"
printf '# sites that live forever on purpose\nMesh@t_age.m31:38:forever\n' >"$WORK/heapwatch.conf"
M31_HEAPWATCH_DIR="$dir" M31_HEAPWATCH_MINUTE_MS=20 M31_AGE=Asset:forever \
    M31_HEAPWATCH_CONF="$WORK/heapwatch.conf" "$WORK/age" >/dev/null 2>"$WORK/age.stderr" &
pid=$!
pids+=("$pid")
ledger="$dir/age.$pid.ledger"
suspects="$dir/age.$pid.suspects"
if ! wait_for 60 grep -qs 'SUSPECT Session t_age.m31:32 ' "$suspects"; then
    bad "no suspect line for the leaking site in 60 s"
    cat "$suspects" 2>/dev/null | head -5
fi
# Found by pid while it runs.
out=$(M31_HEAPWATCH_DIR="$dir" "$LANGC" -heapwatch "$pid" 2>&1)
grep -q "^age (pid $pid), running" <<<"$out" || bad "by pid, a running process: $out"
kill -9 "$pid"
wait "$pid" 2>/dev/null
[ -s "$ledger" ] || bad "the ledger is gone after kill -9"
out=$("$LANGC" -heapwatch "$ledger" 2>&1)
echo "$out" | sed 's/^/    /'
grep -q "^age (pid $pid), not running -- it crashed or was killed" <<<"$out" \
    || bad "the dead process is not reported as dead"
grep -qE '^SUSPECT +Session +t_age.m31:32 +[0-9]+ alive > ' <<<"$out" \
    || bad "the leaking site is not a suspect"
grep -qE '^forever +Asset +t_age.m31:37 +3 alive$' <<<"$out" \
    || bad "M31_AGE=Asset:forever did not mark the type"
grep -qE '^forever +Mesh +t_age.m31:38 +3 alive$' <<<"$out" \
    || bad "the config file's site entry did not mark the site"
grep -qE '^forever +Config +t_age.m31:29 +1 alive \(marked\)$' <<<"$out" \
    || bad "heapwatch.forever did not mark the object"
if grep -qE '^SUSPECT +(Asset|Mesh|Config)' <<<"$out"; then
    bad "a type marked forever is a suspect"
fi
[ -s "$WORK/age.stderr" ] && bad "the program wrote to stderr: $(head -3 "$WORK/age.stderr")"

# --- a clean exit leaves nothing; M31_HEAPWATCH=0 makes nothing ------------
dir2="$WORK/clean-ledgers"
mkdir -p "$dir2"
M31_HEAPWATCH_DIR="$dir2" "$WORK/clean" >/dev/null
[ -z "$(ls -A "$dir2")" ] || bad "a clean exit left files: $(ls "$dir2")"
M31_HEAPWATCH_DIR="$dir2" M31_HEAPWATCH_KEEP=1 "$WORK/clean" >/dev/null
out=$("$LANGC" -heapwatch "$dir2"/clean.*.ledger --all 2>&1)
grep -q "^clean (pid [0-9]*), exited" <<<"$out" || bad "M31_HEAPWATCH_KEEP=1: $out"
grep -qE '^ok +List +t_clean.m31:3 +0 alive, 1 made' <<<"$out" || bad "--all: $out"
rm -f "$dir2"/*
M31_HEAPWATCH_DIR="$dir2" M31_HEAPWATCH=0 M31_HEAPWATCH_KEEP=1 "$WORK/clean" >/dev/null
[ -z "$(ls -A "$dir2")" ] || bad "M31_HEAPWATCH=0 made files: $(ls "$dir2")"

# --- the count word --------------------------------------------------------
if cc -O2 -pthread -I runtime -o "$WORK/bits" runtime/tests/heapwatch/count_bits.c \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/bits.cc"; then
    out=$("$WORK/bits" 2>&1)
    rc=$?
    [ $rc -eq 134 ] || bad "count bits: exit $rc, expected the overflow trap (134)"
    want="ok
trap: reference count overflow: more than 8589934591 references to one object"
    [ "$out" = "$want" ] || bad "count bits printed: $out"
else
    bad "count bits: $(head -5 "$WORK/bits.cc")"
fi

if [ $fail -eq 0 ]; then
    echo "heap watch: all checks passed"
fi
exit $fail
