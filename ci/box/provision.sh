#!/usr/bin/env bash
# Installs on ci-temper (as root) every tool .github/workflows/ci.yml's jobs set up on GitHub's ubuntu-latest runners,
# and the services it ran beside them (postgres:18, redis:7). Idempotent: run again (ci/box/box.sh provision) after a
# version below changes. Tools go where they outlive a sleep (the box's own disk); the gate runs as tl-user, who owns
# /var/lib/ci and its caches. The pinned nightly comes from rust-toolchain.toml at the gate's commit (rustup reads it).
set -euo pipefail
NODE_MAJOR=22                      # actions/setup-node node-version: 22
TURSO_VERSION=v0.7.2               # ci.yml's "Install tursodb for the Turso Sync tests", same sha256
TURSO_SHA256=321e401e44f7ee8ea656caff0f85304025f7e905acafe7bedf6a9304f1af1783
PG_MAJOR=18                        # services.postgres image postgres:18

CI=/var/lib/ci U=tl-user H=/home/tl-user
export DEBIAN_FRONTEND=noninteractive
step() { echo "provision: $*"; }

step apt
apt-get update -q >/dev/null
apt-get install -yq --no-install-recommends build-essential pkg-config libssl-dev libz3-dev clang libclang-dev git jq curl \
    ca-certificates gnupg unzip xz-utils perl openssl iproute2 procps psmisc python3 python3-venv redis-server docker.io >/dev/null
systemctl disable --now docker.service docker.socket >/dev/null 2>&1 || true # the gate starts its own

step "postgresql $PG_MAJOR (PGDG)"
if ! dpkg -s "postgresql-$PG_MAJOR" >/dev/null 2>&1; then
    install -d /usr/share/postgresql-common/pgdg
    curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc https://www.postgresql.org/media/keys/ACCC4CF8.asc
    echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt $(. /etc/os-release; echo "$VERSION_CODENAME")-pgdg main" \
        >/etc/apt/sources.list.d/pgdg.list
    apt-get update -q >/dev/null
    apt-get install -yq --no-install-recommends "postgresql-$PG_MAJOR" >/dev/null
fi
# The gate makes a fresh cluster for each run (as each GitHub job got its own container), so none runs as a service.
systemctl disable --now postgresql redis-server >/dev/null 2>&1 || true

step "node $NODE_MAJOR"
want=$(curl -fsSL https://nodejs.org/dist/index.json | python3 -c "import json,sys; print(next(r['version'] for r in json.load(sys.stdin) if r['version'].startswith('v$NODE_MAJOR.')))")
if [ "$(/opt/ci/node/bin/node --version 2>/dev/null)" != "$want" ]; then
    rm -rf /opt/ci/node.new && mkdir -p /opt/ci/node.new
    curl -fsSL "https://nodejs.org/dist/$want/node-$want-linux-x64.tar.xz" | tar -xJ -C /opt/ci/node.new --strip-components=1
    rm -rf /opt/ci/node && mv /opt/ci/node.new /opt/ci/node
fi

step "mold (latest release, the default linker, as rui314/setup-mold makes it)"
mold_tag=$(curl -fsSL https://api.github.com/repos/rui314/mold/releases/latest | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')
if [ "$(/usr/local/bin/mold --version 2>/dev/null | awk '{print "v"$2}')" != "$mold_tag" ]; then
    curl -fsSL "https://github.com/rui314/mold/releases/download/$mold_tag/mold-${mold_tag#v}-x86_64-linux.tar.gz" | tar -xz -C /usr/local --strip-components=1
fi
ld=$(realpath /usr/bin/ld) # after the first run it already resolves to mold itself
[ "$ld" = /usr/local/bin/mold ] || ln -sf /usr/local/bin/mold "$ld"

step "tursodb $TURSO_VERSION"
if [ ! -x /opt/ci/turso/tursodb ]; then
    curl -fsSL -o /tmp/tursodb.tar.xz "https://github.com/tursodatabase/turso/releases/download/$TURSO_VERSION/turso_cli-x86_64-unknown-linux-gnu.tar.xz"
    echo "$TURSO_SHA256  /tmp/tursodb.tar.xz" | sha256sum -c -
    mkdir -p /opt/ci/turso && tar -xJf /tmp/tursodb.tar.xz -C /opt/ci/turso --strip-components=1 && rm /tmp/tursodb.tar.xz
fi

mkdir -p "$CI" && chown "$U:$U" "$CI" /opt/ci
cat >/opt/ci/env.sh <<ENV
# Sourced by the gate: the tools provision.sh installed, first on PATH, and ci.yml's env.
export RUSTUP_HOME=/opt/ci/rustup CARGO_HOME=/opt/ci/cargo
export PATH=/opt/ci/node/bin:/opt/ci/cargo/bin:/usr/lib/postgresql/$PG_MAJOR/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export CARGO_TERM_COLOR=never RUST_BACKTRACE=1 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
export CARGO_UNSTABLE_FEATURE_UNIFICATION=true CARGO_RESOLVER_FEATURE_UNIFICATION=workspace
export TEMPER_TEST_TURSODB=/opt/ci/turso/tursodb
ENV
chown -R "$U:$U" /opt/ci

step "rustup, stable (affected.py's cargo metadata) and cargo-nextest (latest, as taiki-e/install-action takes it)"
sudo -u "$U" -H bash -c '
    set -euo pipefail; . /opt/ci/env.sh
    command -v rustup >/dev/null || curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --no-modify-path --profile minimal --default-toolchain none
    rustup toolchain install stable --profile minimal >/dev/null
    rustup default stable >/dev/null
    curl -fsSL https://get.nexte.st/latest/linux | tar -xz -C "$CARGO_HOME/bin"
    cargo nextest --version | head -1'
step "done: node $(/opt/ci/node/bin/node --version), $("/usr/lib/postgresql/$PG_MAJOR/bin/postgres" --version), $(redis-server --version | cut -d' ' -f3), $(/usr/local/bin/mold --version)"
