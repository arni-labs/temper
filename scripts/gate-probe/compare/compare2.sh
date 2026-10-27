#!/bin/bash
# NORMAL like for like, one drive, alternating: compare2.sh <round> <reverse: 0|1>
#   wal-full   0.7.2 probe with the switch, synchronous left at FULL (what Temper runs today)
#   wal-nrm    the same binary with TEMPER_TURSO_SYNCHRONOUS=NORMAL
#   mvcc-nrm   MVCC prototype (0.8.3) with NORMAL
home=/home/tl-user/m
configs="wal-full wal-nrm mvcc-nrm"
[ "$2" = 1 ] && configs="mvcc-nrm wal-nrm wal-full"
for load in "closed 300" "open 20 30" "open 40 30" "open 80 30"; do
  for config in $configs; do
    tag=$1-$config-$(echo "$load" | tr ' ' '_')
    unset TEMPER_TURSO_SYNCHRONOUS TEMPER_TURSO_COMMIT_FULL PROBE_BIN
    case $config in
      wal-full) export PROBE_BIN=$home/gate_probe_072sync ;;
      wal-nrm) export PROBE_BIN=$home/gate_probe_072sync TEMPER_TURSO_SYNCHRONOUS=NORMAL ;;
      mvcc-nrm) export TEMPER_TURSO_SYNCHRONOUS=NORMAL ;;
    esac
    journal=wal
    case $config in mvcc-*) journal=mvcc; bash "$home/prep-mvcc.sh" "$tag" > "/tmp/prep-$tag.log" 2>&1 ;; esac
    bash "$home/run.sh" "$journal" "$tag" $load > "/tmp/run-$tag.log" 2>&1
    python3 -I "$home/summary.py" "$tag" "$home/results/$tag.json" "/tmp/m-$tag.err"
  done
done
