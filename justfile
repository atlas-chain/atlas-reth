registry := "0x4400000000000000000000000000000000000044"
dev_key  := "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
dev_addr := "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
rpc      := "http://localhost:8545"
arkiv_node := env_var_or_default("ARKIV_NODE", "cargo run -p arkiv-node --")
arkiv_cli  := env_var_or_default("ARKIV_CLI", "cargo run -p arkiv-cli --")

# Single working dir for every recipe that needs scratch space.
# Always REPO_ROOT/tmp so paths are predictable across local / docker / CI.
tmp_dir := justfile_directory() / "tmp"

# ── Build ────────────────────────────────────────────────────

# Check workspace compiles
check:
    cargo check --workspace

# Build workspace
build:
    cargo build --workspace

# Build workspace (release)
build-release:
    cargo build --workspace --release

# Run clippy across the workspace
lint:
    cargo clippy --workspace -- -D warnings

# Format the workspace
fmt:
    cargo fmt --all

# ── Contracts ────────────────────────────────────────────────

# Compile the Solidity sources in contracts/ and refresh the runtime
# artifact at contracts/artifacts/EntityRegistry.runtime.hex. arkiv-genesis
# reads that file via `include_str!`, so re-run this after editing
# contracts/src/*.sol.
contracts-build:
    #!/usr/bin/env bash
    set -euo pipefail
    cd contracts
    forge build
    jq -r '.deployedBytecode.object' out/EntityRegistry.sol/EntityRegistry.json \
        > artifacts/EntityRegistry.runtime.hex
    echo "wrote contracts/artifacts/EntityRegistry.runtime.hex ($(wc -c < artifacts/EntityRegistry.runtime.hex) bytes)"

# ── Node ─────────────────────────────────────────────────────

# Print an Arkiv dev genesis JSON to stdout (dev.base.json + injected predeploy)
genesis:
    #!/usr/bin/env bash
    set -e
    mkdir -p "{{ tmp_dir }}"
    GENESIS="{{ tmp_dir }}/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS" 2>/dev/null
    cat "$GENESIS"

# Run arkiv-node in dev mode against a freshly assembled Arkiv genesis.
# Generates genesis -> init datadir -> launch node, all against the same
# chainspec file so init/node agree on the genesis hash.
#
# v1 had three node-dev variants (--arkiv.debug / --arkiv.db-url / storaged).
# Post-demolition there is just one: plain op-reth with the predeploy in
# genesis. Phase 2+ will reintroduce the precompile + RPC.
node-dev *args='':
    #!/usr/bin/env bash
    set -e
    DATADIR="{{ tmp_dir }}/node-dev"
    rm -rf "$DATADIR"
    mkdir -p "$DATADIR"
    GENESIS="$DATADIR/genesis.json"
    cp chainspec/dev.base.json "$GENESIS"
    {{ arkiv_cli }} inject-predeploy "$GENESIS"
    {{ arkiv_node }} init --chain "$GENESIS" --datadir "$DATADIR"
    echo "datadir: $DATADIR"
    echo "genesis: $GENESIS"
    echo "registry: {{ registry }}"
    echo "dev account: {{ dev_addr }}"
    {{ arkiv_node }} node \
        --chain "$GENESIS" \
        --dev \
        --dev.block-time 2s \
        --datadir "$DATADIR" \
        --http \
        --http.api eth,net,web3,debug \
        --http.corsdomain '*' \
        --ws \
        --ws.api eth,net,web3,debug \
        --ws.port 8546 \
        --log.file.directory "$DATADIR/logs" \
        --txpool.minimal-protocol-fee 1 \
        --txpool.minimum-priority-fee 0 \
        {{ args }}

# Run arkiv-node with custom args
node *args='':
    {{ arkiv_node }} {{ args }}

# ── CLI ──────────────────────────────────────────────────────

# Run arkiv-cli with arbitrary args
cli *args='':
    {{ arkiv_cli }} {{ args }}

# Create an entity (random payload)
create *args='':
    {{ arkiv_cli }} create --random-payload {{ args }}

# Update an entity (random payload)
update key *args='':
    {{ arkiv_cli }} update --key {{ key }} --random-payload {{ args }}

# Extend an entity's expiration
extend key expires_in='1h':
    {{ arkiv_cli }} extend --key {{ key }} --expires-in {{ expires_in }}

# Transfer entity ownership
transfer key new_owner:
    {{ arkiv_cli }} transfer --key {{ key }} --new-owner {{ new_owner }}

# Delete an entity
delete key:
    {{ arkiv_cli }} delete --key {{ key }}

# Expire an entity (must be past expiration)
expire key:
    {{ arkiv_cli }} expire --key {{ key }}

# Read an entity commitment from the EntityRegistry contract (on-chain).
# NOTE: Post-Phase-1 the contract still has commitments but the rolling
# changeset-hash machinery is going away in v2; this still works for now.
commitment key:
    {{ arkiv_cli }} query --key {{ key }}

# Check dev account balance
balance *args='':
    {{ arkiv_cli }} balance {{ args }}

# Submit a batch of operations from a JSON file in a single tx
batch file:
    {{ arkiv_cli }} batch {{ file }}

# Fire off multiple entity creates
spam *args='':
    {{ arkiv_cli }} spam {{ args }}

# Continuously simulate live system traffic against a running node
simulate *args='':
    cargo run -p arkiv-cli -- simulate {{ args }}

# ── Dev Helpers ──────────────────────────────────────────────

# Verify EntityRegistry is deployed (requires running node)
verify-registry:
    @cast code {{ registry }} --rpc-url {{ rpc }} | head -c 80
    @echo "..."

# Check dev account balance via cast
verify-balance:
    @cast balance {{ dev_addr }} --rpc-url {{ rpc }} --ether

# Send ETH from the dev account to an address
fund address amount="1ether":
    cast send --private-key {{ dev_key }} --rpc-url {{ rpc }} {{ address }} --value {{ amount }}

# Show current block number
block-number:
    @cast block-number --rpc-url {{ rpc }}

# Print current block timing (block number, timestamp, seconds since last block)
# NOTE: arkiv_getBlockTiming RPC is gone with the proxy; this recipe will
# fail until Phase 4 reintroduces a local-backed handler.
block-timing:
    {{ arkiv_cli }} block-timing
