#!/bin/bash

set -euo pipefail

DEV_PRIVATE_KEY="${ARKIV_DEV_PRIVATE_KEY:-ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80}"
RPC_URL="${ARKIV_RPC_URL:-}"

usage() {
    cat >&2 <<'EOF'
usage: fund-account --address=<address> --value=<value> [--rpc-url=<url>] [--private-key=<key>]
EOF
}

case "${1:-}" in
    fund-account)
        shift
        ;;
    --address|--address=*|--value|--value=*|--rpc-url|--rpc-url=*|--private-key|--private-key=*|-h|--help)
        ;;
    *)
        exec arkiv-node "$@"
        ;;
esac

ADDRESS=""
VALUE=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --address=*)
            ADDRESS="${1#*=}"
            ;;
        --address)
            if [ "$#" -lt 2 ]; then
                echo "missing value for --address" >&2
                usage
                exit 2
            fi
            ADDRESS="${2:-}"
            shift
            ;;
        --value=*)
            VALUE="${1#*=}"
            ;;
        --value)
            if [ "$#" -lt 2 ]; then
                echo "missing value for --value" >&2
                usage
                exit 2
            fi
            VALUE="${2:-}"
            shift
            ;;
        --rpc-url=*)
            RPC_URL="${1#*=}"
            ;;
        --rpc-url)
            if [ "$#" -lt 2 ]; then
                echo "missing value for --rpc-url" >&2
                usage
                exit 2
            fi
            RPC_URL="${2:-}"
            shift
            ;;
        --private-key=*)
            DEV_PRIVATE_KEY="${1#*=}"
            ;;
        --private-key)
            if [ "$#" -lt 2 ]; then
                echo "missing value for --private-key" >&2
                usage
                exit 2
            fi
            DEV_PRIVATE_KEY="${2:-}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage
            exit 2
            ;;
    esac
    shift
done

if [ -z "$ADDRESS" ] || [ -z "$VALUE" ]; then
    usage
    exit 2
fi

if [ -z "$RPC_URL" ]; then
    for candidate in http://127.0.0.1:8545 http://host.docker.internal:8545; do
        if cast chain-id --rpc-url "$candidate" >/dev/null 2>&1; then
            RPC_URL="$candidate"
            break
        fi
    done
    RPC_URL="${RPC_URL:-http://127.0.0.1:8545}"
fi

cast send \
    --private-key "$DEV_PRIVATE_KEY" \
    --rpc-url "$RPC_URL" \
    "$ADDRESS" \
    --value "$VALUE"
