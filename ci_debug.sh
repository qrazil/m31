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
dbg)
  ulimit -n; sysctl kern.ipc.somaxconn kern.maxfilesperproc kern.maxfiles
  mkdir -p /tmp/acc && cp ci_dbg/accept.m31 /tmp/acc/main.m31
  ./target/debug/$LANG_BIN --emit-c /tmp/acc/main.m31 -o /tmp/acc.c
  gcc -O0 -I runtime -pthread -o /tmp/acc.bin /tmp/acc.c runtime/rt.c runtime/scheduler.c $RT_REACTOR_C $RT_CTX_ASM
  for i in 1 2; do echo "== accept child direct, run $i"; (ulimit -n 48; WITH_TIMEOUT_NO_DIAG=1 bash runtime/with_timeout.sh 15 /tmp/acc.bin child 2>&1 | tail -30); done
  p=stdlib-net-timeout-park
  ./target/debug/$LANG_BIN --emit-c corpus/modules/$p/main.m31 -o /tmp/$p.c
  gcc -O0 -I runtime -pthread -o /tmp/$p.bin /tmp/$p.c runtime/rt.c runtime/scheduler.c $RT_REACTOR_C $RT_CTX_ASM
  for lim in 256 1024 4096; do for i in 1 2 3 4 5 6; do
    s=$SECONDS; (ulimit -n $lim; WITH_TIMEOUT_NO_DIAG=1 bash runtime/with_timeout.sh 20 /tmp/$p.bin > /tmp/p.o 2>&1; echo "== park ulimit=$lim run $i rc=$? $((SECONDS-s))s"; diff /tmp/p.o corpus/modules/$p/main.out | head -4); done; done
  ;;
probe)
  cc -o /tmp/ap ci_dbg/acceptprobe.c && /tmp/ap
  p=stdlib-net-timeout-park
  ./target/debug/$LANG_BIN --emit-c corpus/modules/$p/main.m31 -o /tmp/$p.c 2>/dev/null
  gcc -O0 -I runtime -pthread -o /tmp/$p.bin /tmp/$p.c runtime/rt.c runtime/scheduler.c $RT_REACTOR_C $RT_CTX_ASM
  for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16; do
    /tmp/$p.bin > /tmp/p.o 2>&1 & pid=$!
    for t in 1 2 3 4 5 6 7 8 9 10 11 12; do sleep 1; kill -0 $pid 2>/dev/null || break; done
    if kill -0 $pid 2>/dev/null; then
      echo "== park run $i HUNG"; netstat -an -p tcp | grep 127.0.0.1 | awk '{print $6}' | sort | uniq -c; lsof -nP -p $pid 2>/dev/null | wc -l
      sample $pid 1 2>&1 | grep -a -A12 'Call graph' | head -30
      kill -9 $pid; head -5 /tmp/p.o
    else wait $pid; echo "== park run $i rc=$?"; head -3 /tmp/p.o | grep -a trap; fi
  done ;;
corpus) RUN_LIMIT=60 bash run.sh ;;
san) RUN_LIMIT=60 bash sanitize.sh ;;
gates) GATE_LIMIT=400 bash gates.sh ;;
esac

