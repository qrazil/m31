#!/usr/bin/env bash
# Heap watch (docs/heap-watch-design.md): the ledger, trace and epoch close,
# read back by the tool.
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
#   - trace, from the start: a connection held by a session held by a cache's
#     map, and a cycle of nodes, are printed as those paths; and turned on
#     and off in a running process through its ledger's control page;
#   - epoch close (M31_HEAPWATCH=full) in a build that does not trap: each
#     close that leaves a cycle is a line in the suspect log, a count in the
#     ledger, and with trace on the cycle in the trace file;
#     M31_HEAPWATCH_TRAP=1 traps; sampled, a close still counts every
#     object left, and names the sites of the sampled ones;
#   - sampling: with a sample every few hundred bytes rather than every
#     object, the leaking site is still found;
#   - the count word: a retain and a release touch only the count.
#
# Every program here is a heap watch build (-DRT_HEAPWATCH, docs/heap-watch-
# design.md, "Builds"), and samples every object (M31_HEAPWATCH_SAMPLE=1),
# so the counts it checks are exact, unless a check says otherwise.
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
    if ! cc -O2 -pthread -DRT_HEAPWATCH -I runtime -o "$WORK/$1" "$WORK/$1.c" \
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

export M31_HEAPWATCH_SAMPLE=1

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

# --- sampled: the leak is still found ----------------------------------------
dir6="$WORK/sampled-ledgers"
mkdir -p "$dir6"
M31_HEAPWATCH_DIR="$dir6" M31_HEAPWATCH_MINUTE_MS=20 M31_HEAPWATCH_SAMPLE=256 \
    "$WORK/age" >/dev/null 2>&1 &
pid=$!
pids+=("$pid")
if ! wait_for 60 grep -qs 'SUSPECT Session t_age.m31:32 ' "$dir6/age.$pid.suspects"; then
    bad "sampled: no suspect line for the leaking site in 60 s"
fi
kill -9 "$pid"
wait "$pid" 2>/dev/null
out=$("$LANGC" -heapwatch "$dir6/age.$pid.ledger" 2>&1)
grep -q '^sampled: one object in every 256 bytes made, on average; counts are estimates$' <<<"$out" \
    || bad "sampled: the report does not say so: $(head -5 <<<"$out")"

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

# --- trace: who holds it, from the start ------------------------------------
build trace runtime/tests/heapwatch/t_trace.m31
dir3="$WORK/trace-ledgers"
mkdir -p "$dir3"
M31_HEAPWATCH_DIR="$dir3" M31_HEAPWATCH_MINUTE_MS=20 M31_HEAPWATCH_DEPTH=8 \
    M31_HEAPWATCH_TRACE=Connection,Session,Map,Node,List "$WORK/trace" >/dev/null 2>&1 &
pid=$!
pids+=("$pid")
tracefile="$dir3/trace.$pid.trace"
if ! wait_for 60 grep -qs '<- cycle' "$tracefile"; then
    bad "trace: no cycle in the trace file in 60 s"
fi
wait_for 20 grep -qs '^2.* Connection  t_trace.m31:29  (suspect' "$tracefile" \
    || bad "trace: the leaking connections were never reported"
kill -9 "$pid"
wait "$pid" 2>/dev/null
out=$("$LANGC" -heapwatch "$dir3/trace.$pid.ledger" --paths 2>&1)
# A connection is held by its session, which the cache's map holds, which
# the cache holds.
grep -qE '^  held by  Session.connection +t_trace.m31:29$' <<<"$out" \
    || bad "trace: no Session.connection holder: $(head -20 <<<"$out")"
grep -qE '^    held by  Map value +t_trace.m31:26$' <<<"$out" || bad "trace: no Map value holder"
grep -qE '^      held by  Cache.by_number +t_trace.m31:26$' <<<"$out" || bad "trace: no Cache.by_number"
# A node is held by the other node's list of links, and that by the node,
# and so on round to itself.
grep -qE '^        held by  Node.links +t_trace.m31:3[34]  <- cycle$' <<<"$out" \
    || bad "trace: the node cycle is not shown as one"

# --- trace: turned on in a running process -----------------------------------
dir4="$WORK/fly-ledgers"
mkdir -p "$dir4"
M31_HEAPWATCH_DIR="$dir4" "$WORK/trace" >/dev/null 2>&1 &
pid=$!
pids+=("$pid")
wait_for 30 test -s "$dir4/trace.$pid.ledger" || bad "on the fly: no ledger"
out=$(M31_HEAPWATCH_DIR="$dir4" "$LANGC" -heapwatch "$pid" --trace Connection Session 2>&1)
grep -q "^trace on for Connection in pid $pid$" <<<"$out" || bad "on the fly: --trace said: $out"
# New sessions are made every few milliseconds; ask until one is recorded.
paths_ok() {
    out=$(M31_HEAPWATCH_DIR="$dir4" "$LANGC" -heapwatch "$pid" --paths 2>&1)
    grep -qE '^  held by  Session.connection +t_trace.m31:29$' <<<"$out"
}
wait_for 30 paths_ok || bad "on the fly: --paths never showed Session.connection: $(head -10 <<<"$out")"
out=$(M31_HEAPWATCH_DIR="$dir4" "$LANGC" -heapwatch "$pid" 2>&1)
# --depth's default (3) has turned trace on for the holders it met, too.
grep -qE '^trace on for Connection t_trace.m31:29, Session t_trace.m31:29(, .*)?; [0-9]+ edges recorded$' <<<"$out" \
    || bad "on the fly: the report does not say what is traced: $out"
out=$(M31_HEAPWATCH_DIR="$dir4" "$LANGC" -heapwatch "$pid" --untrace Connection Session 2>&1)
grep -q "^trace off for Session in pid $pid$" <<<"$out" || bad "on the fly: --untrace said: $out"
kill -9 "$pid"
wait "$pid" 2>/dev/null
out=$("$LANGC" -heapwatch "$dir4/trace.$pid.ledger" --trace Node 2>&1)
grep -q "is not running" <<<"$out" || bad "on the fly: --trace of a dead process: $out"

# --- epoch close outside a test build -----------------------------------------
build epoch runtime/tests/heapwatch/t_epoch.m31
dir5="$WORK/epoch-ledgers"
mkdir -p "$dir5"
export M31_HEAPWATCH=full
out=$(M31_HEAPWATCH_DIR="$dir5" M31_HEAPWATCH_DEPTH=8 M31_HEAPWATCH_TRACE=Node,List "$WORK/epoch" 2>&1)
rc=$?
[ $rc -eq 0 ] && [ "$out" = "done" ] || bad "epoch: exit $rc, printed $out"
ledger=$(ls "$dir5"/epoch.*.ledger 2>/dev/null | head -1)
leaks="1 List @ t_epoch.m31:14, 1 Node @ t_epoch.m31:14, 1 List @ t_epoch.m31:15, 1 Node @ t_epoch.m31:15"
[ "$(grep -c "EPOCH \"request\" closed with leaks: $leaks$" "${ledger%.ledger}.suspects")" = 5 ] \
    || bad "epoch: five leaking closes are not five lines in the log"
out=$("$LANGC" -heapwatch "$ledger" 2>&1)
grep -q '^epoch "request" closed with leaks 5 times; last at minute 0: 1 List @ t_epoch.m31:14' <<<"$out" \
    || bad "epoch: the report: $out"
grep -qE '^        held by  Node.links +t_epoch.m31:1[45]  <- cycle$' "${ledger%.ledger}.trace" \
    || bad "epoch: the trace file has no cycle"
[ "$(grep -c '(epoch "request" left them)' "${ledger%.ledger}.trace")" = 20 ] \
    || bad "epoch: each leak should be traced once, by four sites, five times"
out=$(M31_HEAPWATCH_DIR="$dir5" M31_HEAPWATCH_TRAP=1 "$WORK/epoch" 2>&1)
rc=$?
[ $rc -eq 134 ] && grep -q "^trap: epoch \"request\" closed with leaks: $leaks$" <<<"$out" \
    || bad "epoch: M31_HEAPWATCH_TRAP=1 did not trap: exit $rc, $out"
# Sampled, so that no object is: the count of what is left is still exact.
out=$(M31_HEAPWATCH_DIR="$dir5" M31_HEAPWATCH_SAMPLE=1000000000 M31_HEAPWATCH_TRAP=1 "$WORK/epoch" 2>&1)
rc=$?
[ $rc -eq 134 ] && grep -q '^trap: epoch "request" closed with leaks: 4 objects; none sampled$' <<<"$out" \
    || bad "epoch, sampled: exit $rc, $out"
# Off, or the ledger alone: no epoch is closed, so nothing traps.
for mode in 0 ledger; do
    out=$(M31_HEAPWATCH_DIR="$dir5" M31_HEAPWATCH=$mode M31_HEAPWATCH_TRAP=1 "$WORK/epoch" 2>&1)
    rc=$?
    [ $rc -eq 0 ] && [ "$out" = "done" ] || bad "epoch, M31_HEAPWATCH=$mode: exit $rc, $out"
done
unset M31_HEAPWATCH

# --- the count word --------------------------------------------------------
if cc -O2 -pthread -DRT_HEAPWATCH -I runtime -o "$WORK/bits" runtime/tests/heapwatch/count_bits.c \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/bits.cc"; then
    out=$("$WORK/bits" 2>&1)
    rc=$?
    [ $rc -eq 0 ] && [ "$out" = "ok" ] || bad "count bits: exit $rc, printed $out"
else
    bad "count bits: $(head -5 "$WORK/bits.cc")"
fi

if [ $fail -eq 0 ]; then
    echo "heap watch: all checks passed"
fi
exit $fail
