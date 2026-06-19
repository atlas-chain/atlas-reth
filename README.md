# arkiv-node

An upstream [reth](https://github.com/paradigmxyz/reth) execution node
for the **Arkiv** chain, plus operator tooling. Arkiv adds one native
extension to a regular Ethereum node: a custom `EvmFactory` that
registers the **Arkiv precompile** at `ARKIV_ADDRESS`
(`0x4400…0044`) into revm's `PrecompilesMap`. EOAs, contracts, and
SDKs `CALL` that address with the same `execute(Operation[])` /
`nonces(address)` ABI a Solidity contract would expose.

Entity payloads, the annotation index (per-pair roaring64 bitmaps),
the Tier-2 ART range index, and the global counter / nonces / ID maps
all live in the state trie as Ethereum accounts, committed in
`stateRoot`. No external indexer process, no out-of-trie state.

The binary serves both write and read paths:

- **Writes** target `ARKIV_ADDRESS` with calldata for
  `execute(Operation[])`. The precompile decodes the batch, validates
  ownership / liveness / `Ident32` charset, charges gas, mutates
  entity / pair / index account state plus the storage of an
  internally-managed system account (materialised lazily by
  `arkiv-entitydb`), and emits an `EntityOperation` event per op.
- **Reads** are served by the `arkiv_*` JSON-RPC namespace
  (`arkiv_query`, `arkiv_getEntityCount`, `arkiv_getBlockTiming`)
  backed entirely by local trie state. The query language and its
  interpreter live in the `arkiv-entitydb` crate.

```
                  ┌──────────────────────────────────────────────────┐
                  │ arkiv-node binary                                │
                  │                                                  │
                  │   revm + ArkivEthEvmFactory                      │
   user tx ──────►│   └─► ArkivPrecompile ──► trie state             │
                  │      (at ARKIV_ADDRESS)   (entity / pair /       │
                  │                            index accounts +      │
                  │                            system-account slots) │
                  │                                                  │
   user query ───►│   arkiv_* RPC (local reads via arkiv-entitydb)   │
                  └──────────────────────────────────────────────────┘
```

The chain has a single public address: `ARKIV_ADDRESS =
0x4400…0044`, the precompile registration target. No genesis allocation
is required to run the binary; the custom `EvmFactory` activates the
precompile programmatically.

Internally, `arkiv-entitydb` uses a second fixed address as a storage
host for per-caller nonces, the global entity counter, and the
ID ↔ address maps. That account is **materialised lazily on the first
op** by bumping its nonce to 1 so EIP-161 does not prune the storage.

## What This Repository Contains

| Crate | Role |
|---|---|
| `crates/arkiv-node` | Execution-client binary. Hosts the custom `EvmFactory`, the Arkiv precompile, and the `arkiv_*` RPC namespace. |
| `crates/arkiv-entitydb` | State-model primitives, the six op handlers, system-account accessors, and the query language. |
| `crates/arkiv-genesis` | `ARKIV_ADDRESS` re-export and dev-account alloc helpers. |
| `crates/arkiv-cli` | Operator CLI: submit entity ops, batch ops from JSON, traffic simulator, genesis dev-funding injection. |
| `e2e` | End-to-end tests against an in-process Arkiv-enabled `EthereumNode`. |

External dependencies of note:

| Dep | Repo | Role |
|---|---|---|
| `reth-*` / `reth-ethereum-*` | [`paradigmxyz/reth`](https://github.com/paradigmxyz/reth) | Node builder, Ethereum node components, storage API, EVM config. |

## Quick Start

### Local dev node

```bash
just node-dev
```

Assembles an Arkiv dev genesis (chain ID `1337`, 100 dev accounts
funded), initialises the datadir against it, and launches the node
with 2 s auto-mining. HTTP RPC on `127.0.0.1:8545`, WebSocket on
`127.0.0.1:8546`.

### Submit operations

```bash
just balance
just create --content-type application/json
just update --key 0x... --content-type ...
```

Or batch a sequence in one transaction:

```bash
just batch scripts/fixtures/attributes-all-types.json
```

### Continuous simulation

```bash
just simulate
just simulate --rate 2 --duration 5m
just simulate --max-ops-per-tx 8 --signer-count 25
just simulate --seed 42
```

Each signer holds at most one in-flight tx; up to `--signer-count`
concurrent batches. Each batch carries `1..=--max-ops-per-tx` ops in a
single `execute()` call.

### Inspect the embedded dev genesis

```bash
just genesis
just genesis | jq .alloc
```

## Running A Node

The binary uses the upstream reth Ethereum CLI surface. The precompile
is registered programmatically by the custom `EvmFactory`, and the
system account that hosts Arkiv storage is materialised lazily on the
first op.

```bash
arkiv-cli inject-predeploy genesis.json          # optional dev funding
arkiv-node init --chain genesis.json --datadir ./data
arkiv-node node --chain genesis.json --datadir ./data
```

`inject-predeploy` is a legacy command name; it only splices Arkiv dev
funding accounts into a geth-format genesis `alloc`. It does not deploy
bytecode at `ARKIV_ADDRESS`.

## Documentation

| Doc | What's in it |
|---|---|
| [`docs/1_overview.md`](docs/1_overview.md) | High-level orientation: what Arkiv on reth is, system diagram, content-addressed-code principle. |
| [`docs/2_state-model.md`](docs/2_state-model.md) | Canonical state model: entity / pair / index accounts, system-account slots, op lifecycle, gas, reorg. |
| [`docs/3_query.md`](docs/3_query.md) | Query language, evaluation flow, historical reads, verification recipes. |
| [`docs/4_engineering.md`](docs/4_engineering.md) | Crate layout, genesis construction, testing surface, fault-proof story, open questions. |
| [`docs/6_protocol-schedule-service.md`](docs/6_protocol-schedule-service.md) | HTTP service contract for publishing Arkiv protocol schedules. |

## Build, Test, Lint

```bash
just check
just build
just lint
just fmt

cargo test -p arkiv-entitydb
cargo test -p arkiv-node --lib
cargo test -p arkiv-e2e --test full_pipeline_e2e
```

The workspace pins `reth-*` crates to specific git revs in the root
`Cargo.toml`. Bumping them is a coordinated change; expect to
re-resolve API drift across the `EvmFactory` / precompile integration.

## License

GPL-3.0-or-later. See `LICENSE`.
