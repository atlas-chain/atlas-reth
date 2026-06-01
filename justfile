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

# ── Acceptance harness (ThirdParty/optimism, pinned to op-reth/v2.2.5) ──
#
# The OP Go acceptance-test harness (op-acceptance-tests + the in-process
# op-devstack/sysgo) lives in the Optimism monorepo, vendored as a submodule.
# op-acceptance-tests is part of the monorepo's single Go module and imports
# across ~15 of its packages, so it can only be compiled with the whole tree
# checked out — not in isolation.
#
# Compiling needs only Go — any host Go >= 1.24 (the go.mod floor). No mise.
# Actually *running* the suite needs the full runtime build-deps (contracts,
# cannon prestates, Rust binaries) via the monorepo's own mise-managed tooling —
# see ThirdParty/optimism/docs/ai/acceptance-tests.md.

# Fetch/checkout the OP monorepo submodule at the pinned commit
harness-init:
    git submodule update --init ThirdParty/optimism

# Confirm the Go acceptance harness compiles at the matching tag (compiles every
# test package + its cross-repo import closure, runs no tests).
harness-check:
    cd ThirdParty/optimism && go test -run 'a^' ./op-acceptance-tests/...

# Build the runtime artifacts the harness needs that do NOT build just-in-time:
# the OP contract bundle, the cannon fault-proof prestates (both required by the
# Minimal preset's challenger, even for non-proof tests), and the flashblocks
# builders (op-rbuilder + rollup-boost — pre-built here so the flashblocks suite
# doesn't time out JIT-compiling them mid-test). One-time; re-runs are
# incremental. Needs the mise toolchain — run `mise install` in
# ThirdParty/optimism first. arkiv-node (op-reth) + kona-host are handled by
# `harness-run` / `RUST_JIT_BUILD`, not here.
harness-build-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ justfile_directory() }}/ThirdParty/optimism/packages/contracts-bedrock"
    mise exec -- just install
    mise exec -- just build-no-tests
    cd "{{ justfile_directory() }}/ThirdParty/optimism"
    mise exec -- just cannon-prestates
    ( cd rust/op-rbuilder && mise exec -- cargo build -p op-rbuilder --bin op-rbuilder )
    ( cd rust/rollup-boost && mise exec -- cargo build -p rollup-boost --bin rollup-boost )

# Run an OP acceptance test with arkiv-node substituted as the L2 EL.
# arkiv-node IS op-reth v2.2.5 + the Arkiv precompile, so op-devstack drives it
# through the standard op-reth path: RUST_BINARY_PATH_OP_RETH points the harness'
# rustbin resolver at our release build (skipping its own op-reth compile) and
# DEVSTACK_L2EL_KIND=op-reth selects that backend. RUST_JIT_BUILD=1 lets any other
# Rust harness binary (e.g. kona-host) build on demand; the path override is
# checked first, so arkiv-node is never replaced by an upstream op-reth build.
# Run `just harness-build-deps` once first. Defaults to the base smoke test.
harness-run *test='-run TestRPCConnectivity ./op-acceptance-tests/tests/base/':
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -p arkiv-node --release
    cd "{{ justfile_directory() }}/ThirdParty/optimism"
    RUST_BINARY_PATH_OP_RETH="{{ justfile_directory() }}/target/release/arkiv-node" \
    DEVSTACK_L2EL_KIND=op-reth \
    RUST_JIT_BUILD=1 \
        mise exec -- go test -count=1 -timeout 30m {{ test }}

# Run the whole op-acceptance-tests tree against arkiv-node as the L2 EL.
# Each package spins a full multi-node devnet; running many in parallel exhausts
# the host's ephemeral ports (op-node fails with "bind: can't assign requested
# address"), so this defaults to low package-parallelism (`-p 2`) and serial
# in-package execution (`-parallel 1`). Drop to `jobs=1` for the heaviest sync
# suites. Run `just harness-build-deps` once first. Pass a narrower path to scope it.
harness-suite jobs='2' tree='./op-acceptance-tests/tests/...':
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build -p arkiv-node --release
    cd "{{ justfile_directory() }}/ThirdParty/optimism"
    RUST_BINARY_PATH_OP_RETH="{{ justfile_directory() }}/target/release/arkiv-node" \
    DEVSTACK_L2EL_KIND=op-reth \
    RUST_JIT_BUILD=1 \
        mise exec -- go test -count=1 -timeout 30m -p {{ jobs }} -parallel 1 {{ tree }}
