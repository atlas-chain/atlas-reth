arkiv_address := "0x4400000000000000000000000000000000000044"
dev_key       := "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
dev_addr      := "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
rpc           := "http://localhost:8545"
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

# unit tests across workspace
unit-tests:
    cargo nextest run --workspace --lib

# e2e tests doing series of operations on locally started node (it is actually kind of component test)
e2e-tests:
    cargo nextest run -p arkiv-e2e

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
    echo "arkiv address: {{ arkiv_address }}"
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

# Resume arkiv-node in dev mode against the existing datadir (no wipe, no re-init).
# Use this after a `node-dev` run to restart without losing the MDBX state.
node-dev-resume *args='':
    #!/usr/bin/env bash
    set -e
    DATADIR="{{ tmp_dir }}/node-dev"
    GENESIS="$DATADIR/genesis.json"
    if [ ! -d "$DATADIR" ]; then
        echo "error: no existing datadir at $DATADIR — run 'just node-dev' first" >&2
        exit 1
    fi
    echo "datadir: $DATADIR"
    echo "genesis: $GENESIS"
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

# Read an entity commitment via arkiv_cli query.
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

# ── Profiling ────────────────────────────────────────────────

# Direct revm profile of CREATE ops → tmp/arkiv.create.trace.json.
# Drop the JSON onto https://ui.perfetto.dev for the per-tx flame graph:
# evm_tx → precompile_call → precompile_dispatch → entitydb_create.
# No reth block production / RPC / tokio runtime — single thread.
profile-create:
    cargo test --test profile_create_op_direct -p arkiv-node -- --nocapture
    @echo
    @echo "trace: {{ tmp_dir }}/arkiv.create.trace.json"
    @echo "load:  https://ui.perfetto.dev"

# ── Dev Helpers ──────────────────────────────────────────────

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
