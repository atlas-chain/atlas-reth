# Arkiv on op-reth: Engineering

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
- [3. Testing surface](#3-testing-surface)
- [4. Key design decisions, recapped](#4-key-design-decisions-recapped)
- [5. Things this design does *not* do](#5-things-this-design-does-not-do)
- [6. Fault-proof compatibility](#6-fault-proof-compatibility)
- [7. Open questions](#7-open-questions)

---

## 1. Workspace crates

```
crates/
  arkiv-node/         # binary + custom EvmFactory + Arkiv precompile + arkiv_* RPC
  arkiv-entitydb/     # state model + op handlers + system-state API + query language
  arkiv-cli/          # operator CLI: entity ops, batches, simulate, inject-predeploy
  arkiv-genesis/      # shared lib: ARKIV_ADDRESS + SYSTEM_ACCOUNT_ADDRESS + alloc helpers
e2e/                  # full-pipeline integration tests
contracts/
  src/EntityRegistry.sol    # IEntityRegistry interface — ABI surface for SDK codegen (no deployed bytecode)
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
- `SYSTEM_ACCOUNT_ADDRESS` (`0x4400…0046`) — system-account address.
  Re-exported from `arkiv-entitydb`. Pre-allocated at `nonce=1` by
  `genesis_alloc()`.
- `ARKIV_DEV_MNEMONIC`, `DEV_ADDRESS`, `ARKIV_DEV_ACCOUNT_COUNT` — the
  hardhat-compatible dev mnemonic and the 100 pre-funded dev accounts
  derived from it.
- `system_account() -> GenesisAccount` — empty-code, `nonce=1` entry
  for the system account.
- `genesis_alloc()`, `dev_funding_alloc(...)` — assemble system
  account + dev-funding entries for splicing into a `Genesis.alloc`.

### 1.2 `arkiv-entitydb`

Canonical home of the state model. No `revm` deps, no DB deps —
runs against an abstract `StateAdapter` trait. Contains:

- **Primitives.** `EntityRlp`, `Bitmap` (roaring64), `IndexTree`
  (Adaptive Radix Tree over annotation values), `entity_address`,
  `pair_address`, `index_address`, built-in annotation keys,
  system-account address.
- **`StateAdapter` trait.** `code` / `set_code` / `tombstone_code` /
  `storage` / `set_storage`. Implemented in production by
  `RevmStateAdapter` (precompile path, journaled writes) and
  `RethStateAdapter` (RPC read path, against a `StateProvider`
  snapshot). The `test-utils` feature exposes `InMemoryAdapter` for
  unit tests.
- **Op handlers.** `create` / `update` / `extend` / `transfer` /
  `delete` / `expire`. All indexing logic (system counter, ID maps,
  Tier-1 bitmap deltas and Tier-2 ART deltas across built-in and
  user annotations, RLP encode/decode, tombstoning) lives here.
- **System-state API.** `read_nonce(state, caller) -> Result<u32>`
  and `bump_nonce(state, caller) -> Result<u32>` are the only public
  accessors for the per-EOA nonce slot. The underlying `slot_*`
  helpers (`slot_entity_count`, `slot_id_to_addr`, `slot_addr_to_id`,
  `slot_nonces`) are `pub(crate)` — the system-account storage layout
  is an entitydb implementation detail.
- **Query language.** Lexer (hand-rolled), recursive-descent parser
  producing a `Query` AST, tree-walking interpreter that runs against
  a `StateAdapter` and returns a roaring64 `Bitmap` of matching
  entity IDs. Plus a paginated `execute(state, query_str, params)`
  convenience that resolves IDs to `EntityRlp` via the system-account
  ID map.

### 1.3 `arkiv-node`

The execution-client binary. A thin wrapper around
`reth_optimism_cli::Cli`. Layout:

- `evm.rs` — `ArkivOpEvmFactory` wrapping `OpEvmFactory<OpTx>`.
  Registers the Arkiv precompile at `ARKIV_ADDRESS` into
  `PrecompilesMap` in both `create_evm` and
  `create_evm_with_inspector` so simulation, tracing,
  payload-building, validation, and canonical execution all see the
  same set. Also includes the local newtype around `OpEvmConfig`
  (needed for the orphan-rule `ConfigureEngineEvm<OpExecData>` impl)
  and the `ArkivOpNode` / `ArkivOpExecutorBuilder` layers that wire
  it all together.
- `precompile.rs` — caller restriction (direct CALL only;
  `DELEGATECALL`/`CALLCODE`/value-bearing rejected; `STATICCALL`
  allowed for `nonces(address)` only), calldata decode (selector
  dispatch between `execute(Operation[])` and `nonces(address)`),
  per-op validation (ownership, expiration, `Ident32` charset),
  Solidity-style revert encoding with v1 error selectors,
  `EntityOperation` event emission, gas accounting (pure function of
  op shape), `RevmStateAdapter` over `EvmInternals`, op dispatch
  into `arkiv-entitydb`.
- `rpc.rs` — `arkiv_*` JSON-RPC namespace + wire-format types.
  `RethStateAdapter` wraps a `StateProvider` for read-only state. The
  query handler is a thin shell over `arkiv_entitydb::query::execute`.
- `install.rs` — `extend_rpc_modules` hook registering the `arkiv_*`
  namespace.
- `genesis.rs` — `has_arkiv_system_account(chain)` activation guard
  (`nonce >= 1` at `SYSTEM_ACCOUNT_ADDRESS`).
- `cli.rs` — `ArkivExt` clap args.

There is **no chainspec mutation**. The system account must be in the
loaded chainspec's `alloc` (see
[Genesis construction](#2-genesis-construction)).

### 1.4 `arkiv-cli`

Operator command-line tool. Two distinct surfaces:

**Entity operations** (require an RPC endpoint + signer):
`create`, `update`, `extend`, `transfer`, `delete`, `expire`, `query`,
`balance`, `spam`, `batch`, `simulate`. All ops are submitted as
`execute(Operation[])` calls to `ARKIV_ADDRESS` — the precompile
decodes the calldata, validates, charges gas, mutates state, and
emits the `EntityOperation` log. The CLI speaks the standard Solidity
ABI; from the wire it's indistinguishable from a contract call.

**Genesis post-processing** (no network required):
`arkiv-cli inject-predeploy <input.json>` reads a geth-format genesis
and splices the system account at `SYSTEM_ACCOUNT_ADDRESS` plus
dev-funded accounts into `alloc`. Composes with op-deployer output
for production deployments.

The traffic simulator (`simulate`) rotates through mnemonic-derived
signers, maintains an in-memory pool of alive entities, and submits a
weighted random mix of CRUD ops. State updates come from decoding
`EntityOperation` logs and reading the entity RLP via `eth_getCode`.

### 1.5 `e2e`

End-to-end integration tests. Uses `reth-e2e-test-utils`'
`NodeTestContext` to boot an `ArkivOpNode` in-process, then drives it
via the `World` helper in `e2e/src/lib.rs` (signer pool, nonce
tracking, ABI encoding, query plumbing). `tests/full_pipeline_e2e.rs`
walks a single narrative through every op type and every query
construct.

---

## 2. Genesis construction

Genesis is the thorniest part of integrating with the OP stack. The
current rules:

### 2.1 No runtime mutation

The chainspec is treated as read-only data flowing in from `--chain`.
Whatever needs to be in there must already be in there before
`--chain` is parsed. This is what lets the binary be a true drop-in
op-reth, and what keeps `op-reth init` and `op-reth node` in
agreement on the genesis hash.

### 2.2 Path-A chainspec

OP-reth supports two paths to build an `OpChainSpec`: a **pure-JSON**
path (hardforks in `config.{bedrockBlock, regolithTime, …}`,
EIP-1559 params in `config.optimism`) and a **programmatic** path
(forks attached in code via `LazyLock`).

For an `--chain ./file.json` flow to work for both `init` and `node`,
the chainspec **must** be the pure-JSON form. The programmatic form
loads the JSON with no hardforks active, the engine produces
post-hardfork blocks anyway, and validation explodes.

`chainspec/dev.base.json` is therefore pure-JSON, with all OP
hardforks activated at time 0.

### 2.3 The Holocene `extraData` requirement

After Holocene, EIP-1559 base-fee parameters are encoded in the
previous block's `extraData` (9 bytes:
`[version=0x00][denominator: u32 BE][elasticity: u32 BE]`). When
block 1 is validated against genesis, the consensus path bails if
`genesis.extra_data` isn't exactly 9 bytes.

The decoder has a documented fallback: if both encoded values are
zero, it falls back to the chainspec's
`base_fee_params_at_timestamp`. So `extraData = 0x000000000000000000`
(9 zero bytes) is the canonical "use chainspec params at block 0"
value. `chainspec/dev.base.json` ships with this.

### 2.4 The injection step

`chainspec/dev.base.json` ships with an empty `alloc` — no system
account, no funded accounts. Both are added via
`arkiv-cli inject-predeploy` at recipe time:

```bash
cp chainspec/dev.base.json $TMPDIR/genesis.json
arkiv-cli inject-predeploy $TMPDIR/genesis.json
op-reth init --chain $TMPDIR/genesis.json --datadir $TMPDIR
op-reth node --chain $TMPDIR/genesis.json --datadir $TMPDIR …
```

This composes with op-deployer output for production:

```bash
op-deployer apply --intent intent.toml --workdir ./ops
arkiv-cli inject-predeploy ops/genesis.json
op-reth init --chain ops/genesis.json --datadir ./data
op-reth node --chain ops/genesis.json --datadir ./data
```

`ARKIV_ADDRESS` itself gets no genesis entry — the precompile is
registered programmatically by the custom `EvmFactory`.

### 2.5 System account pre-allocation

`genesis_alloc()` pre-allocates the system account at
`SYSTEM_ACCOUNT_ADDRESS` with `nonce=1` (empty code, empty storage).
The `nonce=1` defeats EIP-161 pruning before the precompile writes
its first slot. Pre-allocation also avoids a per-`Create`
"does the system account exist?" check.

---

## 3. Testing surface

| Layer | Where |
|---|---|
| `arkiv-genesis` unit tests | `crates/arkiv-genesis/src/lib.rs` (`#[cfg(test)]`) — alloc shape, signer derivation. |
| State model + op handlers | `crates/arkiv-entitydb/src/lib.rs` (`#[cfg(test)]`) — against `InMemoryAdapter`. |
| Query lexer + parser unit tests | `crates/arkiv-entitydb/src/query/{lexer,parser}.rs`. |
| Query interpreter integration | `crates/arkiv-entitydb/tests/query_eval.rs` — parse + evaluate end-to-end against `InMemoryAdapter`. |
| Precompile unit tests | `crates/arkiv-node/src/precompile.rs` — ABI round-trip, gas constants, attribute conversion, `Ident32` validation. |
| Direct-revm CREATE profile | `crates/arkiv-node/tests/profile_create_op_direct.rs` — chrome-trace per-tx workload via `ArkivOpEvmFactory::create_evm`. |
| Full pipeline e2e | `e2e/tests/full_pipeline_e2e.rs` — boots an in-process `ArkivOpNode`, walks every op type + every query construct + atBlock + pagination + non-owner revert. |

---

## 4. Key design decisions, recapped

| Decision | Why |
|---|---|
| Precompile target at `ARKIV_ADDRESS = 0x4400…0044` | Matches OP convention for system contract slots; the address is a property of the chain, not the binary. The SDK was already calling this address as the EntityRegistry contract in v1, so keeping it preserves the user-facing ABI. |
| System account at `SYSTEM_ACCOUNT_ADDRESS = 0x4400…0046` | Hosts the precompile's consensus storage (counter, nonces, ID maps) on a dedicated empty-coded account. Splitting it from `ARKIV_ADDRESS` keeps the precompile target itself a pure programmatic-registration target (no genesis dependency, mirrors how standard precompiles like `ecrecover` work). |
| Custom `EvmFactory` (not ExEx) | State mutation happens inside EVM execution; the result lands in `stateRoot` and inherits op-reth's standard reorg machinery for free. |
| Bitmap as account code (`codeHash` = `keccak256(bitmap)`) | Content-addressing in the trie comes for free; query verification is one `eth_getProof` per bitmap. |
| ART as account code (`codeHash` = `keccak256(art_bytes)`) | Same content-addressing trick as bitmaps; range-query verification is one `eth_getProof` per index account. |
| `0xFE` prefix on entity-account code | Defends against accidental `CALL` to an entity address; `INVALID` opcode reverts immediately. |
| Entity tombstone keeps `nonce=1` | Prevents EIP-161 pruning of deleted entities. |
| Gas is pure function of calldata | Consensus determinism by construction — same op batch from any pre-state charges the same gas. |
| Owner / expiry / `Ident32` charset validated in the precompile | One validation surface; SDK clients depend on specific revert selectors (`NotOwner`, `Ident32InvalidByte`, …) which the precompile emits. |
| Owner / `expires_at` live only in the entity RLP | Single source of truth — no separate contract-side mapping to keep in sync with the RLP. The precompile reads them from the RLP for both authorization and event emission. |
| System-state slot layout is `pub(crate)` in entitydb | The storage layout is an entitydb implementation detail; external callers (the precompile, tests) go through `read_nonce` / `bump_nonce` and the op handlers. |
| `arkiv-entitydb` has no revm dep | Reusable for testing (against `InMemoryAdapter`) and for the read path (against `RethStateAdapter`); op handlers only talk to a `StateAdapter` trait. |
| Path-A chainspec | `op-reth init` and `op-reth node` need to agree on genesis hash when reading the same JSON file. |
| `inject-predeploy` as a separate post-process | Composes with op-deployer output rather than forking it; same tool serves dev and prod. |
| `arkiv-genesis` as its own crate | Both binaries need the same constants; lifting them out avoids cross-bin deps. |
| No runtime chainspec mutation | Removes the `init`/`node` genesis-hash divergence bug structurally. |

---

## 5. Things this design does *not* do

- **Built-in `--chain arkiv` name.** Could be done via a custom
  `ChainSpecParser`. Not pursued; the file-based flow works and
  composes uniformly with prod.
- **Mainnet system-account registration in `L2Genesis.s.sol`.** The
  cleanest long-term home for the predeploy entry. Not pursued yet;
  the post-process approach has a tinier surface we own.
- **Arbitrary-pattern glob.** `~` accepts prefix patterns only —
  mid-pattern wildcards and `?` would need a full ART scan rather
  than `iter_prefix`. Not on the critical path.
- **Per-op tx-position metadata.** `transaction_index_in_block` and
  `operation_index_in_transaction` are kept in the wire shape for SDK
  parity but always 0 — revm's precompile context doesn't expose
  either.
- **L1 / op-node / op-batcher / op-proposer.** Out of scope. This
  repo is the L3 execution client only.
- **Fault-proof EVM integration.** The expected path is `kona`
  (Rust FP program) on `asterisc` (RISC-V VM), where
  `arkiv-entitydb` and the Arkiv precompile compile in directly via
  the shared reth crates. Wiring and end-to-end verification are
  tracked as an investigation item; see §6.

---

## 6. Fault-proof compatibility

All state changes go through revm's journaled state: account
creation, `SetCode`, `SetNonce`, `SetState`. These are standard
Ethereum state transitions included in the `stateRoot`. Nothing the
precompile writes is out-of-trie.

The fault-proof path that composes with Arkiv is **`kona`** (Rust
fault-proof program) on **`asterisc`** (RISC-V VM). Kona links
against the same reth crates as the sequencer, so `arkiv-entitydb`
and the Arkiv precompile land in the FP program by ordinary Rust
linkage with no extra glue.

The precompile is deterministic across nodes — gas formulas are pure
functions of op shape, and trie writes are pure functions of
`(op batch, prior trie state)` — so once the kona / asterisc path is
wired up, sequencer, validator, and FP replays produce identical
state.

What such an integration would cover:

- Entity payload integrity: `codeHash` of entity account (which
  commits to owner and expiry as RLP fields).
- Entity metadata: system-account ID maps and entity counter.
- Per-EOA nonces: system-account `nonces` slot.
- Annotation index integrity (per-pair): `codeHash` of each pair
  account (the bitmap content hash itself).
- Range-index integrity (per-key): `codeHash` of each index account
  (the ART content hash itself).

`eth_getProof` works against every Arkiv account exactly as for any
Ethereum account.

---

## 7. Open questions

1. **Op-reth `Bytecodes` retention.** Old bitmap-byte, entity-RLP-byte,
   and ART-byte entries in op-reth's `Bytecodes` table are reachable
   only via historical state roots. Op-reth's retention policy (full
   archive, pruned, snapshot-only) determines how far back historical
   queries can reach. Document the resulting window per node profile.

2. **First-sight overhead.** Every distinct `(k, v)` ever seen
   creates a pair account, and every distinct `k` ever seen creates
   an index account. For chains with extreme annotation cardinality
   (e.g., timestamps used as annotation values), this produces a lot
   of accounts and the ART for the affected key grows linearly. Worth
   modelling against realistic workloads — including the ART
   serialised size at very high cardinality.

3. **ART gas calibration.** `G_ART_INDEXED_ANNOTATION = 6_000` is a
   flat per-attribute charge that under-counts ENTITY_KEY user attrs
   and built-in-key writes (`$owner` on Transfer, `$expiration` on
   Extend, etc.). Both gaps are consensus-safe but should be
   re-evaluated once realistic ART sizes are measured.

4. **Arbitrary-pattern glob.** Today `~` accepts prefix patterns
   (`"image/*"`) only. Mid-pattern wildcards and `?` would need an
   evaluator that scans the full ART rather than calling
   `iter_prefix`. Not on the critical path.

5. **Fees.** Native gas vs. an ERC-20 surcharge enforced by the
   precompile. Independent decision, can be deferred. The
   precompile's gas model is unaffected either way.

6. **Per-op tx-position metadata.** `transaction_index_in_block` and
   `operation_index_in_transaction` are reported as 0 in
   `arkiv_query` responses today — revm's precompile context doesn't
   expose either. Plumbing them through would need a block-builder
   side annotation.

7. **Pair-account / index-account address collisions.**
   `keccak256("arkiv.pair" || …)[:20]` and
   `keccak256("arkiv.index" || …)[:20]` derivations could in
   principle collide with an existing externally-owned account on the
   L3. Genesis-time check + chain bring-up documentation is
   sufficient. (The system account at the fixed
   `SYSTEM_ACCOUNT_ADDRESS` cannot collide with derived addresses.)
