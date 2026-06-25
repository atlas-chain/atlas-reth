#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PAYLOAD_PROVIDER_DIR="${PAYLOAD_PROVIDER_DIR:-"${ROOT_DIR}/../atlas-payload-provider"}"
WORK_DIR="${PAYLOAD_REFERENCE_SMOKE_DIR:-"${ROOT_DIR}/tmp/payload-reference-smoke"}"

ARKIV_NODE_BIN="${ARKIV_NODE_BIN:-"${ROOT_DIR}/target/debug/arkiv-node"}"
ARKIV_CLI_BIN="${ARKIV_CLI_BIN:-"${ROOT_DIR}/target/debug/arkiv-cli"}"
PAYLOAD_PROVIDER_BIN="${PAYLOAD_PROVIDER_BIN:-"${PAYLOAD_PROVIDER_DIR}/target/debug/atlas-payload-provider"}"

RPC_HOST="${RPC_HOST:-127.0.0.1}"
RPC_PORT="${RPC_PORT:-18545}"
WS_PORT="${WS_PORT:-18546}"
RPC_URL="http://${RPC_HOST}:${RPC_PORT}"

PROVIDER_HOST="${PROVIDER_HOST:-127.0.0.1}"
PROVIDER_PORT="${PROVIDER_PORT:-28884}"
PAYLOAD_PROVIDER_URL="http://${PROVIDER_HOST}:${PROVIDER_PORT}"

CONTENT_TYPE="application/vnd.atlas.payload-reference+json"
PAYLOAD_REFERENCE_NONCE="${PAYLOAD_REFERENCE_NONCE:-0x0000000000000000000000000000000000000000000000000000000000000001}"
PAYLOAD_REFERENCE_PAYMENT="${PAYLOAD_REFERENCE_PAYMENT:-100000}"
DEV_PRIVATE_KEY="${DEV_PRIVATE_KEY:-ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
PROVIDER_SIGNER_PRIVATE_KEY="${PROVIDER_SIGNER_PRIVATE_KEY:-0x0000000000000000000000000000000000000000000000000000000000000001}"
PROVIDER_SIGNER_ADDRESS="0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
INGRESS_BEARER_KEY="${INGRESS_BEARER_KEY:-ci-payload-reference-key}"

NODE_PID=""
PROVIDER_PID=""

log() {
    printf '[payload-reference-smoke] %s\n' "$*"
}

dump_logs() {
    local status="$1"
    if [ "$status" -eq 0 ]; then
        return
    fi
    for file in "${WORK_DIR}/payload-provider.log" "${WORK_DIR}/arkiv-node.log"; do
        if [ -f "$file" ]; then
            printf '\n===== %s =====\n' "$file" >&2
            tail -200 "$file" >&2 || true
        fi
    done
}

cleanup() {
    local status=$?
    dump_logs "$status"
    if [ -n "$NODE_PID" ] && kill -0 "$NODE_PID" 2>/dev/null; then
        kill "$NODE_PID" 2>/dev/null || true
    fi
    if [ -n "$PROVIDER_PID" ] && kill -0 "$PROVIDER_PID" 2>/dev/null; then
        kill "$PROVIDER_PID" 2>/dev/null || true
    fi
    wait "$NODE_PID" 2>/dev/null || true
    wait "$PROVIDER_PID" 2>/dev/null || true
}
trap cleanup EXIT

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required tool: $1" >&2
        exit 1
    fi
}

ensure_binary() {
    local bin="$1"
    local manifest="$2"
    local package="$3"
    if [ -x "$bin" ]; then
        return
    fi
    log "building missing binary ${package}"
    cargo build --manifest-path "$manifest" --bin "$package"
}

wait_for_http() {
    local label="$1"
    local url="$2"
    local pid="$3"
    for _ in $(seq 1 120); do
        if curl -fsS "$url" >/dev/null 2>&1; then
            return
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "${label} exited before becoming ready" >&2
            exit 1
        fi
        sleep 1
    done
    echo "timed out waiting for ${label} at ${url}" >&2
    exit 1
}

wait_for_rpc() {
    local pid="$1"
    local request='{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
    for _ in $(seq 1 180); do
        if curl -fsS -H 'content-type: application/json' --data "$request" "$RPC_URL" 2>/dev/null \
            | jq -e '.result == "0x539"' >/dev/null 2>&1; then
            return
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "arkiv-node exited before RPC became ready" >&2
            exit 1
        fi
        sleep 1
    done
    echo "timed out waiting for arkiv-node RPC at ${RPC_URL}" >&2
    exit 1
}

json_rpc() {
    curl -fsS -H 'content-type: application/json' --data "$1" "$RPC_URL"
}

require_tool cargo
require_tool curl
require_tool jq
require_tool base64

if [ ! -d "$PAYLOAD_PROVIDER_DIR" ]; then
    echo "payload provider repo not found at ${PAYLOAD_PROVIDER_DIR}" >&2
    exit 1
fi

ensure_binary "$ARKIV_NODE_BIN" "${ROOT_DIR}/Cargo.toml" arkiv-node
ensure_binary "$ARKIV_CLI_BIN" "${ROOT_DIR}/Cargo.toml" arkiv-cli
ensure_binary "$PAYLOAD_PROVIDER_BIN" "${PAYLOAD_PROVIDER_DIR}/Cargo.toml" atlas-payload-provider

rm -rf "$WORK_DIR"
mkdir -p "$WORK_DIR"

log "starting local payload provider at ${PAYLOAD_PROVIDER_URL}"
PAYLOAD_DIR="${WORK_DIR}/payloads" \
LISTEN_HOST="$PROVIDER_HOST" \
LISTEN_PORT="$PROVIDER_PORT" \
MAX_PAYLOAD_BYTES=1048576 \
INGRESS_BEARER_KEY="$INGRESS_BEARER_KEY" \
SIGNER_PRIVATE_KEY="$PROVIDER_SIGNER_PRIVATE_KEY" \
    "$PAYLOAD_PROVIDER_BIN" >"${WORK_DIR}/payload-provider.log" 2>&1 &
PROVIDER_PID=$!
wait_for_http "payload provider" "${PAYLOAD_PROVIDER_URL}/healthz" "$PROVIDER_PID"

log "preparing dev genesis"
NODE_DIR="${WORK_DIR}/node"
mkdir -p "$NODE_DIR"
GENESIS="${NODE_DIR}/genesis.json"
cp "${ROOT_DIR}/chainspec/dev.base.json" "$GENESIS"
"$ARKIV_CLI_BIN" inject-predeploy "$GENESIS" >/dev/null
"$ARKIV_NODE_BIN" init --chain "$GENESIS" --datadir "$NODE_DIR" >"${WORK_DIR}/arkiv-node-init.log" 2>&1

log "starting arkiv dev chain at ${RPC_URL}"
"$ARKIV_NODE_BIN" node \
    --chain "$GENESIS" \
    --dev \
    --dev.block-time 1s \
    --datadir "$NODE_DIR" \
    --http \
    --http.addr "$RPC_HOST" \
    --http.port "$RPC_PORT" \
    --http.api all \
    --http.corsdomain '*' \
    --ws \
    --ws.addr "$RPC_HOST" \
    --ws.port "$WS_PORT" \
    --ws.api all \
    --txpool.minimal-protocol-fee 1 \
    --txpool.minimum-priority-fee 0 \
    >"${WORK_DIR}/arkiv-node.log" 2>&1 &
NODE_PID=$!
wait_for_rpc "$NODE_PID"

log "submitting payload to local provider"
payload_text="arkiv ci payload reference smoke $(date -u +%Y-%m-%dT%H:%M:%SZ)"
payload_b64="$(printf '%s' "$payload_text" | base64 -w0)"
provider_response="$(
    curl -fsS -X POST "${PAYLOAD_PROVIDER_URL}/arkiv/payloads" \
        -H 'content-type: application/json' \
        -H "authorization: Bearer ${INGRESS_BEARER_KEY}" \
        --data "$(jq -nc \
            --arg ns atlas.ci \
            --arg ct text/plain \
            --arg payload "$payload_b64" \
            --arg nonce "$PAYLOAD_REFERENCE_NONCE" \
            --argjson payment "$PAYLOAD_REFERENCE_PAYMENT" \
            '{namespace:$ns,contentType:$ct,payloadBase64:$payload,nonce:$nonce,payment:$payment}')"
)"

jq -e --arg signer "$PROVIDER_SIGNER_ADDRESS" --arg nonce "$PAYLOAD_REFERENCE_NONCE" --argjson payment "$PAYLOAD_REFERENCE_PAYMENT" '
    .ok == true
    and .payload.signature.scheme == "eip191"
    and (.payload.signature.signer | ascii_downcase) == $signer
    and .payload.signature.receipt.action == "payloadReceived"
    and .payload.signature.receipt.nonce == $nonce
    and .payload.signature.receipt.payment == $payment
' <<<"$provider_response" >/dev/null

reference="$(
    jq -c '{
        kind:"atlas.payloadReference",
        version:1,
        provider:"atlas-payload-provider",
        id:.payload.id,
        namespace:.payload.namespace,
        contentType:.payload.contentType,
        checksum:.payload.checksum,
        sizeBytes:.payload.sizeBytes,
        submittedAt:.payload.submittedAt,
        nonce:.payload.signature.receipt.nonce,
        payment:.payload.signature.receipt.payment,
        signature:.payload.signature
    }' <<<"$provider_response"
)"
printf '%s\n' "$reference" >"${WORK_DIR}/payload-reference.json"

log "checking that a tampered provider signature is rejected"
tampered="$(
    jq -c '.signature.messageHash = "0x0000000000000000000000000000000000000000000000000000000000000000"' \
        <<<"$reference"
)"
if "$ARKIV_CLI_BIN" \
    --rpc-url "$RPC_URL" \
    --private-key "$DEV_PRIVATE_KEY" \
    --gas-price 1000000000 \
    create \
    --content-type "$CONTENT_TYPE" \
    --btl 1000 \
    --payload "$tampered" \
    >"${WORK_DIR}/tampered-create.out" 2>&1; then
    echo "tampered reference unexpectedly succeeded" >&2
    exit 1
fi

log "submitting valid provider reference to dev chain"
create_output="$(
    "$ARKIV_CLI_BIN" \
        --rpc-url "$RPC_URL" \
        --private-key "$DEV_PRIVATE_KEY" \
        --gas-price 1000000000 \
        create \
        --content-type "$CONTENT_TYPE" \
        --btl 1000 \
        --payload "$reference"
)"
printf '%s\n' "$create_output" >"${WORK_DIR}/valid-create.out"
entity_key="$(awk '/entity_key:/ {print $2; exit}' <<<"$create_output")"
if [ -z "$entity_key" ]; then
    echo "valid create output did not include an EntityOperation entity_key" >&2
    exit 1
fi

log "verifying query returns a payload reference summary without raw bytes"
query="$(jq -nc --arg q "\$key = ${entity_key}" --arg ct "$CONTENT_TYPE" \
    '{jsonrpc:"2.0",id:1,method:"arkiv_query",params:[$q,{includeData:{payload:true,contentType:true}}]}')"
query_response="$(json_rpc "$query")"
jq -e \
    --arg id "$(jq -r '.id' <<<"$reference")" \
    --arg ns "$(jq -r '.namespace' <<<"$reference")" \
    --arg ct "$(jq -r '.contentType' <<<"$reference")" \
    --arg checksum "$(jq -r '.checksum' <<<"$reference")" \
    --arg submitted_at "$(jq -r '.submittedAt' <<<"$reference")" \
    --argjson size_bytes "$(jq -r '.sizeBytes' <<<"$reference")" '
    .result.data | length == 1
    and (.[0] | has("value") | not)
    and (.[0] | has("payload") | not)
    and .[0].payloadRef.id == $id
    and .[0].payloadRef.namespace == $ns
    and .[0].payloadRef.provider == "atlas-payload-provider"
    and .[0].payloadRef.contentType == $ct
    and .[0].payloadRef.checksum == $checksum
    and .[0].payloadRef.sizeBytes == $size_bytes
    and .[0].payloadRef.submittedAt == $submitted_at
    and .[0].contentType == $ct
' <<<"$query_response" >/dev/null

log "ok: provider reference accepted on dev chain and tampered signature rejected"
