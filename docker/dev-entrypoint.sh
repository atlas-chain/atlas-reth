#!/bin/bash

set -euo pipefail

if [ "${1:-}" = "fund-account" ]; then
    exec /usr/local/bin/fund-account.sh "$@"
fi

GENESIS="${ARKIV_DEV_GENESIS:-/home/docker/genesis.json}"
CHAINSPEC_TEMPLATE="${ARKIV_DEV_CHAINSPEC:-/opt/arkiv/dev.base.json}"
DATADIR="${ARKIV_DEV_DATADIR:-/home/docker/.local/share/atlas-node}"

if [ "${ARKIV_DEV_USE_EXISTING_GENESIS:-false}" = "true" ] && [ -f "$GENESIS" ]; then
    echo "[dev-entrypoint] existing genesis at ${GENESIS} - skipping bootstrap"
else
    if [ -z "$DATADIR" ] || [ "$DATADIR" = "/" ]; then
        echo "[dev-entrypoint] refusing to delete unsafe data dir: ${DATADIR}" >&2
        exit 1
    fi

    echo "[dev-entrypoint] bootstrapping fresh dev chain at ${GENESIS}"
    mkdir -p "$DATADIR"
    echo "[dev-entrypoint] removing previous data dir contents at ${DATADIR}"
    find "$DATADIR" -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +

    cp "$CHAINSPEC_TEMPLATE" "$GENESIS"
    arkiv-cli inject-predeploy "$GENESIS"
    atlas-node init --chain "$GENESIS" --datadir "$DATADIR"
fi

if [ -n "${ARKIV_NODE_CLI:-}" ]; then
    echo "[dev-entrypoint] using ARKIV_NODE_CLI override: ${ARKIV_NODE_CLI}"
    exec sh -c "exec atlas-node ${ARKIV_NODE_CLI}"
else
    exec atlas-node \
        node \
        --datadir "$DATADIR" \
        --chain "$GENESIS" \
        --http \
        --http.addr 0.0.0.0 \
        --http.port 8545 \
        --http.api eth,net,web3,debug \
        --http.corsdomain '*' \
        --ws \
        --ws.addr 0.0.0.0 \
        --ws.port 8546 \
        --ws.api eth,net,web3,debug \
        --ws.origins '*' \
        --dev.block-time 2s \
        --txpool.minimal-protocol-fee 1 \
        --txpool.minimum-priority-fee 0 \
        --dev
fi
