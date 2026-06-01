//! Canonical home of the Arkiv state model.
//!
//! Every entity and every annotation bitmap lives in op-reth's standard
//! world-state trie as an Ethereum account:
//!
//! - **Entity account** at `entity_address(entityKey)` carries the
//!   RLP-encoded entity (payload + content type + annotations +
//!   owner/expires_at) in `code`, prefixed with `0xFE` so a stray
//!   `CALL` reverts immediately.
//! - **Pair account** at `pair_address(annot_key, annot_val)` carries a
//!   roaring64 bitmap of entity IDs as `code`. `codeHash` is
//!   `keccak256(bitmap_bytes)` by construction — every bitmap is
//!   content-addressed in the trie.
//! - **System account** (internal — see `SYSTEM_ACCOUNT_ADDRESS`) —
//!   empty-coded account that hosts the global entity counter, the
//!   per-caller `nonces` map, and the trie-committed ID ↔ address maps
//!   as storage slots. Materialised lazily on the first write via
//!   `StateAdapter::ensure_account_persists` — no genesis presence
//!   required. Separate from the precompile's registration address
//!   ([`ARKIV_ADDRESS`]) so the precompile itself stays a programmatic
//!   registration target with no on-chain dependency.
//!
//! Top-level exports:
//!
//! - Primitives: [`EntityRlp`], [`Bitmap`], address derivations,
//!   built-in annotation keys, system-account slot keys.
//! - [`StateAdapter`] trait — what the op handlers need from the
//!   underlying state (code + storage R/W). The precompile implements
//!   this over `EvmInternals`; the [`test_utils::InMemoryStateAdapter`]
//!   (behind the `test-utils` feature) implements it over an
//!   [`InMemoryStateDb`].
//! - Op handlers: [`create`], [`update`], [`extend`], [`transfer`],
//!   [`delete`], [`expire`]. All the indexing logic (system counter +
//!   ID maps, bitmap deltas across built-in and user annotations, RLP
//!   encode/decode, tombstoning) lives here. The precompile is a thin
//!   adapter: decode calldata, dispatch.

use alloy_primitives::{Address, B256, keccak256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use eyre::{Result, ensure};
use roaring::RoaringTreemap;

pub mod query;

// ─── Canonical addresses ──────────────────────────────────────────────

/// Canonical Arkiv address — the address the precompile is registered
/// at by the custom `EvmFactory`. EOAs / SDKs `CALL` this address with
/// the `execute(Operation[])` / `nonces(address)` ABI declared by
/// `IEntityRegistry`. The precompile itself touches no storage on this
/// address — consensus state lives on the system account.
///
/// Matches the SDK's `ARKIV_ADDRESS` constant. `arkiv-genesis`
/// re-exports it.
pub const ARKIV_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x44,
]);

/// Address the precompile uses as a storage host — global entity
/// counter, per-caller `nonces` map, and the trie-committed ID ↔
/// address maps live here as storage slots. Materialised lazily on
/// the first storage write via `StateAdapter::ensure_account_persists`
/// (called from [`bump_nonce`]), which bumps the nonce to 1 so EIP-161
/// doesn't prune the account at end-of-tx. No genesis allocation
/// required.
///
/// `pub(crate)` — entitydb is the only crate that should touch this
/// address. External callers go through the op handlers and the
/// `read_nonce` / `bump_nonce` API.
pub(crate) const SYSTEM_ACCOUNT_ADDRESS: Address = Address::new([
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x46,
]);

// ─── Address derivations ──────────────────────────────────────────────

/// Entity-account address. Spec: `entity_address = entityKey[:20]`
/// (statedb-design §2.1). The address is a pure identity anchor;
/// content commitment is via `codeHash`.
#[inline]
pub fn entity_address(entity_key: B256) -> Address {
    Address::from_slice(&entity_key.0[..20])
}

/// Pair-account address. Spec: `pair_addr = keccak256("arkiv.pair" || k
/// || 0x00 || v)[:20]` (statedb-design §2.3). The `0x00` separator
/// prevents prefix collisions; annot keys and values must not contain
/// `0x00` (precompile enforces).
pub fn pair_address(annot_key: &[u8], annot_val: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.pair".len() + annot_key.len() + 1 + annot_val.len());
    buf.extend_from_slice(b"arkiv.pair");
    buf.extend_from_slice(annot_key);
    buf.push(0x00);
    buf.extend_from_slice(annot_val);
    Address::from_slice(&keccak256(buf).0[..20])
}

/// Index-account address. Spec: `index_address(k) =
/// keccak256("arkiv.index" || k)[:20]` (2_state-model §2, Index
/// Accounts). Namespace is disjoint from `"arkiv.pair"`.
pub fn index_address(attr_key: &[u8]) -> Address {
    let mut buf = Vec::with_capacity(b"arkiv.index".len() + attr_key.len());
    buf.extend_from_slice(b"arkiv.index");
    buf.extend_from_slice(attr_key);
    Address::from_slice(&keccak256(buf).0[..20])
}

// ─── Built-in annotation keys ─────────────────────────────────────────
//
// Every entity carries these implicit pairs in addition to its
// user-supplied annotations. The op handlers derive them from the op
// inputs (no caller input needed).

/// Universal "every entity" annotation — every entity is in
/// `("$all", "")`'s bitmap. Lets clients enumerate all entities via a
/// single bitmap read.
pub const ANNOT_ALL: &[u8] = b"$all";

/// `("$creator", creator_address)` — set on `Create`, immutable.
pub const ANNOT_CREATOR: &[u8] = b"$creator";

/// `("$createdAtBlock", be_block_number)` — set on `Create`, immutable.
pub const ANNOT_CREATED_AT_BLOCK: &[u8] = b"$createdAtBlock";

/// `("$owner", owner_address)` — set on `Create`, mutated on
/// `Transfer`.
pub const ANNOT_OWNER: &[u8] = b"$owner";

/// `("$key", entityKey)` — set on `Create`, immutable.
pub const ANNOT_KEY: &[u8] = b"$key";

/// `("$expiration", be_block_number)` — set on `Create`, mutated on
/// `Extend`. Encoded as fixed-width big-endian uint64 so lex order
/// matches numeric order (needed for range scans).
pub const ANNOT_EXPIRATION: &[u8] = b"$expiration";

/// `("$contentType", content_type_bytes)` — set on `Create`, mutated on
/// `Update`.
pub const ANNOT_CONTENT_TYPE: &[u8] = b"$contentType";

// ─── System-account storage slots ─────────────────────────────────────
//
// All four maps live as storage on [`SYSTEM_ACCOUNT_ADDRESS`]. Slot
// keys are scoped by a short tag so the keyspaces can't collide.
// `pub(crate)` so the slot layout stays an entitydb implementation
// detail — external callers go through [`read_nonce`] / [`bump_nonce`]
// and the op handlers.

/// `slot[keccak256("entity_count")]` → next `entity_id` (uint64).
pub(crate) fn slot_entity_count() -> B256 {
    keccak256(b"entity_count")
}

/// `slot[keccak256("id_to_addr" || id_be_bytes)]` → entity_address.
pub(crate) fn slot_id_to_addr(entity_id: u64) -> B256 {
    let mut buf = [0u8; 10 + 8];
    buf[..10].copy_from_slice(b"id_to_addr");
    buf[10..].copy_from_slice(&entity_id.to_be_bytes());
    keccak256(buf)
}

/// `slot[keccak256("addr_to_id" || entity_address_bytes)]` → uint64 ID.
pub(crate) fn slot_addr_to_id(entity_addr: Address) -> B256 {
    let mut buf = [0u8; 10 + 20];
    buf[..10].copy_from_slice(b"addr_to_id");
    buf[10..].copy_from_slice(entity_addr.as_slice());
    keccak256(buf)
}

/// `slot[keccak256("nonces" || caller_address)]` → uint32 entity-key
/// minting nonce, returned by the SDK-visible `nonces(address)` view.
pub(crate) fn slot_nonces(caller: Address) -> B256 {
    let mut buf = [0u8; 6 + 20];
    buf[..6].copy_from_slice(b"nonces");
    buf[6..].copy_from_slice(caller.as_slice());
    keccak256(buf)
}

// ─── Public system-state accessors ────────────────────────────────────

/// Read `caller`'s current entity-key minting nonce. Used by the
/// `nonces(address)` view dispatched from the precompile, and as the
/// `nonce` input to `entityKey` derivation in CREATE.
pub fn read_nonce<S: StateAdapter>(state: &mut S, caller: Address) -> Result<u32> {
    let raw = state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_nonces(caller))?;
    Ok(u32::from_be_bytes(raw.0[28..].try_into().unwrap()))
}

/// Read-then-increment `caller`'s nonce. Returns the value that was
/// there before the increment (the value to use for the entity-key
/// derivation that's about to happen).
///
/// Also lazily materialises the system account: on the first call
/// against a fresh chain, `ensure_account_persists` raises the system
/// account's nonce to 1 so EIP-161 doesn't prune it (and the nonce
/// slot we're about to write) at end-of-tx. Idempotent on subsequent
/// calls. This is the only entry point that touches the system
/// account before any other slot has been written, so it's enough to
/// run the guard here.
pub fn bump_nonce<S: StateAdapter>(state: &mut S, caller: Address) -> Result<u32> {
    state.ensure_account_persists(&SYSTEM_ACCOUNT_ADDRESS)?;
    let slot = slot_nonces(caller);
    let raw = state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot)?;
    let current = u32::from_be_bytes(raw.0[28..].try_into().unwrap());
    let next = current
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("nonce overflow for {caller}"))?;
    let mut buf = [0u8; 32];
    buf[28..].copy_from_slice(&next.to_be_bytes());
    state.set_storage(&SYSTEM_ACCOUNT_ADDRESS, slot, B256::from(buf))?;
    Ok(current)
}

// ─── Storage value encodings (for system-account slots) ──────────────

#[inline]
fn u64_to_storage(n: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&n.to_be_bytes());
    B256::from(buf)
}

#[inline]
fn storage_to_u64(b: B256) -> u64 {
    u64::from_be_bytes(b.0[24..].try_into().unwrap())
}

#[inline]
fn address_to_storage(addr: Address) -> B256 {
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(addr.as_slice());
    B256::from(buf)
}

// ─── Annotation value encodings (for pair-account addresses) ─────────
//
// Encoding choices are critical: lex order of these byte sequences
// must match the intended ordering for range queries. For numeric
// values (block numbers, uint annotations) that means fixed-width
// big-endian. For addresses it doesn't matter (range queries on
// addresses don't make sense); we use the natural 20-byte form.

#[inline]
fn encode_u64_be(n: u64) -> Vec<u8> {
    n.to_be_bytes().to_vec()
}

#[inline]
fn encode_address(addr: Address) -> Vec<u8> {
    addr.as_slice().to_vec()
}

#[inline]
fn encode_b256(b: B256) -> Vec<u8> {
    b.0.to_vec()
}

// ─── Bitmap (roaring64) ───────────────────────────────────────────────

/// Roaring64 bitmap of entity IDs.
///
/// Determinism guarantee: [`Bitmap::to_bytes`] produces the same bytes
/// for any two instances that contain the same set of IDs. Required
/// for `codeHash = keccak256(bitmap_bytes)` to agree across nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bitmap(RoaringTreemap);

impl Bitmap {
    pub fn new() -> Self {
        Self(RoaringTreemap::new())
    }

    /// Deserialize from the portable RoaringFormatSpec layout.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        RoaringTreemap::deserialize_from(bytes)
            .map(Self)
            .map_err(|e| eyre::eyre!("invalid roaring bitmap bytes: {e}"))
    }

    /// Serialize to the portable RoaringFormatSpec layout. Same set →
    /// same bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.0.serialized_size());
        self.0
            .serialize_into(&mut buf)
            .expect("writing to Vec is infallible");
        buf
    }

    pub fn insert(&mut self, id: u64) -> bool {
        self.0.insert(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        self.0.remove(id)
    }

    pub fn contains(&self, id: u64) -> bool {
        self.0.contains(id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter()
    }

    /// In-place set union: `self ∪= other`.
    pub fn union_with(&mut self, other: &Bitmap) {
        self.0 |= &other.0;
    }

    /// In-place set intersection: `self ∩= other`.
    pub fn intersect_with(&mut self, other: &Bitmap) {
        self.0 &= &other.0;
    }

    /// In-place set difference: `self \= other`.
    pub fn subtract(&mut self, other: &Bitmap) {
        self.0 -= &other.0;
    }
}

impl FromIterator<u64> for Bitmap {
    fn from_iter<I: IntoIterator<Item = u64>>(iter: I) -> Self {
        Self(RoaringTreemap::from_iter(iter))
    }
}

// ─── IndexTree (Tier-2 ordered value set) ─────────────────────────────

mod index_tree;
/// Adaptive Radix Tree ordered set of attribute values for a single
/// attribute key, stored in an **index account** at [`index_address`].
/// Provides O(log n) insert/remove, prefix-compressed deterministic
/// serialisation, and ascending range/prefix iteration.
pub use index_tree::IndexTree;

// ─── Entity RLP ───────────────────────────────────────────────────────

/// Prefix prepended to the RLP bytes before storing as account `code`.
/// `0xFE` is the EVM `INVALID` opcode — any `CALL` to an entity
/// address halts immediately.
pub const ENTITY_CODE_PREFIX: u8 = 0xFE;

/// Attribute `value_type` tags. Must match
/// `Entity.ATTR_{UINT,STRING,ENTITY_KEY}` in EntityRegistry.sol and
/// the ABI shape decoded by the precompile.
pub const ATTR_UINT: u8 = 1;
pub const ATTR_STRING: u8 = 2;
pub const ATTR_ENTITY_KEY: u8 = 3;

/// On-trie representation of an entity. Encoded as
/// `0xFE || RLP(EntityRlp)` and stored as the entity-account `code`.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct EntityRlp {
    pub payload: Vec<u8>,
    pub creator: Address,
    pub created_at_block: u64,
    pub owner: Address,
    pub expires_at: u64,
    pub content_type: Vec<u8>,
    pub key: B256,
    pub attributes: Vec<Attribute>,
    /// Block number of the most recent mutation (CREATE / UPDATE /
    /// EXTEND / TRANSFER) — equals `created_at_block` until the
    /// entity is first modified.
    pub last_modified_at_block: u64,
}

/// Discriminated `(key, value)` attribute mirroring the precompile
/// ABI. `value_type` selects how `value` should be interpreted:
/// `ATTR_UINT` → 32-byte big-endian uint256; `ATTR_STRING` → opaque
/// bytes (UTF-8 by SDK convention); `ATTR_ENTITY_KEY` → 32 raw
/// bytes of an entity key.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub struct Attribute {
    pub key: Vec<u8>,
    pub value_type: u8,
    pub value: Vec<u8>,
}

impl EntityRlp {
    /// Encode for storage as account code: `0xFE || RLP(self)`.
    pub fn encode_as_code(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + self.length());
        buf.push(ENTITY_CODE_PREFIX);
        self.encode(&mut buf);
        buf
    }

    /// Decode from account code. Verifies the `0xFE` prefix and then
    /// RLP-decodes the rest.
    pub fn decode_from_code(code: &[u8]) -> Result<Self> {
        ensure!(
            code.first() == Some(&ENTITY_CODE_PREFIX),
            "entity code is missing the {:#x} prefix",
            ENTITY_CODE_PREFIX,
        );
        let mut rest = &code[1..];
        Self::decode(&mut rest).map_err(|e| eyre::eyre!("RLP decode of EntityRlp failed: {e}"))
    }
}

// ─── State adapter trait ──────────────────────────────────────────────

/// Abstract state interface the op handlers run against.
///
/// In production, [`arkiv_node::precompile`] implements this over
/// revm's `EvmInternals`. For tests, [`test_utils::InMemoryStateAdapter`]
/// implements it over an [`test_utils::InMemoryStateDb`].
///
/// Conventions:
/// - `code` returns an empty `Vec` for absent / empty-coded accounts.
/// - `set_code` creates the account if needed and sets `nonce = 1` if
///   it was previously zero.
/// - `tombstone_code` clears the code but preserves `nonce = 1` so
///   EIP-161 doesn't prune the account.
/// - `ensure_account_persists` raises the account's nonce to at least
///   1 so EIP-161 doesn't prune it at end-of-tx. Idempotent. Used by
///   the entitydb to lazily materialise the system account on its
///   first storage write — without it, an empty-coded account that
///   only receives storage writes is still EIP-161-empty (the check
///   ignores storage) and gets pruned along with its slots.
pub trait StateAdapter {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>>;
    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()>;
    fn tombstone_code(&mut self, addr: &Address) -> Result<()>;
    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256>;
    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()>;
    fn ensure_account_persists(&mut self, addr: &Address) -> Result<()>;
}

// ─── Op handlers ──────────────────────────────────────────────────────
//
// Each handler assumes the contract has already validated ownership /
// liveness. It performs all the state mutations: system-account
// counter, ID maps, bitmap deltas (built-in + user annotations), and
// the entity-account RLP write.

/// Create a new entity. Allocates a fresh `entity_id`, writes both ID
/// maps on the Arkiv account, populates all built-in + user bitmaps,
/// and writes the entity RLP.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "entitydb_create",
    level = "debug",
    skip_all,
    fields(
        payload_bytes = payload.len(),
        n_attrs = attributes.len(),
    ),
)]
pub fn create<S: StateAdapter>(
    state: &mut S,
    sender: Address,
    entity_key: B256,
    expires_at: u64,
    current_block: u64,
    payload: Vec<u8>,
    content_type: Vec<u8>,
    attributes: Vec<Attribute>,
) -> Result<()> {
    // 1) Allocate entity_id.
    let count_slot = slot_entity_count();
    let prev = state.storage(&SYSTEM_ACCOUNT_ADDRESS, count_slot)?;
    let entity_id = storage_to_u64(prev);
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        count_slot,
        u64_to_storage(entity_id + 1),
    )?;

    // 2) Write ID maps.
    let entity_addr = entity_address(entity_key);
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_id_to_addr(entity_id),
        address_to_storage(entity_addr),
    )?;
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_addr_to_id(entity_addr),
        u64_to_storage(entity_id),
    )?;

    // 3) Insert into every bitmap (built-in + user).
    for a in built_in_annotations(
        sender,
        sender,
        entity_key,
        current_block,
        expires_at,
        &content_type,
    )
    .into_iter()
    .chain(user_annotations(&attributes))
    {
        insert_into_indexes(state, &a.key, &a.value, entity_id, a.tier2)?;
    }

    // 4) Write the entity RLP.
    let entity = EntityRlp {
        payload,
        creator: sender,
        created_at_block: current_block,
        owner: sender,
        expires_at,
        content_type,
        key: entity_key,
        attributes,
        last_modified_at_block: current_block,
    };
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Replace an entity's payload / content type / annotations.
///
/// Preserves `creator`, `created_at_block`, `key`, `owner`,
/// `expires_at`. Bitmap diff: only annotations that changed get
/// touched (incl. `$contentType` if the content type changed).
#[tracing::instrument(
    name = "entitydb_update",
    level = "debug",
    skip_all,
    fields(
        payload_bytes = payload.len(),
        n_attrs = attributes.len(),
    ),
)]
pub fn update<S: StateAdapter>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    payload: Vec<u8>,
    content_type: Vec<u8>,
    attributes: Vec<Attribute>,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    // Annotation diff. Built-ins that don't change on UPDATE
    // (`$creator`, `$createdAtBlock`, `$key`, `$owner`, `$expiration`,
    // `$all`) aren't included on either side, so the diff doesn't
    // touch them. `$contentType` IS in the diff so it moves if the
    // content type changed.
    let old_annotations = updatable_annotations(&entity.content_type, &entity.attributes);
    let new_annotations = updatable_annotations(&content_type, &attributes);
    apply_annotation_diff(state, &old_annotations, &new_annotations, entity_id)?;

    entity.payload = payload;
    entity.content_type = content_type;
    entity.attributes = attributes;
    entity.last_modified_at_block = current_block;
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Extend an entity's `expires_at`. Updates the `$expiration` bitmap
/// and re-encodes the RLP with the new value.
#[tracing::instrument(name = "entitydb_extend", level = "debug", skip_all)]
pub fn extend<S: StateAdapter>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    new_expires_at: u64,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    remove_from_indexes(
        state,
        ANNOT_EXPIRATION,
        &encode_u64_be(entity.expires_at),
        entity_id,
        true,
    )?;
    insert_into_indexes(
        state,
        ANNOT_EXPIRATION,
        &encode_u64_be(new_expires_at),
        entity_id,
        true,
    )?;

    entity.expires_at = new_expires_at;
    entity.last_modified_at_block = current_block;
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Hand an entity's ownership to `new_owner`. Updates the `$owner`
/// bitmap and re-encodes the RLP.
#[tracing::instrument(name = "entitydb_transfer", level = "debug", skip_all)]
pub fn transfer<S: StateAdapter>(
    state: &mut S,
    entity_key: B256,
    current_block: u64,
    new_owner: Address,
) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let mut entity = read_entity(state, entity_addr)?;

    remove_from_indexes(state, ANNOT_OWNER, &encode_address(entity.owner), entity_id, false)?;
    insert_into_indexes(state, ANNOT_OWNER, &encode_address(new_owner), entity_id, false)?;

    entity.owner = new_owner;
    entity.last_modified_at_block = current_block;
    state.set_code(&entity_addr, entity.encode_as_code())?;

    Ok(())
}

/// Remove an entity. Clears every bitmap entry (built-in + user),
/// clears both ID-map slots on the Arkiv account, and tombstones the
/// entity account (`code = nil`, `nonce = 1`).
#[tracing::instrument(name = "entitydb_delete", level = "debug", skip_all)]
pub fn delete<S: StateAdapter>(state: &mut S, entity_key: B256) -> Result<()> {
    let entity_addr = entity_address(entity_key);
    let entity_id = read_entity_id(state, entity_addr)?;
    let entity = read_entity(state, entity_addr)?;

    for a in built_in_annotations(
        entity.creator,
        entity.owner,
        entity_key,
        entity.created_at_block,
        entity.expires_at,
        &entity.content_type,
    )
    .into_iter()
    .chain(user_annotations(&entity.attributes))
    {
        remove_from_indexes(state, &a.key, &a.value, entity_id, a.tier2)?;
    }

    // Clear ID-map slots.
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_id_to_addr(entity_id),
        B256::ZERO,
    )?;
    state.set_storage(
        &SYSTEM_ACCOUNT_ADDRESS,
        slot_addr_to_id(entity_addr),
        B256::ZERO,
    )?;

    // Tombstone — keeps nonce=1 to defeat EIP-161.
    state.tombstone_code(&entity_addr)?;

    Ok(())
}

/// Identical state path to [`delete`]. The contract has already
/// validated `block.number > expiresAt`.
#[tracing::instrument(name = "entitydb_expire", level = "debug", skip_all)]
pub fn expire<S: StateAdapter>(state: &mut S, entity_key: B256) -> Result<()> {
    delete(state, entity_key)
}

// ─── Internal helpers ─────────────────────────────────────────────────

fn read_entity<S: StateAdapter>(state: &mut S, entity_addr: Address) -> Result<EntityRlp> {
    let code = state.code(&entity_addr)?;
    ensure!(!code.is_empty(), "no entity at {entity_addr}");
    EntityRlp::decode_from_code(&code)
}

fn read_entity_id<S: StateAdapter>(state: &mut S, entity_addr: Address) -> Result<u64> {
    let slot = slot_addr_to_id(entity_addr);
    Ok(storage_to_u64(
        state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot)?,
    ))
}

/// A built-in or user annotation in its on-chain form: the (key, value)
/// bytes that go into the tier-1 pair bitmap, plus whether tier-2
/// (`IndexTree`) indexing applies.
///
/// Tier-2 backs ordered iteration (`>`, `>=`, `<`, `<=`, `~`, `!~`).
/// For random-distribution values (addresses, hashes) or singleton
/// values (`$all`), ordered iteration produces no semantically
/// meaningful query, so tier-2 maintenance is skipped — those
/// annotations remain tier-1-only and exact-equality lookups still
/// work via the pair bitmap.
///
/// Distinct from [`Attribute`], which is the precompile-ABI mirror of
/// a user-supplied attribute and carries an explicit `value_type`
/// discriminator. `Annotation` is internal: every annotation has an
/// implicit type (built-ins by their key, user attrs by the
/// caller-supplied `value_type`), but by this point the type info has
/// been folded into the canonical `value` bytes plus the `tier2` flag,
/// so the discriminator itself is no longer carried.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Annotation {
    key: Vec<u8>,
    value: Vec<u8>,
    tier2: bool,
}

/// All built-in annotations for an entity, with each annotation's
/// tier-2 status set per the skip-list decision in #96. Used by
/// `create` (to insert) and `delete` / `expire` (to remove).
fn built_in_annotations(
    creator: Address,
    owner: Address,
    entity_key: B256,
    created_at_block: u64,
    expires_at: u64,
    content_type: &[u8],
) -> Vec<Annotation> {
    vec![
        // Singleton — only one value, no ordered iteration.
        Annotation { key: ANNOT_ALL.to_vec(), value: Vec::new(), tier2: false },
        // 20-byte address, random distribution.
        Annotation { key: ANNOT_CREATOR.to_vec(), value: encode_address(creator), tier2: false },
        // Numeric range scans are real use cases.
        Annotation { key: ANNOT_CREATED_AT_BLOCK.to_vec(), value: encode_u64_be(created_at_block), tier2: true },
        // 20-byte address, random distribution.
        Annotation { key: ANNOT_OWNER.to_vec(), value: encode_address(owner), tier2: false },
        // 32-byte entity hash, random distribution.
        Annotation { key: ANNOT_KEY.to_vec(), value: encode_b256(entity_key), tier2: false },
        // Numeric range scans are real use cases.
        Annotation { key: ANNOT_EXPIRATION.to_vec(), value: encode_u64_be(expires_at), tier2: true },
        // Glob / prefix scans (e.g. `~ "video/*"`) are real use cases.
        Annotation { key: ANNOT_CONTENT_TYPE.to_vec(), value: content_type.to_vec(), tier2: true },
    ]
}

/// User-supplied attributes flattened to annotations. Tier-2 is
/// maintained for `ATTR_UINT` / `ATTR_STRING` (range and glob queries
/// are meaningful) and skipped for `ATTR_ENTITY_KEY` (random 32-byte
/// hashes — ordered iteration produces no meaningful query).
///
/// The value bytes are stored verbatim — the precompile is responsible
/// for producing the canonical byte form per `value_type` (32-byte BE
/// for `ATTR_UINT`, packed bytes for `ATTR_STRING`, 32 raw bytes for
/// `ATTR_ENTITY_KEY`). The `value_type` discriminator itself is not
/// part of the on-chain index key/value bytes — it is only consulted
/// here to set the tier-2 flag.
fn user_annotations<'a>(
    attributes: &'a [Attribute],
) -> impl Iterator<Item = Annotation> + 'a {
    attributes.iter().map(|a| Annotation {
        key: a.key.clone(),
        value: a.value.clone(),
        tier2: a.value_type != ATTR_ENTITY_KEY,
    })
}

/// Annotations that an UPDATE op diffs: the user attributes plus
/// `$contentType`. Other built-ins (`$creator` / `$key` /
/// `$createdAtBlock` / `$owner` / `$expiration` / `$all`) don't change
/// on UPDATE and so aren't in the diff set.
fn updatable_annotations(content_type: &[u8], attributes: &[Attribute]) -> Vec<Annotation> {
    let mut out = Vec::with_capacity(1 + attributes.len());
    out.push(Annotation {
        key: ANNOT_CONTENT_TYPE.to_vec(),
        value: content_type.to_vec(),
        tier2: true,
    });
    out.extend(user_annotations(attributes));
    out
}

/// Diff two annotation sets and apply removals + insertions to the
/// corresponding tier-1 pair bitmaps (and tier-2 index trees, for
/// annotations whose `tier2` flag is set).
fn apply_annotation_diff<S: StateAdapter>(
    state: &mut S,
    old: &[Annotation],
    new: &[Annotation],
    entity_id: u64,
) -> Result<()> {
    use std::collections::BTreeSet;
    let old_set: BTreeSet<&Annotation> = old.iter().collect();
    let new_set: BTreeSet<&Annotation> = new.iter().collect();
    for a in old.iter().filter(|a| !new_set.contains(*a)) {
        remove_from_indexes(state, &a.key, &a.value, entity_id, a.tier2)?;
    }
    for a in new.iter().filter(|a| !old_set.contains(*a)) {
        insert_into_indexes(state, &a.key, &a.value, entity_id, a.tier2)?;
    }
    Ok(())
}

/// Insert `entity_id` into the tier-1 pair bitmap for
/// `(annot_key, annot_val)`. If `tier2` is set and this is the first
/// entity to use that pair, also insert `annot_val` into the
/// per-attribute tier-2 [`IndexTree`].
fn insert_into_indexes<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    entity_id: u64,
    tier2: bool,
) -> Result<()> {
    let mut bitmap = read_pair_bitmap(state, annot_key, annot_val)?;
    let was_empty = bitmap.is_empty();
    bitmap.insert(entity_id);
    state.set_code(&pair_address(annot_key, annot_val), bitmap.to_bytes())?;
    if tier2 && was_empty {
        let mut tree = read_index_tree(state, annot_key)?;
        tree.insert(annot_val.to_vec());
        state.set_code(&index_address(annot_key), tree.to_bytes())?;
    }
    Ok(())
}

/// Remove `entity_id` from the tier-1 pair bitmap for
/// `(annot_key, annot_val)`. If `tier2` is set and this was the last
/// entity using that pair, also remove `annot_val` from the
/// per-attribute tier-2 [`IndexTree`] (tombstoning the index account
/// if the tree becomes empty).
fn remove_from_indexes<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
    entity_id: u64,
    tier2: bool,
) -> Result<()> {
    let mut bitmap = read_pair_bitmap(state, annot_key, annot_val)?;
    if bitmap.is_empty() {
        // Bitmap doesn't exist yet — nothing to remove. Shouldn't
        // happen under well-formed ops; tolerated.
        return Ok(());
    }
    bitmap.remove(entity_id);
    state.set_code(&pair_address(annot_key, annot_val), bitmap.to_bytes())?;
    if tier2 && bitmap.is_empty() {
        let mut tree = read_index_tree(state, annot_key)?;
        tree.remove(annot_val);
        if tree.is_empty() {
            state.tombstone_code(&index_address(annot_key))?;
        } else {
            state.set_code(&index_address(annot_key), tree.to_bytes())?;
        }
    }
    Ok(())
}

// ─── Public read-side helpers (used by the query interpreter) ─────────

/// Read the pair-account bitmap for `(annot_key, annot_val)`. An
/// account with empty code (never written, or tombstoned) decodes to
/// an empty [`Bitmap`] — not an error.
pub fn read_pair_bitmap<S: StateAdapter>(
    state: &mut S,
    annot_key: &[u8],
    annot_val: &[u8],
) -> Result<Bitmap> {
    let pair_addr = pair_address(annot_key, annot_val);
    let code = state.code(&pair_addr)?;
    if code.is_empty() {
        Ok(Bitmap::new())
    } else {
        Bitmap::from_bytes(&code)
    }
}

/// Bitmap of every live entity ID — the `$all` built-in bitmap.
/// Convenience wrapper around [`read_pair_bitmap`].
pub fn all_entities<S: StateAdapter>(state: &mut S) -> Result<Bitmap> {
    read_pair_bitmap(state, ANNOT_ALL, b"")
}

/// Read the Tier-2 [`IndexTree`] for `attr_key`. An absent or
/// tombstoned index account decodes to an empty tree — not an error.
pub fn read_index_tree<S: StateAdapter>(state: &mut S, attr_key: &[u8]) -> Result<IndexTree> {
    let code = state.code(&index_address(attr_key))?;
    if code.is_empty() {
        Ok(IndexTree::new())
    } else {
        IndexTree::from_bytes(&code)
    }
}

/// Resolve a query-hit entity ID to its on-trie [`EntityRlp`].
///
/// Returns `Ok(None)` if the ID's `id_to_addr` slot is zero (never
/// written, or cleared by `delete` / `expire`) or if the entity
/// account has empty code (tombstoned). Returns `Err` only on
/// underlying state errors or malformed entity bytes.
pub fn resolve_id<S: StateAdapter>(state: &mut S, id: u64) -> Result<Option<EntityRlp>> {
    let raw = state.storage(&SYSTEM_ACCOUNT_ADDRESS, slot_id_to_addr(id))?;
    if raw == B256::ZERO {
        return Ok(None);
    }
    let entity_addr = Address::from_slice(&raw.0[12..]);
    let code = state.code(&entity_addr)?;
    if code.is_empty() {
        return Ok(None);
    }
    Ok(Some(EntityRlp::decode_from_code(&code)?))
}

// ─── Test backend (in-memory state DB) ────────────────────────────────

#[cfg(feature = "test-utils")]
pub mod test_utils {
    //! In-memory [`StateAdapter`] implementation, suitable for unit
    //! tests in this crate and for downstream test code that wants to
    //! drive the op handlers without a revm context.

    use super::*;
    use std::collections::HashMap;

    /// Per-account state: nonce, code, and the storage map.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct AccountState {
        pub nonce: u64,
        pub code: Vec<u8>,
        pub storage: HashMap<B256, B256>,
    }

    /// Toy state DB: account address → [`AccountState`]. Stand-in for
    /// revm's state DB during tests.
    #[derive(Debug, Clone, Default)]
    pub struct InMemoryStateDb {
        accounts: HashMap<Address, AccountState>,
    }

    impl InMemoryStateDb {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn account(&self, addr: &Address) -> Option<&AccountState> {
            self.accounts.get(addr)
        }

        pub fn account_mut(&mut self, addr: &Address) -> &mut AccountState {
            self.accounts.entry(*addr).or_default()
        }
    }

    /// Thin [`StateAdapter`] over a borrowed [`InMemoryStateDb`].
    pub struct InMemoryStateAdapter<'a> {
        db: &'a mut InMemoryStateDb,
    }

    impl<'a> InMemoryStateAdapter<'a> {
        pub fn new(db: &'a mut InMemoryStateDb) -> Self {
            Self { db }
        }
    }

    impl StateAdapter for InMemoryStateAdapter<'_> {
        fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
            Ok(self
                .db
                .account(addr)
                .map(|a| a.code.clone())
                .unwrap_or_default())
        }

        fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()> {
            let acc = self.db.account_mut(addr);
            acc.code = code;
            if acc.nonce == 0 {
                acc.nonce = 1;
            }
            Ok(())
        }

        fn tombstone_code(&mut self, addr: &Address) -> Result<()> {
            let acc = self.db.account_mut(addr);
            acc.code = Vec::new();
            // Preserve nonce >= 1 to defeat EIP-161 pruning.
            if acc.nonce == 0 {
                acc.nonce = 1;
            }
            Ok(())
        }

        fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
            Ok(self
                .db
                .account(addr)
                .and_then(|a| a.storage.get(&slot).copied())
                .unwrap_or_default())
        }

        fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()> {
            self.db.account_mut(addr).storage.insert(slot, value);
            Ok(())
        }

        fn ensure_account_persists(&mut self, addr: &Address) -> Result<()> {
            let acc = self.db.account_mut(addr);
            if acc.nonce == 0 {
                acc.nonce = 1;
            }
            Ok(())
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{InMemoryStateAdapter, InMemoryStateDb};
    use alloy_primitives::U256;
    use alloy_primitives::b256;

    // ─── Primitives ──────────────────────────────────────────────────

    #[test]
    fn entity_address_truncates_to_first_20_bytes() {
        let key = b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff");
        assert_eq!(entity_address(key).as_slice(), &key.0[..20]);
    }

    #[test]
    fn pair_address_separator_prevents_prefix_collision() {
        assert_ne!(pair_address(b"ab", b"c"), pair_address(b"a", b"bc"));
    }

    #[test]
    fn bitmap_serialization_is_deterministic() {
        // Same set inserted in different orders → identical bytes.
        let ids = [3u64, 1, 2, 42, 1_000_001, 1_000_000];
        let mut a = Bitmap::new();
        let mut b = Bitmap::new();
        for id in ids {
            a.insert(id);
        }
        for id in ids.iter().rev() {
            b.insert(*id);
        }
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn entity_rlp_roundtrip_via_code() {
        let original = EntityRlp {
            payload: b"hello".to_vec(),
            creator: Address::repeat_byte(0xaa),
            created_at_block: 1234,
            owner: Address::repeat_byte(0xbb),
            expires_at: 99_999,
            content_type: b"application/json".to_vec(),
            key: b256!("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"),
            attributes: vec![
                Attribute {
                    key: b"title".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"the answer".to_vec(),
                },
                Attribute {
                    key: b"priority".to_vec(),
                    value_type: ATTR_UINT,
                    value: U256::from(42).to_be_bytes::<32>().to_vec(),
                },
                Attribute {
                    key: b"replyTo".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: vec![0xab; 32],
                },
            ],
            last_modified_at_block: 1234,
        };
        let code = original.encode_as_code();
        assert_eq!(code[0], ENTITY_CODE_PREFIX);
        assert_eq!(
            EntityRlp::decode_from_code(&code).expect("decode"),
            original
        );
    }

    #[test]
    fn entity_rlp_decode_requires_fe_prefix() {
        let entity = EntityRlp {
            payload: vec![],
            creator: Address::ZERO,
            created_at_block: 0,
            owner: Address::ZERO,
            expires_at: 0,
            content_type: vec![],
            key: B256::ZERO,
            attributes: vec![],
            last_modified_at_block: 0,
        };
        let mut bad = entity.encode_as_code();
        bad[0] = 0x00;
        assert!(EntityRlp::decode_from_code(&bad).is_err());
    }

    // ─── Op handlers (against InMemoryStateAdapter) ───────────────────────

    fn fresh_db() -> InMemoryStateDb {
        InMemoryStateDb::default()
    }

    fn alice() -> Address {
        Address::repeat_byte(0xaa)
    }
    fn bob() -> Address {
        Address::repeat_byte(0xbb)
    }
    fn entity_key_n(n: u8) -> B256 {
        B256::from([n; 32])
    }

    #[track_caller]
    fn read_bitmap(db: &InMemoryStateDb, annot_key: &[u8], annot_val: &[u8]) -> Bitmap {
        let addr = pair_address(annot_key, annot_val);
        let code = db
            .account(&addr)
            .map(|a| a.code.clone())
            .unwrap_or_default();
        if code.is_empty() {
            Bitmap::new()
        } else {
            Bitmap::from_bytes(&code).expect("decode bitmap")
        }
    }

    #[test]
    fn create_writes_entity_and_all_bitmaps() {
        let mut db = fresh_db();
        let key = entity_key_n(0x42);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                b"hello".to_vec(),
                b"text/plain".to_vec(),
                vec![
                    Attribute {
                        key: b"tag".to_vec(),
                        value_type: ATTR_STRING,
                        value: b"music".to_vec(),
                    },
                    Attribute {
                        key: b"score".to_vec(),
                        value_type: ATTR_UINT,
                        value: U256::from(7).to_be_bytes::<32>().to_vec(),
                    },
                ],
            )
            .expect("create");
        }

        // Entity-account code written; round-trips through EntityRlp.
        let entity_addr = entity_address(key);
        let code = db.account(&entity_addr).expect("entity acc").code.clone();
        let entity = EntityRlp::decode_from_code(&code).expect("decode");
        assert_eq!(entity.owner, alice());
        assert_eq!(entity.creator, alice());
        assert_eq!(entity.expires_at, 100);
        assert_eq!(entity.created_at_block, 10);

        // System counter advanced to 1; this entity got id=0.
        let count = db
            .account(&SYSTEM_ACCOUNT_ADDRESS)
            .expect("system acc")
            .storage
            .get(&slot_entity_count())
            .copied()
            .unwrap_or_default();
        assert_eq!(storage_to_u64(count), 1);

        // All built-ins + user annotations contain entity_id=0.
        assert!(read_bitmap(&db, ANNOT_ALL, b"").contains(0));
        assert!(read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_EXPIRATION, &100u64.to_be_bytes()).contains(0));
        assert!(read_bitmap(&db, ANNOT_CONTENT_TYPE, b"text/plain").contains(0));
        assert!(read_bitmap(&db, b"tag", b"music").contains(0));
        assert!(read_bitmap(&db, b"score", &U256::from(7).to_be_bytes::<32>()).contains(0));
    }

    #[test]
    fn transfer_moves_owner_bitmap() {
        let mut db = fresh_db();
        let key = entity_key_n(1);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
            transfer(&mut state, key, 20, bob()).unwrap();
        }
        assert!(!read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_OWNER, bob().as_slice()).contains(0));

        // Entity RLP reflects the new owner.
        let entity_addr = entity_address(key);
        let entity = EntityRlp::decode_from_code(&db.account(&entity_addr).unwrap().code).unwrap();
        assert_eq!(entity.owner, bob());
    }

    #[test]
    fn extend_moves_expiration_bitmap() {
        let mut db = fresh_db();
        let key = entity_key_n(2);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                vec![],
                vec![],
            )
            .unwrap();
            extend(&mut state, key, 20, 500).unwrap();
        }
        assert!(!read_bitmap(&db, ANNOT_EXPIRATION, &100u64.to_be_bytes()).contains(0));
        assert!(read_bitmap(&db, ANNOT_EXPIRATION, &500u64.to_be_bytes()).contains(0));

        let entity_addr = entity_address(key);
        let entity = EntityRlp::decode_from_code(&db.account(&entity_addr).unwrap().code).unwrap();
        assert_eq!(entity.expires_at, 500);
    }

    #[test]
    fn update_diffs_only_changed_annotations() {
        let mut db = fresh_db();
        let key = entity_key_n(3);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"tag".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"a".to_vec(),
                }],
            )
            .unwrap();
            // Change the tag value; keep content type the same.
            update(
                &mut state,
                key,
                20,
                vec![0xff],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"tag".to_vec(),
                    value_type: ATTR_STRING,
                    value: b"b".to_vec(),
                }],
            )
            .unwrap();
        }
        // tag=a bitmap loses the entity, tag=b gains it.
        assert!(!read_bitmap(&db, b"tag", b"a").contains(0));
        assert!(read_bitmap(&db, b"tag", b"b").contains(0));
        // content type unchanged → bitmap still contains it.
        assert!(read_bitmap(&db, ANNOT_CONTENT_TYPE, b"text/plain").contains(0));
        // Owner/expiration untouched.
        assert!(read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
    }

    #[test]
    fn delete_clears_bitmaps_and_tombstones_account() {
        let mut db = fresh_db();
        let key = entity_key_n(4);
        let entity_addr = entity_address(key);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
            delete(&mut state, key).unwrap();
        }
        // Bitmaps drop the entity.
        assert!(!read_bitmap(&db, ANNOT_ALL, b"").contains(0));
        assert!(!read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));

        // ID-map slots cleared.
        let count_slot = slot_entity_count();
        let _ = count_slot; // counter NOT decremented — only ID maps cleared.
        let id_to_addr_slot = slot_id_to_addr(0);
        assert_eq!(
            db.account(&SYSTEM_ACCOUNT_ADDRESS)
                .unwrap()
                .storage
                .get(&id_to_addr_slot)
                .copied()
                .unwrap_or_default(),
            B256::ZERO
        );

        // Entity account tombstoned: code empty, nonce=1.
        let acc = db.account(&entity_addr).expect("entity acc still exists");
        assert!(acc.code.is_empty());
        assert_eq!(acc.nonce, 1);
    }

    // ─── IndexTree ───────────────────────────────────────────────────

    fn read_art_raw(db: &InMemoryStateDb, attr_key: &[u8]) -> IndexTree {
        let addr = index_address(attr_key);
        let code = db
            .account(&addr)
            .map(|a| a.code.clone())
            .unwrap_or_default();
        if code.is_empty() {
            IndexTree::new()
        } else {
            IndexTree::from_bytes(&code).expect("decode IndexTree")
        }
    }

    #[test]
    fn index_tree_round_trip() {
        let mut tree = IndexTree::new();
        tree.insert(b"apple".to_vec());
        tree.insert(b"banana".to_vec());
        tree.insert(b"cherry".to_vec());
        let decoded = IndexTree::from_bytes(&tree.to_bytes()).expect("decode");
        let vals: Vec<Vec<u8>> = decoded.iter_gte(b"").collect();
        assert_eq!(
            vals,
            [b"apple".to_vec(), b"banana".to_vec(), b"cherry".to_vec()]
        );
    }

    #[test]
    fn index_tree_serialization_is_deterministic() {
        let vals = [b"z".to_vec(), b"a".to_vec(), b"m".to_vec()];
        let mut a = IndexTree::new();
        let mut b = IndexTree::new();
        for v in &vals {
            a.insert(v.clone());
        }
        for v in vals.iter().rev() {
            b.insert(v.clone());
        }
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    #[test]
    fn index_tree_range_and_prefix_ops() {
        let mut tree = IndexTree::new();
        for v in [b"aaa", b"aab", b"abc", b"bbb", b"ccc"] {
            tree.insert(v.to_vec());
        }
        let gt: Vec<Vec<u8>> = tree.iter_gt(b"aab").collect();
        assert_eq!(gt, [b"abc".to_vec(), b"bbb".to_vec(), b"ccc".to_vec()]);

        let lte: Vec<Vec<u8>> = tree.iter_lte(b"aab").collect();
        assert_eq!(lte, [b"aaa".to_vec(), b"aab".to_vec()]);

        let prefix: Vec<Vec<u8>> = tree.iter_prefix(b"aa").collect();
        assert_eq!(prefix, [b"aaa".to_vec(), b"aab".to_vec()]);
    }

    #[test]
    fn insert_into_indexes_writes_tier2_on_first_entity() {
        let mut db = fresh_db();
        let val = b"hello".to_vec();
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"tag", &val, 0, true).unwrap();
        }
        let tree = read_art_raw(&db, b"tag");
        let vals: Vec<Vec<u8>> = tree.iter_gte(b"").collect();
        assert_eq!(
            vals,
            [b"hello".to_vec()],
            "index should contain value after first entity"
        );

        // Second insert of same value — bitmap was non-empty, ART unchanged.
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"tag", &val, 1, true).unwrap();
        }
        let tree2 = read_art_raw(&db, b"tag");
        assert_eq!(tree2.iter_gte(b"").count(), 1);
    }

    #[test]
    fn remove_from_indexes_removes_tier2_on_last_entity() {
        let mut db = fresh_db();
        let val = b"hello".to_vec();
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            insert_into_indexes(&mut state, b"tag", &val, 0, true).unwrap();
            insert_into_indexes(&mut state, b"tag", &val, 1, true).unwrap();
        }

        // Remove first entity — bitmap still has entity 1, ART unchanged.
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            remove_from_indexes(&mut state, b"tag", &val, 0, true).unwrap();
        }
        assert!(
            !read_art_raw(&db, b"tag").is_empty(),
            "index should survive while entity 1 remains"
        );

        // Remove last entity — bitmap is now empty, index account tombstoned.
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            remove_from_indexes(&mut state, b"tag", &val, 1, true).unwrap();
        }
        assert!(
            read_art_raw(&db, b"tag").is_empty(),
            "index should be empty after last entity removed"
        );
    }

    #[test]
    fn expire_has_same_state_path_as_delete() {
        let mut db_a = fresh_db();
        let mut db_b = fresh_db();
        let key = entity_key_n(5);
        for db in [&mut db_a, &mut db_b] {
            let mut state = InMemoryStateAdapter::new(db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        {
            let mut state = InMemoryStateAdapter::new(&mut db_a);
            delete(&mut state, key).unwrap();
        }
        {
            let mut state = InMemoryStateAdapter::new(&mut db_b);
            expire(&mut state, key).unwrap();
        }
        // Equal: both paths produce the same account map.
        for addr in [
            entity_address(key),
            SYSTEM_ACCOUNT_ADDRESS,
            pair_address(ANNOT_ALL, b""),
            pair_address(ANNOT_OWNER, alice().as_slice()),
        ] {
            assert_eq!(
                db_a.account(&addr),
                db_b.account(&addr),
                "mismatch at {addr}"
            );
        }
    }

    // ─── Tier-2 skip-list (issue #96) ────────────────────────────────

    #[track_caller]
    fn index_account_present(db: &InMemoryStateDb, attr_key: &[u8]) -> bool {
        db.account(&index_address(attr_key)).is_some()
    }

    #[test]
    fn create_skips_tier2_index_for_skip_listed_built_ins() {
        let mut db = fresh_db();
        let key = entity_key_n(1);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        for k in [ANNOT_ALL, ANNOT_CREATOR, ANNOT_OWNER, ANNOT_KEY] {
            assert!(
                !index_account_present(&db, k),
                "index_address({:?}) should not exist for skip-listed built-in",
                std::str::from_utf8(k).unwrap()
            );
        }
    }

    #[test]
    fn create_keeps_tier2_index_for_tier2_built_ins() {
        let mut db = fresh_db();
        let key = entity_key_n(2);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        for k in [ANNOT_CREATED_AT_BLOCK, ANNOT_EXPIRATION, ANNOT_CONTENT_TYPE] {
            assert!(
                index_account_present(&db, k),
                "index_address({:?}) should exist for tier-2 built-in",
                std::str::from_utf8(k).unwrap()
            );
        }
    }

    #[test]
    fn create_skips_tier2_index_for_user_entity_key_attribute() {
        let mut db = fresh_db();
        let key = entity_key_n(3);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"ref".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: entity_key_n(99).as_slice().to_vec(),
                }],
            )
            .unwrap();
        }
        assert!(!index_account_present(&db, b"ref"));
    }

    #[test]
    fn create_keeps_tier2_index_for_user_uint_and_string_attributes() {
        let mut db = fresh_db();
        let key = entity_key_n(4);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![
                    Attribute {
                        key: b"tag".to_vec(),
                        value_type: ATTR_STRING,
                        value: b"music".to_vec(),
                    },
                    Attribute {
                        key: b"score".to_vec(),
                        value_type: ATTR_UINT,
                        value: U256::from(7).to_be_bytes::<32>().to_vec(),
                    },
                ],
            )
            .unwrap();
        }
        assert!(index_account_present(&db, b"tag"));
        assert!(index_account_present(&db, b"score"));
    }

    #[test]
    fn eq_lookup_via_tier1_still_works_for_skip_listed_attrs() {
        // Tier-1 pair bitmaps are unaffected by the skip list.
        let mut db = fresh_db();
        let key = entity_key_n(5);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"ref".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: entity_key_n(99).as_slice().to_vec(),
                }],
            )
            .unwrap();
        }
        assert!(read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_KEY, key.as_slice()).contains(0));
        assert!(read_bitmap(&db, b"ref", entity_key_n(99).as_slice()).contains(0));
    }

    #[test]
    fn delete_does_not_touch_absent_tier2_indexes() {
        // delete must not error or allocate index accounts for
        // skip-listed annotations, even though the same code path
        // tombstones the kept ones.
        let mut db = fresh_db();
        let key = entity_key_n(6);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![Attribute {
                    key: b"ref".to_vec(),
                    value_type: ATTR_ENTITY_KEY,
                    value: entity_key_n(99).as_slice().to_vec(),
                }],
            )
            .unwrap();
            delete(&mut state, key).unwrap();
        }
        for k in [
            ANNOT_ALL,
            ANNOT_CREATOR,
            ANNOT_OWNER,
            ANNOT_KEY,
            b"ref" as &[u8],
        ] {
            assert!(
                !index_account_present(&db, k),
                "skip-listed index_address({:?}) should still not exist after delete",
                std::str::from_utf8(k).unwrap()
            );
        }
    }

    #[test]
    fn transfer_does_not_touch_owner_tier2_index() {
        // $owner is skip-listed; transfer moves the tier-1 bitmap but
        // never allocates an $owner index account.
        let mut db = fresh_db();
        let key = entity_key_n(7);
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            create(
                &mut state,
                alice(),
                key,
                100,
                10,
                vec![],
                b"text/plain".to_vec(),
                vec![],
            )
            .unwrap();
        }
        assert!(!index_account_present(&db, ANNOT_OWNER));
        {
            let mut state = InMemoryStateAdapter::new(&mut db);
            transfer(&mut state, key, 11, bob()).unwrap();
        }
        assert!(!index_account_present(&db, ANNOT_OWNER));
        // Tier-1 bitmap moved.
        assert!(!read_bitmap(&db, ANNOT_OWNER, alice().as_slice()).contains(0));
        assert!(read_bitmap(&db, ANNOT_OWNER, bob().as_slice()).contains(0));
    }
}
