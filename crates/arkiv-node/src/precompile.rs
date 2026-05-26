//! Arkiv precompile — the single entry point Arkiv exposes to the EVM.
//!
//! Sits at [`ARKIV_ADDRESS`] (`0x44…0044`), the address the SDK already
//! targets for `execute(Operation[])` / `nonces(address)` calls. There
//! is no `EntityRegistry` Solidity predeploy in v2 — this precompile
//! decodes the SDK calldata directly, runs ownership / expiration /
//! charset validation, mints entity keys, emits `EntityOperation`
//! events, and dispatches to the [`arkiv_entitydb`] state handlers.
//!
//! Layered responsibilities:
//!
//! 1. Caller restrictions (`DELEGATECALL` / `CALLCODE` rejected;
//!    `STATICCALL` allowed only for `nonces`; value-bearing CALLs
//!    rejected). Each rejection is a `PrecompileError::Fatal` —
//!    `Result::Err` halts execution, unlike Solidity reverts which
//!    return data.
//! 2. Selector dispatch on the first 4 calldata bytes:
//!    `execute(Operation[])` (write) or `nonces(address)` (view).
//! 3. Per-op validation (charset, ownership, expiration, BTL, transfer
//!    constraints) — failures are returned as Solidity-style reverts
//!    so the SDK's error decoders match v1.
//! 4. State mutation via [`arkiv_entitydb`]'s op handlers, threaded
//!    through a [`RevmStateAdapter`] over revm's `EvmInternals`.
//! 5. Log emission (`EntityOperation`) — addressed at `ARKIV_ADDRESS`
//!    so the SDK's `eth_getLogs` filter on the SDK constant resolves
//!    every event.

use alloy_evm::{EvmInternals, precompiles::{DynPrecompile, PrecompileInput}};
use alloy_primitives::{Address, B256, Bytes, FixedBytes, Log, U256, keccak256};
use alloy_sol_types::{SolCall, SolError, SolEvent, sol};
use arkiv_entitydb::{ARKIV_ADDRESS, NumericAnnotation, StateAdapter, StringAnnotation};
use revm::{
    precompile::{
        PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
    },
    state::Bytecode,
};

// ─── ABI mirror of `EntityRegistry.sol` ──────────────────────────────
//
// Field order / types / names must stay in lockstep with
// `contracts/src/EntityRegistry.sol`. We declare the `execute` and
// `nonces` function shapes so `alloy-sol-types` generates the selector
// constants + decoders + the `EntityOperation` event encoder.

sol! {
    #[derive(Debug)]
    struct Mime128 {
        bytes32[4] data;
    }

    #[derive(Debug)]
    struct Attribute {
        bytes32 name;            // Ident32 UDVT in Solidity; bytes32 on the wire
        uint8 valueType;
        bytes32[4] value;
    }

    #[derive(Debug)]
    struct Operation {
        uint8 operationType;
        bytes32 entityKey;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
        uint32 btl;              // BlockNumber32 UDVT in Solidity
        address newOwner;
    }

    function execute(Operation[] ops) external;
    function nonces(address owner) external view returns (uint32);

    event EntityOperation(
        bytes32 indexed entityKey,
        uint8 indexed operationType,
        address indexed owner,
        uint32 expiresAt,        // BlockNumber32 UDVT
        bytes32 entityHash       // always bytes32(0) in v2
    );

    // ── Error selectors (v1-compatible) ──────────────────────────────
    //
    // Returned as Solidity-style revert payloads so SDK error decoders
    // match v1 byte-for-byte.

    error Ident32Empty();
    error Ident32InvalidByte(uint256 position, bytes1 value);

    error EmptyBatch();
    error InvalidOpType(uint8 operationType);
    error ZeroBtl();
    error EntityNotFound(bytes32 entityKey);
    error NotOwner(bytes32 entityKey, address caller, address owner);
    error EntityExpired(bytes32 entityKey, uint32 expiresAt);
    error ExpiryNotExtended(bytes32 entityKey, uint32 newExpiresAt, uint32 currentExpiresAt);
    error TransferToZeroAddress(bytes32 entityKey);
    error TransferToSelf(bytes32 entityKey);
    error EntityNotExpired(bytes32 entityKey, uint32 expiresAt);
}

// Op-type + attribute valueType tags. Must match `Entity.{CREATE..EXPIRE}`
// and `Entity.ATTR_{UINT,STRING,ENTITY_KEY}` in EntityRegistry.sol.
const OP_CREATE: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_EXTEND: u8 = 3;
const OP_TRANSFER: u8 = 4;
const OP_DELETE: u8 = 5;
const OP_EXPIRE: u8 = 6;

const ATTR_UINT: u8 = 1;
const ATTR_STRING: u8 = 2;
const ATTR_ENTITY_KEY: u8 = 3;

// ─── Gas model ────────────────────────────────────────────────────────
//
// Pure function of op shape — required for cross-node consensus on the
// returned `gas_used`. Anchored to EVM costs: SSTORE_INIT ≈ 22,100,
// SSTORE_RESET ≈ 5,000, per code byte ≈ 200.

const G_EXECUTE_BASE: u64 = 5_000; // fixed overhead for the execute() call
const G_NONCES: u64 = 800;
const G_CREATE: u64 = 80_000;
const G_UPDATE: u64 = 30_000;
const G_EXTEND: u64 = 25_000;
const G_TRANSFER: u64 = 25_000;
// DELETE / EXPIRE: base covers Tier-1 bitmap removes (same as before)
// plus a flat allowance for worst-case Tier-2 index removes (max 32
// UINT/STRING attrs × G_ART_INDEXED_ANNOTATION / 3 average).
const G_DELETE: u64 = 62_000;
const G_EXPIRE: u64 = 62_000;
const G_BYTE: u64 = 16;
const G_ANNOTATION: u64 = 5_000;
// Tier-2 index account read + conditional write per UINT / STRING
// attribute; charged conservatively (always) so the gas formula stays
// a pure function of calldata.
const G_ART_INDEXED_ANNOTATION: u64 = 6_000;

const PRECOMPILE_NAME: &str = "ARKIV";

pub fn arkiv_precompile() -> DynPrecompile {
    let id = PrecompileId::custom(PRECOMPILE_NAME);
    let call = move |mut input: PrecompileInput<'_>| -> PrecompileResult {
        let _call_span = tracing::debug_span!("precompile_call").entered();

        // Reject DELEGATECALL / CALLCODE: the precompile mutates state
        // at its own address, so DELEGATECALL semantics (caller's
        // storage) would silently corrupt unrelated accounts.
        if input.target_address != input.bytecode_address {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: DELEGATECALL/CALLCODE not allowed".into(),
            ));
        }
        if input.value != U256::ZERO {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: value-bearing call not allowed".into(),
            ));
        }

        if input.data.len() < 4 {
            return Ok(revert(empty_calldata_revert(), input.reservoir));
        }
        let (selector_bytes, body) = input.data.split_at(4);
        let selector: [u8; 4] = selector_bytes.try_into().expect("split_at(4) on len>=4");

        match selector {
            noncesCall::SELECTOR => dispatch_nonces(body, &mut input),
            executeCall::SELECTOR => dispatch_execute(body, &mut input),
            _ => Ok(revert(unknown_selector_revert(selector), input.reservoir)),
        }
    };
    DynPrecompile::new_stateful(id, call)
}

// ─── Dispatch: nonces(address) ───────────────────────────────────────

fn dispatch_nonces(body: &[u8], input: &mut PrecompileInput<'_>) -> PrecompileResult {
    if G_NONCES > input.gas {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, input.reservoir));
    }
    let decoded = match noncesCall::abi_decode_raw(body) {
        Ok(d) => d,
        Err(e) => {
            return Ok(revert(
                format!("arkiv precompile: invalid nonces calldata: {e}").into_bytes().into(),
                input.reservoir,
            ));
        }
    };
    let nonce = {
        let mut adapter = RevmStateAdapter::new(&mut input.internals);
        arkiv_entitydb::read_nonce(&mut adapter, decoded.owner)
            .map_err(|e| PrecompileError::Fatal(format!("arkiv precompile: read nonce: {e}")))?
    };
    let ret = noncesCall::abi_encode_returns(&nonce);
    Ok(PrecompileOutput::new(G_NONCES, ret.into(), input.reservoir))
}

// ─── Dispatch: execute(Operation[]) ──────────────────────────────────

fn dispatch_execute(body: &[u8], input: &mut PrecompileInput<'_>) -> PrecompileResult {
    if input.is_static {
        return Err(PrecompileError::Fatal(
            "arkiv precompile: execute() not allowed in STATICCALL".into(),
        ));
    }
    let ops = match executeCall::abi_decode_raw(body) {
        Ok(d) => d.ops,
        Err(e) => {
            return Ok(revert(
                format!("arkiv precompile: invalid execute calldata: {e}").into_bytes().into(),
                input.reservoir,
            ));
        }
    };
    if ops.is_empty() {
        return Ok(revert(EmptyBatch {}.abi_encode().into(), input.reservoir));
    }

    let gas_used = total_gas(&ops);
    if gas_used > input.gas {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, input.reservoir));
    }

    let current_block: u64 = input.internals.block_number().saturating_to();
    let caller = input.caller;
    let chain_id: u64 = input.internals.chain_id();

    // Apply ops one at a time. A validation failure encodes a
    // Solidity-style revert payload that the SDK can decode (rolls
    // back the whole batch atomically — revm reverts on the precompile
    // boundary).
    let dispatch_span = tracing::debug_span!("precompile_dispatch", n_ops = ops.len()).entered();
    for (i, op) in ops.iter().enumerate() {
        match apply_op(input, caller, chain_id, current_block, op) {
            Ok(()) => {}
            Err(ApplyError::Revert(payload)) => {
                drop(dispatch_span);
                tracing::debug!(op_index = i, "arkiv precompile: op reverted");
                return Ok(revert(payload, input.reservoir));
            }
            Err(ApplyError::Fatal(msg)) => {
                drop(dispatch_span);
                return Err(PrecompileError::Fatal(msg));
            }
        }
    }
    drop(dispatch_span);

    Ok(PrecompileOutput::new(gas_used, Bytes::new(), input.reservoir))
}

// ─── Per-op application ──────────────────────────────────────────────

#[derive(Debug)]
enum ApplyError {
    /// Solidity-style revert with abi-encoded error payload.
    Revert(Bytes),
    /// Halt-execution-with-message — for genuine internal failures
    /// (state-adapter errors, decoder bugs). Maps to
    /// `PrecompileError::Fatal`.
    Fatal(String),
}

impl<E: std::fmt::Display> From<E> for ApplyError {
    // Convert eyre/anyhow-style errors from `arkiv_entitydb` into
    // fatals — those represent state-DB failures, not validation.
    fn from(err: E) -> Self {
        ApplyError::Fatal(err.to_string())
    }
}

fn apply_op(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    chain_id: u64,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    match op.operationType {
        OP_CREATE => apply_create(input, caller, chain_id, current_block, op),
        OP_UPDATE => apply_update(input, caller, current_block, op),
        OP_EXTEND => apply_extend(input, caller, current_block, op),
        OP_TRANSFER => apply_transfer(input, caller, current_block, op),
        OP_DELETE => apply_delete(input, caller, current_block, op),
        OP_EXPIRE => apply_expire(input, current_block, op),
        t => Err(ApplyError::Revert(InvalidOpType { operationType: t }.abi_encode().into())),
    }
}

fn apply_create(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    chain_id: u64,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    if op.btl == 0 {
        return Err(ApplyError::Revert(ZeroBtl {}.abi_encode().into()));
    }
    validate_attribute_names(&op.attributes)?;

    let expires_at = current_block.saturating_add(op.btl as u64);
    let (strings, numerics) = convert_attributes(&op.attributes)?;
    let entity_key = {
        let mut adapter = RevmStateAdapter::new(&mut input.internals);
        let current_nonce = arkiv_entitydb::bump_nonce(&mut adapter, caller)
            .map_err(|e| ApplyError::Fatal(format!("bump nonce: {e}")))?;
        let entity_key = derive_entity_key(chain_id, caller, current_nonce);
        arkiv_entitydb::create(
            &mut adapter,
            caller,
            entity_key,
            expires_at,
            current_block,
            op.payload.to_vec(),
            mime128_to_bytes(&op.contentType),
            strings,
            numerics,
        )?;
        entity_key
    };
    emit_entity_op(input, entity_key, OP_CREATE, caller, expires_at);
    Ok(())
}

fn apply_update(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    validate_attribute_names(&op.attributes)?;
    let entity = load_entity_for_owner(input, caller, current_block, op.entityKey, false)?;
    let (strings, numerics) = convert_attributes(&op.attributes)?;
    {
        let mut adapter = RevmStateAdapter::new(&mut input.internals);
        arkiv_entitydb::update(
            &mut adapter,
            op.entityKey,
            current_block,
            op.payload.to_vec(),
            mime128_to_bytes(&op.contentType),
            strings,
            numerics,
        )?;
    }
    emit_entity_op(input, op.entityKey, OP_UPDATE, entity.owner, entity.expires_at);
    Ok(())
}

fn apply_extend(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    if op.btl == 0 {
        return Err(ApplyError::Revert(ZeroBtl {}.abi_encode().into()));
    }
    let entity = load_entity_for_owner(input, caller, current_block, op.entityKey, false)?;
    let new_expires_at = current_block.saturating_add(op.btl as u64);
    if new_expires_at <= entity.expires_at {
        return Err(ApplyError::Revert(
            ExpiryNotExtended {
                entityKey: op.entityKey,
                newExpiresAt: clip_u32(new_expires_at),
                currentExpiresAt: clip_u32(entity.expires_at),
            }
            .abi_encode()
            .into(),
        ));
    }
    {
        let mut adapter = RevmStateAdapter::new(&mut input.internals);
        arkiv_entitydb::extend(&mut adapter, op.entityKey, current_block, new_expires_at)?;
    }
    emit_entity_op(input, op.entityKey, OP_EXTEND, entity.owner, new_expires_at);
    Ok(())
}

fn apply_transfer(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    let entity = load_entity_for_owner(input, caller, current_block, op.entityKey, false)?;
    if op.newOwner == Address::ZERO {
        return Err(ApplyError::Revert(
            TransferToZeroAddress { entityKey: op.entityKey }.abi_encode().into(),
        ));
    }
    if op.newOwner == entity.owner {
        return Err(ApplyError::Revert(
            TransferToSelf { entityKey: op.entityKey }.abi_encode().into(),
        ));
    }
    {
        let mut adapter = RevmStateAdapter::new(&mut input.internals);
        arkiv_entitydb::transfer(&mut adapter, op.entityKey, current_block, op.newOwner)?;
    }
    emit_entity_op(input, op.entityKey, OP_TRANSFER, op.newOwner, entity.expires_at);
    Ok(())
}

fn apply_delete(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    let entity = load_entity_for_owner(input, caller, current_block, op.entityKey, false)?;
    {
        let mut adapter = RevmStateAdapter::new(&mut input.internals);
        arkiv_entitydb::delete(&mut adapter, op.entityKey)?;
    }
    emit_entity_op(input, op.entityKey, OP_DELETE, entity.owner, entity.expires_at);
    Ok(())
}

fn apply_expire(
    input: &mut PrecompileInput<'_>,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    // EXPIRE doesn't require ownership; only that the entity exists
    // and is past its expiry. `load_entity_for_owner(..., allow_expired=true)`
    // skips the expiry guard but still rejects missing entities.
    let entity =
        load_entity(input, op.entityKey)?.ok_or_else(|| not_found_revert(op.entityKey))?;
    if entity.expires_at > current_block {
        return Err(ApplyError::Revert(
            EntityNotExpired {
                entityKey: op.entityKey,
                expiresAt: clip_u32(entity.expires_at),
            }
            .abi_encode()
            .into(),
        ));
    }
    {
        let mut adapter = RevmStateAdapter::new(&mut input.internals);
        arkiv_entitydb::expire(&mut adapter, op.entityKey)?;
    }
    emit_entity_op(input, op.entityKey, OP_EXPIRE, entity.owner, entity.expires_at);
    Ok(())
}

// ─── Validation helpers ──────────────────────────────────────────────

/// Bitmap of valid identifier characters: a-z, 0-9, '.', '-', '_'.
/// Bit `b` set ⇔ byte value `b` is allowed.
const IDENT_CHARSET: u128 = {
    let mut m: u128 = 0;
    m |= 1u128 << 0x2D; // '-'
    m |= 1u128 << 0x2E; // '.'
    let mut b = 0x30u8;
    while b <= 0x39 {
        // '0'..='9'
        m |= 1u128 << b;
        b += 1;
    }
    m |= 1u128 << 0x5F; // '_'
    let mut b = 0x61u8;
    while b <= 0x7A {
        // 'a'..='z'
        m |= 1u128 << b;
        b += 1;
    }
    m
};

/// Bitmap of valid leading bytes: a-z only.
const IDENT_LEADING: u128 = {
    let mut m: u128 = 0;
    let mut b = 0x61u8;
    while b <= 0x7A {
        m |= 1u128 << b;
        b += 1;
    }
    m
};

fn validate_ident32(raw: B256) -> Result<(), ApplyError> {
    let bytes = raw.0;
    let b0 = bytes[0];
    if b0 == 0 {
        return Err(ApplyError::Revert(Ident32Empty {}.abi_encode().into()));
    }
    if (b0 as u128) > 127 || (IDENT_LEADING >> b0) & 1 == 0 {
        return Err(ApplyError::Revert(
            Ident32InvalidByte {
                position: U256::ZERO,
                value: FixedBytes::<1>::from([b0]),
            }
            .abi_encode()
            .into(),
        ));
    }
    let mut j = 1;
    while j < 32 {
        let b = bytes[j];
        if b == 0 {
            // Remaining bytes must all be zero (no embedded nulls).
            for k in (j + 1)..32 {
                if bytes[k] != 0 {
                    return Err(ApplyError::Revert(
                        Ident32InvalidByte {
                            position: U256::from(k),
                            value: FixedBytes::<1>::from([bytes[k]]),
                        }
                        .abi_encode()
                        .into(),
                    ));
                }
            }
            return Ok(());
        }
        if (b as u128) > 127 || (IDENT_CHARSET >> b) & 1 == 0 {
            return Err(ApplyError::Revert(
                Ident32InvalidByte {
                    position: U256::from(j),
                    value: FixedBytes::<1>::from([b]),
                }
                .abi_encode()
                .into(),
            ));
        }
        j += 1;
    }
    Ok(())
}

fn validate_attribute_names(attrs: &[Attribute]) -> Result<(), ApplyError> {
    for a in attrs {
        validate_ident32(a.name)?;
    }
    Ok(())
}

// ─── Entity lookup + ownership guards ────────────────────────────────

struct ExistingEntity {
    owner: Address,
    expires_at: u64,
}

fn load_entity(
    input: &mut PrecompileInput<'_>,
    entity_key: B256,
) -> Result<Option<ExistingEntity>, ApplyError> {
    let entity_addr = arkiv_entitydb::entity_address(entity_key);
    let mut adapter = RevmStateAdapter::new(&mut input.internals);
    let code = adapter.code(&entity_addr)?;
    if code.is_empty() {
        return Ok(None);
    }
    let rlp = arkiv_entitydb::EntityRlp::decode_from_code(&code)
        .map_err(|e| ApplyError::Fatal(format!("decode entity {entity_addr}: {e}")))?;
    Ok(Some(ExistingEntity { owner: rlp.owner, expires_at: rlp.expires_at }))
}

/// Load an entity and assert: exists, not expired (unless
/// `allow_expired`), owned by `caller`. Returns the existing
/// `(owner, expiresAt)` for the caller's downstream use (event
/// emission, transfer's new-owner check, ...).
fn load_entity_for_owner(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    current_block: u64,
    entity_key: B256,
    allow_expired: bool,
) -> Result<ExistingEntity, ApplyError> {
    let entity = load_entity(input, entity_key)?
        .ok_or_else(|| not_found_revert(entity_key))?;
    if !allow_expired && entity.expires_at <= current_block {
        return Err(ApplyError::Revert(
            EntityExpired {
                entityKey: entity_key,
                expiresAt: clip_u32(entity.expires_at),
            }
            .abi_encode()
            .into(),
        ));
    }
    if entity.owner != caller {
        return Err(ApplyError::Revert(
            NotOwner { entityKey: entity_key, caller, owner: entity.owner }
                .abi_encode()
                .into(),
        ));
    }
    Ok(entity)
}

fn not_found_revert(entity_key: B256) -> ApplyError {
    ApplyError::Revert(EntityNotFound { entityKey: entity_key }.abi_encode().into())
}

fn empty_calldata_revert() -> Bytes {
    b"arkiv precompile: calldata too short for selector".to_vec().into()
}

fn unknown_selector_revert(selector: [u8; 4]) -> Bytes {
    format!("arkiv precompile: unknown selector 0x{}", alloy_primitives::hex::encode(selector))
        .into_bytes()
        .into()
}

fn revert(data: Bytes, reservoir: u64) -> PrecompileOutput {
    PrecompileOutput::revert(0, data, reservoir)
}

// ─── Entity-key derivation ───────────────────────────────────────────

/// `keccak256(abi.encodePacked(chainId, ARKIV_ADDRESS, owner, nonce))` —
/// matches the SDK's local key derivation
/// ([arkiv-sdk-js/src/utils/arkivTransactions.ts](arkiv-sdk-js/src/utils/arkivTransactions.ts#L116))
/// and the v1 `EntityRegistry.entityKey(...)` formula. The "registry"
/// address slot is reused for our single Arkiv address so the SDK's
/// derivation continues to match.
fn derive_entity_key(chain_id: u64, owner: Address, nonce: u32) -> B256 {
    let mut buf = Vec::with_capacity(32 + 20 + 20 + 4);
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    buf.extend_from_slice(ARKIV_ADDRESS.as_slice());
    buf.extend_from_slice(owner.as_slice());
    buf.extend_from_slice(&nonce.to_be_bytes());
    keccak256(&buf)
}

// ─── Event emission ──────────────────────────────────────────────────

fn emit_entity_op(
    input: &mut PrecompileInput<'_>,
    entity_key: B256,
    op_type: u8,
    owner: Address,
    expires_at: u64,
) {
    let event = EntityOperation {
        entityKey: entity_key,
        operationType: op_type,
        owner,
        expiresAt: clip_u32(expires_at),
        entityHash: B256::ZERO,
    };
    input
        .internals
        .log(Log { address: ARKIV_ADDRESS, data: event.encode_log_data() });
}

// ─── Gas accounting ──────────────────────────────────────────────────

fn total_gas(ops: &[Operation]) -> u64 {
    ops.iter()
        .map(op_gas)
        .fold(G_EXECUTE_BASE, u64::saturating_add)
}

fn op_gas(op: &Operation) -> u64 {
    let base = match op.operationType {
        OP_CREATE => G_CREATE,
        OP_UPDATE => G_UPDATE,
        OP_EXTEND => G_EXTEND,
        OP_TRANSFER => G_TRANSFER,
        OP_DELETE => G_DELETE,
        OP_EXPIRE => G_EXPIRE,
        // Malformed op-types still get charged so a bad batch can't
        // dodge gas; dispatch will revert anyway.
        _ => G_CREATE,
    };

    if !matches!(op.operationType, OP_CREATE | OP_UPDATE) {
        return base;
    }

    let payload_bytes = op.payload.len() as u64;
    let annotation_count = op.attributes.len() as u64;
    // Each annotation's name (≤32 bytes) + value (≤128 bytes) lands in
    // both the entity RLP and a pair-account bitmap.
    let annotation_bytes = annotation_count.saturating_mul(32 + 128);
    // UINT and STRING attributes also update the Tier-2 index account.
    // ENTITY_KEY attributes are excluded from index maintenance.
    let indexed_count = op
        .attributes
        .iter()
        .filter(|a| a.valueType == ATTR_UINT || a.valueType == ATTR_STRING)
        .count() as u64;

    base.saturating_add(payload_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_count.saturating_mul(G_ANNOTATION))
        .saturating_add(indexed_count.saturating_mul(G_ART_INDEXED_ANNOTATION))
}

// ─── Encoding helpers ────────────────────────────────────────────────

fn pack_bytes32_4(words: &[FixedBytes<32>; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    for w in words {
        out.extend_from_slice(w.as_slice());
    }
    strip_trailing_zeros(out)
}

fn strip_trailing_zeros(mut v: Vec<u8>) -> Vec<u8> {
    while matches!(v.last(), Some(0)) {
        v.pop();
    }
    v
}

fn mime128_to_bytes(m: &Mime128) -> Vec<u8> {
    pack_bytes32_4(&m.data)
}

fn ident32_to_bytes(name: B256) -> Vec<u8> {
    strip_trailing_zeros(name.0.to_vec())
}

fn convert_attributes(
    attrs: &[Attribute],
) -> Result<(Vec<StringAnnotation>, Vec<NumericAnnotation>), ApplyError> {
    let mut strings = Vec::new();
    let mut numerics = Vec::new();
    for a in attrs {
        let key = ident32_to_bytes(a.name);
        match a.valueType {
            ATTR_UINT => {
                // SDK packs the uint256 left-aligned into value[0]; the
                // remaining three words are zero.
                numerics.push(NumericAnnotation {
                    key,
                    value: U256::from_be_slice(a.value[0].as_slice()),
                });
            }
            ATTR_STRING => {
                strings.push(StringAnnotation {
                    key,
                    value: pack_bytes32_4(&a.value),
                });
            }
            ATTR_ENTITY_KEY => {
                // 32 raw bytes — no trailing-zero strip (a real key may
                // end in zeros).
                strings.push(StringAnnotation {
                    key,
                    value: a.value[0].as_slice().to_vec(),
                });
            }
            t => {
                return Err(ApplyError::Fatal(format!(
                    "unknown attribute valueType {t}"
                )));
            }
        }
    }
    Ok((strings, numerics))
}

/// `u64` block-number to `uint32` for event/error fields. Block numbers
/// only fit u32 by chain assumption; we saturate so a buggy override
/// can't produce nonsense ABI data.
fn clip_u32(n: u64) -> u32 {
    n.min(u32::MAX as u64) as u32
}

// ─── StateAdapter over revm's `EvmInternals` ─────────────────────────

struct RevmStateAdapter<'a, 'b> {
    internals: &'a mut EvmInternals<'b>,
}

impl<'a, 'b> RevmStateAdapter<'a, 'b> {
    fn new(internals: &'a mut EvmInternals<'b>) -> Self {
        Self { internals }
    }

    /// `set_code` doesn't bump the nonce; new accounts would land with
    /// `nonce = 0` and EIP-161 would prune them. Force `nonce >= 1`.
    fn ensure_nonce_at_least_one(&mut self, addr: Address) -> eyre::Result<()> {
        let nonce = self
            .internals
            .load_account_code(addr)
            .map_err(|e| eyre::eyre!("load_account_code({addr}): {e:?}"))?
            .data
            .nonce();
        if nonce == 0 {
            self.internals
                .bump_nonce(addr)
                .map_err(|e| eyre::eyre!("bump_nonce({addr}): {e:?}"))?;
        }
        Ok(())
    }
}

impl StateAdapter for RevmStateAdapter<'_, '_> {
    fn code(&mut self, addr: &Address) -> eyre::Result<Vec<u8>> {
        let load = self
            .internals
            .load_account_code(*addr)
            .map_err(|e| eyre::eyre!("load_account_code({addr}): {e:?}"))?;
        Ok(load
            .data
            .code()
            .map(|c| c.original_byte_slice().to_vec())
            .unwrap_or_default())
    }

    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> eyre::Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::from(code));
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code({addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn tombstone_code(&mut self, addr: &Address) -> eyre::Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::new());
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code (tombstone, {addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> eyre::Result<B256> {
        let key = U256::from_be_bytes(slot.0);
        let load = self
            .internals
            .sload(*addr, key)
            .map_err(|e| eyre::eyre!("sload({addr}, {slot}): {e:?}"))?;
        Ok(B256::from(load.data.to_be_bytes()))
    }

    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> eyre::Result<()> {
        let key = U256::from_be_bytes(slot.0);
        let val = U256::from_be_bytes(value.0);
        self.internals
            .sstore(*addr, key, val)
            .map_err(|e| eyre::eyre!("sstore({addr}, {slot}): {e:?}"))?;
        Ok(())
    }

    fn ensure_account_persists(&mut self, addr: &Address) -> eyre::Result<()> {
        self.ensure_nonce_at_least_one(*addr)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address as Addr;

    // End-to-end dispatch lives in the e2e crate — `EvmInternals`
    // can't be constructed standalone.

    #[test]
    fn op_type_constants_match_contract() {
        assert_eq!(OP_CREATE, 1);
        assert_eq!(OP_UPDATE, 2);
        assert_eq!(OP_EXTEND, 3);
        assert_eq!(OP_TRANSFER, 4);
        assert_eq!(OP_DELETE, 5);
        assert_eq!(OP_EXPIRE, 6);
    }

    #[test]
    fn attr_type_constants_match_contract() {
        assert_eq!(ATTR_UINT, 1);
        assert_eq!(ATTR_STRING, 2);
        assert_eq!(ATTR_ENTITY_KEY, 3);
    }

    #[test]
    fn arkiv_precompile_constructs() {
        let _ = arkiv_precompile();
    }

    #[test]
    fn operation_decodes_through_execute_call() {
        let op = Operation {
            operationType: OP_CREATE,
            entityKey: B256::ZERO,
            payload: Bytes::from_static(&[1, 2, 3]),
            contentType: Mime128 { data: [FixedBytes::ZERO; 4] },
            attributes: vec![],
            btl: 100,
            newOwner: Addr::ZERO,
        };
        let calldata = executeCall { ops: vec![op] }.abi_encode();
        // First 4 bytes are the selector, then the encoded args.
        assert_eq!(&calldata[..4], &executeCall::SELECTOR);
        let decoded = executeCall::abi_decode_raw(&calldata[4..]).expect("decode");
        assert_eq!(decoded.ops.len(), 1);
        assert_eq!(decoded.ops[0].operationType, OP_CREATE);
        assert_eq!(decoded.ops[0].btl, 100);
    }

    #[test]
    fn nonces_selector_round_trips() {
        let call = noncesCall { owner: Addr::repeat_byte(0xaa) };
        let calldata = call.abi_encode();
        assert_eq!(&calldata[..4], &noncesCall::SELECTOR);
        let decoded = noncesCall::abi_decode_raw(&calldata[4..]).expect("decode");
        assert_eq!(decoded.owner, Addr::repeat_byte(0xaa));
        // Return shape: a single uint32 word.
        let ret = noncesCall::abi_encode_returns(&42u32);
        assert_eq!(ret.len(), 32);
        assert_eq!(ret[28..], [0, 0, 0, 42]);
    }

    #[test]
    fn entity_operation_topic0_matches_v1_signature() {
        // keccak256("EntityOperation(bytes32,uint8,address,uint32,bytes32)")
        let expected = keccak256(b"EntityOperation(bytes32,uint8,address,uint32,bytes32)");
        assert_eq!(EntityOperation::SIGNATURE_HASH, expected);
    }

    #[test]
    fn pack_bytes32_4_strips_trailing_zeros() {
        let mut w0 = [0u8; 32];
        w0[..10].copy_from_slice(b"text/plain");
        let m = Mime128 {
            data: [FixedBytes::from(w0), FixedBytes::ZERO, FixedBytes::ZERO, FixedBytes::ZERO],
        };
        assert_eq!(mime128_to_bytes(&m), b"text/plain".to_vec());
    }

    #[test]
    fn ident32_strips_trailing_zeros() {
        let mut buf = [0u8; 32];
        buf[..3].copy_from_slice(b"tag");
        assert_eq!(ident32_to_bytes(B256::from(buf)), b"tag".to_vec());
    }

    fn ident_b256(s: &[u8]) -> B256 {
        let mut buf = [0u8; 32];
        buf[..s.len()].copy_from_slice(s);
        B256::from(buf)
    }

    #[test]
    fn validate_ident32_accepts_valid_names() {
        for name in [
            b"a".as_slice(),
            b"abc".as_slice(),
            b"a1".as_slice(),
            b"a_b-c.d".as_slice(),
            b"abcdefghijklmnopqrstuvwxyz012345".as_slice(), // exactly 32 bytes
        ] {
            validate_ident32(ident_b256(name)).expect(&format!("{name:?}"));
        }
    }

    #[test]
    fn validate_ident32_rejects_empty() {
        let raw = B256::ZERO;
        let err = validate_ident32(raw).unwrap_err();
        match err {
            ApplyError::Revert(bytes) => {
                assert_eq!(&bytes[..4], &Ident32Empty::SELECTOR);
            }
            ApplyError::Fatal(_) => panic!("expected revert"),
        }
    }

    #[test]
    fn validate_ident32_rejects_uppercase_leading() {
        let err = validate_ident32(ident_b256(b"A")).unwrap_err();
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &Ident32InvalidByte::SELECTOR);
        let decoded = Ident32InvalidByte::abi_decode_raw(&bytes[4..]).expect("decode");
        assert_eq!(decoded.position, U256::ZERO);
        assert_eq!(decoded.value, FixedBytes::<1>::from([b'A']));
    }

    #[test]
    fn validate_ident32_rejects_embedded_null() {
        // "ab\0c" — null in the middle, then nonzero
        let mut raw = [0u8; 32];
        raw[..4].copy_from_slice(b"ab\0c");
        let err = validate_ident32(B256::from(raw)).unwrap_err();
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        let decoded = Ident32InvalidByte::abi_decode_raw(&bytes[4..]).expect("decode");
        assert_eq!(decoded.position, U256::from(3u32));
        assert_eq!(decoded.value, FixedBytes::<1>::from([b'c']));
    }

    #[test]
    fn derive_entity_key_matches_sdk_formula() {
        let owner = Addr::repeat_byte(0xab);
        let key = derive_entity_key(1234, owner, 5);
        let mut buf = Vec::with_capacity(32 + 20 + 20 + 4);
        buf.extend_from_slice(&U256::from(1234u64).to_be_bytes::<32>());
        buf.extend_from_slice(ARKIV_ADDRESS.as_slice());
        buf.extend_from_slice(owner.as_slice());
        buf.extend_from_slice(&5u32.to_be_bytes());
        assert_eq!(key, keccak256(&buf));
    }

    #[test]
    fn convert_uint_attribute_packs_be_word() {
        let mut val = [0u8; 32];
        val[31] = 42;
        let attrs = vec![Attribute {
            name: ident_b256(b"score"),
            valueType: ATTR_UINT,
            value: [
                FixedBytes::from(val),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let (strings, numerics) = convert_attributes(&attrs).expect("convert");
        assert!(strings.is_empty());
        assert_eq!(numerics.len(), 1);
        assert_eq!(numerics[0].key, b"score".to_vec());
        assert_eq!(numerics[0].value, U256::from(42));
    }

    #[test]
    fn op_gas_charges_per_op_correctly() {
        let mk = |op_type| Operation {
            operationType: op_type,
            entityKey: B256::ZERO,
            payload: Bytes::new(),
            contentType: Mime128 { data: [FixedBytes::ZERO; 4] },
            attributes: vec![],
            btl: 0,
            newOwner: Addr::ZERO,
        };
        assert_eq!(op_gas(&mk(OP_CREATE)), G_CREATE);
        assert_eq!(op_gas(&mk(OP_UPDATE)), G_UPDATE);
        assert_eq!(op_gas(&mk(OP_EXTEND)), G_EXTEND);
        assert_eq!(op_gas(&mk(OP_TRANSFER)), G_TRANSFER);
        assert_eq!(op_gas(&mk(OP_DELETE)), G_DELETE);
        assert_eq!(op_gas(&mk(OP_EXPIRE)), G_EXPIRE);
    }
}
