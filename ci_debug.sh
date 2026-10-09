#!/usr/bin/env bash
set -u
cd "$(dirname "$0")"
. ./config.sh; . ./runtime/arch.sh
export WITH_TIMEOUT_NO_DIAG=
cargo build 2>&1 | tail -1
stress() { # name carriers runs
  p=$1; ./target/debug/$LANG_BIN --emit-c corpus/modules/$p/main.m31 -o /tmp/$p.c 2>/dev/null
  gcc -O0 -I runtime -pthread -o /tmp/$p.bin /tmp/$p.c runtime/rt.c runtime/scheduler.c $RT_REACTOR_C $RT_CTX_ASM || return
  bad=0
  for i in $(seq 1 $3); do
    LANG_NUM_CARRIERS=$2 bash runtime/with_timeout.sh 25 /tmp/$p.bin > /tmp/o.txt 2>&1; rc=$?
    if [ $rc -ne 0 ] || ! diff -q /tmp/o.txt corpus/modules/$p/main.out >/dev/null; then bad=$((bad+1)); echo "-- $p carriers=$2 run $i rc=$rc"; head -5 /tmp/o.txt; fi
  done
  echo "STRESS $p carriers=$2: $bad bad of $3"
}
stress stdlib-net-timeout-park 3 40
stress stdlib-net-timeout-park 1 20
stress stdlib-net-timeout-park 2 20
stress stdlib-timer 3 20
stress stdlib-net-timeout 3 20
echo done
