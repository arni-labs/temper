#!/bin/bash
# Time Genesis registry installs on a running Temper, before and after a kernel
# change, with one instrument: the same app ref into fresh test tenants, timed
# by curl. Infra runs it on staging's main Temper with `before` on the current
# binary and `after` once the change rolls there. The server's own split of
# each install (`install_reconcile` and `total` phase logs) is in its logs for
# the window this script prints.
#
#   measure-genesis-install.sh before|after [after-restart]
#
# Without `after-restart`, each of RUNS fresh tenants `<TENANT_PREFIX>-<label>-<n>`
# gets a first install of APP_REF, then the same ref again. With
# `after-restart`, run once the server has restarted, the same tenants get the
# same ref once more.
#
# With TENANT set, every run installs into that one tenant instead, e.g.
# TENANT=default on a throwaway Temper whose own TEMPER_API_KEY is an operator
# only in "default". Only the first install of a round meets an empty tenant,
# so it alone is `first-install`; every later install is `same-ref-again`.
# Empty the server's data dir between rounds to get a first install again.
# A key answers only in its own tenant (401 elsewhere), so this cannot reach
# a tenant the key does not belong to.
#
# Environment (the key is read from the environment and never printed):
#   TEMPER_URL     base URL of the Temper, e.g. https://<staging main Temper>
#   TEMPER_API_KEY a key that may install into the test tenants
#                  (Cedar install_app) and read their installs
#   REGISTRY_URL   the Genesis registry that Temper is configured with
#                  (TEMPER_GENESIS_REGISTRY_URL; it installs from no other)
#   APP_REF        default temperpaw/paw-patrol@e3cca7fccf9f6e2dee982f6d97ebc94101535257,
#                  the public TemperPaw app with the most specs on production
#                  Genesis (29, 57 with its closure); pass the same app's ref in
#                  the registry the Temper uses if that registry names it otherwise
#   TENANT_PREFIX  default kgtiming; never a person's tenant
#   TENANT         unset by default; one tenant for every run (see above)
#   RUNS           default 3
#
# Kept check verdicts are shared by the whole server, keyed by what a spec's
# checks read, not by tenant: only run 1 of the first `after` round meets
# specs this server has never checked, so it is the cold number; runs 2 and up
# are first installs whose checks are already kept.
#
# Temper has no uninstall route, so the test tenants keep what this installs;
# the script ends by listing each test tenant's installs.
set -euo pipefail

label="${1:-}"
case "$label" in
    before | after) ;;
    *)
        echo "usage: $0 before|after [after-restart]" >&2
        exit 2
        ;;
esac
phase="${2:-first}"
case "$phase" in first | after-restart) ;; *)
    echo "the second argument is after-restart or nothing" >&2
    exit 2
    ;;
esac

: "${TEMPER_URL:?set TEMPER_URL}"
: "${TEMPER_API_KEY:?set TEMPER_API_KEY}"
: "${REGISTRY_URL:?set REGISTRY_URL}"
APP_REF="${APP_REF:-temperpaw/paw-patrol@e3cca7fccf9f6e2dee982f6d97ebc94101535257}"
TENANT_PREFIX="${TENANT_PREFIX:-kgtiming}"
TENANT="${TENANT:-}"
RUNS="${RUNS:-3}"
base="${TEMPER_URL%/}"

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
# The key goes to curl through a file only this user can read, never argv.
umask 077
printf 'Authorization: Bearer %s\n' "$TEMPER_API_KEY" > "$out/auth"

# The tenant run $1 installs into.
tenant_for() {
    if [ -n "$TENANT" ]; then
        echo "$TENANT"
    else
        echo "$TENANT_PREFIX-$label-$1"
    fi
}

# One install of APP_REF into $1. Prints "<http code> <time_total>".
install() {
    local tenant="$1"
    rm -f "$out/answer.json"
    local body
    body=$(printf '{"tenant":"%s","app_ref":"%s","registry_url":"%s","registry_tenant":"default","follow_policy":"pinned"}' \
        "$tenant" "$APP_REF" "$REGISTRY_URL")
    curl -sS -o "$out/answer.json" -w '%{http_code} %{time_total}\n' \
        -H @"$out/auth" -H "X-Tenant-Id: $tenant" -H 'Content-Type: application/json' \
        --max-time 1800 -X POST --data "$body" "$base/api/genesis/apps/install" || true
}

started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "# $label $phase $APP_REF on $base, started $started"
printf 'label\tcase\trun\ttenant\thttp\ttime_total_s\n'
for run in $(seq 1 "$RUNS"); do
    tenant="$(tenant_for "$run")"
    if [ "$phase" = first ] && { [ -z "$TENANT" ] || [ "$run" = 1 ]; }; then
        cases="first-install same-ref-again"
    elif [ "$phase" = first ]; then
        cases="same-ref-again same-ref-again"
    else
        cases="after-restart"
    fi
    for case_name in $cases; do
        read -r code seconds < <(install "$tenant")
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$case_name" "$run" "$tenant" "$code" "$seconds"
        if [ "$code" != 200 ]; then
            echo "# $tenant answered $code: $(head -c 2000 "$out/answer.json" 2>/dev/null)" >&2
        fi
    done
done
ended="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "# ended $ended; the server's install_reconcile and total phase lines for $APP_REF are in its logs between $started and $ended"

echo "# what each test tenant has installed now (no uninstall route exists):"
if [ -n "$TENANT" ]; then listed_runs=1; else listed_runs="$RUNS"; fi
for run in $(seq 1 "$listed_runs"); do
    tenant="$(tenant_for "$run")"
    curl -sS -H @"$out/auth" -H "X-Tenant-Id: $tenant" "$base/api/genesis/apps/installs" \
        | tr '}' '\n' | grep -o '"app_name":"[^"]*"\|"app_ref":"[^"]*"' | paste -sd' ' - \
        | sed "s/^/#   $tenant: /" || echo "#   $tenant: could not be listed"
done
