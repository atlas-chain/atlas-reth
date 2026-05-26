# AGENTS.md

Orientation for coding agents working in this repo. For human-facing
docs read [`README.md`](README.md) and
[`docs/1_overview.md`](docs/1_overview.md) first. The canonical
state model is [`docs/2_state-model.md`](docs/2_state-model.md);
query semantics and verification recipes are in
[`docs/3_query.md`](docs/3_query.md); crate layout, genesis,
testing, and the fault-proof story are in
[`docs/4_engineering.md`](docs/4_engineering.md).

## What this repo is

A drop-in [op-reth](https://github.com/ethereum-optimism/optimism)
build for the **Arkiv** chain plus operator tooling. Arkiv = OP-stack
L2/L3 with one extension to op-reth: a custom `EvmFactory` that
registers the **Arkiv precompile** at `ARKIV_ADDRESS` (`0x4400…0044`)
into revm's `PrecompilesMap`. EOAs and SDKs `CALL` that address with
the `execute(Operation[])` / `nonces(address)` ABI declared by
`IEntityRegistry` (interface-only — there is no deployed contract).
The precompile owns per-op validation (ownership, expiration,
`Ident32` charset), `EntityOperation` event emission, gas accounting,
and dispatch into `arkiv-entitydb`. Entity payloads, the annotation
index (per-pair roaring64 bitmaps), the Tier-2 ART range index, and
the global counter / nonces / ID maps all live in the L3 trie. No
external indexer process, no out-of-trie state.

Two canonical addresses:

- `ARKIV_ADDRESS = 0x4400…0044` — precompile registration target. No
  genesis presence; activation is programmatic via `EvmFactory`.
- `SYSTEM_ACCOUNT_ADDRESS = 0x4400…0046` — empty-coded account at
  `nonce=1` in genesis, hosts the precompile's consensus storage
  (per-caller nonces, global entity counter, ID ↔ address maps).
  `nonce=1` so EIP-161 doesn't prune the storage on the first write.

## Workspace layout

```
crates/
  arkiv-node/         # binary: custom EvmFactory + Arkiv precompile + arkiv_* RPC
  arkiv-entitydb/     # state model + op handlers + system-state API + query language
  arkiv-cli/          # operator CLI: entity ops, batches, simulate, inject-predeploy
  arkiv-genesis/      # shared lib: ARKIV_ADDRESS + SYSTEM_ACCOUNT_ADDRESS + alloc helpers
e2e/                  # full-pipeline integration tests (uses NodeTestContext)
contracts/
  src/EntityRegistry.sol    # IEntityRegistry interface — ABI surface for SDK codegen (no deployed bytecode)
chainspec/dev.base.json     # geth-format dev chainspec (system account injected at recipe time)
docs/1_overview.md          # high-level orientation — read this first
docs/2_state-model.md       # canonical state model — read if touching precompile / op handlers / gas
docs/3_query.md             # query language + verification — read if touching the query path
docs/4_engineering.md       # crate layout, genesis, testing, FP, open questions
scripts/fixtures/           # batch JSON fixtures
demo/fixtures/              # smaller demo fixtures
justfile                    # all dev recipes
```

## Where things live

| Concern | File |
|---|---|
| Custom `EvmFactory` wrapping `OpEvmFactory<OpTx>` | `crates/arkiv-node/src/evm.rs` |
| Arkiv precompile (selector dispatch, validation, gas, op dispatch, event emission) | `crates/arkiv-node/src/precompile.rs` |
| `arkiv_*` RPC namespace + `RethStateAdapter` | `crates/arkiv-node/src/rpc.rs` |
| RPC installation hook | `crates/arkiv-node/src/install.rs` |
| System-account detection (nonce ≥ 1 at `SYSTEM_ACCOUNT_ADDRESS`) | `crates/arkiv-node/src/genesis.rs` |
| CLI flags + chainspec gating | `crates/arkiv-node/src/{cli,main}.rs` |
| Entity / pair / index layout, RLP, bitmap, ART | `crates/arkiv-entitydb/src/lib.rs` |
| `StateAdapter` trait + `InMemoryAdapter` (test-utils feature) | `crates/arkiv-entitydb/src/lib.rs` |
| Op handlers (`create` / `update` / `extend` / `transfer` / `delete` / `expire`) | `crates/arkiv-entitydb/src/lib.rs` |
| System-state API (`read_nonce` / `bump_nonce`) + `pub(crate)` slot layout | `crates/arkiv-entitydb/src/lib.rs` |
| Query lexer / parser / AST / interpreter | `crates/arkiv-entitydb/src/query/` |
| `ARKIV_ADDRESS`, `SYSTEM_ACCOUNT_ADDRESS`, alloc helpers | `crates/arkiv-genesis/src/lib.rs` |
| `IEntityRegistry` ABI surface (interface only, no bytecode) | `contracts/src/EntityRegistry.sol` |
| CLI commands + batch format | `crates/arkiv-cli/src/main.rs` |
| Traffic simulator | `crates/arkiv-cli/src/simulate.rs` |
| Full-pipeline e2e test | `e2e/tests/full_pipeline_e2e.rs` |

The precompile's `sol!` block in `crates/arkiv-node/src/precompile.rs`
mirrors the ABI declared in `contracts/src/EntityRegistry.sol`. Keep
the two in lockstep — SDK consumers codegen against the .sol; the
node decodes calldata against the `sol!` block.

## Commands

Use `just` recipes. **Compile/run/network commands are long-running —
defer them to the user per the tool-usage policy and wait for output.**

Read-only / fast (fine to run yourself):

```
just genesis            # print assembled dev genesis JSON
just fmt                # rustfmt
```

Defer to the user:

```
just check              # cargo check --workspace
just build              # cargo build --workspace
just lint               # cargo clippy --workspace -- -D warnings
just node-dev           # full dev node (HTTP 8545, WS 8546)
just simulate ...       # continuous traffic generator
just batch <fixture>    # submit a batch JSON
```

Tests:

```
cargo test -p arkiv-entitydb                       # state model + query
cargo test -p arkiv-node --lib                     # precompile + rpc unit tests
cargo test -p arkiv-e2e --test full_pipeline_e2e   # full pipeline e2e
```

## Conventions and gotchas

- **Edition 2024**, MSRV `1.94`. Keep that in mind before reaching for
  nightly-only features.
- **`reth-*` and `reth-optimism-*` are pinned to specific git revs** in
  the root `Cargo.toml`. Bumping them is a coordinated change; expect
  API drift to surface across the `EvmFactory` / precompile integration.
- **No runtime mutation of state to install the system account.** The
  `SYSTEM_ACCOUNT_ADDRESS` account (empty code, `nonce=1`) must be in
  `alloc` from block 0. `arkiv-cli inject-predeploy` is the supported
  path; the same chainspec file must drive both `init` and `node` so
  genesis hashes match. See
  [`docs/4_engineering.md`](docs/4_engineering.md) §2.
- **The precompile is registered programmatically by `EvmFactory`** —
  no on-chain bytecode is deployed at `ARKIV_ADDRESS`. The system
  account at `SYSTEM_ACCOUNT_ADDRESS` exists purely as a storage host;
  activation guard is `nonce >= 1` at that address.
- **`contracts/src/EntityRegistry.sol` is an interface only.** It
  declares the ABI (`execute(Operation[])`, `nonces(address)`,
  `EntityOperation` event, struct / error layouts) that the precompile
  implements; no bytecode is deployed. SDK consumers codegen against
  this file.
- **Gas must be a pure function of calldata.** Two nodes executing the
  same op batch from different pre-states must charge identical gas.
  Don't introduce state-dependent gas paths.
- **The precompile owns content semantics.** It validates ownership,
  liveness, and `Ident32` charset; emits `EntityOperation` events;
  charges gas; and dispatches to `arkiv-entitydb`'s op handlers via a
  `StateAdapter` impl over `EvmInternals`. The op handlers do the
  actual indexing math (entity counter, ID maps, bitmap deltas, ART
  deltas, RLP encode).
- **`arkiv-entitydb` owns the system-account slot layout.** `slot_*`
  helpers are `pub(crate)`; external callers use the public
  `read_nonce(state, caller)` / `bump_nonce(state, caller)` accessors.
  The precompile never does direct `sload`/`sstore` against
  `SYSTEM_ACCOUNT_ADDRESS`.
- **Per-op authorization** lives in the precompile: CREATE is open to
  any EOA; UPDATE / EXTEND / TRANSFER / DELETE require
  `input.caller == stored owner`; EXPIRE is caller-agnostic but
  requires `block.number > expiresAt`. DB chains forbid user-deployed
  contracts and disable EIP-7702, so `input.caller` is by construction
  the EOA that signed the tx.
- **Query language scope.** Equality family (`=`, `!=`, `IN`,
  `NOT IN`, `&&`, `||`, `NOT`, `*` / `$all`), range (`<`, `>`, `<=`,
  `>=`), and prefix-glob (`~`, `!~`) are all implemented. Range and
  prefix-glob evaluate against the Tier-2 ART index account
  (`keccak256("arkiv.index" || k)[:20]`). Arbitrary-pattern glob
  (mid-pattern wildcards) is **not** supported.
- **`transaction_index_in_block` and `operation_index_in_transaction`
  are 0 on the wire.** revm's precompile context doesn't expose either;
  they're kept in the response shape for SDK parity but never carry
  real values today.

## Working style for this repo

- Prefer matching the existing terse, comment-light Rust style. Only
  add comments when the *why* is non-obvious.
- When touching the precompile / state model / gas, update
  [`docs/2_state-model.md`](docs/2_state-model.md) in the same
  change. That doc is the canonical spec; downstream clients read
  from it.
- When touching the query path / RPC, update
  [`docs/3_query.md`](docs/3_query.md) in the same change.
- When touching genesis / system-account logic or the crate layout,
  update [`docs/4_engineering.md`](docs/4_engineering.md) if the
  operator-facing flow changes.
