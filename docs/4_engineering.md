# Arkiv on Reth: Engineering

This document covers how the codebase is organised, how to deploy a
chain that runs Arkiv, what's tested where, the fault-proof story,
and outstanding open questions.

For higher-level context see [`1_overview.md`](1_overview.md); for
the canonical state-model spec see
[`2_state-model.md`](2_state-model.md); for query semantics and
verification recipes see [`3_query.md`](3_query.md).

## Contents

- [1. Workspace crates](#1-workspace-crates)
- [2. Genesis construction](#2-genesis-construction)
- [3. Experimental protocol schedule](#3-experimental-protocol-schedule)
- [4. Testing surface](#4-testing-surface)
- [5. Key design decisions, recapped](#5-key-design-decisions-recapped)
- [6. Things this design does *not* do](#6-things-this-design-does-not-do)
- [7. Fault-proof compatibility](#7-fault-proof-compatibility)
- [8. Open questions](#8-open-questions)

---

## 1. Workspace crates

```
crates/
  arkiv-node/         # binary + custom EvmFactory + Arkiv precompile + arkiv_* RPC
  arkiv-entitydb/     # state model + op handlers + system-state API + query language
  arkiv-cli/          # operator CLI: entity ops, batches, simulate, dev-funding injection
  arkiv-genesis/      # shared lib: ARKIV_ADDRESS re-export + dev-funding alloc helpers
e2e/                  # full-pipeline integration tests
contracts/
  src/EntityRegistry.sol    # IEntityRegistry interface - ABI surface for SDK codegen
docs/
  1_overview.md       # high-level orientation
  2_state-model.md    # canonical state model
  3_query.md          # query language + verification recipes
  4_engineering.md    # this file
```

### 1.1 `arkiv-genesis`

Pure library. Owns:

- `ARKIV_ADDRESS` (`0x4400…0044`) — precompile registration target.
  Re-exported from `arkiv-entitydb`. No genesis allocation.
- `ARKIV_DEV_MNEMONIC`, `DEV_ADDRESS`, `ARKIV_DEV_ACCOUNT_COUNT` — the
  hardhat-compatible dev mnemonic and the 100 pre-funded dev accounts
  derived from it.
- `genesis_alloc()`, `dev_funding_alloc(...)` — assemble dev-funding
  entries for splicing into a geth-format `Genesis.alloc`.

The system account that hosts the precompile's consensus storage is
materialised lazily by `arkiv-entitydb` on its first write — it does
not appear in `genesis_alloc()`.

### 1.2 `arkiv-entitydb`

Canonical home of the state model. No `revm` deps, no DB deps —
runs against an abstract `StateAdapter` trait. Contains:

- **Primitives.** `EntityRlp`, `Bitmap` (roaring64), `IndexTree`
  (Adaptive Radix Tree over annotation values), `entity_address`,
  `pair_address`, `index_address`, built-in annotation keys,
  system-account address.
- **`StateAdapter` trait.** `code` / `set_code` / `tombstone_code` /
  `storage` / `set_storage` / `ensure_account_persists`. Implemented
  in production by `ReadWriteStateAdapter` (precompile path, journaled
  writes) and `ReadOnlyStateAdapter` (RPC read path, against a
  `StateProvider` snapshot). The `test-utils` feature exposes
  `InMemoryStateAdapter` for unit tests.
- **Op handlers.** `create` / `update` / `extend` / `transfer` /
  `delete` / `expire`. All indexing logic lives here.
- **System-state API.** `read_nonce(state, caller) -> Result<u32>`
  and `bump_nonce(state, caller) -> Result<u32>` are the only public
  accessors for the per-caller nonce slot. The underlying `slot_*`
  helpers are `pub(crate)`.
- **Query language.** Lexer, recursive-descent parser, AST, and
  tree-walking interpreter.

### 1.3 `arkiv-node`

The execution-client binary. It uses the upstream reth Ethereum CLI
surface and composes `EthereumNode` with an Arkiv executor builder.
Layout:

- `evm.rs` — `ArkivEthEvmFactory` wrapping `EthEvmFactory`.
  Registers the Arkiv precompile at `ARKIV_ADDRESS` into
  `PrecompilesMap` in both `create_evm` and
  `create_evm_with_inspector` so simulation, tracing,
  payload-building, validation, and canonical execution all see the
  same set. `ArkivEthExecutorBuilder` builds
  `EthEvmConfig::new_with_evm_factory(...)` for `EthereumNode`.
- `precompile.rs` — caller restriction (direct CALL only;
  `DELEGATECALL`/`CALLCODE`/value-bearing rejected; `STATICCALL`
  allowed for `nonces(address)` only), calldata decode, per-op
  validation, Solidity-style revert encoding, `EntityOperation`
  event emission, deterministic gas accounting, and dispatch into
  `arkiv-entitydb`.
- `rpc.rs` — `arkiv_*` JSON-RPC namespace + wire-format types.
  `ReadOnlyStateAdapter` wraps a `StateProvider` for read-only state.
- `install.rs` — `extend_rpc_modules` hook registering the `arkiv_*`
  namespace.

There is no chainspec mutation and no Arkiv-specific chainspec gate.
Any valid geth-format Ethereum genesis that reth can parse works; the
precompile is registered programmatically by the custom `EvmFactory`,
and the system-account storage host is created on the first write.

### 1.4 `arkiv-cli`

Operator command-line tool. Two distinct surfaces:

**Entity operations** (require an RPC endpoint + signer):
`create`, `update`, `extend`, `transfer`, `delete`, `expire`, `query`,
`balance`, `spam`, `batch`, `simulate`. All ops are submitted as
`execute(Operation[])` calls to `ARKIV_ADDRESS` — the precompile
decodes the calldata, validates, charges gas, mutates state, and
emits the `EntityOperation` log.

**Genesis post-processing** (no network required):
`arkiv-cli inject-predeploy <input.json>` reads a geth-format genesis
and splices the dev-funded accounts into `alloc`. The command name is
legacy; it does not deploy bytecode at `ARKIV_ADDRESS`. The system
account is not injected — it is materialised lazily on the first op.

The traffic simulator (`simulate`) rotates through mnemonic-derived
signers, maintains an in-memory pool of alive entities, and submits a
weighted random mix of CRUD ops. State updates come from decoding
`EntityOperation` logs and reading the entity RLP via `eth_getCode`.

### 1.5 `e2e`

End-to-end integration tests. Uses `reth-e2e-test-utils`'
`NodeTestContext` to boot an Arkiv-enabled `EthereumNode` in-process,
then drives it via the `World` helper in `e2e/src/lib.rs` (signer
pool, nonce tracking, ABI encoding, query plumbing).
`tests/full_pipeline_e2e.rs` walks a single narrative through every
op type and every query construct.

---

## 2. Genesis construction

The runtime chainspec is treated as read-only data flowing in from
`--chain`. The Arkiv precompile and system account do not need genesis
entries:

- `ARKIV_ADDRESS` has no bytecode in genesis. It is activated by
  `ArkivEthEvmFactory`.
- The entitydb system account is materialised lazily on first write by
  `StateAdapter::ensure_account_persists`.
- Dev funding is optional and is injected by `arkiv-cli
  inject-predeploy` into a geth-format genesis `alloc`.

Local development uses `chainspec/dev.base.json`, which is a
geth-format Ethereum genesis. The `just genesis` and `just node-dev`
recipes copy that file, inject dev-funded accounts, then use the same
JSON for `init` and `node` so the genesis hash matches.
The dev genesis intentionally keeps `baseFeePerGas` at `0x1`; Arkiv's
experimental 0.44 gwei EIP-1559 minimum base-fee floor is applied in
the patched next-block base-fee calculation, not by mutating genesis.

```bash
cp chainspec/dev.base.json $TMPDIR/genesis.json
arkiv-cli inject-predeploy $TMPDIR/genesis.json
arkiv-node init --chain $TMPDIR/genesis.json --datadir $TMPDIR
arkiv-node node --chain $TMPDIR/genesis.json --datadir $TMPDIR
```

For production, provide the chain's geth-format genesis directly:

```bash
arkiv-node init --chain genesis.json --datadir ./data
arkiv-node node --chain genesis.json --datadir ./data
```

`arkiv-cli inject-predeploy genesis.json` remains useful when a dev or
test deployment wants the known mnemonic accounts funded. It is not a
required production step.

---

## 3. Experimental protocol schedule

Advanced testnets can enable a central protocol-schedule endpoint with
`ARKIV_PROTOCOL_SCHEDULE_URL`. When set, `arkiv-node` polls the URL
every 60 seconds, validates the JSON, persists the last accepted
response, and installs the schedule into the patched `alloy-eips`
base-fee helpers. If the endpoint is unavailable or returns invalid
JSON, the node keeps using its last accepted local schedule.

Optional environment:

- `ARKIV_PROTOCOL_SCHEDULE_URL` — enables polling when non-empty.
- `ARKIV_PROTOCOL_SCHEDULE_PATH` — persistence path; defaults to
  `arkiv-protocol-schedule.json` in the current working directory.
- `ARKIV_PROTOCOL_SCHEDULE_POLL_SECONDS` — poll interval; defaults to
  `60`.

Schema:

```json
{
  "chainId": 12345,
  "version": 7,
  "currentBlock": 100,
  "schedule": [
    {
      "activationBlock": 0,
      "minBaseFeePerGas": "440000000",
      "elasticityMultiplier": 2,
      "baseFeeMaxChangeDenominator": 8,
      "maxBlockGasLimit": "30000000"
    },
    {
      "activationBlock": 120,
      "minBaseFeePerGas": "800000000",
      "elasticityMultiplier": 4,
      "baseFeeMaxChangeDenominator": 8,
      "maxBlockGasLimit": "60000000"
    }
  ]
}
```

`version` must not go backwards relative to the last accepted response.
`currentBlock` is optional. When present, the node installs only
entries with `activationBlock <= currentBlock`; this lets the service
publish a full schedule while preventing future entries from taking
effect early. When absent, the node installs the full schedule and the
patched low-level helpers use the latest entry. Numeric gas fields may
be decimal strings or `0x`-prefixed hex strings.

This is an experimental testnet control plane, not a production
hardfork mechanism. The base-fee floor, elasticity multiplier, and
base-fee max-change denominator affect consensus header validation.
The `maxBlockGasLimit` value caps payload-builder gas-limit selection;
the upstream parent/child gas-limit delta validation still applies.

---

## 4. Testing surface

| Layer | Where |
|---|---|
| `arkiv-genesis` unit tests | `crates/arkiv-genesis/src/lib.rs` (`#[cfg(test)]`) — alloc shape, signer derivation. |
| State model + op handlers | `crates/arkiv-entitydb/src/lib.rs` (`#[cfg(test)]`) — against `InMemoryStateAdapter`. |
| Query lexer + parser unit tests | `crates/arkiv-entitydb/src/query/{lexer,parser}.rs`. |
| Query interpreter integration | `crates/arkiv-entitydb/tests/query_eval.rs` — parse + evaluate end-to-end against `InMemoryStateAdapter`. |
| Precompile unit tests | `crates/arkiv-node/src/precompile.rs` — ABI round-trip, gas constants, attribute conversion, `Ident32` validation. |
| Direct-revm CREATE profile | `crates/arkiv-node/tests/profile_create_op_direct.rs` — chrome-trace per-tx workload via `ArkivEthEvmFactory::create_evm`. |
| Full pipeline e2e | `e2e/tests/full_pipeline_e2e.rs` — boots an in-process Arkiv-enabled `EthereumNode`, walks every op type + every query construct + atBlock + pagination + non-owner revert. |

---

## 5. Key design decisions, recapped

| Decision | Why |
|---|---|
| Precompile target at `ARKIV_ADDRESS = 0x4400…0044` | Stable public address for the native ABI surface; no bytecode is deployed there. |
| System account (entitydb-internal at `0x4400…0046`) | Hosts consensus storage (counter, nonces, ID maps) on a dedicated empty-coded account. Splitting it from `ARKIV_ADDRESS` keeps the precompile target itself a pure programmatic-registration target. |
| Lazy system-account materialisation | `arkiv-entitydb::bump_nonce` calls `StateAdapter::ensure_account_persists` on first touch, bumping the account's nonce to 1 so EIP-161 does not prune it. |
| Custom `EvmFactory` (not ExEx) | State mutation happens inside EVM execution; the result lands in `stateRoot` and inherits reth's standard reorg machinery. |
| Bitmap as account code (`codeHash` = `keccak256(bitmap)`) | Content-addressing in the trie comes for free; query verification is one `eth_getProof` per bitmap. |
| ART as account code (`codeHash` = `keccak256(art_bytes)`) | Same content-addressing trick as bitmaps; range-query verification is one `eth_getProof` per index account. |
| `0xFE` prefix on entity-account code | Defends against accidental `CALL` to an entity address; `INVALID` opcode reverts immediately. |
| Entity tombstone keeps `nonce=1` | Prevents EIP-161 pruning of deleted entities. |
| Gas is pure function of calldata | Consensus determinism by construction — same op batch from any pre-state charges the same gas. |
| Owner / expiry / `Ident32` charset validated in the precompile | One validation surface; SDK clients depend on specific revert selectors. |
| Ownership follows EVM `msg.sender` | Plain reth does not enforce EOA-only calls. Contracts can own and mutate entities through their own address unless a future chain rule forbids contract callers. |
| `arkiv-entitydb` has no revm dep | Reusable for testing and read-only RPC evaluation; op handlers only talk to a `StateAdapter` trait. |
| No runtime chainspec mutation | Keeps `init` and `node` in agreement on the genesis hash. |

---

## 6. Things this design does *not* do

- **Built-in `--chain arkiv` name.** Could be done via a custom
  `ChainSpecParser`. Not pursued; the file-based flow works and
  composes uniformly with production genesis files.
- **Normal deployed Solidity contract.** `EntityRegistry.sol` is an
  interface for SDK codegen. The Rust precompile is the database
  engine.
- **EOA-only authorization.** Current semantics are normal EVM
  `msg.sender` semantics. If Arkiv needs EOA-only ownership, that
  should be enforced as a transaction validation or chain-rule change.
- **Arbitrary-pattern glob.** `~` accepts prefix patterns only.
- **Per-op tx-position metadata.** `transaction_index_in_block` and
  `operation_index_in_transaction` are kept in the wire shape for SDK
  parity but always 0.
- **Fault-proof EVM integration.** The expected path is a Rust FP
  program linking the same reth and Arkiv crates. Wiring and
  end-to-end verification remain an investigation item; see §6.

---

## 7. Fault-proof compatibility

All state changes go through revm's journaled state: account
creation, `SetCode`, `SetNonce`, `SetState`. These are standard
Ethereum state transitions included in the `stateRoot`. Nothing the
precompile writes is out-of-trie.

The precompile is deterministic across nodes — gas formulas are pure
functions of op shape, and trie writes are pure functions of
`(op batch, prior trie state)` — so any replay environment that links
the same reth EVM stack and Arkiv precompile should produce identical
state.

What such an integration would cover:

- Entity payload integrity: `codeHash` of entity account.
- Entity metadata: system-account ID maps and entity counter.
- Per-caller nonces: system-account `nonces` slot.
- Annotation index integrity (per-pair): `codeHash` of each pair
  account.
- Range-index integrity (per-key): `codeHash` of each index account.

`eth_getProof` works against every Arkiv account exactly as for any
Ethereum account.

---

## 8. Open questions

1. **Bytecode retention.** Old bitmap-byte, entity-RLP-byte, and
   ART-byte entries are reachable only via historical state roots.
   The node's retention policy determines how far back historical
   queries can reach.

2. **Caller policy.** The migration intentionally adopts normal
   `msg.sender` ownership semantics. Production may still want an
   EOA-only rule or an explicit contract-caller policy.

3. **First-sight overhead.** Every distinct `(k, v)` ever seen
   creates a pair account, and every distinct `k` ever seen creates
   an index account.

4. **ART gas calibration.** `G_ART_INDEXED_ANNOTATION = 6_000` is a
   flat per-attribute charge that should be re-evaluated once
   realistic ART sizes are measured.

5. **Arbitrary-pattern glob.** Mid-pattern wildcards and `?` would
   need an evaluator that scans the full ART rather than calling
   `iter_prefix`.

6. **Fees.** Native gas vs. an ERC-20 surcharge enforced by the
   precompile. Independent decision, can be deferred.

7. **Per-op tx-position metadata.** Plumbing real tx/operation
   positions through would need a block-builder side annotation.
