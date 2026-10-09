#!/usr/bin/env bash
# DEBUG ONLY (ci-debug/* branches). One stage per invocation.
set -u
cd "$(dirname "$0")"
stage=$1
export WITH_TIMEOUT_NO_DIAG=
bash --version | head -1
sw_vers 2>/dev/null; sysctl -n hw.ncpu 2>/dev/null; date
. ./config.sh; . ./runtime/arch.sh
case $stage in
stall) set -x; bash runtime/with_timeout.sh 200 bash -x runtime/net_timeout_stall_test.sh ;;
parked) set -x; bash runtime/with_timeout.sh 400 bash -x runtime/parked_waits_carriers_test.sh ;;
progs)
  set -x
  for p in stdlib-timer stdlib-net-timeout-park stdlib-net-timeout; do
    ./build.sh corpus/modules/$p/main.m31 -o /tmp/$p || echo BUILD-FAIL $p
    for c in 1 2; do
      date; LANG_NUM_CARRIERS=$c bash runtime/with_timeout.sh 100 /tmp/$p > /tmp/$p.out 2>&1; echo "rc=$? $p carriers=$c"
      diff /tmp/$p.out corpus/modules/$p/main.out | head -5; tail -50 /tmp/$p.out | head -60
    done
  done ;;
netprogs)
  for p in stdlib-net-accept stdlib-net-timeout-park; do
    ./target/debug/$LANG_BIN --emit-c corpus/modules/$p/main.m31 -o /tmp/$p.c
    for flags in "-DRC_DEBUG" ""; do
      for cc in gcc clang; do
        $cc -O0 $flags -I runtime -pthread -o /tmp/$p.bin /tmp/$p.c runtime/rt.c runtime/scheduler.c $RT_REACTOR_C $RT_CTX_ASM || echo CCFAIL
        for i in 1 2 3 4 5; do
          s=$SECONDS
          bash runtime/with_timeout.sh 30 /tmp/$p.bin > /tmp/$p.o 2>&1; rc=$?
          echo "== $p $cc flags='$flags' run $i rc=$rc $((SECONDS-s))s"
          diff /tmp/$p.o corpus/modules/$p/main.out | head -8
          [ $rc -ne 0 ] && tail -12 /tmp/$p.o | cut -c1-200
        done
      done
    done
  done ;;
corpus) RUN_LIMIT=60 bash run.sh ;;
san) RUN_LIMIT=60 bash sanitize.sh ;;
gates) GATE_LIMIT=400 bash gates.sh ;;
esac

