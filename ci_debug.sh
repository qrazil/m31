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
corpus) RUN_LIMIT=60 bash run.sh ;;
san) RUN_LIMIT=60 bash sanitize.sh ;;
gates) GATE_LIMIT=400 bash gates.sh ;;
esac
echo "stage $stage exit=$?"
