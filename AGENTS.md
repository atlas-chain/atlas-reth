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
L2/L3 with two extra predeploys (`EntityRegistry` at `0x4400…0044` and
a singleton system account at `0x4400…0046`) and a custom op-reth
`EvmFactory` that registers the Arkiv precompile into revm's
`PrecompilesMap` at `0x4400…0045`. Entity payloads, the annotation
index (per-pair roaring64 bitmaps), and the global ID/counter maps all
live in the L3 trie. No external indexer process, no out-of-trie
state.

## Workspace layout

```
crates/
  arkiv-node/         # binary: custom EvmFactory + Arkiv precompile + arkiv_* RPC
  arkiv-entitydb/     # state model + op handlers + query language (lexer/parser/interpreter)
  arkiv-cli/          # operator CLI: entity ops, batches, simulate, inject-predeploy
  arkiv-genesis/      # shared lib: predeploy addresses, runtime bytecode, alloc helpers
e2e/                  # full-pipeline integration tests (uses NodeTestContext)
contracts/
  src/EntityRegistry.sol             # in-tree contract source
  artifacts/EntityRegistry.runtime.hex   # baked into arkiv-genesis via include_str!
chainspec/dev.base.json   # geth-format dev chainspec (predeploys injected at recipe time)
docs/1_overview.md        # high-level orientation — read this first
docs/2_state-model.md     # canonical state model — read if touching precompile / op handlers / gas
docs/3_query.md           # query language + verification — read if touching the query path
docs/4_engineering.md     # crate layout, genesis, testing, FP, open questions
scripts/fixtures/         # batch JSON fixtures
demo/fixtures/            # smaller demo fixtures
justfile                  # all dev recipes
```

## Where things live

| Concern | File |
|---|---|
| Custom `EvmFactory` wrapping `OpEvmFactory<OpTx>` | `crates/arkiv-node/src/evm.rs` |
| Arkiv precompile (caller restriction, gas, dispatch) | `crates/arkiv-node/src/precompile.rs` |
| `arkiv_*` RPC namespace + `RethStateAdapter` | `crates/arkiv-node/src/rpc.rs` |
| RPC installation hook | `crates/arkiv-node/src/install.rs` |
| Predeploy detection (bytecode hash) | `crates/arkiv-node/src/genesis.rs` |
| CLI flags + predeploy gating | `crates/arkiv-node/src/{cli,main}.rs` |
| Entity / pair / system layout, RLP, bitmap | `crates/arkiv-entitydb/src/lib.rs` |
| `StateAdapter` trait + `InMemoryAdapter` (test-utils feature) | `crates/arkiv-entitydb/src/lib.rs` |
| Op handlers (`create` / `update` / `extend` / `transfer` / `delete` / `expire`) | `crates/arkiv-entitydb/src/lib.rs` |
| Query lexer / parser / AST / interpreter | `crates/arkiv-entitydb/src/query/` |
| Predeploy address + runtime bytecode loader | `crates/arkiv-genesis/src/lib.rs` |
| EntityRegistry contract source | `contracts/src/EntityRegistry.sol` |
| CLI commands + batch format | `crates/arkiv-cli/src/main.rs` |
| Traffic simulator | `crates/arkiv-cli/src/simulate.rs` |
| Full-pipeline e2e test | `e2e/tests/full_pipeline_e2e.rs` |

External: ABI types and operation encoders used by `arkiv-cli` come
from `arkiv-bindings` (pinned by rev in the root `Cargo.toml`, sourced
from [`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts)).
The node itself doesn't depend on `arkiv-bindings`; it uses the in-tree
contract source.

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
just contracts-build    # forge build + refresh runtime hex artifact
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
- **No runtime mutation of state to install the predeploys.** They
  must be in `alloc` from block 0. `arkiv-cli inject-predeploy` is the
  supported path; the same chainspec file must drive both `init` and
  `node` so genesis hashes match. See
  [`docs/4_engineering.md`](docs/4_engineering.md) §2.
- **The runtime bytecode is committed.** Edit
  `contracts/src/EntityRegistry.sol` → run `just contracts-build` to
  refresh `contracts/artifacts/EntityRegistry.runtime.hex`. The hex is
  baked into `arkiv-genesis` via `include_str!`. Without that step the
  binary still ships the old bytecode.
- **Predeploy detection is bytecode-equality-gated.** If you change
  the contract source without rebuilding the artifact, the activation
  guard silently fails to detect the predeploy.
- **Gas must be a pure function of calldata.** Two nodes executing the
  same op batch from different pre-states must charge identical gas.
  Don't introduce state-dependent gas paths.
- **The contract↔precompile boundary owns content semantics.** The
  contract validates ownership, liveness, and `Ident32` charset. The
  precompile decodes the calldata, charges gas, and dispatches to
  `arkiv-entitydb`'s op handlers via a `StateAdapter` impl over
  `EvmInternals`. The op handlers do the actual indexing math
  (system counter, ID maps, bitmap deltas, RLP encode).
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
- When touching genesis / predeploy logic or the crate layout, update
  [`docs/4_engineering.md`](docs/4_engineering.md) if the
  operator-facing flow changes.
