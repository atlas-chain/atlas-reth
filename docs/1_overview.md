# Arkiv on Reth: Overview

`arkiv-node` builds on upstream [reth](https://github.com/paradigmxyz/reth)
to turn a regular Ethereum execution node into an **Arkiv** node by
adding two things:

1. A custom Ethereum `EvmFactory` that registers an **Arkiv precompile**
   at `ARKIV_ADDRESS` (`0x4400000000000000000000000000000000000044`)
   into `PrecompilesMap` for every revm context (canonical execution,
   payload-building, simulation, validation, tracing). EOAs, contracts,
   and SDKs `CALL` this address with the `execute(Operation[])` /
   `nonces(address)` ABI declared by
   [`IEntityRegistry`](../contracts/src/EntityRegistry.sol).
2. An `arkiv_*` JSON-RPC namespace on the node's standard transports.

The precompile owns per-op validation (ownership, expiration,
`Ident32` charset), `EntityOperation` event emission, gas accounting,
and dispatch into `arkiv-entitydb`.

Every entity, every annotation index, and every counter lives inside
reth's standard world-state trie, committed in `stateRoot`. Reads are
served by the `arkiv_*` namespace backed entirely by local state. No
external indexer process, no JSON-RPC bridge, no ExEx, no out-of-trie
state.

The binary is a **reth Ethereum node with an Arkiv executor**. Any
valid geth-format Ethereum genesis that reth can parse works. The
custom `EvmFactory` installs the Arkiv precompile, the `arkiv_*` RPC
namespace exposes the read path, and the system account is created on
the first write.

---

## System overview

```
                  ┌──────────────────────────────────────────────────────────────────────────────────┐
                  │ arkiv-node binary                                                                │
                  │                                                                                  │
                  │   ┌──────────────────────────────────────┐                                       │
   user tx ──────►│   │ revm                                 │                                       │
                  │   │   ArkivEthEvmFactory installs the    │                                       │
                  │   │   ArkivPrecompile at ARKIV_ADDR;     │    ┌──────────────────────────────┐   │
                  │   │   precompile dispatches ops ─────────┼───►│ arkiv-entitydb               │   │
                  │   └──────────────────────────────────────┘    │                              │   │
                  │                                               │   reads/writes entity, pair, │   │
                  │   ┌──────────────────────────────────────┐    │   and index accounts +       │   │
   user query ───►│   │ arkiv_* RPC                          │    │   system-account storage on  │   │
                  │   │   parses + evaluates queries via ────┼───►│   the passed-in StateDB /    │   │
                  │   │   read-only StateProvider            │    │   StateProvider              │   │
                  │   └──────────────────────────────────────┘    └──────────────────────────────┘   │
                  └──────────────────────────────────────────────────────────────────────────────────┘
```

Everything flows through revm's journaled state and ends up in the
`stateRoot`. There is no separate KV table for Arkiv data.

---

## The fundamental building block

Arkiv reuses the same trie mechanism Ethereum uses for smart-contract
code: store arbitrary bytes in an account's `code`, and let
`codeHash = keccak256(code)` content-address them in the trie. This
technique applies to three kinds of Arkiv data:

- **Entity content.** One account per entity; `code` is the
  RLP-encoded entity (payload, content type, owner, expiry,
  annotations, full key, creation block).
- **Tier-1 equality index.** One account per `(annotKey, annotVal)`
  ever seen; `code` is a roaring64 bitmap of entity IDs that match
  the pair.
- **Tier-2 ordered index.** One account per `annotKey` with at least
  one live value; `code` is a serialised Adaptive Radix Tree (ART)
  over the set of values currently in use, supplying the ordered
  enumeration that backs range and prefix-glob queries.

Because each kind of account is content-addressed via `codeHash`,
every Arkiv read inherits the standard Ethereum guarantees: the
bytes are committed in `stateRoot`, clients can prove authenticity
with `eth_getProof` + `eth_getCode`, and queries against any retained
historical block resolve by routing reads through that block's state.

The system account (managed internally by `arkiv-entitydb`) uses
ordinary storage slots (not `code`) for the global counter / nonces /
ID maps — those values need slot-keyed random access, not
content-addressing.

---

## Where to read next

- [`2_state-model.md`](2_state-model.md) — canonical state-model
  spec. Account shapes, value encodings, op lifecycle, write-path
  invariants, gas, reorg. Read this if you're touching the
  precompile, the op handlers, or the gas model.
- [`3_query.md`](3_query.md) — query language, evaluation flow,
  historical reads, verification recipes. Read this if you're
  building a client or an `arkiv_*` consumer.
- [`4_engineering.md`](4_engineering.md) — workspace crate layout,
  genesis construction, testing surface, fault-proof story,
  open questions. Read this if you're working on the codebase or
  deploying a chain.
- [`contracts/src/EntityRegistry.sol`](../contracts/src/EntityRegistry.sol)
  — the `IEntityRegistry` ABI surface implemented by the precompile.
