#!/usr/bin/env bash
set -Eeuo pipefail

[[ "${EUID}" -eq 0 ]] || { echo "run with sudo" >&2; exit 1; }

KILNR_ROOT="/opt/kilnr"
KILNR_COMPOSE="${KILNR_ROOT}/docker-compose.yml"
NETWORK="${KILNR_PROXY_NETWORK:-kilnr-proxy}"
CADDY_CONTAINER="${KILNR_CADDY_CONTAINER:-caddy}"

CADDY_SERVICE="$(
    docker inspect "$CADDY_CONTAINER"         --format '{{ index .Config.Labels "com.docker.compose.service" }}'         2>/dev/null || true
)"
[[ -n "$CADDY_SERVICE" ]] || CADDY_SERVICE="$CADDY_CONTAINER"

if [[ -f "$KILNR_COMPOSE" ]]; then
    docker compose -f "$KILNR_COMPOSE" down || true
fi

CADDY_WORKDIR="$(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{ index .Config.Labels "com.docker.compose.project.working_dir" }}' \
        2>/dev/null || true
)"
CADDYFILE="$(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{range .Mounts}}{{if eq .Destination "/etc/caddy/Caddyfile"}}{{.Source}}{{end}}{{end}}' \
        2>/dev/null || true
)"

if [[ -n "$CADDYFILE" && -f "$CADDYFILE" ]]; then
    if [[ -x /usr/local/libexec/kilnr/config-tool ]]; then
        /usr/local/libexec/kilnr/config-tool strip-caddy "$CADDYFILE"
    else
        sed -i '/# BEGIN KILNR/,/# END KILNR/d' "$CADDYFILE"
    fi
fi

if [[ -n "$CADDY_WORKDIR" ]]; then
    override="${CADDY_WORKDIR}/docker-compose.override.yml"
    if [[ -f "$override" ]] && grep -q '# KILNR MANAGED OVERRIDE' "$override"; then
        rm -f "$override"
    fi
    (cd "$CADDY_WORKDIR" && docker compose up -d "$CADDY_SERVICE") || true
fi

docker image rm kilnr-web:local >/dev/null 2>&1 || true
docker network rm "$NETWORK" >/dev/null 2>&1 || true
rm -f /etc/kilnr/web.json

echo "Kilnr Web removed. /opt/kilnr backups were preserved."
