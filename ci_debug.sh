#!/usr/bin/env bash
set -u
cd "$(dirname "$0")"
. ./config.sh; . ./runtime/arch.sh
cargo build 2>&1 | tail -1
p=stdlib-net-timeout-park
./target/debug/$LANG_BIN --emit-c corpus/modules/$p/main.m31 -o /tmp/$p.c 2>/dev/null
gcc -O0 -I runtime -pthread -o /tmp/$p.bin /tmp/$p.c runtime/rt.c runtime/scheduler.c $RT_REACTOR_C $RT_CTX_ASM
hung=0
for i in $(seq 1 60); do
  /tmp/$p.bin > /tmp/p.o 2>&1 & pid=$!
  for t in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do sleep 1; kill -0 $pid 2>/dev/null || break; done
  if kill -0 $pid 2>/dev/null; then
    hung=$((hung+1))
    echo "== park run $i HUNG pid=$pid"; head -5 /tmp/p.o
    netstat -an -p tcp | grep 127.0.0.1 | awk '{print $6}' | sort | uniq -c
    sample $pid 1 2>&1 | grep -a -A40 'Call graph' | head -70
    kill -9 $pid
    [ $hung -ge 3 ] && break
  else wait $pid; rc=$?; [ $rc -ne 0 ] && echo "== park run $i rc=$rc: $(grep -a -m1 trap /tmp/p.o)"; fi
done
echo "HUNG total $hung"
