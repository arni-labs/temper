# WAL vs MVCC x synchronous, on a synced store (scratch)

The instrument behind research/katagami/2026-10-27-temper-turso-full-or-normal.md and
2026-10-27-turso-upsert-cdc.md (Garden). Runs on a Tensorlake sandbox from /home/tl-user/m, with
gate_probe builds, tursodb 0.8.3 (the sync server) and fsync_spy.so (../fsync_spy.c) beside it.

- run.sh <wal|mvcc> <tag> <closed N | open RATE SECONDS>: one probe run on a synced store, fsyncs counted.
- prep-mvcc.sh <tag>: a fresh MVCC store for one run (WAL bootstrap, switch, change-id bump).
- compare2.sh <round> <reverse 0|1>: wal-full, wal-nrm, mvcc-nrm under each load, alternating order.
- summary.py: one line per run.
- fsync-repro.sh [tursodb]: fsync count of stock tursodb per transaction (strace), WAL vs BEGIN CONCURRENT vs BEGIN.

Switches read by the scratch builds: TEMPER_TURSO_MVCC=1, TEMPER_TURSO_SYNCHRONOUS=NORMAL|FULL|OFF,
TEMPER_TURSO_COMMIT_FULL=1 (MVCC: FULL only for each COMMIT).
