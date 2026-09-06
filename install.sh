#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE="${1:-install}"

die() {
    echo "kilnr install: $*" >&2
    exit 1
}

[[ "${EUID}" -eq 0 ]] || die "run with sudo"
[[ "$MODE" == "install" || "$MODE" == "--update" ]] || die "usage: sudo ./install.sh [--update]"

if [[ "$MODE" == "--update" ]] && systemctl is-active --quiet kilnr-controller.service 2>/dev/null; then
    die "a Kilnr build is currently running; wait for kilnr-controller.service to become inactive"
fi

if [[ "$MODE" == "--update" ]]; then
    systemctl stop kilnr-cleanup.timer kilnr-cleanup.service 2>/dev/null || true
fi

"$ROOT_DIR/libexec/check-platform"

command -v apt-get >/dev/null || die "apt-get not found"

if [[ "$MODE" != "--update" ]]; then
    apt-get update
fi
DEBIAN_FRONTEND=noninteractive apt-get install -y \
    git make acl curl iptables

command -v docker >/dev/null || die "Docker is not installed. Install Docker first; Kilnr will not alter the daemon configuration."
docker info >/dev/null 2>&1 || die "Docker daemon is not reachable"
getent group docker >/dev/null || die "Docker group is missing; this installer expects a rootful Docker Engine"
command -v systemctl >/dev/null || die "systemd is required"
command -v git-shell >/dev/null || die "git-shell is missing"

# Compile the pinned Rust application in a reproducible toolchain container.
docker run --rm \
    -v "$ROOT_DIR:/src" \
    -w /src \
    rust:1.85-bookworm \
    cargo build --locked --release

ensure_group() {
    local name="$1"
    getent group "$name" >/dev/null || groupadd --system "$name"
}

ensure_group git
ensure_group kilnr
ensure_group kilnr-web
ensure_group kilnr-submit
ensure_group kilnr-readers

ensure_user() {
    local name="$1"
    local home="$2"
    local shell="$3"
    local group="$4"

    if getent passwd "$name" >/dev/null; then
        local current_home current_shell
        current_home="$(getent passwd "$name" | cut -d: -f6)"
        current_shell="$(getent passwd "$name" | cut -d: -f7)"
        [[ "$current_home" == "$home" ]] || die "existing user '$name' has home $current_home, expected $home"
        [[ "$current_shell" == "$shell" ]] || die "existing user '$name' has shell $current_shell, expected $shell"
    else
        useradd \
            --system \
            --gid "$group" \
            --home-dir "$home" \
            --no-create-home \
            --shell "$shell" \
            "$name"
    fi
}

ensure_user git /srv/git /usr/bin/git-shell git
ensure_user kilnr /var/lib/kilnr /usr/sbin/nologin kilnr
ensure_user kilnr-web /var/lib/kilnr-web /usr/sbin/nologin kilnr-web

usermod -aG kilnr-submit git
usermod -aG kilnr-submit kilnr
usermod -aG kilnr-readers kilnr-web

if [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]] && getent passwd "${SUDO_USER}" >/dev/null; then
    usermod -aG kilnr-readers "${SUDO_USER}"
fi

install -d -o root -g root -m 0755 /srv/git
install -d -o git -g git -m 0700 /srv/git/.ssh
if [[ ! -e /srv/git/.ssh/authorized_keys ]]; then
    install -o git -g git -m 0600 /dev/null /srv/git/.ssh/authorized_keys
fi
chown git:git /srv/git/.ssh/authorized_keys
chmod 0600 /srv/git/.ssh/authorized_keys

install -d -o root -g root -m 0755 /var/lib/kilnr /var/lib/kilnr/locks
install -d -o root -g kilnr-submit -m 0750 /var/lib/kilnr/locks/projects
install -d -o kilnr -g kilnr-submit -m 0710 /var/lib/kilnr/queue
install -d -o kilnr -g kilnr-submit -m 3730 \
    /var/lib/kilnr/queue/tmp \
    /var/lib/kilnr/queue/incoming
install -d -o kilnr -g kilnr -m 0750 \
    /var/lib/kilnr/queue/running \
    /var/lib/kilnr/builds
install -d -o kilnr -g kilnr -m 0700 \
    /var/lib/kilnr/controller-home \
    /var/lib/kilnr/secret-staging \
    /var/lib/kilnr/job-runtime \
    /var/lib/kilnr/cache

controller_lock="/var/lib/kilnr/locks/controller.lock"
if [[ -e "$controller_lock" || -L "$controller_lock" ]]; then
    [[ -f "$controller_lock" && ! -L "$controller_lock" ]] \
        || die "unsafe controller lock entry: $controller_lock"
    [[ "$(stat -c '%h' "$controller_lock")" == "1" ]] \
        || die "controller lock entry has unexpected links: $controller_lock"
    chown root:kilnr "$controller_lock"
    chmod 0660 "$controller_lock"
else
    install -o root -g kilnr -m 0660 /dev/null "$controller_lock"
fi

install -d -o root -g root -m 0755 /etc/kilnr /etc/kilnr/projects
install -d -o root -g kilnr -m 0750 /etc/kilnr/secrets
for project_config in /etc/kilnr/projects/*.json; do
    [[ -e "$project_config" ]] || continue
    project_name="$(basename "$project_config" .json)"
    [[ "$project_name" =~ ^[a-z0-9][a-z0-9_-]{0,62}$ ]] || continue
    project_lock="/var/lib/kilnr/locks/projects/${project_name}.lock"
    if [[ -e "$project_lock" || -L "$project_lock" ]]; then
        [[ -f "$project_lock" && ! -L "$project_lock" ]] \
            || die "unsafe project lock entry: $project_lock"
        [[ "$(stat -c '%h' "$project_lock")" == "1" ]] \
            || die "project lock entry has unexpected links: $project_lock"
        chown root:kilnr-submit "$project_lock"
        chmod 0660 "$project_lock"
    else
        install -o root -g kilnr-submit -m 0660 /dev/null "$project_lock"
    fi
    install -d -o root -g kilnr -m 0750 "/etc/kilnr/secrets/$project_name"
done
install -d -o root -g root -m 0755 /usr/local/libexec/kilnr /usr/local/libexec/kilnr/git-hooks
# Remove obsolete helper modules from installations created before the Rust
# migration so an update cannot leave an executable legacy implementation.
for obsolete in artifacts.py kilnr_permissions.py kilnr_project_lock.py kilnr_retention.py kilnr_secrets.py pipeline.py; do
    rm -f "/usr/local/libexec/kilnr/$obsolete"
done
# CLI readers can traverse Kilnr state and read build output, but not queue/secrets.
setfacl -m g:kilnr-readers:r-x,d:g:kilnr-readers:r-x /var/lib/kilnr/builds
setfacl -R -m g:kilnr-readers:rX /var/lib/kilnr/builds
setfacl -m u:git:rwx /var/lib/kilnr/queue/incoming
find /var/lib/kilnr/builds -type d -exec chmod g-s {} +

install -o root -g root -m 0755 "$ROOT_DIR/target/release/kilnr" /usr/local/bin/kilnr
install -o root -g root -m 0755 "$ROOT_DIR/target/release/kilnr" /usr/local/libexec/kilnr/kilnr-agent

install -d -o root -g root -m 0755 /usr/local/share/kilnr
rm -rf /usr/local/share/kilnr/web-src
install -d -o root -g root -m 0755 /usr/local/share/kilnr/web-src
cp "$ROOT_DIR/Cargo.toml" "$ROOT_DIR/Cargo.lock" /usr/local/share/kilnr/web-src/
cp -R "$ROOT_DIR/src" "$ROOT_DIR/web/frontend" "$ROOT_DIR/web/Dockerfile" /usr/local/share/kilnr/web-src/
rm -rf \
    /usr/local/share/kilnr/web-src/frontend/node_modules \
    /usr/local/share/kilnr/web-src/frontend/dist
chown -R root:root /usr/local/share/kilnr/web-src
find /usr/local/share/kilnr/web-src -type d -exec chmod 0755 {} +
find /usr/local/share/kilnr/web-src -type f -exec chmod 0644 {} +

for name in \
    controller enqueue execute notify-discord rerun cleanup web \
    project-create project-delete project-lock-run project-rename project-webhook-set \
    secret-set secret-set-file secret-list secret-delete git-key-add config-tool permissions
do
    ln -sfn kilnr-agent "/usr/local/libexec/kilnr/$name"
done

/usr/local/libexec/kilnr/permissions --provision-lock-namespace /var/lib/kilnr
/usr/local/libexec/kilnr/permissions --normalize-builds /var/lib/kilnr/builds

for name in doctor network-setup network-teardown; do
    install -o root -g root -m 0755 "$ROOT_DIR/libexec/$name" "/usr/local/libexec/kilnr/$name"
done

install -o root -g root -m 0755 \
    "$ROOT_DIR/libexec/git-hooks/post-receive" \
    /usr/local/libexec/kilnr/git-hooks/post-receive

if [[ ! -f /etc/kilnr/defaults.json ]]; then
    install -o root -g root -m 0644 "$ROOT_DIR/config/defaults.json" /etc/kilnr/defaults.json
fi

# Configure the isolated CI subnet once. Override on first install with:
#   sudo KILNR_CI_SUBNET=172.31.50.0/24 ./install.sh
if [[ ! -f /etc/kilnr/network.env ]]; then
    SUBNET="${KILNR_CI_SUBNET:-172.30.0.0/24}"
    GATEWAY="$(
        /usr/local/libexec/kilnr/config-tool gateway "$SUBNET"
    )"

    cat >/etc/kilnr/network.env <<EOF
NETWORK=kilnr-ci
BRIDGE=kilnr0
SUBNET=${SUBNET}
GATEWAY=${GATEWAY}
EOF
    chown root:root /etc/kilnr/network.env
    chmod 0644 /etc/kilnr/network.env
fi

for unit in kilnr-controller.service kilnr-queue.path kilnr-network.service kilnr-cleanup.service kilnr-cleanup.timer; do
    install -o root -g root -m 0644 "$ROOT_DIR/systemd/$unit" "/etc/systemd/system/$unit"
done

systemctl daemon-reload
systemctl enable --now kilnr-network.service
systemctl enable --now kilnr-queue.path
systemctl enable --now kilnr-cleanup.timer

echo
echo "Kilnr core installed."
echo
echo "Next:"
echo "  kilnr git-key add"
echo "  kilnr project create my_app"
echo
if [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
    echo "Your user '${SUDO_USER}' was added to kilnr-readers."
    echo "Reconnect your shell before using 'kilnr status' if the group is not visible yet."
fi
echo "Optional web UI:"
echo "  sudo ./install-web.sh kilnr.example.com"
