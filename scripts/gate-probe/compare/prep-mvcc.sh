#!/bin/bash
# Make an MVCC store for one measured run: prep-mvcc.sh <tag>
# 1. One move in WAL mode bootstraps the synced file, runs the store's migrations and pushes them (tables empty).
# 2. tursodb 0.8.3 switches the file to MVCC in its own process (capture off: the switch is refused with it on).
# 3. With capture on, one commit record numbered above every change so far and above the last pushed change,
#    because 0.8.3 numbers changes from 1 again after a switch (tursodatabase/turso#10935).
# Tables are empty at the switch, so the AUTOINCREMENT sequences need no carrying over (turso#9504).
# The run then uses the same sync server: run.sh mvcc <tag> ... reuses /tmp/sync-<tag>.
set -u
tag=$1
home=/home/tl-user/m
dir=/var/lib/temper/m-$tag
db=$dir/probe.db
mkdir -p "$dir" /tmp/sync-$tag
if [ -f "$db-log" ]; then skip_probe=1; else skip_probe=""; fi
port=$(cat /tmp/sync-$tag.port 2>/dev/null || echo $((18500 + RANDOM % 400)))
echo $port > /tmp/sync-$tag.port
( cd /tmp/sync-$tag && RUST_LOG=warn exec "$home/tursodb" /tmp/sync-$tag/remote.db --sync-server 127.0.0.1:$port < /dev/null >> /tmp/sync-$tag.log 2>&1 ) &
server=$!
for i in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:$port/ && break; sleep 0.2; done
[ -z "$skip_probe" ] && PROBE_SYNC_URL=http://127.0.0.1:$port "$home/gate_probe" "$db" closed 1 > /tmp/prep-$tag.json 2> /tmp/prep-$tag.err
echo "one WAL move: exit $?"
kill $server; wait $server 2>/dev/null
hint=$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["last_pushed_change_id_hint"])' "$db-info")
echo "pushed hint $hint"
[ -z "$skip_probe" ] && printf "PRAGMA journal_mode = 'mvcc';\n" | "$home/tursodb" "$db" 2>&1
printf "PRAGMA capture_data_changes_conn('full');\nINSERT INTO turso_cdc (change_id, change_time, change_type) VALUES (MAX(%s, (SELECT COALESCE(MAX(change_id), 0) FROM turso_cdc)) + 1, 0, 2);\nSELECT MAX(change_id) FROM turso_cdc;\nPRAGMA journal_mode;\n" "$hint" | "$home/tursodb" "$db" 2>&1
echo "header bytes 18-19:$(od -An -tu1 -j18 -N2 "$db")"
