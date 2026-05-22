# Arkiv State Model

This document is the canonical design spec for how Arkiv state is laid
out in op-reth's world-state trie, and how the six entity ops mutate
it. Read this if you're touching the precompile, the op handlers, or
the gas model.

For higher-level context see [`1_overview.md`](1_overview.md); for
query semantics and verification recipes see [`3_query.md`](3_query.md);
for crate-level engineering details see
[`4_engineering.md`](4_engineering.md).

## Contents

- [Abstract](#abstract)
- [1. Architecture](#1-architecture)
  - [Overview](#overview)
  - [Reth Integration](#reth-integration)
  - [EntityRegistry Smart Contract](#entityregistry-smart-contract)
  - [Arkiv Precompile](#arkiv-precompile)
  - [arkiv-entitydb crate](#arkiv-entitydb-crate)
- [2. State Model](#2-state-model)
  - [Entity Accounts](#entity-accounts)
  - [System Account](#system-account)
  - [Pair Accounts (Content-Addressed Bitmaps)](#pair-accounts-content-addressed-bitmaps)
  - [Index Accounts (Tier-2 ART)](#index-accounts-tier-2-art)
  - [Why Numerical IDs](#why-numerical-ids)
- [3. Lifecycle](#3-lifecycle)
  - [Create](#create)
  - [Update](#update)
  - [Extend](#extend)
  - [Transfer](#transfer)
  - [Delete](#delete)
  - [Expire](#expire)
- [4. Gas Model](#4-gas-model)
- [5. Reorg Handling](#5-reorg-handling)
- [6. Storage Layout Summary](#6-storage-layout-summary)

---

## Abstract

All Arkiv state used to serve entity reads and annotation queries
lives in op-reth's world-state trie, committed in the L3 `stateRoot`.

**The fundamental building block is the same one Ethereum uses for
smart-contract code: store arbitrary bytes in an account's `code`,
and let `codeHash = keccak256(code)` content-address them in the
trie.** Arkiv applies this technique to three kinds of data:

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

The write path has three components. The **`EntityRegistry`
contract** is the user-facing entry point: it validates each op batch
(ownership, liveness, attribute names) and dispatches to the **Arkiv
precompile**. The precompile is the bridge between the contract and
**`arkiv-entitydb`**: it charges gas as a pure function of calldata,
then dispatches into the entitydb crate, which owns the indexing
logic.

---

## 1. Architecture

### Overview

Three components inside `arkiv-op-reth`:

1. The **`EntityRegistry` smart contract** — user-facing entry point
   on the L3. Holds `(owner, expiresAt)` per entity; validates
   ownership, liveness, and `Ident32` charset; mints entity keys;
   collects fees; dispatches to the precompile; emits per-op logs.
2. The **Arkiv precompile** — invoked by `EntityRegistry` from inside
   EVM execution. A thin revm-side adapter: caller restriction,
   calldata decode, gas accounting, dispatch into `arkiv-entitydb` via
   a `StateAdapter` impl over `EvmInternals`.
3. The **`arkiv-entitydb` crate** — canonical home of the state
   model. Owns the entity / pair / system / index layout, RLP,
   roaring bitmap, the ART index implementation, the six op
   handlers, and the query language. No `revm` deps; runs against
   an abstract `StateAdapter` trait.

Every state-dependent mutation that affects consensus — entity
account writes, pair account writes (bitmaps), index account writes
(serialised ART), system account writes — flows through revm's
journaled state and is committed in the L3 `stateRoot`.

### Reth Integration

A single integration point on op-reth's standard extension surface:
an Arkiv precompile registered into `PrecompilesMap` via a custom
`EvmFactory` wrapping `OpEvmFactory<OpTx>`. The custom factory
inserts the precompile in both `create_evm` and
`create_evm_with_inspector` so simulation, tracing, payload-building,
validation, and canonical execution all see the same set.

No `BlockExecutor` wrapper, no system call, no ExEx, no
`arkiv_stateRoot` slot, no custom MDBX tables.

### EntityRegistry Smart Contract

`EntityRegistry` owns ownership, lifetime, and attribute-name
validation. The Solidity source lives in
[`contracts/src/EntityRegistry.sol`](../contracts/src/EntityRegistry.sol);
the runtime bytecode is built with `just contracts-build` and
committed to `contracts/artifacts/EntityRegistry.runtime.hex`
(consumed by `arkiv-genesis` via `include_str!`).

**SDK compatibility constraint.** The external surface — the
`execute(Operation[])` selector, the `EntityOperation` event
signature, the `nonces(address)` and `entityKey(address,uint32)`
views, the `Operation` / `Attribute` / `Mime128` / `Ident32` /
`BlockNumber32` struct and type layouts, and the op-type constants
(`CREATE=1 .. EXPIRE=6`) — is held identical to arkiv-contracts v1.
Internal storage and the contract↔precompile boundary are free to
evolve.

The contract stores only what it needs:

```solidity
struct EntityRecord {
    address       owner;
    BlockNumber32 expiresAt;     // packs with owner into one slot
}

mapping(address owner    => uint32)        public nonces;
mapping(bytes32 entityKey => EntityRecord) public entities;
```

Op set: `create | update | delete | extend | transfer | expire`. The
contract validates each op against the `entities` mapping in order,
applies its own state changes, emits the per-op `EntityOperation`
event, and accumulates a per-op record:

| Op | Contract validation | Contract state change |
|---|---|---|
| `create` | `btl > 0`; `validateIdent32` on every attribute name | mint `entityKey`; insert `(owner=sender, expiresAt)` |
| `update` | exists; `msg.sender == owner`; not expired; `validateIdent32` on every attribute name | none |
| `extend` | exists; `msg.sender == owner`; not expired; `btl > 0`; `newExpiresAt > stored` | update `expiresAt` |
| `transfer` | exists; `msg.sender == owner`; not expired; `newOwner ≠ 0`; `newOwner ≠ owner` | update `owner` |
| `delete` | exists; `msg.sender == owner`; not expired | remove entry |
| `expire` (anyone may call) | exists; `block.number > expiresAt` | remove entry |

`entityKey` is minted from a sender-scoped nonce:
```
entityKey = keccak256(chainId || registryAddress || msg.sender || nonces[msg.sender])
```
The derivation is exposed via the `entityKey(address,uint32)` view so
clients holding the sender's current `nonces` value can predict the
key before submitting the tx.

After validating and updating its own state, the contract dispatches
the whole batch to the precompile in a single `CALL`:

```solidity
struct OpRecord {                              // internal
    uint8                operationType;        // Entity.CREATE .. Entity.EXPIRE
    address              sender;               // msg.sender at validate time
    bytes32              entityKey;
    address              newOwner;             // CREATE / TRANSFER
    BlockNumber32        newExpiresAt;         // CREATE / EXTEND
    bytes                payload;              // CREATE / UPDATE
    Mime128              contentType;          // CREATE / UPDATE
    Entity.Attribute[]   attributes;           // CREATE / UPDATE
}

function _callPrecompile(OpRecord[] memory records) internal {
    (bool ok, bytes memory ret) = ARKIV_PRECOMPILE.call(abi.encode(records));
    if (!ok) revert PrecompileFailed(ret);
}
```

There are **no `old*` fields** — for ops that need the entity's
pre-op `owner` or `expiresAt` (to remove from a bitmap, or to
preserve in the re-encoded RLP), the precompile reads them from the
existing entity account's RLP, which carries `owner` and `expires_at`
(see [EntityRLP](#entityrlp)).

### Arkiv Precompile

The revm-side adapter. Per call:

- Caller restriction: refuses non-direct calls (STATICCALL,
  DELEGATECALL, value-bearing, or any caller other than
  `EntityRegistry`).
- Decode the `abi.encode(OpRecord[])` batch.
- Compute gas as a pure function of op shape (§4). Charge up-front;
  halt `OutOfGas` if the budget doesn't cover the batch.
- Wrap `EvmInternals` in a `RevmStateAdapter` that implements
  `arkiv_entitydb::StateAdapter` (`code` / `set_code` /
  `tombstone_code` / `storage` / `set_storage`).
- For each `OpRecord`, convert the ABI types into `arkiv-entitydb`
  types (`Ident32` → bytes, `Mime128` → bytes, `Attribute` →
  `StringAnnotation` / `NumericAnnotation` per `valueType`) and call
  the matching `arkiv_entitydb::{create,update,extend,transfer,delete,expire}`.

The precompile does **not** validate ownership, liveness, or
attribute names — the contract has already done that. It does no
content validation today either (e.g. payload size caps); the contract
is the validation surface.

### arkiv-entitydb crate

Canonical home of the state model. No `revm` deps, no DB deps. Runs
against an abstract trait:

```rust
pub trait StateAdapter {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>>;
    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()>;
    fn tombstone_code(&mut self, addr: &Address) -> Result<()>;
    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256>;
    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()>;
}
```

The trait has two production implementations and one test
implementation:

- `arkiv_node::precompile::RevmStateAdapter` — write path. Wraps
  `&mut EvmInternals` and goes through the journal so reverts roll
  back cleanly on dispatch failure.
- `arkiv_node::rpc::RethStateAdapter` — read path. Wraps a
  `StateProviderBox` from reth; mutating methods bail (unreachable
  from the read path).
- `arkiv_entitydb::test_utils::InMemoryAdapter` — `cfg(test-utils)`.
  Drives the op handlers in unit tests without a revm context.

The op handlers (`create` / `update` / `extend` / `transfer` /
`delete` / `expire`) all take `&mut S: StateAdapter` and do the
indexing math.

---

## 2. State Model

All Arkiv state lives in four kinds of Ethereum accounts: entity
accounts (one per entity), pair accounts (one per `(annotKey,
annotVal)` ever seen — these hold the bitmaps), index accounts (one
per `annotKey` with at least one live value — these hold the ART), and
the singleton system account. The `EntityRegistry` contract holds its
own per-entity `(owner, expiresAt)` mapping plus the sender-scoped
`nonces`. All in the trie, all committed in `stateRoot`.

### Entity Accounts

#### Address Derivation

```
entityKey      = keccak256(chainId || registryAddress || msg.sender || nonces[msg.sender])
entity_address = entityKey[:20]
```

`nonces[msg.sender]` is held in `EntityRegistry`, incremented once per
`Create` op. The address is a pure identity anchor; content
commitment is via `codeHash`.

#### Account Structure

```
Entity Account  (address = entityKey[:20])
  nonce    = 1                               // prevents EIP-161 empty-account deletion on tombstoning
  balance  = 0
  codeHash = keccak256(0xFE || RLP(entity))  // commits to full entity content in the trie
  code     = 0xFE || RLP(entity)             // stored by op-reth in its Bytecodes table, keyed by codeHash

  storage slots: none
```

Entity accounts have **zero storage slots**. A single `SetCode` call
is the entirety of the entity's per-account trie footprint.

#### codeHash and RLP Storage

`codeHash` is set to `keccak256(0xFE || RLP(entity))`. Op-reth stores
the corresponding bytes in its `Bytecodes` table keyed by `codeHash`,
exactly as it does for contract bytecode. `eth_getCode(entity_address)`
retrieves the full RLP; `eth_getProof(entity_address)` includes
`codeHash` in the account node, verifiable against the L3 `stateRoot`.

The `0xFE` prefix ensures that any EVM `CALL` to an entity address
executes `INVALID` and reverts immediately. The RLP bytes are never
interpreted as bytecode.

#### EntityRLP

```rust
struct EntityRlp {
    payload:                Vec<u8>,
    creator:                Address,
    created_at_block:       u64,
    owner:                  Address,
    expires_at:             u64,
    content_type:           Vec<u8>,
    key:                    B256,                  // full 32-byte entityKey
    string_annotations:     Vec<StringAnnotation>,
    numeric_annotations:    Vec<NumericAnnotation>,
    last_modified_at_block: u64,
}
```

The RLP is **self-sufficient for query reads**: every field a client
needs to render an entity comes from a single
`eth_getCode(entity_address)`. No second lookup against
`EntityRegistry`'s storage required.

This intentionally duplicates `owner` and `expires_at` between the
entity RLP and the `EntityRegistry` contract's `entities` mapping.
The two are written together by the precompile (single revm tx, both
via journaled state) so they stay in lockstep across reorgs and
re-execution. The contract is the source of truth for **owner /
expiry validation** (cheap, no RLP decode in Solidity); the RLP is
the source of truth for **query reads** (single account read, no
stitching).

`creator` and `created_at_block` are immutable — set once at `Create`,
never updated. `owner` is rewritten on `Transfer`; `expires_at` on
`Extend`. `last_modified_at_block` is rewritten on every mutating op.
The corresponding built-in annotations (`$creator`, `$createdAtBlock`,
`$owner`, `$expiration`) provide the reverse direction (search) via
bitmaps.

The full 32-byte `key` is in the RLP so callers with only the 20-byte
address can recover the complete key.

### System Account

A singleton account at a fixed address. Pre-allocated in genesis with
`nonce = 1` (to defeat EIP-161) and empty storage.

```
System Account  (address = 0x4400000000000000000000000000000000000046)
  nonce    = 1
  storage slots:
    slot[keccak256("entity_count")]                  →  uint64       // next entity ID
    slot[keccak256("id_to_addr", uint64_id)]         →  address      // ID → entity_address
    slot[keccak256("addr_to_id", entity_address)]    →  uint64       // entity_address → ID
```

The three adjacent predeploys at `0x44…0044 / 0045 / 0046` are:

| Address | What |
|---|---|
| `0x4400…0044` | `EntityRegistry` Solidity contract |
| `0x4400…0045` | Arkiv precompile (native Rust, registered by the custom `EvmFactory`) |
| `0x4400…0046` | System account (no code; pre-allocated with `nonce=1` and empty storage) |

The `entity_count` slot is the canonical source for ID assignment.
Every node executing the same block sees the same value and assigns
IDs identically.

The `id_to_addr` and `addr_to_id` slots give both directions of the
ID ↔ address map, both trie-committed. Both are written at `Create`
and both are cleared at `Delete` / `Expire`. The address-to-ID
direction is needed during `Delete`/`Expire` to look up the entity's
ID without decoding the RLP; the ID-to-address direction is the
query-time resolver for bitmap hits.

### Pair Accounts (Content-Addressed Bitmaps)

One account per `(annotKey, annotVal)` pair ever seen. Created
lazily the first time the pair appears in an op. The bitmap of entity
IDs matching this pair is stored as the account's code; **the bitmap
is content-addressed in the trie because `codeHash = keccak256(bitmap_bytes)`
by construction**.

```
Pair Account  (address = keccak256("arkiv.pair" || key_bytes || 0x00 || val_bytes)[:20])
  nonce    = 1
  codeHash = keccak256(roaring64_bitmap_bytes)
  code     = roaring64_bitmap_bytes

  storage slots: none
```

On bitmap update, `SetCode` is called with the new bytes; `codeHash`
updates automatically to the keccak hash of the new content. Old
bitmap bytes remain in op-reth's `Bytecodes` table indefinitely,
keyed by their old hash — historical bitmap versions stay retrievable
via `eth_getCode(pair_address, blockN)` against any retained block.

The 0xFE-prefix trick used for entity accounts is **not** applied
here. A `CALL` to a pair account is not something the design needs to
defend against, and applying the prefix would defeat content-addressing.

The 20-byte pair address is derivable directly from
`(annotKey, annotVal)`, so equality queries can locate the bitmap
without consulting any index.

### Index Accounts (Tier-2 ART)

One account per attribute key with at least one live value. The
account's code is a serialised **Adaptive Radix Tree (ART)** over the
set of `annotVal` bytes that currently appear in some live entity's
annotations for that key. The ART supplies the ordered enumeration
that equality bitmaps can't: range scans (`>`, `<`, `>=`, `<=`) walk
keys in lex order; prefix-glob scans (`~`, `!~`) walk keys sharing a
byte prefix.

```
Index Account  (address = keccak256("arkiv.index" || annotKey)[:20])
  nonce    = 1
  codeHash = keccak256(art_bytes)
  code     = serialised ART bytes

  storage slots: none
```

The `"arkiv.index"` prefix is disjoint from `"arkiv.pair"` so the two
namespaces cannot collide. `codeHash` is the keccak hash of the
serialised ART by construction, so **the index is content-addressed in
the trie** for the same reason pair bitmaps are. Historical ART
versions stay retrievable via `eth_getCode(index_address, blockN)`.

The ART stores **values only** — it is an ordered set of `annotVal`
bytes. The corresponding pair address is re-derivable as
`keccak256("arkiv.pair" || k || 0x00 || v)[:20]` from each value the
ART yields. The ART itself carries no entity IDs and no bitmap.

#### What Gets Indexed

Every `(annotKey, annotVal)` pair that the op handlers touch — both
built-ins (`$all`, `$creator`, `$createdAtBlock`, `$owner`, `$key`,
`$expiration`, `$contentType`) and user-supplied UINT / STRING /
ENTITY_KEY attributes — is maintained in the ART for `annotKey`. Built-in
keys are not excluded; the ART for `$owner` lets a client enumerate the
set of owners that currently hold at least one entity, even though no
range-query operator currently targets `$owner`.

The invariant the write path maintains is:

> A value `v` is present in the ART for key `k` iff the Tier-1 pair
> bitmap for `(k, v)` is non-empty after the current op.

The ART is therefore inserted into on the first entity to carry a
given `(k, v)` and removed from on the last entity to leave it. When
the ART becomes empty (no live values for `k`), the index account is
tombstoned (`tombstone_code`) — same `nonce=1, empty code` shape as a
deleted entity account.

#### Value Encoding in the ART

ART keys are the raw bytes of `annotVal` as the pair-account derivation
already encodes them — same bytes used to derive `pair_address`. The
encoding is chosen so lexicographic byte order matches the order the
query operators need:

| Source                         | ART key bytes                                                                                |
| ------------------------------ | -------------------------------------------------------------------------------------------- |
| User UINT attribute            | 32-byte big-endian (`U256::to_be_bytes`) — lex order = numeric order                         |
| User STRING attribute          | Raw UTF-8 bytes, trailing `0x00` stripped by the precompile before reaching the op handler   |
| User ENTITY_KEY attribute      | 32 raw bytes (no strip; an entity key may end in zeros)                                      |
| `$expiration`, `$createdAtBlock` | 8-byte big-endian uint64 (fixed-width — lex order = block-number order)                    |
| `$owner`, `$creator`           | 20-byte address                                                                              |
| `$key`                         | 32-byte entity key                                                                           |
| `$contentType`                 | trailing-zero-stripped 128-byte MIME container                                               |
| `$all`                         | empty (single ART entry for the empty value)                                                 |

The ART serialisation format is deterministic — identical trees
produce identical bytes — so the `codeHash` agrees across nodes. The
format is pinned at consensus and lives in
`crates/arkiv-entitydb/src/index_tree.rs`; any future change to it is
a hard fork.

### Why Numerical IDs

Bitmaps are `roaring64` — compressed bitsets over 64-bit unsigned
integers. Ethereum addresses (20 bytes) cannot be stored directly in
a roaring bitmap; each entity is therefore assigned a compact
`uint64` ID at `Create` time. Both directions of the ID ↔ address
mapping live on the system account and are trie-committed.

---

## 3. Lifecycle

`EntityRegistry` validates ownership / liveness / charset from its
own storage + calldata and updates its storage before calling the
precompile. The precompile then dispatches to `arkiv-entitydb`. Every
write goes through revm's journaled state.

Whenever the op needs the entity's pre-op `owner` or `expires_at`
(for a bitmap removal, or to preserve in a re-encoded RLP), it reads
the existing entity account's RLP. The contract never forwards
`old*` fields.

Every pair-bitmap mutation triggers an ART maintenance step. On
first-entity-into-pair, the value is inserted into the index account
for that key; on last-entity-out-of-pair, the value is removed (and
the index account is tombstoned if its ART has become empty). The op
descriptions below list pair-bitmap writes; each implies the
corresponding ART write under that invariant.

### Create

**Contract:**
1. Read and increment `nonces[msg.sender]`; derive `entityKey`.
2. `validateIdent32` on every attribute name.
3. Insert `entities[entityKey] = (msg.sender, expiresAt)`.

**Op handler (`arkiv_entitydb::create`):**
1. Read and increment `entity_count` on the system account; the new
   value is `entity_id`.
2. Write the system-account ID maps:
   `slot[keccak256("id_to_addr", entity_id)] = entity_address`;
   `slot[keccak256("addr_to_id", entity_address)] = entity_id`.
3. For each annotation `(k, v)` — including built-ins `$all`,
   `$creator`, `$createdAtBlock`, `$owner`, `$key`, `$expiration`,
   `$contentType` (values derived from the record):
   - Derive `pair_addr = keccak256("arkiv.pair" || k || 0x00 || v)[:20]`.
   - Read `pair_addr.code` (treat as empty bitmap if absent).
   - Deserialize, add `entity_id`, re-serialize. `SetCode(pair_addr, new_bytes)`.
4. Encode the entity RLP. `SetCode(entity_address, 0xFE || RLP)`.

### Update

**Contract:** validates ownership + liveness + `Ident32` charset on
every new attribute name. No storage change.

**Op handler:**
1. Read `entity_id` from `system.slot[keccak256("addr_to_id", entity_address)]`.
2. Decode the current entity RLP to recover `owner`, `expires_at`,
   `creator`, `created_at_block`, `key`, and the old annotation set.
3. Diff `(content_type + user annotations)` between old and new.
4. For each pair removed: `read_pair_bitmap`, remove `entity_id`,
   `SetCode` back.
5. For each pair added: same, add `entity_id`.
6. Re-encode the entity RLP using the new
   `payload`/`content_type`/`attributes` and the preserved
   `owner`/`expires_at`/`creator`/`created_at_block`/`key`. Set
   `last_modified_at_block = current_block`. `SetCode`.

Built-ins `$creator`, `$createdAtBlock`, `$key`, `$owner`,
`$expiration`, `$all` don't change on UPDATE, so the diff doesn't
touch them.

### Extend

**Contract:** validates ownership + liveness + `newExpiresAt >
stored.expiresAt`. Updates `entities[entityKey].expiresAt`.

**Op handler:**
1. Decode the current entity RLP. Read its `expires_at` (old value).
2. Remove `entity_id` from the `$expiration = old` pair account's
   bitmap; add it to the `$expiration = newExpiresAt` pair account's
   bitmap.
3. Re-encode the entity RLP with `expires_at = newExpiresAt`,
   `last_modified_at_block = current_block`; everything else
   preserved. `SetCode`.

### Transfer

**Contract:** validates ownership + liveness + non-zero / different
`newOwner`. Updates `entities[entityKey].owner`.

**Op handler:**
1. Decode the current entity RLP. Read its `owner` (old value).
2. Remove `entity_id` from the `$owner = old` pair account's bitmap;
   add it to the `$owner = newOwner` pair account's bitmap.
3. Re-encode the entity RLP with `owner = newOwner`,
   `last_modified_at_block = current_block`; everything else
   preserved. `SetCode`.

### Delete

**Contract:** validates ownership + liveness. Removes
`entities[entityKey]`.

**Op handler:**
1. Read `entity_id` from the system account's `addr_to_id` slot.
2. Decode the entity RLP to recover the full annotation set + built-ins.
3. For each pair (built-in + user): `read_pair_bitmap`, remove
   `entity_id`, `SetCode` back.
4. Clear both system-account ID slots.
5. `tombstone_code(entity_address)` — empty code, `nonce` stays at 1.

> **Why nonce stays at 1.** If nonce were zeroed, the account would
> become EIP-161-empty (nonce=0, balance=0, no code). Post-Cancun
> `handleDestruction` returns `"unexpected storage wiping"` when a
> prior non-empty storage root exists. Keeping nonce at 1 prevents
> EIP-161 from treating the account as empty; the account remains as
> a tombstone in the trie.

### Expire

Anyone may call `EntityRegistry.expire(entityKey)` once `block.number
> expiresAt`. The contract gates on the expiration check, removes the
entry, and dispatches to the precompile, which executes the same
state changes as `Delete`. There is no out-of-band housekeeping path;
expiration is contract-driven so it lives on the canonical execution
path along with every other state-mutating op.

---

## 4. Gas Model

Gas is charged as a pure function of operation inputs, with no
dependency on any pre-existing state. The precompile computes per-op
cost from calldata only and charges it via standard revm precompile
gas accounting (`PrecompileOutput::new` for success,
`halt(OutOfGas)` for budget exhaustion).

| Op | Base | Per-byte | Per-annotation | Per indexed user attr |
|---|---|---|---|---|
| `Create` | `G_CREATE` | `G_BYTE = 16` × `(payload_bytes + annotation_bytes)` | `G_ANNOTATION = 5_000` | `G_ART_INDEXED_ANNOTATION = 6_000` |
| `Update` | `G_UPDATE` | same | same | same |
| `Extend` | `G_EXTEND` | — | — | — |
| `Transfer` | `G_TRANSFER` | — | — | — |
| `Delete` | `G_DELETE` | — | — | — |
| `Expire` | `G_EXPIRE` | — | — | — |

`annotation_bytes` is `annotation_count × (32 + 128)` — the max
`Ident32` name plus the max `value128` payload per annotation. All
constants live in `crates/arkiv-node/src/precompile.rs`.

`G_ART_INDEXED_ANNOTATION` is charged per user attribute whose
`valueType` is `UINT` or `STRING` — i.e. attributes for which the
write path will read and (conditionally) rewrite an ART. It is
charged conservatively: always, regardless of whether the specific
op actually mutates the ART (only first-entity-into-pair and
last-entity-out-of-pair do), so the gas formula stays a pure function
of calldata. ENTITY_KEY user attributes and built-in-key ART writes
(`$owner` on Transfer, `$expiration` on Extend, etc.) are not
included in this count today — a small consensus-safe under-charge
documented in `precompile.rs:75`.

Per-batch gas is computed before any state changes are applied. On
out-of-gas the entire call budget is consumed (matching EVM OOG
semantics natively via `PrecompileHalt::OutOfGas`).

Two nodes executing the same op batch always compute identical gas
regardless of their current state — the formulas reference only
calldata. This is the consensus-determinism property required for the
precompile to be part of the state-transition function.

---

## 5. Reorg Handling

Op-reth's standard reorg machinery handles every piece of Arkiv
state: entity accounts, pair accounts, index accounts, the system
account, and the contract's `entities` mapping all revert via the
trie. There is no journal table, no Arkiv-side revert handler, no
notification stream the precompile subscribes to, no out-of-trie
state to worry about.

The design is reorg-safe by construction: every consensus-critical
write goes through `EvmInternals` (`set_code` / `set_storage` /
`bump_nonce`) and lands in the journal, so reverts roll back cleanly.
No fix-up code required.

---

## 6. Storage Layout Summary

```
Trie (committed in stateRoot):

  EntityRegistry contract  (0x4400…0044):
    storage:
      nonces[sender]                                        → uint32
      entities[entityKey]                                   → (owner, expiresAt)

  System account  (0x4400…0046):
    nonce                                                   → 1
    storage:
      slot[keccak256("entity_count")]                       → uint64
      slot[keccak256("id_to_addr", uint64_id)]              → entity_address
      slot[keccak256("addr_to_id", entity_address)]         → uint64_id

  Entity account  (one per entity; address = entityKey[:20]):
    nonce                                                   → 1
    codeHash                                                → keccak256(0xFE || RLP(entity))
    code                                                    → 0xFE || RLP(entity)
    storage: (none)

  Pair account  (one per (k, v); address = keccak256("arkiv.pair" || k || 0x00 || v)[:20]):
    nonce                                                   → 1
    codeHash                                                → keccak256(bitmap_bytes)        (CONTENT HASH)
    code                                                    → roaring64 bitmap bytes
    storage: (none)

  Index account  (one per annotKey with ≥1 live value; address = keccak256("arkiv.index" || k)[:20]):
    nonce                                                   → 1
    codeHash                                                → keccak256(art_bytes)           (CONTENT HASH)
    code                                                    → serialised ART bytes
    storage: (none)

MDBX (op-reth's environment):
  Standard op-reth tables only (Accounts, Storages, Bytecodes, ChangeSets, …).
  No custom Arkiv tables.
```

Zero custom MDBX tables. No journal table. No `arkiv_stateRoot`
slot. No content-addressed-bitmap side store (because bitmaps **are**
content-addressed natively — `codeHash` of a pair account is the
bitmap content hash by construction).

### Properties

| Property | This design |
|---|---|
| Entity payload committed in trie | Yes — `codeHash` in entity account |
| Bitmap content committed in trie | Yes — `codeHash` of pair account is bitmap content hash |
| ART content committed in trie | Yes — `codeHash` of index account is ART content hash |
| Ownership / lifetime committed in trie | Yes — `entities` mapping in `EntityRegistry` |
| Custom MDBX tables required | None |
| Journal / out-of-trie consensus-critical state | None |
| Third-party proof of entity state | Yes — `eth_getProof` against any retained block |
| Third-party proof of equality query result | Yes — bitmap is content-addressed; ID map is trie-committed |
| Range / prefix-glob query support | Yes — Tier-2 ART in index accounts |
| Arbitrary-pattern glob | No — `~` is prefix-only (`"prefix*"`) |
| Historical entity reads | Yes — trie versioning |
| Historical equality / range queries | Yes — pair and index `codeHash` retained at all blocks |
| External process required | No |
| Reorg handling required | No — op-reth standard |
| Gas model deterministic | Yes — pure function of op shape |
