#!/usr/bin/env bash
set -Eeuo pipefail

die() {
    echo "kilnr web install: $*" >&2
    exit 1
}

[[ "${EUID}" -eq 0 ]] || die "run with sudo"
[[ "$#" -eq 1 ]] || die "usage: sudo ./install-web.sh <domain>"

DOMAIN="$1"

[[ "$DOMAIN" =~ ^[A-Za-z0-9.-]+$ ]] \
    || die "invalid domain"

CADDY_CONTAINER="${KILNR_CADDY_CONTAINER:-caddy}"
NETWORK="${KILNR_PROXY_NETWORK:-kilnr-proxy}"

KILNR_ROOT="/opt/kilnr"
KILNR_COMPOSE="${KILNR_ROOT}/docker-compose.yml"

WEB_SOURCE="/usr/local/share/kilnr/web-src"
WEB_CONFIG="/etc/kilnr/web.json"

AUTH_USER="${KILNR_WEB_USER:-kilnr}"


#
# Requirements
#

command -v docker >/dev/null \
    || die "docker not found"

command -v python3 >/dev/null \
    || die "python3 not found"
docker compose version >/dev/null 2>&1 \
    || die "docker compose plugin unavailable"

[[ -f "$WEB_SOURCE/Dockerfile" ]] \
    || die "Kilnr web source is not installed; run install.sh first"
docker inspect "$CADDY_CONTAINER" >/dev/null 2>&1 \
    || die "running Caddy container '$CADDY_CONTAINER' not found"


#
# Discover the existing Caddy Compose project.
#

CADDY_WORKDIR="$(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{ index .Config.Labels "com.docker.compose.project.working_dir" }}'
)"

CADDY_COMPOSE_FILES="$(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{ index .Config.Labels "com.docker.compose.project.config_files" }}'
)"

CADDY_SERVICE="$(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{ index .Config.Labels "com.docker.compose.service" }}'
)"

[[ -n "$CADDY_SERVICE" ]] \
    || CADDY_SERVICE="$CADDY_CONTAINER"

[[ -n "$CADDY_WORKDIR" ]] \
    || die "cannot determine Caddy Compose working directory"

[[ -n "$CADDY_COMPOSE_FILES" ]] \
    || die "cannot determine Caddy Compose file"


#
# Docker Compose can report multiple files separated by commas.
#
# The first one is the base compose file. Kilnr installs a conventional
# docker-compose.override.yml beside it so future normal:
#
#     docker compose up -d
#
# invocations keep the Kilnr proxy network.
#

CADDY_COMPOSE="${CADDY_COMPOSE_FILES%%,*}"

CADDY_OVERRIDE="$(
    printf '%s/docker-compose.override.yml' \
        "$CADDY_WORKDIR"
)"


#
# Remember every network Caddy currently has.
#
# After installation we verify that all of them still exist. Kilnr must
# never disconnect Caddy from Jellyfin, Seerr, or any unrelated service.
#
# Docker's Go template may emit a final blank line; filter it out.
#

mapfile -t CADDY_ORIGINAL_NETWORKS < <(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{range $name, $_ := .NetworkSettings.Networks}}{{$name}}{{println}}{{end}}' \
    | sed '/^[[:space:]]*$/d'
)

[[ "${#CADDY_ORIGINAL_NETWORKS[@]}" -gt 0 ]] \
    || die "Caddy has no Docker networks"


#
# Locate the host Caddyfile.
#

CADDYFILE="$(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{range .Mounts}}{{if eq .Destination "/etc/caddy/Caddyfile"}}{{.Source}}{{end}}{{end}}'
)"

[[ -f "$CADDYFILE" ]] \
    || die "cannot locate host Caddyfile mount"


#
# Never overwrite someone else's existing Compose override.
#

if [[ -f "$CADDY_OVERRIDE" ]] \
    && ! grep -q '# KILNR MANAGED OVERRIDE' "$CADDY_OVERRIDE"
then
    die "$CADDY_OVERRIDE already exists and is not managed by Kilnr; integrate kilnr-proxy manually"
fi


#
# Runtime identities used by kilnr-web.
#

KILNR_WEB_UID="$(
    id -u kilnr-web
)"

KILNR_WEB_GID="$(
    id -g kilnr-web
)"

KILNR_READERS_GID="$(
    getent group kilnr-readers \
        | cut -d: -f3
)"

mkdir -p "$KILNR_ROOT"


#
# Shared Caddy <-> Kilnr network.
#
# It is internal because kilnr-web itself does not need Internet access.
#

if ! docker network inspect "$NETWORK" >/dev/null 2>&1
then
    docker network create \
        --driver bridge \
        --internal \
        "$NETWORK" \
        >/dev/null
fi

[[ "$(
    docker network inspect "$NETWORK" \
        --format '{{.Internal}}'
)" == "true" ]] \
    || die "existing network $NETWORK is not internal"


#
# Stage generated configuration before modifying the live installation.
#

TMP_DIR="$(
    mktemp -d
)"

trap 'rm -rf "$TMP_DIR"' EXIT

TMP_OVERRIDE="${TMP_DIR}/docker-compose.override.yml"
TMP_COMPOSE="${TMP_DIR}/kilnr-compose.yml"
TMP_CADDYFILE="${TMP_DIR}/Caddyfile"


#
# IMPORTANT:
#
# Keep Caddy on its Compose default network AND add kilnr_proxy.
#
# An earlier Kilnr 0.1.0 template declared only kilnr_proxy here.
# Docker Compose consequently recreated Caddy without its original default
# network, breaking reverse proxies such as:
#
#     reverse_proxy jellyfin:8096
#
# The explicit `default` entry below is therefore reliability-critical.
#

cat >"$TMP_OVERRIDE" <<EOF
# KILNR MANAGED OVERRIDE
services:
  ${CADDY_SERVICE}:
    networks:
      default:
      kilnr_proxy:

networks:
  kilnr_proxy:
    external: true
    name: ${NETWORK}
EOF


#
# Kilnr Web has its own Compose project.
#
# It publishes no host port. Caddy reaches it only through kilnr-proxy.
#

cat >"$TMP_COMPOSE" <<EOF
services:
  kilnr-web:
    build:
      context: ${WEB_SOURCE}
      dockerfile: Dockerfile

    image: kilnr-web:local
    container_name: kilnr-web

    restart: unless-stopped
    init: true

    user: "${KILNR_WEB_UID}:${KILNR_WEB_GID}"

    group_add:
      - "${KILNR_READERS_GID}"

    environment:
      KILNR_WEB_HOST: "0.0.0.0"
      KILNR_WEB_PORT: "8088"
      KILNR_WEB_STATIC: "/opt/kilnr/static"

    volumes:
      - type: bind
        source: /var/lib/kilnr/builds
        target: /var/lib/kilnr/builds
        read_only: true

    read_only: true

    tmpfs:
      - /tmp:size=16m,mode=1777

    cap_drop:
      - ALL

    security_opt:
      - no-new-privileges:true

    pids_limit: 128
    mem_limit: 128m
    cpus: 0.50

    healthcheck:
      test:
        - CMD
        - python3
        - -c
        - "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8088/healthz', timeout=2).read()"

      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 5s

    networks:
      - kilnr_proxy

networks:
  kilnr_proxy:
    external: true
    name: ${NETWORK}
EOF


#
# Authentication
#

echo "Choose the password for https://${DOMAIN}"
echo "Username: ${AUTH_USER}"

read -rsp "Password: " PASSWORD
echo

read -rsp "Confirm password: " PASSWORD2
echo

[[ -n "$PASSWORD" ]] \
    || die "password cannot be empty"

[[ "$PASSWORD" == "$PASSWORD2" ]] \
    || die "passwords do not match"


#
# Hash the password with the actual Caddy installation already in use.
#
# The clear-text password is passed through the container environment only
# for this short-lived command and is never written into the Caddyfile.
#

CADDY_HASH="$(
    docker exec \
        -e KILNR_PASSWORD="$PASSWORD" \
        "$CADDY_CONTAINER" \
        sh -c \
        'caddy hash-password --plaintext "$KILNR_PASSWORD"'
)"

unset PASSWORD PASSWORD2

[[ -n "$CADDY_HASH" ]] \
    || die "Caddy generated an empty password hash"


#
# Generate the Kilnr Caddy block while preserving every existing site.
#

cp "$CADDYFILE" "$TMP_CADDYFILE"


write_caddy_block() {
    local directive="$1"

    DOMAIN="$DOMAIN" \
    AUTH_USER="$AUTH_USER" \
    CADDY_HASH="$CADDY_HASH" \
    AUTH_DIRECTIVE="$directive" \
    CADDYFILE="$TMP_CADDYFILE" \
    python3 <<'PY'
import os
import re
from pathlib import Path

path = Path(
    os.environ["CADDYFILE"]
)

text = path.read_text(
    encoding="utf-8"
)

#
# Kilnr owns only the marked section.
#

text = re.sub(
    r"\n?# BEGIN KILNR\n.*?# END KILNR\n?",
    "\n",
    text,
    flags=re.DOTALL,
)

block = f"""# BEGIN KILNR
{os.environ['DOMAIN']} {{
    @health path /healthz
    handle @health {{
        reverse_proxy kilnr-web:8088
    }}

    handle {{
        {os.environ['AUTH_DIRECTIVE']} {{
            {os.environ['AUTH_USER']} {os.environ['CADDY_HASH']}
        }}

        encode zstd gzip
        reverse_proxy kilnr-web:8088
    }}
}}
# END KILNR
"""

path.write_text(
    text.rstrip()
    + "\n\n"
    + block,
    encoding="utf-8",
)
PY
}


CADDY_IMAGE="$(
    docker inspect "$CADDY_CONTAINER" \
        --format '{{.Config.Image}}'
)"


validate_caddy() {
    docker run \
        --rm \
        -v "${TMP_CADDYFILE}:/etc/caddy/Caddyfile:ro" \
        "$CADDY_IMAGE" \
        caddy validate \
        --config /etc/caddy/Caddyfile \
        >/dev/null 2>&1
}


#
# New Caddy releases use basic_auth.
# Older supported releases used basicauth.
#

write_caddy_block "basic_auth"

if ! validate_caddy
then
    cp "$CADDYFILE" "$TMP_CADDYFILE"

    write_caddy_block "basicauth"

    validate_caddy || {
        docker run \
            --rm \
            -v "${TMP_CADDYFILE}:/etc/caddy/Caddyfile:ro" \
            "$CADDY_IMAGE" \
            caddy validate \
            --config /etc/caddy/Caddyfile

        die "generated Caddyfile is invalid"
    }
fi


#
# Validate both Compose configurations BEFORE replacing live files.
#

docker compose \
    -f "$CADDY_COMPOSE" \
    -f "$TMP_OVERRIDE" \
    config \
    >/dev/null

docker compose \
    -f "$TMP_COMPOSE" \
    config \
    >/dev/null


#
# Regression guard:
#
# Inspect the fully merged Compose configuration and ensure the Caddy
# service still contains both:
#
#   default
#   kilnr_proxy
#

MERGED_COMPOSE_JSON="${TMP_DIR}/merged-compose.json"

docker compose \
    -f "$CADDY_COMPOSE" \
    -f "$TMP_OVERRIDE" \
    config \
    --format json \
    >"$MERGED_COMPOSE_JSON"


MERGED_CADDY_NETWORKS="$(
    CADDY_SERVICE="$CADDY_SERVICE" \
    python3 - "$MERGED_COMPOSE_JSON" <<'PY'
import json
import os
import sys
from pathlib import Path

path = Path(sys.argv[1])

with path.open(
    "r",
    encoding="utf-8",
) as f:
    data = json.load(f)

service = os.environ["CADDY_SERVICE"]

try:
    networks = data["services"][service]["networks"]
except (KeyError, TypeError):
    raise SystemExit(
        "cannot determine merged Caddy networks"
    )

if isinstance(networks, dict):
    names = networks.keys()
elif isinstance(networks, list):
    names = networks
else:
    raise SystemExit(
        "invalid merged Caddy networks"
    )

print(
    "\n".join(
        sorted(names)
    )
)
PY
)"


grep -Fxq "default" <<<"$MERGED_CADDY_NETWORKS" \
    || die "generated Compose configuration would disconnect Caddy from its default network"

grep -Fxq "kilnr_proxy" <<<"$MERGED_CADDY_NETWORKS" \
    || die "generated Compose configuration does not attach Caddy to kilnr_proxy"


#
# Backups
#

BACKUP_DIR="${KILNR_ROOT}/backups/$(date +%Y%m%d-%H%M%S)"

mkdir -p "$BACKUP_DIR"

cp \
    "$CADDYFILE" \
    "$BACKUP_DIR/Caddyfile"

if [[ -f "$CADDY_OVERRIDE" ]]
then
    cp \
        "$CADDY_OVERRIDE" \
        "$BACKUP_DIR/docker-compose.override.yml"
fi

if [[ -f "$KILNR_COMPOSE" ]]
then
    cp \
        "$KILNR_COMPOSE" \
        "$BACKUP_DIR/docker-compose.yml"
fi


#
# Install generated files.
#

install \
    -o root \
    -g root \
    -m 0644 \
    "$TMP_OVERRIDE" \
    "$CADDY_OVERRIDE"

install \
    -o root \
    -g root \
    -m 0644 \
    "$TMP_COMPOSE" \
    "$KILNR_COMPOSE"

install \
    -o root \
    -g root \
    -m 0644 \
    "$TMP_CADDYFILE" \
    "$CADDYFILE"


cat >"$WEB_CONFIG" <<EOF
{
  "public_url": "https://${DOMAIN}"
}
EOF

chown root:root "$WEB_CONFIG"
chmod 0644 "$WEB_CONFIG"


#
# Start Kilnr Web first.
#

docker compose \
    -f "$KILNR_COMPOSE" \
    up -d --build


#
# Recreate/update Caddy using its original Compose project.
#

(
    cd "$CADDY_WORKDIR"

    docker compose \
        up -d \
        "$CADDY_SERVICE"
)


#
# Wait for kilnr-web.
#

for _ in $(seq 1 30)
do
    HEALTH="$(
        docker inspect kilnr-web \
            --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' \
            2>/dev/null \
            || true
    )"

    [[ "$HEALTH" == "healthy" ]] \
        && break

    sleep 1
done


HEALTH="$(
    docker inspect kilnr-web \
        --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}'
)"

[[ "$HEALTH" == "healthy" ]] \
    || die "kilnr-web did not become healthy (status: $HEALTH)"


#
# Verify Kilnr's shared network.
#

[[ "$(
    docker inspect "$CADDY_CONTAINER" \
        --format "{{if index .NetworkSettings.Networks \"${NETWORK}\"}}yes{{else}}no{{end}}"
)" == "yes" ]] \
    || die "Caddy is not connected to $NETWORK"

[[ "$(
    docker inspect kilnr-web \
        --format "{{if index .NetworkSettings.Networks \"${NETWORK}\"}}yes{{else}}no{{end}}"
)" == "yes" ]] \
    || die "kilnr-web is not connected to $NETWORK"


#
# Most importantly: verify that every network Caddy had BEFORE Kilnr was
# installed still exists AFTER the Compose recreation.
#

for original_network in "${CADDY_ORIGINAL_NETWORKS[@]}"
do
    [[ -n "$original_network" ]] || continue

    [[ "$(
        docker inspect "$CADDY_CONTAINER" \
            --format "{{if index .NetworkSettings.Networks \"${original_network}\"}}yes{{else}}no{{end}}"
    )" == "yes" ]] \
        || die "Caddy lost its pre-existing Docker network: ${original_network}"
done


echo
echo "Kilnr Web installed."
echo "URL:      https://${DOMAIN}"
echo "Username: ${AUTH_USER}"
echo "Compose:  ${KILNR_COMPOSE}"
echo "Caddy:    ${CADDYFILE}"
echo "Backup:   ${BACKUP_DIR}"