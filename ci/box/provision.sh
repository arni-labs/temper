#!/usr/bin/env bash
# Installs on ci-temper (as root) every tool .github/workflows/ci.yml's jobs set up on GitHub's ubuntu-latest runners,
# and the services it ran beside them (postgres:18, redis:7). Idempotent: run again (ci/box/box.sh provision) after a
# version below changes. Tools go where they outlive a sleep (the box's own disk). The pinned nightly comes from
# rust-toolchain.toml at the gate's commit (rustup reads it).
# The box's users (ci-migration, Forum r_17qbqd5ah0xf7940: no code from a commit runs as a user that can read a key):
#   root     /opt/ci/route.sh, the fixed wrapper (from /tmp/ci-route.sh, which box.sh provision uploads), keys/,
#            repo.git, box.lock, logs/; /opt/ci (node, tursodb) is root's and read-only to the others.
#            in/ is tl-user's, for box.sh's uploads.
#   ci-test  runs the commit's gate.sh and every job: no sudo, no read on keys/. Its work, rustup and cargo homes are
#            in /var/lib/ci/work, and Docker is its own rootless dockerd (data in ~ci-test/.local/share/docker).
#   tl-user  the route's entry: its sudo is `sudo -n /opt/ci/route.sh` and nothing else
set -euo pipefail
NODE_MAJOR=22                      # actions/setup-node node-version: 22
TURSO_VERSION=v0.7.2               # ci.yml's "Install tursodb for the Turso Sync tests", same sha256
TURSO_SHA256=321e401e44f7ee8ea656caff0f85304025f7e905acafe7bedf6a9304f1af1783
PG_MAJOR=18                        # services.postgres image postgres:18
MOBY=fbf3ed25f893e6ce21336f1101590e40a13934f4 # docker-v29.1.3, the docker.io here: contrib/dockerd-rootless.sh
ROOTLESS_SHA256=8eb00d61b5e78ffc0dcd628f056389b5859cc61d6fe3ce17b4a3a4b08143c8a5

CI=/var/lib/ci T=ci-test W=/var/lib/ci/work
export DEBIAN_FRONTEND=noninteractive
step() { echo "provision: $*"; }

step apt
apt-get update -q >/dev/null
apt-get install -yq --no-install-recommends build-essential pkg-config libssl-dev libz3-dev clang libclang-dev git jq curl \
    ca-certificates gnupg unzip xz-utils perl openssl iproute2 procps psmisc python3 python3-venv redis-server docker.io \
    rootlesskit uidmap slirp4netns >/dev/null
systemctl disable --now docker.service docker.socket >/dev/null 2>&1 || true # the gate starts its own
pkill -x dockerd 2>/dev/null || true # a rootful one from before the wrapper

step "users"
getent passwd "$T" >/dev/null || useradd -m -s /bin/bash "$T"
grep -q "^$T:" /etc/subuid || echo "$T:200000:65536" >>/etc/subuid
grep -q "^$T:" /etc/subgid || echo "$T:200000:65536" >>/etc/subgid
s=$(mktemp); echo "tl-user ALL=(root) NOPASSWD: /opt/ci/route.sh" >"$s"
visudo -cqf "$s" && for f in $(grep -l '^tl-user' /etc/sudoers.d/* 2>/dev/null); do rm -f "$f"; done && install -m 440 "$s" /etc/sudoers.d/ci-route; rm -f "$s"
gpasswd -d tl-user sudo >/dev/null 2>&1 || true
step "rootless docker ($MOBY)"
curl -fsSL -o /tmp/dockerd-rootless.sh "https://raw.githubusercontent.com/moby/moby/$MOBY/contrib/dockerd-rootless.sh"
echo "$ROOTLESS_SHA256  /tmp/dockerd-rootless.sh" | sha256sum -c - >/dev/null && install -m 755 /tmp/dockerd-rootless.sh /usr/local/bin/dockerd-rootless.sh

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
# The latest release's tag from where github.com redirects it: the API's unauthenticated limit answers non-JSON at times.
mold_tag=$(curl -fsSI https://github.com/rui314/mold/releases/latest | sed -n 's|^location: .*/tag/\([^[:space:]]*\).*|\1|Ip')
[ -n "$mold_tag" ] || { echo "provision: no latest mold release found"; exit 1; }
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

install -m 755 /tmp/ci-route.sh /opt/ci/route.sh
cat >/opt/ci/env.sh <<ENV
# Sourced by the gate: the tools provision.sh installed, first on PATH, and ci.yml's env.
export RUSTUP_HOME=$W/rustup CARGO_HOME=$W/cargo
export PATH=/opt/ci/node/bin:$W/cargo/bin:/usr/lib/postgresql/$PG_MAJOR/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export CARGO_TERM_COLOR=never RUST_BACKTRACE=1 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_PROFILE_TEST_DEBUG=line-tables-only
export CARGO_UNSTABLE_FEATURE_UNIFICATION=true CARGO_RESOLVER_FEATURE_UNIFICATION=workspace
export TEMPER_TEST_TURSODB=/opt/ci/turso/tursodb
ENV

step layout
install -d -o root -g root -m 755 "$CI" "$CI/logs" && install -d -o root -g root -m 700 "$CI/keys"
chown -R root:root "$CI/keys" "$CI/logs" && find "$CI/keys" -type f ! -name '*.pub' -exec chmod 600 {} +
install -d -o "$T" -g "$T" -m 755 "$W" && install -d -o tl-user -g tl-user -m 700 "$CI/in"
# tl-user's warm state from before the wrapper moves to ci-test (a rename, so target/ and the registry stay): rustup
# and cargo homes (nextest with them), both trees and their clone, which becomes ci-test's test.git.
for d in /opt/ci/rustup:rustup /opt/ci/cargo:cargo "$CI/repo.git:test.git" "$CI/tree:tree" "$CI/tree-ws:tree-ws" "$CI/runs:runs"; do
    [ -e "${d%%:*}" ] && [ ! -e "$W/${d#*:}" ] && mv "${d%%:*}" "$W/${d#*:}"
done
rm -rf "$CI/gate.sh" "$CI"/gate-*.sh "$CI/pgrun" "$CI/docker.pid" "$CI/docker.sock" "$CI/.pgpw"
# Tests from tl-user's gates left fixed-name paths in /tmp (/tmp/crucible_test_write, ...) that ci-test can't replace.
find /tmp -mindepth 1 -maxdepth 1 -user tl-user -exec rm -rf {} +
chown -R "$T:$T" "$W" "/home/$T"
[ ! -d "$CI/repo.git" ] || chown -R root:root "$CI/repo.git"
[ ! -e "$CI/box.lock" ] || chown root:root "$CI/box.lock"
chown -R root:root /opt/ci && chmod -R go-w /opt/ci
# ci-test fetches the commits from root's repo.git, which git otherwise refuses as another user's (safe.directory).
git config --system --get-all safe.directory | grep -qxF "$CI/repo.git" || git config --system --add safe.directory "$CI/repo.git"

step "rustup, stable (affected.py's cargo metadata) and cargo-nextest (latest, as taiki-e/install-action takes it), as $T"
runuser -u "$T" -- env -i HOME="/home/$T" PATH=/usr/bin:/bin bash -c '
    set -euo pipefail; . /opt/ci/env.sh
    [ ! -d '"$W"'/test.git ] || git -C '"$W"'/test.git worktree repair '"$W"'/tree '"$W"'/tree-ws 2>/dev/null || true
    command -v rustup >/dev/null || curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --no-modify-path --profile minimal --default-toolchain none
    rustup toolchain install stable --profile minimal >/dev/null
    rustup default stable >/dev/null
    curl -fsSL https://get.nexte.st/latest/linux | tar -xz -C "$CARGO_HOME/bin"
    cargo nextest --version | head -1'
step "done: node $(/opt/ci/node/bin/node --version), $("/usr/lib/postgresql/$PG_MAJOR/bin/postgres" --version), $(redis-server --version | cut -d' ' -f3), $(/usr/local/bin/mold --version)," \
    "route.sh $(sha256sum /opt/ci/route.sh | cut -c1-12), sudo for tl-user: $(sudo -l -U tl-user | sed -n '/may run/,$p' | tail -n +2 | xargs)," \
    "$T: $(runuser -u "$T" -- sh -c 'ls /var/lib/ci/keys >/dev/null 2>&1 && echo READS-KEYS || echo no-keys; sudo -n true 2>/dev/null && echo SUDO || echo no-sudo' | xargs)"
