# Arkiv on Pure Reth: Migration Plan

This plan describes how to remove the OP-stack/op-reth integration
layer while keeping the Arkiv database operations, precompile ABI, and
operator tooling on top of upstream reth.

The target is a regular reth `EthereumNode` with an Arkiv EVM factory
that registers the Arkiv precompile at `ARKIV_ADDRESS`. In this model
Arkiv remains a native precompile-backed interface. The Solidity file
continues to define the SDK ABI unless a separate deployed-contract
design is chosen later.

## Goals

- Remove `reth-optimism-*`, `alloy-op-evm`, `op-alloy-*`, and
  `op-revm` dependencies from the production node.
- Keep `arkiv-entitydb` as the canonical state model and op handler
  crate.
- Keep the `execute(Operation[])`, `nonces(address)`, and
  `EntityOperation` ABI surface stable.
- Keep the `arkiv_*` JSON-RPC namespace for query and entity reads.
- Preserve consensus storage in the L3 trie through the existing
  `StateAdapter` abstraction.

## Non-goals

- Do not implement Arkiv as a normal deployed Solidity contract in this
  migration. A contract cannot directly run the Rust op handlers or use
  the same code/storage layout without a native host hook.
- Do not change entity key derivation, annotation encoding, bitmap
  encoding, Tier-2 ART indexing, or nonce layout unless a follow-up spec
  explicitly requires it.
- Do not bump reth git revisions as part of the first migration unless
  required by compiler errors.

## Current Split

Reusable pieces:

- `crates/arkiv-entitydb`: already chain-agnostic behind
  `StateAdapter`.
- `crates/arkiv-node/src/precompile.rs`: mostly generic revm/alloy
  precompile code.
- `crates/arkiv-node/src/state_adapter.rs`: already bridges revm
  execution state and reth read-only providers.
- `crates/arkiv-node/src/rpc.rs`: mostly generic over reth providers,
  except installer bounds.
- `contracts/src/EntityRegistry.sol`: ABI interface only.
- `crates/arkiv-cli`: mostly RPC/ABI driven, but should be audited for
  OP-specific assumptions.

OP-specific pieces to replace:

- `crates/arkiv-node/src/evm.rs`: `ArkivOpEvmFactory`,
  `ArkivOpEvmConfig`, `ArkivOpNode`, OP payload attributes, OP
  primitives, and OP post-exec plumbing.
- `crates/arkiv-node/src/main.rs`: `reth_optimism_cli::Cli`,
  `OpChainSpecParser`, and `OpNode`.
- `crates/arkiv-node/src/cli.rs`: `RollupArgs`.
- `crates/arkiv-node/src/install.rs`: `OpPrimitives` bound.
- `e2e`: OP chainspec, OP payload attributes, and L1-info deposit
  setup.

## Design Decisions To Make First

1. Base chain semantics

   Decide whether Arkiv should run as a plain Ethereum-style L1/dev
   chain or as a custom reth chain spec with Arkiv-specific hardfork
   toggles. The first migration should use the pinned reth
   `ChainSpec`/`EthereumNode` path unless a custom chain spec is
   required for launch.

2. Caller authorization

   The current design relies on DB-chain assumptions: user-deployed
   contracts are forbidden and EIP-7702 is disabled, so
   `input.caller` is effectively the signing EOA. Plain reth does not
   enforce that by default. Before production, choose one:

   - enforce Arkiv as EOA-only at transaction validation or chain rules;
   - allow contract callers and define ownership by `msg.sender`;
   - add precompile-level checks if revm exposes enough caller/origin
     context for the chosen rule.

3. Contract story

   Keep `EntityRegistry.sol` as an interface-only ABI for the native
   precompile. If a deployed contract is wanted, specify it as a
   separate wrapper or gateway, not as a replacement for the native
   database engine.

## Migration Steps

### 1. Add a Pure Reth EVM Factory

- Introduce `ArkivEthEvmFactory` based on `alloy_evm::EthEvmFactory`
  and `EthEvm`.
- Register `arkiv_precompile()` into `PrecompilesMap` in both
  `create_evm` and `create_evm_with_inspector`.
- Preserve the current `evm_tx` tracing span around `transact_raw` if
  profiling still needs it.
- Build the executor with
  `EthEvmConfig::new_with_evm_factory(ctx.chain_spec(), ArkivEthEvmFactory::default())`.

The pinned reth tree already includes examples for this pattern:
`examples/custom-evm` and `examples/precompile-cache`.

### 2. Replace Node Wiring

- Replace `ArkivOpNode` with either:
  - `EthereumNode::components().executor(ArkivEthExecutorBuilder)`, or
  - a thin `ArkivEthNode` wrapper only if add-on types require it.
- Replace OP add-ons with `EthereumAddOns`.
- Replace `OpChainSpec`/`OpPrimitives`/`OpEngineTypes` with
  `ChainSpec`/`EthPrimitives`/`EthEngineTypes`.
- Replace `reth_optimism_cli::Cli` with the upstream reth Ethereum CLI
  surface.
- Remove rollup-specific CLI arguments.

### 3. Generalize RPC Installation

- Remove the `T::Types: NodeTypes<Primitives = OpPrimitives>` bound in
  `install.rs`.
- Keep the provider clone and `ArkivRpc::new(provider)` flow.
- Compile the RPC module against `EthereumNode` first, then decide if
  it can stay generic over node primitives.

### 4. Remove OP Dependencies

- Delete production dependencies on:
  - `reth-optimism-node`
  - `reth-optimism-cli`
  - `reth-optimism-chainspec`
  - `reth-optimism-primitives`
  - `alloy-op-evm`
  - `op-alloy-consensus`
  - `op-revm`
- Add any missing upstream reth crates used by the Ethereum node path,
  such as `reth-node-ethereum`, `reth-ethereum`, or
  `reth-evm-ethereum`, matching the existing pinned reth revision.
- Keep dependency changes separate from behavioral changes when
  possible.

### 5. Port Tests

- Keep `arkiv-entitydb` unit and query tests unchanged.
- Port direct revm precompile tests from `OpTx`/`OpEvmContext` to
  `TxEnv`/`EthEvmContext`.
- Replace the OP e2e harness with `EthereumNode` and normal Ethereum
  payload attributes.
- Remove canonical L1-info deposit setup from e2e.
- Add regression coverage for the chosen caller authorization rule.

### 6. Update Docs

- Update `docs/1_overview.md` and `docs/4_engineering.md` to describe
  Arkiv on reth instead of Arkiv on op-reth.
- Update `docs/2_state-model.md` only if the precompile semantics,
  caller authorization, gas, or state layout change.
- Update `docs/3_query.md` only if query or RPC semantics change.
- Update `README.md`, package metadata, and repository keywords to
  remove OP-stack positioning.

## Validation

Run these after the migration compiles:

```sh
cargo check
cargo test -p arkiv-entitydb
cargo test -p arkiv-node --lib
cargo test -p arkiv-e2e --test full_pipeline_e2e
cargo clippy --all
```

For documentation-only commits to this plan, `git diff --check` is
sufficient.

## Expected Risk

The technical port is feasible because the Arkiv state model is already
behind `StateAdapter`, and upstream reth supports custom EVM factories
on `EthereumNode`.

The highest-risk item is not mechanical compilation. It is preserving
the security model previously provided by OP/DB-chain assumptions,
especially whether `input.caller` should continue to mean the signing
EOA or should become normal EVM `msg.sender`.
