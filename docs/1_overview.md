# Arkiv on op-reth: Overview

`arkiv-op-reth` builds on [op-reth](https://github.com/ethereum-optimism/optimism)
to turn an OP-stack L2/L3 node into an **Arkiv** node by adding two
things:

1. A custom op-reth `EvmFactory` that registers an **Arkiv precompile**
   at `ARKIV_ADDRESS` (`0x4400000000000000000000000000000000000044`)
   into `PrecompilesMap` for every revm context (canonical execution,
   payload-building, simulation, validation, tracing). EOAs and SDKs
   `CALL` this address with the `execute(Operation[])` /
   `nonces(address)` ABI declared by
   [`IEntityRegistry`](../contracts/src/EntityRegistry.sol).
2. An `arkiv_*` JSON-RPC namespace on the node's standard transports.

The precompile owns per-op validation (ownership, expiration,
`Ident32` charset), `EntityOperation` event emission, gas accounting,
and dispatch into `arkiv-entitydb`. The chain's genesis must include a
single empty-coded account at `SYSTEM_ACCOUNT_ADDRESS`
(`0x4400000000000000000000000000000000000046`) with `nonce=1` — it
hosts the precompile's consensus storage (per-caller nonces, the
global entity counter, the ID ↔ address maps). The `nonce=1` keeps
EIP-161 from pruning the account before the precompile writes its
first slot. `arkiv-cli inject-predeploy` adds the system account when
post-processing a standard OP genesis. `ARKIV_ADDRESS` itself has no
genesis entry — the precompile is registered programmatically.

Every entity, every annotation index, and every counter lives inside
op-reth's standard world-state trie, committed in the L3 `stateRoot`.
Reads are served by the `arkiv_*` namespace backed entirely by local
state. There is no EntityRegistry contract, no external indexer
process, no JSON-RPC bridge, no ExEx, no out-of-trie state.

The binary is a **drop-in op-reth**: against a chainspec without the
system account it refuses to start. Against a chainspec containing it,
the binary installs the custom `EvmFactory` + the `arkiv_*` RPC and
serves the full Arkiv surface.

---

## System overview

```
                  ┌──────────────────────────────────────────────────┐
                  │ arkiv-node binary                                │
                  │                                                  │
                  │   ┌──────────────────────────────────────────┐   │
   user tx ──────►│   │ revm  ─── ArkivOpEvmFactory inserts ─────┼─► trie state
                  │   │       │   ArkivPrecompile at ARKIV_ADDR  │   │   entity / pair / index
                  │   │       │   into PrecompilesMap            │   │   accounts + SYSTEM_ACCOUNT
                  │   │       └──► dispatches into arkiv-entitydb│   │   storage (counter, nonces,
                  │   └──────────────────────────────────────────┘   │   ID maps) — committed in stateRoot
                  │                                                  │
                  │   ┌──────────────────────────────────────────┐   │
   user query ───►│   │ arkiv_* RPC                              │   │
                  │   │   parse → evaluate via arkiv-entitydb    │   │
                  │   │   against a read-only StateProvider      │   │
                  │   └──────────────────────────────────────────┘   │
                  └──────────────────────────────────────────────────┘
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

The system account uses ordinary storage slots (not `code`) for the
global counter / nonces / ID maps — those values need slot-keyed
random access, not content-addressing.

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
