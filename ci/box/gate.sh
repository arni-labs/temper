#!/usr/bin/env bash
# Runs on ci-temper, as ci-test: tests one commit as .github/workflows/ci.yml did on GitHub's runners for a push to
# main, by the same steps and the commit's own scripts (scripts/ci/affected.py decides what the change reaches, exactly
# as there), and builds the Linux binary as linux-binary.yml did:
#   gate.sh <commit> <base>        a push to main from <base>; CI_ALL=1 runs the full lane (the nightly)
# /opt/ci/route.sh (the fixed wrapper) runs it as ci-test, which holds no key and has no sudo, after it put the two
# commits in /var/lib/ci/work/test.git (refs/ci/commit, refs/ci/base); it publishes and sends the verdict. Warm by design: one
# worktree at one path, so a checkout keeps unchanged files' times and cargo rebuilds only what the commit changed;
# target/ survives the clean between runs. Postgres 18 and Redis start fresh for each run, as ci.yml's services did.
# Three lanes run side by side, each in its own cargo target so none waits on another's build lock: lint (Compile &
# Lint), tests (shards, doctests, Postgres, DST, specs; the full lane's Tests) and other (other workspaces, the
# TypeScript SDK, the Linux binary). Answers go to /var/lib/ci/work/runs/<commit>/ (<job>.txt ending `exit <code>`;
# summary.txt; verdict.txt last; temper-x86_64-linux{,.sha256,.inputs} when built). With CI_PUBLISH=1 a passed push's
# binary and its inputs are also left in publish/ there, for route.sh to put in R2.
# Left out on purpose: Focused tests (a dispatch with a nextest filter: run `cargo nextest` on the box) and
# linux-binary.yml's GitHub release and downstream dispatch: a passed push's binary goes to R2 instead (route.sh), and
# the Factory, not the box, starts what installs it.
# Exit 0 when every job the change needed passed, 1 when one failed, 2 on a usage error. Prints no key.
set -uo pipefail
commit=${1:?usage: gate.sh <commit> <base>} base=${2:?usage: gate.sh <commit> <base>}
CI=/var/lib/ci/work repo=/var/lib/ci/work/test.git tree=/var/lib/ci/work/tree
. /opt/ci/env.sh
[ "$(id -u)" != 0 ] || { echo "gate: runs as ci-test, through /opt/ci/route.sh"; exit 2; }
t0=$SECONDS
commit=$(git -C "$repo" rev-parse --verify "$commit^{commit}") || exit 2
base=$(git -C "$repo" rev-parse --verify "$base^{commit}") || exit 2
# Each tested commit keeps a ref, so its objects stay; the oldest beyond 50 go.
git -C "$repo" update-ref "refs/ci/tested/$commit" "$commit"
git -C "$repo" for-each-ref --sort=-committerdate --format='%(refname)' refs/ci/tested | tail -n +51 | xargs -r -n1 git -C "$repo" update-ref -d
# checkout <dir>: the commit there, warm (unchanged files keep their times; target/ stays).
checkout() {
    if git -C "$1" rev-parse --git-dir >/dev/null 2>&1; then
        git -C "$1" reset -q --hard && git -C "$1" checkout -q --force --detach "$commit" \
          && git -C "$1" clean -q -ffdx -e target/ -e node_modules/
    else
        rm -rf "$1"; git -C "$repo" worktree prune; git -C "$repo" worktree add -q --force --detach "$1" "$commit"
    fi
}
# Other workspaces get a checkout of their own, as each GitHub job had: scripts/ci/workspaces.py runs cargo inside
# path dependencies of the root (vendor/pprof), which writes an untracked Cargo.lock there. In the shared tree the
# clean took it away each run and the next build made it again, newer, so cargo rebuilt pprof and everything over it,
# the Linux binary's fat LTO included.
ws_tree=$CI/tree-ws
checkout "$tree" && checkout "$ws_tree" || exit 2
# Disk: cargo's targets only grow, and the box's disk is 100 GB (the most Tensorlake gives a sandbox in this
# namespace). Below 8 GB free, the tests' target (the biggest: 55 GB when every package is built, which leaves about
# 16 GB) starts again, and this run builds it cold.
free=$(df -Pk "$CI" | awk 'NR == 2 {print int($4 / 1048576)}')
[ "$free" -ge 8 ] || { echo "gate: $free GB free, so target/debug starts again"; rm -rf "$tree/target/debug"; }
out=$CI/runs/$commit && rm -rf "$out" && mkdir -p "$out"
ulimit -n "$(ulimit -Hn)"
cd "$tree" || exit 2
exec 3>&1
echo "gate: $commit against $base on $(nproc) cores; checkout in $((SECONDS - t0)) s"

# What the run tests, as ci.yml's `changes` job decides it: a push tests what it moved main by (lane pr), the nightly
# everything (lane full).
if [ "${CI_ALL:-0}" = 1 ]; then event=schedule; else event=push; fi
EVENT=$event BASE_SHA=$base HEAD_SHA=$commit RUSTUP_TOOLCHAIN=stable GITHUB_OUTPUT= python3 scripts/ci/affected.py \
    >"$out/affected.txt" 2>"$out/affected.err" || { cat "$out/affected.err"; echo "gate: affected.py failed"; exit 1; }
cat "$out/affected.err"
get() { sed -n "s/^$1=//p" "$out/affected.txt"; }
lane=$(get lane) mode=$(get mode) packages=$(get packages) test_args=$(get test_args) rust=$(get rust) sdk_ts=$(get sdk_ts)
specs=$(get specs) shards=$(get shards)
echo "gate: lane $lane, mode $mode, rust $rust, sdk_ts $sdk_ts, specs $specs; packages: ${packages:-none}"

# --- services: a fresh PostgreSQL 18 and Redis for each run, as each GitHub job got its own -------------------------
PG=$CI/pgrun
pg_fresh() { # <database>...: a new cluster with password temper on 127.0.0.1:5432, and the databases named
    pg_ctl -D "$PG" -m immediate stop >/dev/null 2>&1; rm -rf "$PG"
    printf temper >"$CI/.pgpw" && initdb -D "$PG" -U postgres --pwfile="$CI/.pgpw" -A scram-sha-256 >/dev/null && rm -f "$CI/.pgpw" || return
    printf "listen_addresses = '127.0.0.1'\nunix_socket_directories = '%s'\nmax_connections = 300\nfsync = off\nsynchronous_commit = off\nfull_page_writes = off\n" "$PG" >>"$PG/postgresql.conf"
    pg_ctl -D "$PG" -l "$CI/pg.log" -w start >/dev/null || { tail -20 "$CI/pg.log"; return 1; }
    local db; for db in "$@"; do PGPASSWORD=temper psql -qh 127.0.0.1 -U postgres -c "CREATE DATABASE $db" postgres || return; done
}
redis_fresh() {
    redis-cli -p 6379 shutdown nosave >/dev/null 2>&1 || true
    redis-server --port 6379 --bind 127.0.0.1 --save '' --appendonly no --daemonize yes --logfile "$CI/redis.log" >/dev/null
    for _ in $(seq 50); do redis-cli -p 6379 ping >/dev/null 2>&1 && return; sleep 0.1; done; return 1
}
# Docker, for the tests that start their own Postgres with testcontainers (temper-actor-runtime's setup_test_pg), as
# GitHub's runner has it: ci-test's own rootless dockerd (rootlesskit), so a container is ci-test and no more. Only for
# the run's length (route.sh stops every ci-test process after the gate); images stay in ~/.local/share/docker.
docker_up() {
    export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/docker.sock
    docker info >/dev/null 2>&1 && return
    dockerd-rootless.sh >"$CI/dockerd.log" 2>&1 &
    for _ in $(seq 60); do docker info >/dev/null 2>&1 && return; sleep 1; done
    tail -20 "$CI/dockerd.log"; return 1
}
services_down() {
    pg_ctl -D "$PG" -m fast stop >/dev/null 2>&1; redis-cli -p 6379 shutdown nosave >/dev/null 2>&1
    pkill -u "$(id -u)" -x rootlesskit 2>/dev/null; true
}
trap services_down EXIT

# --- the jobs: one function each, named as ci.yml names them --------------------------------------------------------
gepa_modules() {
    rustup target add wasm32-unknown-unknown >/dev/null || return
    local m; for m in gepa-replay gepa-reflective gepa-score gepa-pareto gepa-verify; do
        (cd "wasm-modules/$m" && if [ -f Cargo.lock ]; then cargo build --locked --quiet --target wasm32-unknown-unknown --release; \
         else cargo build --quiet --target wasm32-unknown-unknown --release; fi) || return
    done
}
prod_files() { find crates -name '*.rs' -not -path '*/tests/*' -not -name '*_test.rs' -not -name '*_tests.rs' -not -name 'tests.rs' -not -path '*/temper-macros/*' "$@"; }
job_compile_lint() { # Compile & Lint, in its own target
    export CARGO_TARGET_DIR=$tree/target/lint
    rustup component add clippy rustfmt >/dev/null || return
    echo "== cargo fmt --check"; cargo fmt --check || return
    echo "== cargo clippy"; cargo clippy --locked --workspace --all-targets -- -D warnings || return
    echo "== No TODO/FIXME/HACK in production code"
    local f code found=false
    for f in $(prod_files); do
        code="$(awk '/^#\[cfg\(test\)\]/{exit} {print}' "$f")"
        if echo "$code" | grep -E '(TODO|FIXME|XXX|HACK)\b' | grep -v '// ci-ok' | grep -qv '^[[:space:]]*//' 2>/dev/null; then
            echo "FAIL: $f contains TODO/FIXME/HACK"; found=true
        fi
    done
    [ $found = false ] || return 1
    echo "== Pre-commit exclusion regression"; bash .claude/hooks/test-pre-commit.sh || return
    echo "== No unwrap() in production code"
    local filtered
    for f in $(prod_files -not -path '*/benches/*'); do
        code="$(awk '/^#\[cfg\(test\)\]/{exit} {print}' "$f")"
        filtered="$(echo "$code" | grep '\.unwrap()' | grep -v '// ci-ok' | grep -v '^[[:space:]]*//' | grep -v '\.read()\.unwrap()' \
          | grep -v '\.write()\.unwrap()' | grep -v '\.lock()\.unwrap()' | grep -v 'with_ymd_and_hms.*\.unwrap()' || true)"
        [ -z "$filtered" ] || { echo "FAIL: $f contains .unwrap()"; echo "$filtered"; found=true; }
    done
    [ $found = false ] || return 1
    echo "== ratchets"
    bash scripts/readability-ratchet.sh check .ci/readability-baseline.env || return
    bash scripts/check-storage-dispatch-boundary.sh || return
    bash scripts/check-additive-migrations.sh || return
    echo "== Embedded http_fetch module matches its source"
    rustup target add wasm32-unknown-unknown >/dev/null && bash scripts/build-http-fetch-wasm.sh --check || return
    echo "== Dependency isolation"
    if cargo tree --locked --no-dev -p temper-jit 2>/dev/null | grep -q temper-verify; then echo "FAIL: temper-jit depends on temper-verify"; return 1; fi
    local crate; for crate in temper-jit temper-server temper-runtime; do
        if cargo tree --locked --no-dev -p "$crate" 2>/dev/null | grep -qE 'stateright|proptest'; then echo "FAIL: $crate includes stateright or proptest"; return 1; fi
    done
    echo "== Instrumentation hygiene"; cargo run --locked -q -p temper-server --bin check_instrumentation
}
job_sdk_ts() { # TypeScript SDK
    cd packages/temper-sdk-ts || return
    local sum; sum=$(sha256sum package-lock.json | cut -c1-64)
    [ "$(cat node_modules/.ci-lock 2>/dev/null)" = "$sum" ] || { npm ci --no-audit --no-fund && echo "$sum" >node_modules/.ci-lock; } || return
    npx tsc --noEmit && npx vitest run
}
job_other_workspaces() { cd "$ws_tree" && CARGO_TARGET_DIR=$ws_tree/target/workspaces python3 scripts/ci/workspaces.py; }
# The pull-request lane's Tests shards, Postgres rollback migrations, DST/Platform cells and Spec Verification, one
# after another in one target (they build the same crates: every build resolves features across the workspace).
job_tests() {
    export DATABASE_URL=postgres://postgres:temper@localhost:5432/postgres REDIS_URL=redis://localhost:6379
    local failed="" n args doc
    if [ "$lane" = full ]; then
        pg_fresh && redis_fresh && docker_up && gepa_modules || return
        echo "== cargo nextest run --workspace (skip dst_)"
        cargo nextest run --locked --workspace -E 'not test(dst_)' || failed="$failed tests;"
        echo "== observe-gated tests"
        { cargo test --locked -p temper-server --features observe --test spec_validate_endpoint \
          && cargo test --locked -p temper-server --features observe --lib observe:: \
          && cargo test --locked -p temper-server --features observe --lib api::repl::; } || failed="$failed observe;"
        echo "== cargo test --doc --workspace"; cargo test --locked --doc --workspace || failed="$failed doctests;"
    elif [ "$mode" != none ]; then
        pg_fresh && redis_fresh && docker_up || return
        # Each shard's packages once, whole (one box needs no --partition); a shard with gepa builds those modules first.
        n=$(python3 -c 'import json,sys; print(len(json.loads(sys.argv[1])["include"]))' "$shards")
        if python3 -c 'import json,sys; sys.exit(0 if any(c.get("gepa") for c in json.loads(sys.argv[1])["include"]) else 1)' "$shards"; then
            gepa_modules || return
        fi
        while IFS=$'\t' read -r args doc; do
            if [ -n "$args" ]; then echo "== cargo nextest run $args -E 'not test(dst_)'"
                cargo nextest run --locked $args -E 'not test(dst_)' || failed="$failed shard[$args];"; fi
            if [ -n "$doc" ]; then echo "== cargo test --doc $doc"; cargo test --locked --doc $doc || failed="$failed doctests;"; fi
        done < <(python3 -c '
import json, sys
seen = set()
for c in json.loads(sys.argv[1])["include"]:
    key = (c["args"], c["doc"])
    if key not in seen: seen.add(key); print(c["args"] + "\t" + c["doc"])' "$shards")
        echo "gate: $n shard cells run as their distinct package sets"
    fi
    if [ -n "$packages" ]; then # Postgres rollback migrations
        local sel pargs pfilter; sel=$(PACKAGES=$packages GITHUB_OUTPUT= python3 scripts/ci/postgres_ignored.py) || return
        pargs=$(sed -n 's/^args=//p' <<<"$sel") pfilter=$(sed -n 's/^filter=//p' <<<"$sel")
        if [ -n "$pargs" ]; then
            pg_fresh morrow_temper || return
            echo "== Postgres rollback migrations: $pargs"
            TEMPER_TEST_POSTGRES_URL=postgres://postgres:temper@localhost:5432/postgres \
            TEST_POLICY_DATABASE_URL=postgres://postgres:temper@localhost:5432/morrow_temper \
              cargo nextest run --locked --profile postgres $pargs --run-ignored only --test-threads 1 --no-fail-fast -E "$pfilter" \
              || failed="$failed postgres;"
        fi
    fi
    if [ -n "$test_args" ]; then # DST/Platform Tests: every suite's cells; the pr lane runs platform-random as a smoke
        local suite filter
        while IFS=$'\t' read -r suite filter; do
            echo "== DST $suite"
            if [ "$suite" = platform-random ] && [ "$lane" = full ]; then
                local i; for i in 0 1 2 3; do
                    TEMPER_DST_RANDOM_MODE=full TEMPER_DST_SHARD_INDEX=$i TEMPER_DST_SHARD_COUNT=4 \
                      cargo nextest run --locked $test_args --no-tests=warn -E "$filter" || failed="$failed dst-$suite-$i;"
                done
            else
                TEMPER_DST_RANDOM_MODE=$([ "$lane" = pr ] && echo smoke || echo full) \
                  cargo nextest run --locked $test_args --no-tests=warn -E "$filter" || failed="$failed dst-$suite;"
            fi
        done <<'SUITES'
core	test(dst_) and not binary(/^dst_platform_/)
platform-boot	test(dst_) and binary(=dst_platform_boot)
platform-consistency	test(dst_) and binary(/^dst_platform_/) and not binary(=dst_platform_boot) and not binary(=dst_platform_random)
platform-random	test(dst_) and binary(=dst_platform_random)
SUITES
    fi
    if [ "$specs" = true ] && [[ " $packages " != *" temper-cli "* ]]; then # temper-cli's own tests run its verify::
        echo "== Spec Verification (L0-L3)"; cargo test --locked -p temper-cli verify:: || failed="$failed specs;"
    fi
    [ -z "$failed" ] || { echo "failed:$failed"; return 1; }
}
# linux-binary.yml: a push that changes no file the last binary was built from keeps that binary; otherwise build it.
job_linux_binary() {
    export CARGO_TARGET_DIR=$tree/target/release-bin
    local prev=$CI/runs/$base
    if [ -s "$prev/temper-x86_64-linux.inputs" ] && [ -s "$prev/temper-x86_64-linux" ] \
       && ! git diff --name-only "$base" "$commit" | grep -Fxqf "$prev/temper-x86_64-linux.inputs"; then
        cp "$prev"/temper-x86_64-linux{,.sha256,.inputs} "$out/" && echo "main-${base:0:12}" >"$out/kept-from.txt" \
          && echo "no file ${base:0:12}'s binary was built from changed: it stands"
        return
    fi
    GIT_COMMIT_SHA=$commit cargo build --release --locked -p temper-cli || return
    local bin=$CARGO_TARGET_DIR/release/temper dir; dir=$(mktemp -d)
    ! ldd "$bin" | grep -q 'not found' || { ldd "$bin"; return 1; }
    TURSO_URL="file:$dir/smoke.db" TEMPER_API_KEY="$(openssl rand -hex 16)" "$bin" serve --port 3999 --tenant default --storage turso --no-observe >"$dir/serve.log" 2>&1 &
    local pid=$! up=""
    for _ in $(seq 1 120); do curl -fsS http://127.0.0.1:3999/healthz >/dev/null 2>&1 && { up=1; break; }; kill -0 $pid 2>/dev/null || break; sleep 1; done
    [ -n "$up" ] || { tail -40 "$dir/serve.log"; kill $pid 2>/dev/null; return 1; }
    local version; version=$(curl -fsS http://127.0.0.1:3999/version); kill $pid
    echo "temper serves /healthz; /version: $version"
    cp "$bin" "$out/temper-x86_64-linux" && (cd "$out" && sha256sum temper-x86_64-linux >temper-x86_64-linux.sha256)
    # The files it was built from, as linux-binary.yml lists them, so the next push can tell whether it stands.
    local root="$tree/" file d
    perl -pe 's/^[^:]*:\s*//; s/\\ /\x01/g; s/\s+/\n/g; s/\x01/ /g' "$CARGO_TARGET_DIR/release/temper.d" | sed -n "s|^$root||p" >"$dir/read"
    while IFS= read -r file; do d=$(dirname "$file")
        while [ "$d" != . ] && [ ! -f "$d/Cargo.toml" ]; do d=$(dirname "$d"); done; [ "$d" = . ] || echo "$d"
    done <"$dir/read" | sort -u >"$dir/crates"
    { cat "$dir/read"; tr '\n' '\0' <"$dir/crates" | xargs -0 -r git ls-files --
      git ls-files '*Cargo.toml' Cargo.lock rust-toolchain.toml rust-toolchain .cargo .github/workflows/linux-binary.yml ci/box/gate.sh; } \
      | sort -u >"$out/temper-x86_64-linux.inputs"
    echo "built: sha256 $(cut -d' ' -f1 "$out/temper-x86_64-linux.sha256"), $(wc -l <"$out/temper-x86_64-linux.inputs") inputs"
}

run_job() { # <name>: its output to <name>.txt, ending `exit <code>`; a line to the summary
    local name=$1 s=$SECONDS c
    ( set -o pipefail; eval "job_${1//-/_}" ) >"$out/$name.txt" 2>&1 </dev/null; c=$?
    echo "exit $c" >>"$out/$name.txt"
    printf '%-24s exit %-3s %5s s\n' "$name" "$c" "$((SECONDS - s))" | tee -a "$out/summary.txt" >&3
}
# A lane that compiles yields the cores to the tests lane: some tests hold wall-clock budgets (.config/nextest.toml).
lane_run() { local j nice=$1; shift; [ "$nice" = 0 ] || renice -n "$nice" "$BASHPID" >/dev/null; for j in "$@"; do run_job "$j"; done; }

# Fail fast (ci-migration, 25 Nov): once a job fails the verdict is red, so the lanes still running stop then instead of
# running to their end; each failed job's tail still goes in the log below. set -m gives each lane its own process
# group, so the kill reaches every cargo, node and docker client it started. /proc, since a finished lane can linger
# as a zombie until it is waited for.
lanes_wait() {
    local p alive
    while :; do
        alive=(); for p in "$@"; do [ -r "/proc/$p/status" ] && ! grep -q '^State:[[:space:]]*Z' "/proc/$p/status" && alive+=("$p"); done
        [ ${#alive[@]} -gt 0 ] || break
        if awk '$3 != 0 {f = 1} END {exit !f}' "$out/summary.txt"; then
            echo "gate: a job failed, so the lanes still running stop" >&3
            for p in "${alive[@]}"; do kill -TERM -- "-$p" 2>/dev/null; done; break
        fi
        sleep 2
    done
    wait "$@" 2>/dev/null
}

# The pinned nightly (rust-toolchain.toml) once, before the lanes share it.
rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install >/dev/null || { echo "gate: the pinned toolchain did not install"; exit 1; }
lint=() tests=() other=()
[ "$rust" = true ] && lint+=(compile-lint)
[ "$rust" = true ] && tests+=(tests)
[ "$rust" = true ] && [ "$mode" != none ] && other+=(other-workspaces)
[ "$sdk_ts" = true ] && other+=(sdk-ts)
[ "$event" = push ] && other+=(linux-binary)
( peak=0; while sleep 2; do
    used=$(awk '/MemTotal/ {t=$2} /MemAvailable/ {a=$2} END {print int((t - a) / 1024)}' /proc/meminfo)
    [ "$used" -le "$peak" ] || { peak=$used; echo "$peak" >"$out/peak-mem-mb.txt"; }
done ) >/dev/null 2>&1 &
sampler=$!
touch "$out/summary.txt"
set -m
lane_run 10 "${lint[@]}" & l1=$!
lane_run 0 "${tests[@]}" & l2=$!
lane_run 10 "${other[@]}" & l3=$!
set +m
lanes_wait $l1 $l2 $l3
kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null
echo "gate: peak memory in use $(cat "$out/peak-mem-mb.txt" 2>/dev/null || echo 0) MB of $(awk '/MemTotal/ {print int($2 / 1024)}' /proc/meminfo) MB"
verdict=passed
for f in $(awk '$3 != 0 {print $1}' "$out/summary.txt"); do verdict=failed; echo "== $f (last 40 lines)"; tail -40 "$out/$f.txt"; done
echo "gate: $(wc -l <"$out/summary.txt") jobs, verdict $verdict in $((SECONDS - t0)) s"
# A passed push to publish (CI_PUBLISH=1, from route.sh): the binary and its inputs, which route.sh hashes and puts in
# R2 temper-releases. A copy that fails fails the verdict, since the line needs the binary.
if [ $verdict = passed ] && [ "${CI_PUBLISH:-0}" = 1 ]; then
    mkdir -p "$out/publish" && cp "$out"/temper-x86_64-linux{,.inputs} "$out/publish/" \
        || { verdict=failed; echo "gate: no binary was built or kept to publish"; }
fi
echo "$verdict" >"$out/verdict.txt"
[ $verdict = passed ]
