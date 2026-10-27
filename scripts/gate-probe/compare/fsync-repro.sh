#!/bin/bash
# fsync and fdatasync calls of stock tursodb for one transaction of N inserts, counted by strace.
# Usage: fsync-repro.sh [tursodb]   (on the sandbox; installs strace if missing)
command -v strace > /dev/null || { sudo apt-get update -qq > /dev/null; sudo apt-get install --yes -qq strace > /dev/null; }
bin=${1:-/home/tl-user/m/tursodb}
dir=/tmp/fsync-repro
mkdir -p "$dir"
$bin --version | head -1
strace -V | head -1
count() {  # count <journal> <key> <capture> <begin> <n>
  db=$dir/$1-$RANDOM.db
  printf "PRAGMA journal_mode = '%s';\nCREATE TABLE t (id %s, v TEXT);\n" "$1" "$2" | $bin "$db" > /dev/null 2>&1
  sql=$(printf "PRAGMA capture_data_changes_conn('%s');\n%s;\n" "$3" "$4"; for i in $(seq 1 "$5"); do printf "INSERT INTO t (v) VALUES ('x');\n"; done; printf "COMMIT;\n")
  printf "%s\n" "$sql" | strace -f -qq -e trace=fsync,fdatasync -o "$dir/trace.txt" $bin "$db" > "$dir/out.txt" 2>&1
  echo "$(grep --count -E 'fsync|fdatasync' "$dir/trace.txt") syncs, $(grep --count -i error "$dir/out.txt") errors"
}
for key in "INTEGER PRIMARY KEY AUTOINCREMENT" "INTEGER PRIMARY KEY"; do
  for capture in off full; do
    for n in 0 10 20; do
      echo "$key | capture $capture | $n inserts | WAL, BEGIN IMMEDIATE: $(count wal "$key" $capture 'BEGIN IMMEDIATE' $n) | MVCC, BEGIN CONCURRENT: $(count mvcc "$key" $capture 'BEGIN CONCURRENT' $n) | MVCC, BEGIN: $(count mvcc "$key" $capture 'BEGIN' $n)"
    done
  done
done
echo "--- where the syncs land, MVCC BEGIN CONCURRENT, AUTOINCREMENT, capture off, 10 inserts"
db=$dir/where-$RANDOM.db
printf "PRAGMA journal_mode = 'mvcc';\nCREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);\n" | $bin "$db" > /dev/null 2>&1
sql=$(printf "BEGIN CONCURRENT;\n"; for i in $(seq 1 10); do printf "INSERT INTO t (v) VALUES ('x');\n"; done; printf "COMMIT;\n")
printf "%s\n" "$sql" | strace -f -qq -y -e trace=fsync,fdatasync -o "$dir/where.txt" $bin "$db" > /dev/null 2>&1
sed -E 's/^[0-9]+ +//; s/<\/tmp\/fsync-repro\/[^>]*\//</' "$dir/where.txt" | sort | uniq -c
