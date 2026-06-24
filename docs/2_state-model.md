# Arkiv State Model

This document is the canonical design spec for how Arkiv state is laid
out in reth's world-state trie, and how the six entity ops mutate
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
  - [Arkiv Precompile](#arkiv-precompile)
  - [arkiv-entitydb crate](#arkiv-entitydb-crate)
- [2. State Model](#2-state-model)
  - [Canonical addresses](#canonical-addresses)
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
lives in reth's world-state trie, committed in `stateRoot`.

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

A fourth kind of account — the singleton **system account** — uses
ordinary storage slots (not `code`) to hold per-EOA nonces, the global
entity counter, and the ID ↔ address maps. Slot-keyed access fits
those values better than content-addressing.

The write path has two components. The **Arkiv precompile** is the
user-facing entry point: EOAs `CALL` it at `ARKIV_ADDRESS` with the
`execute(Operation[])` / `nonces(address)` ABI, the precompile
decodes the calldata, validates each op (ownership, liveness,
`Ident32` charset), charges gas as a pure function of calldata,
emits the per-op `EntityOperation` event, and dispatches into
**`arkiv-entitydb`** — the crate that owns the indexing logic and
the system-account slot layout.

---

## 1. Architecture

### Overview

Two components inside the Arkiv reth workspace:

1. The **Arkiv precompile** — registered at `ARKIV_ADDRESS` by the
   custom `EvmFactory`. Per call: caller restrictions; selector
   dispatch (`execute` write path; `nonces` read path); per-op
   validation (ownership, liveness, `Ident32` charset); gas
   accounting as a pure function of calldata; dispatch into
   `arkiv-entitydb` via a `StateAdapter` impl over `EvmInternals`;
   `EntityOperation` event emission.
2. The **`arkiv-entitydb` crate** — canonical home of the state
   model. Owns the entity / pair / index / system-account layout,
   RLP, roaring bitmap, the ART index implementation, the six op
   handlers, the system-account slot layout (`pub(crate)`) with
   `read_nonce` / `bump_nonce` as the public surface, and the query
   language. No `revm` deps; runs against an abstract `StateAdapter`
   trait.

Every state-dependent mutation that affects consensus — entity
account writes, pair account writes (bitmaps), index account writes
(serialised ART), system-account storage writes — flows through
revm's journaled state and is committed in the block `stateRoot`.

### Reth Integration

A single integration point on reth's standard extension surface:
an Arkiv precompile registered into `PrecompilesMap` via a custom
`EvmFactory` wrapping `EthEvmFactory`. The custom factory inserts
the precompile in both `create_evm` and
`create_evm_with_inspector` so simulation, tracing, payload-building,
validation, and canonical execution all see the same set.

### Arkiv Precompile

The user-facing entry point and the EVM-side adapter to
`arkiv-entitydb`. Per call:

- **Caller restrictions.** Reject `DELEGATECALL` / `CALLCODE`
  (defensive: the precompile does not currently mutate state at its
  own address, but if it ever does, delegated semantics would
  silently corrupt unrelated accounts) and value-bearing calls.
  `STATICCALL` is allowed only for the `nonces(address)` view —
  `execute()` requires a regular `CALL`.
- **Selector dispatch.** First four calldata bytes select between
  `execute(Operation[])` (write) and `nonces(address)` (read-only).
- **Per-op validation.** Each `Operation` is validated in order:
  attribute-name `Ident32` charset, op-type-specific preconditions
  (entity exists / caller is owner / not expired / etc.). Failures
  return Solidity-style reverts using the standard error selectors
  (`Ident32Empty`, `NotOwner`, `EntityNotFound`, ...) so SDK error
  decoders resolve them. For `Create` / `Update`, if
  `Operation.contentType` is exactly
  `application/vnd.atlas.payload-reference+json`, `Operation.payload`
  must be v1 Atlas payload-provider reference JSON. The precompile
  parses that JSON, reconstructs the provider receipt, verifies the
  EIP-191 signature, recovers the signer, and checks the recovered
  signer against the consensus allowlist. It never calls the provider
  or any other network service.
- **Authorization.** Per-op, against `input.caller`:
  - `Create` — open to any EOA.
  - `Update` / `Extend` / `Transfer` / `Delete` — caller must equal
    the entity's stored owner (read from the entity RLP).
  - `Expire` — caller-agnostic; only requires `block.number > expiresAt`.

  On plain reth, `input.caller` is normal EVM `msg.sender`. A contract
  caller owns and mutates entities through the contract's address unless
  a future chain rule or transaction-validation rule forbids contract
  callers.
- **Gas accounting.** Computed from calldata only (§4). Charged
  up-front; halt `OutOfGas` if the budget doesn't cover the batch.
- **Dispatch.** Wraps `EvmInternals` in a `ReadWriteStateAdapter`
  implementing `arkiv_entitydb::StateAdapter`, converts ABI types
  into entitydb's value types, and calls the matching
  `arkiv_entitydb::{create, update, extend, transfer, delete, expire}`.
  Nonce reads for entity-key derivation go through
  `arkiv_entitydb::bump_nonce`; the `nonces(address)` view goes
  through `arkiv_entitydb::read_nonce`. The precompile never
  touches system-account slots directly.
- **Event emission.** One `EntityOperation` log per validated op,
  emitted from `ARKIV_ADDRESS` so the SDK's `eth_getLogs` filter on
  that address resolves every event.

The precompile is the validation surface — `arkiv-entitydb` trusts
its inputs.

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
    fn ensure_account_persists(&mut self, addr: &Address) -> Result<()>;
}
```

`ensure_account_persists` bumps the account's nonce to ≥ 1 so EIP-161
doesn't prune it at end-of-tx (the empty-account check ignores
storage). Idempotent. Used by `bump_nonce` to lazily materialise the
system account on its first storage write — no genesis allocation
required.

The trait has two production implementations and one test
implementation:

- `arkiv_node::state_adapter::ReadWriteStateAdapter` — write path.
  Wraps `&mut EvmInternals` and goes through the journal so reverts
  roll back cleanly on dispatch failure.
- `arkiv_node::state_adapter::ReadOnlyStateAdapter` — read path. Wraps
  a `StateProviderBox` from reth; mutating methods bail (unreachable
  from the read path).
- `arkiv_entitydb::test_utils::InMemoryStateAdapter` — `cfg(test-utils)`.
  Drives the op handlers in unit tests without a revm context.

The op handlers (`create` / `update` / `extend` / `transfer` /
`delete` / `expire`) all take `&mut S: StateAdapter` and do the
indexing math.

System-state access goes through the public API:

- `read_nonce(state, caller) -> Result<u32>` — current nonce for
  the caller. Used by the precompile's `nonces(address)` dispatch
  and by clients computing entity keys locally.
- `bump_nonce(state, caller) -> Result<u32>` — read-then-increment.
  Returns the pre-bump value (the nonce that should be used for the
  entity key about to be derived). Used by the precompile's CREATE
  path.

The underlying `slot_*` helpers (`slot_entity_count`,
`slot_id_to_addr`, `slot_addr_to_id`, `slot_nonces`) are
`pub(crate)` — the storage layout is an entitydb implementation
detail, not part of the public API.

---

## 2. State Model

### Canonical addresses

| Address | What | Genesis presence |
|---|---|---|
| `ARKIV_ADDRESS = 0x4400000000000000000000000000000000000044` | Precompile registration target. EOAs `CALL` here with `execute(Operation[])` / `nonces(address)` calldata. | None. The custom `EvmFactory` registers the precompile programmatically; no contract bytecode is deployed. |
| System account (entitydb-internal, `pub(crate)` in `arkiv-entitydb`, address `0x4400…0046`) | Singleton account hosting the precompile's consensus storage: per-caller `nonces`, the global `entity_count`, and the ID ↔ address maps. | None. Materialised lazily on the first write via `StateAdapter::ensure_account_persists`, which bumps the nonce to 1 so EIP-161 doesn't prune the account. |

All other Arkiv state lives on accounts whose addresses are derived
from content: entity accounts at `entityKey[:20]`, pair accounts at
`keccak256("arkiv.pair" || k || 0x00 || v)[:20]`, index accounts at
`keccak256("arkiv.index" || k)[:20]`.

### Entity Accounts

#### Address Derivation

```
entityKey      = keccak256(chainId || ARKIV_ADDRESS || msg.sender || nonces[msg.sender])
entity_address = entityKey[:20]
```

`nonces[msg.sender]` is held on the system account, incremented once
per `Create` op via `arkiv_entitydb::bump_nonce`. The address is a
pure identity anchor; content commitment is via `codeHash`.

Clients holding the sender's current `nonces` value (via the
`nonces(address)` view) can compute the entity key locally before
submitting the tx — no on-chain query needed.

#### Account Structure

```
Entity Account  (address = entityKey[:20])
  nonce    = 1                               // prevents EIP-161 empty-account deletion on tombstoning
  balance  = 0
  codeHash = keccak256(0xFE || RLP(entity))  // commits to full entity content in the trie
  code     = 0xFE || RLP(entity)             // stored by reth in its Bytecodes table, keyed by codeHash

  storage slots: none
```

Entity accounts have **zero storage slots**. A single `SetCode` call
is the entirety of the entity's per-account trie footprint.

#### codeHash and RLP Storage

`codeHash` is set to `keccak256(0xFE || RLP(entity))`. Reth stores
the corresponding bytes in its `Bytecodes` table keyed by `codeHash`,
exactly as it does for contract bytecode. `eth_getCode(entity_address)`
retrieves the full RLP; `eth_getProof(entity_address)` includes
`codeHash` in the account node, verifiable against the block
`stateRoot`.

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

The RLP is the **single source of truth** for the entity's owner and
expiry. The precompile reads them out of the RLP when it needs to
authorize an op (`caller == owner`?) or check liveness (`expires_at >
current_block`?). There is no separate contract-side mapping holding
copies.

The RLP is also **self-sufficient for query reads**: every field a
client needs to render an entity comes from a single
`eth_getCode(entity_address)`.

`creator` and `created_at_block` are immutable — set once at `Create`,
never updated. `owner` is rewritten on `Transfer`; `expires_at` on
`Extend`. `last_modified_at_block` is rewritten on every mutating op.
The corresponding built-in annotations (`$creator`, `$createdAtBlock`,
`$owner`, `$expiration`) provide the reverse direction (search) via
bitmaps.

The full 32-byte `key` is in the RLP so callers with only the 20-byte
address can recover the complete key.

#### Inline and reference payloads

For ordinary entities, `payload` is the raw inline byte payload and
`content_type` is the MIME type supplied by the caller.

For detached payloads, the caller uses the reserved content type
`application/vnd.atlas.payload-reference+json`. In that mode,
`payload` is the exact JSON reference bytes accepted by the
precompile, not the original off-chain payload bytes. The v1 reference
shape is:

```json
{
  "kind": "atlas.payloadReference",
  "version": 1,
  "provider": "atlas-payload-provider",
  "id": "<sha256(namespace || 0x00 || payload)>",
  "namespace": "atlas.test",
  "contentType": "text/plain",
  "checksum": "sha256:<sha256(payload)>",
  "sizeBytes": 42,
  "submittedAt": "2026-06-24T15:24:30Z",
  "signature": {
    "scheme": "eip191",
    "signer": "0x...",
    "receipt": {
      "service": "atlas-payload-provider",
      "action": "payloadReceived",
      "payloadId": "<same as id>",
      "namespace": "<same as namespace>",
      "checksum": "<same as checksum>",
      "sizeBytes": 42,
      "submittedAt": "<same as submittedAt>"
    },
    "messageHash": "0x...",
    "signature": "0x<r><s><v>",
    "r": "0x...",
    "s": "0x...",
    "v": 27
  }
}
```

The precompile verifies only the provider's signed payload metadata in
v1. It does not prove that the provider signature is bound to the
Arkiv entity key, attributes, expiry, owner, chain ID, or precompile
address. A later provider signing scheme must bind those fields before
the chain can treat the receipt as full operation-intent proof.

The production signer allowlist is consensus-defined in the
precompile. Local development chain ID `1337` additionally trusts the
deterministic signer derived from private key `0x...01`
(`0x7e5f4552091a69125d5dfcb7b8c2659029395bdf`) so CI can run a
local payload-provider service without access to production signing
keys.

Query and proof behavior remains byte-exact: `arkiv_query` returns the
reference JSON as `value` when payload data is included, and
`eth_getProof(entity_address)` commits to those exact reference bytes
through the entity account `codeHash`.

### System Account

A singleton account at a fixed address (entitydb-internal — both the
address constant and the `slot_*` helpers are `pub(crate)` in
`arkiv-entitydb`; external code goes through `read_nonce` /
`bump_nonce` and the op handlers). Empty code, empty storage at
genesis-time *(if anything were there at all — there is no genesis
allocation)*. The first `bump_nonce` call materialises the account by
bumping its nonce to 1 via `StateAdapter::ensure_account_persists`,
keeping EIP-161 from pruning the account at end-of-tx.

The precompile writes the following slots over the life of the chain:

```
System account  (address = 0x4400000000000000000000000000000000000046, pub(crate))
  nonce    = 1   (lazily set on first write)
  storage slots:
    slot[keccak256("entity_count")]                  →  uint64       // next entity ID
    slot[keccak256("id_to_addr"  || uint64_id)]      →  address      // ID → entity_address
    slot[keccak256("addr_to_id"  || entity_address)] →  uint64       // entity_address → ID
    slot[keccak256("nonces"      || caller)]         →  uint32       // per-EOA entity-key minting nonce
```

The `entity_count` slot is the canonical source for ID assignment.
Every node executing the same block sees the same value and assigns
IDs identically. `id_to_addr` and `addr_to_id` give both directions
of the ID ↔ address map; both are written at `Create` and both are
cleared at `Delete` / `Expire`. `nonces[caller]` is bumped once per
`Create` op by the caller and is returned verbatim by the
`nonces(address)` view.

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
bitmap bytes remain in reth's `Bytecodes` table indefinitely,
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

The precompile validates and authorizes each op against `input.caller`
and the entity's RLP-encoded state, then dispatches to the matching
`arkiv-entitydb` op handler. Every write goes through revm's
journaled state.

The handlers read the entity's pre-op `owner` / `expires_at` /
attribute set from the existing entity account's RLP whenever they
need them — there is no separate contract-side mapping to consult.

Every pair-bitmap mutation triggers an ART maintenance step. On
first-entity-into-pair, the value is inserted into the index account
for that key; on last-entity-out-of-pair, the value is removed (and
the index account is tombstoned if its ART has become empty). The op
descriptions below list pair-bitmap writes; each implies the
corresponding ART write under that invariant.

### Create

**Precompile:** validates `btl > 0` and `Ident32` charset on every
attribute name.

**Op handler (`arkiv_entitydb::create`):**
1. Bump the caller's `nonces[caller]` slot; the pre-bump value is
   the nonce used for the new `entityKey`. (The precompile derives
   the key via `derive_entity_key(chain_id, caller, nonce)` and
   passes it to the handler.)
2. Read and increment `entity_count` on the system account; the new
   value is `entity_id`.
3. Write the system-account ID maps:
   `slot[keccak256("id_to_addr", entity_id)] = entity_address`;
   `slot[keccak256("addr_to_id", entity_address)] = entity_id`.
4. For each annotation `(k, v)` — including built-ins `$all`,
   `$creator`, `$createdAtBlock`, `$owner`, `$key`, `$expiration`,
   `$contentType` (values derived from the record):
   - Derive `pair_addr = keccak256("arkiv.pair" || k || 0x00 || v)[:20]`.
   - Read `pair_addr.code` (treat as empty bitmap if absent).
   - Deserialize, add `entity_id`, re-serialize. `SetCode(pair_addr, new_bytes)`.
5. Encode the entity RLP with `owner = creator = caller`,
   `expires_at = current_block + btl`. `SetCode(entity_address, 0xFE || RLP)`.

### Update

**Precompile:** validates the entity exists, the caller is the
stored owner, the entity has not expired, and `Ident32` charset on
every new attribute name.

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

**Precompile:** validates the entity exists, the caller is the
stored owner, the entity has not expired, `btl > 0`, and
`newExpiresAt > stored.expiresAt`.

**Op handler:**
1. Decode the current entity RLP. Read its `expires_at` (old value).
2. Remove `entity_id` from the `$expiration = old` pair account's
   bitmap; add it to the `$expiration = newExpiresAt` pair account's
   bitmap.
3. Re-encode the entity RLP with `expires_at = newExpiresAt`,
   `last_modified_at_block = current_block`; everything else
   preserved. `SetCode`.

### Transfer

**Precompile:** validates the entity exists, the caller is the
stored owner, the entity has not expired, and `newOwner` is non-zero
and different from the current owner.

**Op handler:**
1. Decode the current entity RLP. Read its `owner` (old value).
2. Remove `entity_id` from the `$owner = old` pair account's bitmap;
   add it to the `$owner = newOwner` pair account's bitmap.
3. Re-encode the entity RLP with `owner = newOwner`,
   `last_modified_at_block = current_block`; everything else
   preserved. `SetCode`.

### Delete

**Precompile:** validates the entity exists, the caller is the
stored owner, and the entity has not expired.

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

Anyone may submit an `Expire` op once `block.number > expiresAt` —
no ownership check. The precompile validates the entity exists and is
past its expiry, then dispatches to the same state-changing path as
`Delete`. There is no out-of-band housekeeping; expiration lives on
the canonical execution path along with every other state-mutating op.

---

## 4. Gas Model

Arkiv currently carries experimental protocol-level EIP-1559 knobs in
its patched `alloy-eips` dependency. The genesis `baseFeePerGas`
remains `0x1`; by default, next-block base-fee calculation clamps all
computed results to at least `440_000_000` wei per gas, or 0.44 gwei
(`0x1a39de00`). Advanced testnets may override the base-fee floor,
elasticity multiplier, base-fee max-change denominator, and payload
gas-limit cap through the central protocol schedule described in
[`4_engineering.md`](4_engineering.md). The publishing service
contract is specified in
[`6_protocol-schedule-service.md`](6_protocol-schedule-service.md).
This is consensus behavior: every execution node validating the chain
must run the same rule.

Gas is charged as a pure function of operation inputs, with no
dependency on any pre-existing state. The precompile computes per-op
cost from calldata only and charges it via standard revm precompile
gas accounting (`PrecompileOutput::new` for success,
`halt(OutOfGas)` for budget exhaustion).

| Op | Base | Per-byte | Per-annotation | Per indexed user attr | Payload reference |
|---|---|---|---|---|---|
| `Create` | `G_CREATE` | `G_BYTE = 16` × `(payload_bytes + annotation_bytes)` | `G_ANNOTATION = 5_000` | `G_ART_INDEXED_ANNOTATION = 6_000` | `G_PAYLOAD_REFERENCE_VERIFY = 50_000` when content type is reserved |
| `Update` | `G_UPDATE` | same | same | same | same |
| `Extend` | `G_EXTEND` | — | — | — | — |
| `Transfer` | `G_TRANSFER` | — | — | — | — |
| `Delete` | `G_DELETE` | — | — | — | — |
| `Expire` | `G_EXPIRE` | — | — | — | — |

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
included in this count today — a small consensus-safe under-charge.

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
state: entity accounts, pair accounts, index accounts, and the
system account all revert via the trie. There is no journal table,
no Arkiv-side revert handler, no notification stream the precompile
subscribes to, no out-of-trie state to worry about.

The design is reorg-safe by construction: every consensus-critical
write goes through `EvmInternals` (`set_code` / `set_storage` /
`bump_nonce`) and lands in the journal, so reverts roll back cleanly.
No fix-up code required.

---

## 6. Storage Layout Summary

```
Trie (committed in stateRoot):

  Arkiv precompile target  (ARKIV_ADDRESS = 0x4400…0044):
    No genesis entry. Programmatic registration via custom EvmFactory.
    All CALLs to this address land on the native precompile, not on bytecode.

  System account  (entitydb-internal, address = 0x4400…0046):
    nonce                                                   → 1   (lazily set on first write)
    storage:
      slot[keccak256("entity_count")]                       → uint64
      slot[keccak256("id_to_addr"  || uint64_id)]           → entity_address
      slot[keccak256("addr_to_id"  || entity_address)]      → uint64_id
      slot[keccak256("nonces"      || caller)]              → uint32

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

MDBX (reth's environment):
  Standard reth tables only (Accounts, Storages, Bytecodes, ChangeSets, ...).
  No custom Arkiv tables.
```

### Properties

| Property | This design |
|---|---|
| Entity payload committed in trie | Yes — `codeHash` in entity account |
| Bitmap content committed in trie | Yes — `codeHash` of pair account is bitmap content hash |
| ART content committed in trie | Yes — `codeHash` of index account is ART content hash |
| Ownership / lifetime committed in trie | Yes — owner / `expires_at` are fields in the entity RLP |
| Custom MDBX tables required | None |
| Journal / out-of-trie consensus-critical state | None |
| Third-party proof of entity state | Yes — `eth_getProof` against any retained block |
| Third-party proof of equality query result | Yes — bitmap is content-addressed; ID map is trie-committed |
| Range / prefix-glob query support | Yes — Tier-2 ART in index accounts |
| Arbitrary-pattern glob | No — `~` is prefix-only (`"prefix*"`) |
| Historical entity reads | Yes — trie versioning |
| Historical equality / range queries | Yes — pair and index `codeHash` retained at all blocks |
| External process required | No |
| Reorg handling required | No — reth standard |
| Gas model deterministic | Yes — pure function of op shape |
