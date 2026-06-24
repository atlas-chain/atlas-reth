#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <arkiv-node-docker-image>" >&2
    exit 2
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="$1"
BIN_DIR="${PAYLOAD_REFERENCE_IMAGE_BIN_DIR:-"${RUNNER_TEMP:-"${ROOT_DIR}/tmp"}/payload-reference-image-binaries"}"
CONTAINER_ID=""

cleanup() {
    if [ -n "$CONTAINER_ID" ]; then
        docker rm -f "$CONTAINER_ID" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

mkdir -p "$BIN_DIR"
rm -f "${BIN_DIR}/arkiv-node" "${BIN_DIR}/arkiv-cli"

echo "[payload-reference-smoke] pulling ${IMAGE}"
docker pull "$IMAGE"

CONTAINER_ID="$(docker create "$IMAGE")"
docker cp "${CONTAINER_ID}:/usr/local/bin/arkiv-node" "${BIN_DIR}/arkiv-node"
docker cp "${CONTAINER_ID}:/usr/local/bin/arkiv-cli" "${BIN_DIR}/arkiv-cli"
chmod +x "${BIN_DIR}/arkiv-node" "${BIN_DIR}/arkiv-cli"

ARKIV_NODE_BIN="${BIN_DIR}/arkiv-node" \
ARKIV_CLI_BIN="${BIN_DIR}/arkiv-cli" \
    "${ROOT_DIR}/scripts/payload-reference-dev-chain-smoke.sh"
