# arkiv-op-reth

An [op-reth](https://github.com/ethereum-optimism/optimism)-derived
execution node for the **Arkiv** chain, plus operator tooling. Arkiv is
an OP-stack L2/L3 with two predeploys (`EntityRegistry` at
`0x4400…0044` and a singleton system account at `0x4400…0046`) and one
in-process extension to op-reth: a custom `EvmFactory` that registers
the **Arkiv precompile** into revm's `PrecompilesMap` at `0x4400…0045`.
Entity payloads, the annotation index (per-pair roaring64 bitmaps), and
the global ID/counter maps all live in the L3 state trie as Ethereum
accounts — committed in `stateRoot`. There is no external indexer
process and no out-of-trie state.

The binary serves both write and read paths:

- **Writes** go through `EntityRegistry.execute(Operation[])`. The
  contract validates ownership, liveness, and attribute-name charset
  (`Ident32`), then `CALL`s the precompile, which decodes the batch,
  charges gas, and mutates entity / pair / system-account state via
  revm's journaled state.
- **Reads** are served by the `arkiv_*` JSON-RPC namespace
  (`arkiv_query`, `arkiv_getEntityCount`, `arkiv_getBlockTiming`)
  backed entirely by local trie state. The query language and its
  interpreter live in the `arkiv-entitydb` crate.

```
                  ┌──────────────────────────────────────────────────┐
                  │ arkiv-node binary                                │
                  │                                                  │
                  │   revm + ArkivOpEvmFactory                       │
   user tx ──────►│   └─► ArkivPrecompile ──► trie state             │
                  │                           (entity / pair /       │
                  │                            system accounts)      │
                  │                                                  │
   user query ───►│   arkiv_* RPC (local reads via arkiv-entitydb)   │
                  └──────────────────────────────────────────────────┘
```

The query language today is the **equality family** (`=`, `!=`, `IN`,
`NOT IN`, `&&`, `||`, `NOT`, `*` / `$all`). Range and glob queries
(`<`, `>`, `~`) need an ordered sibling index over `(annot_key,
annot_val)` pairs that isn't built yet — they're parse errors.

---

## What this repository contains

| Crate | Role |
|---|---|
| `crates/arkiv-node` | Execution-client binary. Hosts the custom `EvmFactory`, the Arkiv precompile, and the `arkiv_*` RPC namespace. |
| `crates/arkiv-entitydb` | State-model primitives (entity / pair / system layout, RLP, bitmap), the six op handlers (`create` / `update` / `extend` / `transfer` / `delete` / `expire`), and the query language (lexer + parser + tree-walking interpreter). |
| `crates/arkiv-genesis` | Predeploy address, runtime-bytecode loader, genesis-alloc helpers. |
| `crates/arkiv-cli` | Operator CLI: submit entity ops, batch ops from JSON, traffic simulator, genesis post-processing. |
| `e2e` | End-to-end tests against an in-process `ArkivOpNode`. |

External dependencies of note:

| Dep | Repo | Role |
|---|---|---|
| `reth-optimism-*` | [`ethereum-optimism/optimism`](https://github.com/ethereum-optimism/optimism) | OP-reth runtime, chainspec, primitives. |
| `reth-*` | [`paradigmxyz/reth`](https://github.com/paradigmxyz/reth) | Node builder, storage API. |
| `arkiv-bindings` | [`arkiv-contracts`](https://github.com/Arkiv-Network/arkiv-contracts) | ABI types used by `arkiv-cli` for tx submission. (Not used by the node itself — the contract source lives in-tree at `contracts/src/EntityRegistry.sol`.) |

---

## Quick start

### Local dev node

```bash
just node-dev
```

Assembles an Arkiv dev genesis (chain ID `1337`, 100 dev accounts
funded, predeploys at `0x4400…0044` / `0x4400…0046`), initialises the
datadir against it, and launches the node with 2 s auto-mining. HTTP
RPC on `127.0.0.1:8545`, WebSocket on `127.0.0.1:8546`.

### Submit operations

```bash
just balance                                 # 10,000 ETH on the dev account
just create --content-type application/json  # mint an entity
just update --key 0x... --content-type ...   # update it
```

Or batch a sequence in one transaction:

```bash
just batch scripts/fixtures/attributes-all-types.json
```

### Continuous simulation

```bash
just simulate                                          # 0.5 batches/s, 10 signers, until Ctrl-C
just simulate --rate 2 --duration 5m                   # 2 batches/s for 5 min
just simulate --max-ops-per-tx 8 --signer-count 25     # bigger batches, more parallelism
just simulate --seed 42                                # deterministic run
```

Each signer holds at most one in-flight tx; up to `--signer-count`
concurrent batches. Each batch carries `1..=--max-ops-per-tx` ops in a
single `execute()` call.

### Inspect the embedded dev chainspec

```bash
just genesis            # prints assembled JSON to stdout
just genesis | jq .alloc
```

---

## Project layout

```
.
├── crates/
│   ├── arkiv-node/           # binary + custom EvmFactory + arkiv_* RPC
│   ├── arkiv-entitydb/       # state model + op handlers + query language
│   ├── arkiv-cli/            # operator CLI
│   └── arkiv-genesis/        # shared genesis primitives
├── contracts/
│   ├── src/EntityRegistry.sol         # in-tree contract source
│   └── artifacts/EntityRegistry.runtime.hex   # baked into arkiv-genesis at build
├── e2e/                      # full-pipeline integration tests
├── chainspec/dev.base.json   # geth-format dev chainspec sans predeploy
├── docs/
│   ├── 1_overview.md         # high-level orientation
│   ├── 2_state-model.md      # canonical state model
│   ├── 3_query.md            # query language + verification recipes
│   └── 4_engineering.md      # crate layout, genesis, testing, FP, open questions
├── scripts/fixtures/         # example batch JSON files
├── demo/fixtures/            # smaller demo fixtures
├── docker/                   # runtime + dev container images
└── justfile                  # all dev/test recipes
```

---

## Running against a real OP chain

For production / testnet deployment the `EntityRegistry` predeploy
must be in the genesis allocs from block 0:

```bash
op-deployer apply --intent intent.toml --workdir ./ops     # standard OP genesis
arkiv-cli inject-predeploy ops/genesis.json                # add predeploys + dev funding
op-reth init --chain ops/genesis.json --datadir ./data
op-reth node --chain ops/genesis.json --datadir ./data
```

`inject-predeploy` reads the input genesis, splices the predeploy runtime
code + system account + dev-funded accounts into `alloc`, and writes
back. The same chainspec drives both `init` and `node`, so genesis
hashes match.

See [`docs/4_engineering.md`](docs/4_engineering.md) §2 for the
genesis-construction rules (Path-A chainspecs, Holocene `extraData`,
why we don't mutate the chainspec at startup).

---

## Documentation

| Doc | What's in it |
|---|---|
| [`docs/1_overview.md`](docs/1_overview.md) | High-level orientation: what `arkiv-op-reth` is, system diagram, content-addressed-code principle |
| [`docs/2_state-model.md`](docs/2_state-model.md) | Canonical state model: entity / pair / index / system accounts, op lifecycle, gas, reorg |
| [`docs/3_query.md`](docs/3_query.md) | Query language, evaluation flow, historical reads, verification recipes |
| [`docs/4_engineering.md`](docs/4_engineering.md) | Crate layout, genesis construction, testing surface, fault-proof story, open questions |

External references:

- EntityRegistry contract: <https://github.com/Arkiv-Network/arkiv-contracts>
- op-reth: <https://github.com/ethereum-optimism/optimism/tree/develop/rust/op-reth>
- reth: <https://github.com/paradigmxyz/reth>

---

## Build, test, lint

```bash
just check          # cargo check --workspace
just build          # cargo build --workspace
just lint           # cargo clippy --workspace -- -D warnings
just fmt            # cargo fmt --all

cargo test -p arkiv-entitydb               # state-model + query unit tests
cargo test -p arkiv-e2e --test full_pipeline_e2e  # full pipeline against in-process node
```

The workspace pins `reth-*` and `reth-optimism-*` to specific git revs
in the root `Cargo.toml`. Bumping them is a coordinated change; expect
to re-resolve API drift across the EvmFactory / precompile integration.

---

## License

GPL-3.0-or-later. See `LICENSE`.
