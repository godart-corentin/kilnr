#!/usr/bin/env bash
set -Eeuo pipefail
umask 0022

ROOT_DIR="$(
    cd "$(dirname "${BASH_SOURCE[0]}")/.." \
    && pwd
)"

cargo test --manifest-path "$ROOT_DIR/Cargo.toml" --all-targets --locked
cargo clippy --manifest-path "$ROOT_DIR/Cargo.toml" --all-targets --locked -- -D warnings
cargo fmt --manifest-path "$ROOT_DIR/Cargo.toml" --check
npm --prefix "$ROOT_DIR/web/frontend" test
npm --prefix "$ROOT_DIR/web/frontend" run build

for unit in \
    "$ROOT_DIR/systemd/kilnr-controller.service" \
    "$ROOT_DIR/systemd/kilnr-queue.path" \
    "$ROOT_DIR/systemd/kilnr-network.service"
do
    grep -q \
        '^\[Unit\]$' \
        "$unit"
done

echo "OK systemd unit sections"
