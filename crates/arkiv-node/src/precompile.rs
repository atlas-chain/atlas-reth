//! Arkiv precompile — the single entry point Arkiv exposes to the EVM.
//!
//! Sits at [`ARKIV_ADDRESS`] (`0x44…0044`), the address EOAs and SDKs
//! `CALL` for `execute(Operation[])` / `nonces(address)`. The
//! precompile decodes the calldata directly, runs ownership /
//! expiration / charset validation, mints entity keys, emits
//! `EntityOperation` events, and dispatches to the [`arkiv_entitydb`]
//! state handlers.
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
//!    so SDK error decoders resolve them.
//! 4. State mutation via [`arkiv_entitydb`]'s op handlers, threaded
//!    through a [`ReadWriteStateAdapter`](crate::state_adapter::ReadWriteStateAdapter)
//!    over revm's `EvmInternals`.
//! 5. Log emission (`EntityOperation`) — addressed at `ARKIV_ADDRESS`
//!    so the SDK's `eth_getLogs` filter on that address resolves
//!    every event.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{
    Address, B256, Bytes, FixedBytes, Log, Signature, U256, eip191_hash_message, hex, keccak256,
};
use alloy_sol_types::{SolCall, SolError, SolEvent, sol};
use arkiv_entitydb::{
    ARKIV_ADDRESS, ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, Attribute as EntityAttribute,
    StateAdapter,
};
use revm::precompile::{
    PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
};
use serde::{Deserialize, Serialize};

use crate::state_adapter::ReadWriteStateAdapter;

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
        bytes32 entityHash       // always bytes32(0); reserved for future use
    );

    // ── Error selectors ──────────────────────────────────────────────
    //
    // Returned as Solidity-style revert payloads so SDK error decoders
    // resolve them.

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
    error AttributeValueMalformed(bytes32 name, uint8 valueType, uint256 wordIndex);
    error AttributeStringInvalidByte(bytes32 name, uint256 position, bytes1 value);
    error PayloadReferenceMalformed();
    error PayloadReferenceUnsupportedVersion(uint256 version);
    error PayloadProviderUnknown(string provider);
    error PayloadProviderSignerNotAllowed(address signer);
    error PayloadProviderReceiptMismatch();
    error PayloadProviderSignatureInvalid();
    error PayloadReferenceContentTypeInvalid(bytes contentType);
}

// Op-type tags. Must match `Entity.{CREATE..EXPIRE}` in
// EntityRegistry.sol. Attribute `valueType` tags (`ATTR_*`) live in
// `arkiv_entitydb` since they're part of the on-disk shape.
const OP_CREATE: u8 = 1;
const OP_UPDATE: u8 = 2;
const OP_EXTEND: u8 = 3;
const OP_TRANSFER: u8 = 4;
const OP_DELETE: u8 = 5;
const OP_EXPIRE: u8 = 6;

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
// JSON receipt parsing + EIP-191 message hash + secp256k1 recovery for
// reference-backed CREATE / UPDATE payloads. The branch is selected
// from calldata content type, never state or network I/O.
const G_PAYLOAD_REFERENCE_VERIFY: u64 = 50_000;

const PRECOMPILE_NAME: &str = "ARKIV";

const PAYLOAD_REFERENCE_CONTENT_TYPE: &[u8] = b"application/vnd.atlas.payload-reference+json";
const PAYLOAD_REFERENCE_KIND: &str = "atlas.payloadReference";
const PAYLOAD_REFERENCE_VERSION: u64 = 1;
const PAYLOAD_PROVIDER_SERVICE: &str = "atlas-payload-provider";
const PAYLOAD_PROVIDER_RECEIPT_ACTION: &str = "payloadReceived";
const MAX_PAYLOAD_PROVIDER_NAMESPACE_BYTES: usize = 64;
const MAX_PAYLOAD_PROVIDER_CONTENT_TYPE_BYTES: usize = 128;
const DEV_CHAIN_ID: u64 = 1337;

const TRUSTED_PAYLOAD_PROVIDER_SIGNERS: [Address; 1] = [Address::new([
    0xbd, 0xd2, 0x3f, 0xd1, 0xba, 0xb3, 0xf4, 0x07, 0x5e, 0xde, 0xf4, 0x73, 0x8d, 0x1d, 0x78, 0xa6,
    0xbc, 0x5c, 0x23, 0x6c,
])];

// Dev-chain-only signer for local integration tests. Private key:
// 0x0000000000000000000000000000000000000000000000000000000000000001.
const TRUSTED_DEV_PAYLOAD_PROVIDER_SIGNERS: [Address; 1] = [Address::new([
    0x7e, 0x5f, 0x45, 0x52, 0x09, 0x1a, 0x69, 0x12, 0x5d, 0x5d, 0xfc, 0xb7, 0xb8, 0xc2, 0x65, 0x90,
    0x29, 0x39, 0x5b, 0xdf,
])];

pub fn arkiv_precompile() -> DynPrecompile {
    let id = PrecompileId::custom(PRECOMPILE_NAME);
    let call = move |mut input: PrecompileInput<'_>| -> PrecompileResult {
        let _call_span = tracing::debug_span!("precompile_call").entered();

        // Reject DELEGATECALL / CALLCODE. Defensive: the precompile
        // does not currently mutate state at its own address, but if
        // it ever does, delegated semantics (caller's storage) would
        // silently corrupt unrelated accounts.
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
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::OutOfGas,
            input.reservoir,
        ));
    }
    let decoded = match noncesCall::abi_decode_raw(body) {
        Ok(d) => d,
        Err(e) => {
            return Ok(revert(
                format!("arkiv precompile: invalid nonces calldata: {e}")
                    .into_bytes()
                    .into(),
                input.reservoir,
            ));
        }
    };
    let nonce = {
        let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
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
                format!("arkiv precompile: invalid execute calldata: {e}")
                    .into_bytes()
                    .into(),
                input.reservoir,
            ));
        }
    };
    if ops.is_empty() {
        return Ok(revert(EmptyBatch {}.abi_encode().into(), input.reservoir));
    }

    let gas_used = total_gas(&ops);
    if gas_used > input.gas {
        return Ok(PrecompileOutput::halt(
            PrecompileHalt::OutOfGas,
            input.reservoir,
        ));
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

    Ok(PrecompileOutput::new(
        gas_used,
        Bytes::new(),
        input.reservoir,
    ))
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
        OP_UPDATE => apply_update(input, caller, chain_id, current_block, op),
        OP_EXTEND => apply_extend(input, caller, current_block, op),
        OP_TRANSFER => apply_transfer(input, caller, current_block, op),
        OP_DELETE => apply_delete(input, caller, current_block, op),
        OP_EXPIRE => apply_expire(input, current_block, op),
        t => Err(ApplyError::Revert(
            InvalidOpType { operationType: t }.abi_encode().into(),
        )),
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
    let content_type = mime128_to_bytes(&op.contentType);
    validate_payload_reference_if_needed(&op.payload, &content_type, chain_id)?;

    let expires_at = current_block.saturating_add(op.btl as u64);
    let attributes = convert_attributes(&op.attributes)?;
    let entity_key = {
        let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
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
            content_type,
            attributes,
        )?;
        entity_key
    };
    emit_entity_op(input, entity_key, OP_CREATE, caller, expires_at);
    Ok(())
}

fn apply_update(
    input: &mut PrecompileInput<'_>,
    caller: Address,
    chain_id: u64,
    current_block: u64,
    op: &Operation,
) -> Result<(), ApplyError> {
    validate_attribute_names(&op.attributes)?;
    let content_type = mime128_to_bytes(&op.contentType);
    validate_payload_reference_if_needed(&op.payload, &content_type, chain_id)?;
    let entity = load_entity_for_owner(input, caller, current_block, op.entityKey, false)?;
    let attributes = convert_attributes(&op.attributes)?;
    {
        let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
        arkiv_entitydb::update(
            &mut adapter,
            op.entityKey,
            current_block,
            op.payload.to_vec(),
            content_type,
            attributes,
        )?;
    }
    emit_entity_op(
        input,
        op.entityKey,
        OP_UPDATE,
        entity.owner,
        entity.expires_at,
    );
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
        let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
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
            TransferToZeroAddress {
                entityKey: op.entityKey,
            }
            .abi_encode()
            .into(),
        ));
    }
    if op.newOwner == entity.owner {
        return Err(ApplyError::Revert(
            TransferToSelf {
                entityKey: op.entityKey,
            }
            .abi_encode()
            .into(),
        ));
    }
    {
        let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
        arkiv_entitydb::transfer(&mut adapter, op.entityKey, current_block, op.newOwner)?;
    }
    emit_entity_op(
        input,
        op.entityKey,
        OP_TRANSFER,
        op.newOwner,
        entity.expires_at,
    );
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
        let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
        arkiv_entitydb::delete(&mut adapter, op.entityKey)?;
    }
    emit_entity_op(
        input,
        op.entityKey,
        OP_DELETE,
        entity.owner,
        entity.expires_at,
    );
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
    let entity = load_entity(input, op.entityKey)?.ok_or_else(|| not_found_revert(op.entityKey))?;
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
        let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
        arkiv_entitydb::expire(&mut adapter, op.entityKey)?;
    }
    emit_entity_op(
        input,
        op.entityKey,
        OP_EXPIRE,
        entity.owner,
        entity.expires_at,
    );
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
    if bytes[0] == 0 {
        return Err(ApplyError::Revert(Ident32Empty {}.abi_encode().into()));
    }
    let mut seen_zero = false;
    for (position, &b) in bytes.iter().enumerate() {
        if b == 0 {
            seen_zero = true;
        } else {
            let charset = if position == 0 {
                IDENT_LEADING
            } else {
                IDENT_CHARSET
            };
            let charset_bad = (b as u128) > 127 || (charset >> b) & 1 == 0;
            if seen_zero || charset_bad {
                return Err(ApplyError::Revert(
                    Ident32InvalidByte {
                        position: U256::from(position),
                        value: FixedBytes::<1>::from([b]),
                    }
                    .abi_encode()
                    .into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_attribute_names(attrs: &[Attribute]) -> Result<(), ApplyError> {
    for a in attrs {
        validate_ident32(a.name)?;
    }
    Ok(())
}

// ─── Payload-provider references ────────────────────────────────────

/// V1 detached payload reference encoded into `Operation.payload`
/// whenever `Operation.contentType` is
/// `application/vnd.atlas.payload-reference+json`.
///
/// The signed receipt proves that a trusted provider accepted payload
/// bytes identified by `(namespace, payloadId, checksum, sizeBytes)`.
/// It deliberately does not prove the rest of the Arkiv operation
/// intent; see `NEW-CONTRACT.md` for the next signing-scheme step.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadReferenceJson {
    kind: String,
    version: u64,
    provider: String,
    id: String,
    namespace: String,
    #[serde(default)]
    content_type: Option<String>,
    checksum: String,
    size_bytes: u64,
    submitted_at: String,
    signature: PayloadSignatureJson,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PayloadSignatureJson {
    scheme: String,
    signer: String,
    #[serde(alias = "claim")]
    receipt: PayloadReceiptJson,
    #[serde(rename = "messageHash")]
    message_hash: String,
    signature: String,
    r: String,
    s: String,
    v: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PayloadReceiptJson {
    service: String,
    action: String,
    #[serde(rename = "payloadId")]
    payload_id: String,
    namespace: String,
    checksum: String,
    #[serde(rename = "sizeBytes")]
    size_bytes: u64,
    #[serde(rename = "submittedAt")]
    submitted_at: String,
}

fn validate_payload_reference_if_needed(
    payload: &[u8],
    content_type: &[u8],
    chain_id: u64,
) -> Result<(), ApplyError> {
    if content_type == PAYLOAD_REFERENCE_CONTENT_TYPE {
        validate_payload_reference(payload, chain_id)?;
    }
    Ok(())
}

fn validate_payload_reference(payload: &[u8], chain_id: u64) -> Result<(), ApplyError> {
    let reference: PayloadReferenceJson =
        serde_json::from_slice(payload).map_err(|_| payload_reference_malformed())?;

    if reference.kind != PAYLOAD_REFERENCE_KIND {
        return Err(payload_reference_malformed());
    }
    if reference.version != PAYLOAD_REFERENCE_VERSION {
        return Err(ApplyError::Revert(
            PayloadReferenceUnsupportedVersion {
                version: U256::from(reference.version),
            }
            .abi_encode()
            .into(),
        ));
    }
    if reference.provider != PAYLOAD_PROVIDER_SERVICE {
        return Err(payload_provider_unknown(reference.provider));
    }

    validate_payload_id(&reference.id)?;
    validate_payload_namespace(&reference.namespace)?;
    validate_payload_content_type(reference.content_type.as_deref())?;
    validate_payload_checksum(&reference.checksum)?;
    validate_payload_size(reference.size_bytes)?;
    validate_submitted_at(&reference.submitted_at)?;

    let expected_receipt = PayloadReceiptJson {
        service: PAYLOAD_PROVIDER_SERVICE.to_string(),
        action: PAYLOAD_PROVIDER_RECEIPT_ACTION.to_string(),
        payload_id: reference.id,
        namespace: reference.namespace,
        checksum: reference.checksum,
        size_bytes: reference.size_bytes,
        submitted_at: reference.submitted_at,
    };
    validate_payload_signature(&expected_receipt, &reference.signature, chain_id)
}

fn validate_payload_signature(
    expected_receipt: &PayloadReceiptJson,
    signature: &PayloadSignatureJson,
    chain_id: u64,
) -> Result<(), ApplyError> {
    if signature.scheme != "eip191" {
        return Err(payload_provider_signature_invalid());
    }
    if signature.receipt != *expected_receipt {
        return Err(ApplyError::Revert(
            PayloadProviderReceiptMismatch {}.abi_encode().into(),
        ));
    }
    if signature.v != 27 && signature.v != 28 {
        return Err(payload_provider_signature_invalid());
    }

    let r = decode_prefixed_hex_32(&signature.r)?;
    let s = decode_prefixed_hex_32(&signature.s)?;
    let packed = decode_prefixed_hex_exact(&signature.signature, 65)?;
    if packed[..32] != r || packed[32..64] != s || packed[64] != signature.v {
        return Err(payload_provider_signature_invalid());
    }

    let receipt_json =
        serde_json::to_vec(expected_receipt).expect("payload receipt serializes to JSON");
    let message_hash = eip191_hash_message(receipt_json);
    let expected_hash = hex::encode_prefixed(message_hash.as_slice());
    if signature.message_hash != expected_hash {
        return Err(payload_provider_signature_invalid());
    }

    let declared_signer = parse_payload_provider_signer(&signature.signer)?;
    let alloy_sig =
        Signature::from_scalars_and_parity(B256::from(r), B256::from(s), signature.v == 28);
    let recovered = alloy_sig
        .recover_address_from_prehash(&message_hash)
        .map_err(|_| payload_provider_signature_invalid())?;
    if recovered != declared_signer {
        return Err(payload_provider_signature_invalid());
    }
    if !is_trusted_payload_provider_signer(declared_signer, chain_id) {
        return Err(ApplyError::Revert(
            PayloadProviderSignerNotAllowed {
                signer: declared_signer,
            }
            .abi_encode()
            .into(),
        ));
    }

    Ok(())
}

fn parse_payload_provider_signer(value: &str) -> Result<Address, ApplyError> {
    value
        .parse()
        .map_err(|_| payload_provider_signature_invalid())
}

fn is_trusted_payload_provider_signer(signer: Address, chain_id: u64) -> bool {
    TRUSTED_PAYLOAD_PROVIDER_SIGNERS.contains(&signer)
        || (chain_id == DEV_CHAIN_ID && TRUSTED_DEV_PAYLOAD_PROVIDER_SIGNERS.contains(&signer))
}

fn decode_prefixed_hex_32(value: &str) -> Result<[u8; 32], ApplyError> {
    let bytes = decode_prefixed_hex_exact(value, 32)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn decode_prefixed_hex_exact(value: &str, expected_bytes: usize) -> Result<Vec<u8>, ApplyError> {
    let Some(hex_body) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    else {
        return Err(payload_provider_signature_invalid());
    };
    if hex_body.len() != expected_bytes * 2 {
        return Err(payload_provider_signature_invalid());
    }
    let mut out = vec![0u8; expected_bytes];
    hex::decode_to_slice(hex_body, &mut out).map_err(|_| payload_provider_signature_invalid())?;
    Ok(out)
}

fn validate_payload_id(value: &str) -> Result<(), ApplyError> {
    if value.len() == 64 && value.bytes().all(is_lower_hex) {
        Ok(())
    } else {
        Err(payload_reference_malformed())
    }
}

fn validate_payload_checksum(value: &str) -> Result<(), ApplyError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(payload_reference_malformed());
    };
    if hex.len() == 64 && hex.bytes().all(is_lower_hex) {
        Ok(())
    } else {
        Err(payload_reference_malformed())
    }
}

fn validate_payload_namespace(value: &str) -> Result<(), ApplyError> {
    if value.is_empty() || value.len() > MAX_PAYLOAD_PROVIDER_NAMESPACE_BYTES {
        return Err(payload_reference_malformed());
    }
    if value
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(payload_reference_malformed())
    }
}

fn validate_payload_content_type(value: Option<&str>) -> Result<(), ApplyError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty() || value.len() > MAX_PAYLOAD_PROVIDER_CONTENT_TYPE_BYTES {
        return Err(payload_reference_malformed());
    }
    if value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        Ok(())
    } else {
        Err(payload_reference_malformed())
    }
}

fn validate_payload_size(size_bytes: u64) -> Result<(), ApplyError> {
    if size_bytes > 0 {
        Ok(())
    } else {
        Err(payload_reference_malformed())
    }
}

fn validate_submitted_at(value: &str) -> Result<(), ApplyError> {
    if value.is_empty() || value.len() > 64 || !value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(payload_reference_malformed());
    }
    Ok(())
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'a'..=b'f')
}

fn payload_reference_malformed() -> ApplyError {
    ApplyError::Revert(PayloadReferenceMalformed {}.abi_encode().into())
}

fn payload_provider_unknown(provider: String) -> ApplyError {
    ApplyError::Revert(PayloadProviderUnknown { provider }.abi_encode().into())
}

fn payload_provider_signature_invalid() -> ApplyError {
    ApplyError::Revert(PayloadProviderSignatureInvalid {}.abi_encode().into())
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
    let mut adapter = ReadWriteStateAdapter::new(&mut input.internals);
    let code = adapter.code(&entity_addr)?;
    if code.is_empty() {
        return Ok(None);
    }
    let rlp = arkiv_entitydb::EntityRlp::decode_from_code(&code)
        .map_err(|e| ApplyError::Fatal(format!("decode entity {entity_addr}: {e}")))?;
    Ok(Some(ExistingEntity {
        owner: rlp.owner,
        expires_at: rlp.expires_at,
    }))
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
    let entity = load_entity(input, entity_key)?.ok_or_else(|| not_found_revert(entity_key))?;
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
            NotOwner {
                entityKey: entity_key,
                caller,
                owner: entity.owner,
            }
            .abi_encode()
            .into(),
        ));
    }
    Ok(entity)
}

fn not_found_revert(entity_key: B256) -> ApplyError {
    ApplyError::Revert(
        EntityNotFound {
            entityKey: entity_key,
        }
        .abi_encode()
        .into(),
    )
}

fn empty_calldata_revert() -> Bytes {
    b"arkiv precompile: calldata too short for selector"
        .to_vec()
        .into()
}

fn unknown_selector_revert(selector: [u8; 4]) -> Bytes {
    format!(
        "arkiv precompile: unknown selector 0x{}",
        alloy_primitives::hex::encode(selector)
    )
    .into_bytes()
    .into()
}

fn revert(data: Bytes, reservoir: u64) -> PrecompileOutput {
    PrecompileOutput::revert(0, data, reservoir)
}

// ─── Entity-key derivation ───────────────────────────────────────────

/// `keccak256(abi.encodePacked(chainId, ARKIV_ADDRESS, owner, nonce))` —
/// matches the SDK's local key derivation
/// ([arkiv-sdk-js/src/utils/arkivTransactions.ts](arkiv-sdk-js/src/utils/arkivTransactions.ts#L116)),
/// so clients that hold the current `nonces[caller]` can predict the
/// entity key before submitting the tx.
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
    input.internals.log(Log {
        address: ARKIV_ADDRESS,
        data: event.encode_log_data(),
    });
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

    let reference_verify = if mime128_to_bytes(&op.contentType) == PAYLOAD_REFERENCE_CONTENT_TYPE {
        G_PAYLOAD_REFERENCE_VERIFY
    } else {
        0
    };

    base.saturating_add(payload_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_count.saturating_mul(G_ANNOTATION))
        .saturating_add(indexed_count.saturating_mul(G_ART_INDEXED_ANNOTATION))
        .saturating_add(reference_verify)
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

// `pack_bytes32_4` strips trailing zeros to recover the string length
// from the 128-byte buffer, so a non-zero byte after a zero would make
// `"x"` and `"x\0…nonzero"` round-trip differently. Same shape as
// `validate_ident32`'s null check, scaled to 128 bytes.
fn reject_embedded_null_in_string(a: &Attribute) -> Result<(), ApplyError> {
    let mut seen_zero = false;
    for (position, b) in a.value.iter().flat_map(|w| w.as_slice().iter()).enumerate() {
        if *b == 0 {
            seen_zero = true;
        } else if seen_zero {
            return Err(ApplyError::Revert(
                AttributeStringInvalidByte {
                    name: a.name,
                    position: U256::from(position),
                    value: FixedBytes::<1>::from([*b]),
                }
                .abi_encode()
                .into(),
            ));
        }
    }
    Ok(())
}

// `ATTR_STRING` is UTF-8 by SDK convention; entity_db documents
// "UTF-8 by SDK convention" but stores bytes verbatim. Since the
// precompile is the only validation boundary, enforce UTF-8 here so
// downstream consumers (query engine, RPC JSON serialization) don't
// silently mishandle non-UTF-8 bytes. Assumes
// `reject_embedded_null_in_string` already ran, so the content is
// `pack_bytes32_4`'s output (prefix before the first zero).
fn reject_invalid_utf8_in_string(a: &Attribute) -> Result<(), ApplyError> {
    let content = pack_bytes32_4(&a.value);
    if let Err(e) = std::str::from_utf8(&content) {
        let position = e.valid_up_to();
        return Err(ApplyError::Revert(
            AttributeStringInvalidByte {
                name: a.name,
                position: U256::from(position),
                value: FixedBytes::<1>::from([content[position]]),
            }
            .abi_encode()
            .into(),
        ));
    }
    Ok(())
}

// Fixed-width value types (UINT, ENTITY_KEY) pack into value[0]; the
// remaining three words must be zero. Anything else is malformed input
// — there's no upstream contract to canonicalize it.
fn reject_non_zero_upper_words(a: &Attribute) -> Result<(), ApplyError> {
    for (i, word) in a.value.iter().enumerate().skip(1) {
        if *word != FixedBytes::ZERO {
            return Err(ApplyError::Revert(
                AttributeValueMalformed {
                    name: a.name,
                    valueType: a.valueType,
                    wordIndex: U256::from(i),
                }
                .abi_encode()
                .into(),
            ));
        }
    }
    Ok(())
}

fn convert_attributes(attrs: &[Attribute]) -> Result<Vec<EntityAttribute>, ApplyError> {
    attrs
        .iter()
        .map(|a| {
            let key = ident32_to_bytes(a.name);
            let value = match a.valueType {
                // SDK packs the uint256 left-aligned into value[0]; the
                // remaining three words are zero. Store the 32 raw
                // big-endian bytes verbatim.
                ATTR_UINT => {
                    reject_non_zero_upper_words(a)?;
                    a.value[0].as_slice().to_vec()
                }
                ATTR_STRING => {
                    reject_embedded_null_in_string(a)?;
                    reject_invalid_utf8_in_string(a)?;
                    pack_bytes32_4(&a.value)
                }
                // 32 raw bytes — no trailing-zero strip (a real key may
                // end in zeros).
                ATTR_ENTITY_KEY => {
                    reject_non_zero_upper_words(a)?;
                    a.value[0].as_slice().to_vec()
                }
                t => {
                    return Err(ApplyError::Fatal(format!(
                        "unknown attribute valueType {t}"
                    )));
                }
            };
            Ok(EntityAttribute {
                key,
                value_type: a.valueType,
                value,
            })
        })
        .collect()
}

/// `u64` block-number to `uint32` for event/error fields. Block numbers
/// only fit u32 by chain assumption; we saturate so a buggy override
/// can't produce nonsense ABI data.
fn clip_u32(n: u64) -> u32 {
    n.min(u32::MAX as u64) as u32
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
            contentType: Mime128 {
                data: [FixedBytes::ZERO; 4],
            },
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
        let call = noncesCall {
            owner: Addr::repeat_byte(0xaa),
        };
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
    fn payload_reference_fixture_verifies() {
        validate_payload_reference(&payload_reference_fixture(), 1).expect("valid fixture");
    }

    #[test]
    fn payload_reference_requires_supported_version() {
        let mut reference = payload_reference_value();
        reference["version"] = serde_json::json!(2);
        let err = validate_payload_reference(reference.to_string().as_bytes(), 1).unwrap_err();
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &PayloadReferenceUnsupportedVersion::SELECTOR);
        let decoded = PayloadReferenceUnsupportedVersion::abi_decode_raw(&bytes[4..])
            .expect("decode unsupported version");
        assert_eq!(decoded.version, U256::from(2));
    }

    #[test]
    fn payload_reference_rejects_receipt_mismatch() {
        let mut reference = payload_reference_value();
        reference["checksum"] = serde_json::json!(
            "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        );
        let err = validate_payload_reference(reference.to_string().as_bytes(), 1).unwrap_err();
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &PayloadProviderReceiptMismatch::SELECTOR);
    }

    #[test]
    fn payload_reference_rejects_tampered_signature_hash() {
        let mut reference = payload_reference_value();
        reference["signature"]["messageHash"] =
            serde_json::json!("0x0000000000000000000000000000000000000000000000000000000000000000");
        let err = validate_payload_reference(reference.to_string().as_bytes(), 1).unwrap_err();
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &PayloadProviderSignatureInvalid::SELECTOR);
    }

    #[test]
    fn payload_reference_content_type_triggers_verification() {
        let err = validate_payload_reference_if_needed(b"{}", PAYLOAD_REFERENCE_CONTENT_TYPE, 1)
            .unwrap_err();
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &PayloadReferenceMalformed::SELECTOR);

        validate_payload_reference_if_needed(b"{}", b"application/json", 1)
            .expect("non-reference MIME keeps inline behavior");
    }

    #[test]
    fn dev_payload_provider_signer_is_dev_chain_only() {
        let dev_signer = TRUSTED_DEV_PAYLOAD_PROVIDER_SIGNERS[0];
        assert!(is_trusted_payload_provider_signer(dev_signer, DEV_CHAIN_ID));
        assert!(!is_trusted_payload_provider_signer(dev_signer, 1));
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
            data: [
                FixedBytes::from(w0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
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

    fn mime(s: &str) -> Mime128 {
        let mut buf = [0u8; 128];
        buf[..s.len()].copy_from_slice(s.as_bytes());
        Mime128 {
            data: [
                FixedBytes::from_slice(&buf[..32]),
                FixedBytes::from_slice(&buf[32..64]),
                FixedBytes::from_slice(&buf[64..96]),
                FixedBytes::from_slice(&buf[96..128]),
            ],
        }
    }

    fn payload_reference_value() -> serde_json::Value {
        serde_json::from_slice(&payload_reference_fixture()).expect("fixture json")
    }

    fn payload_reference_fixture() -> Vec<u8> {
        br#"{"kind":"atlas.payloadReference","version":1,"provider":"atlas-payload-provider","id":"a806b74c6c933e9c0c3cfd7c099c7c6cdbf86bef1a48da310a90bd050c37b4e5","namespace":"atlas.test","contentType":"text/plain","checksum":"sha256:86a4700d6cf4c679fb010312f20e911e86beb1336e5b78ad8b02f1ac6e10c878","sizeBytes":42,"submittedAt":"2026-06-24T15:24:30Z","signature":{"scheme":"eip191","signer":"0xbdd23fd1bab3f4075edef4738d1d78a6bc5c236c","receipt":{"service":"atlas-payload-provider","action":"payloadReceived","payloadId":"a806b74c6c933e9c0c3cfd7c099c7c6cdbf86bef1a48da310a90bd050c37b4e5","namespace":"atlas.test","checksum":"sha256:86a4700d6cf4c679fb010312f20e911e86beb1336e5b78ad8b02f1ac6e10c878","sizeBytes":42,"submittedAt":"2026-06-24T15:24:30Z"},"messageHash":"0x3d89466d4e80c9dfee28158c8802d1750540d670ce2339afb339956718677d1b","signature":"0xddd862a7c78414936141b1279cf05390814534cc67dc9d2cadfd497c557e853b004cb80c547fabd8d6281497c0c4c134b82e34909d91cdfcccfdbc966d0b15051b","r":"0xddd862a7c78414936141b1279cf05390814534cc67dc9d2cadfd497c557e853b","s":"0x004cb80c547fabd8d6281497c0c4c134b82e34909d91cdfcccfdbc966d0b1505","v":27}}"#.to_vec()
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
        let out = convert_attributes(&attrs).expect("convert");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, b"score".to_vec());
        assert_eq!(out[0].value_type, ATTR_UINT);
        assert_eq!(out[0].value, U256::from(42).to_be_bytes::<32>().to_vec());
    }

    #[test]
    fn convert_string_attribute_packs_and_strips_trailing_zeros() {
        // "hello" left-aligned into value[0]; rest of value[0] and
        // value[1..4] are zero. pack_bytes32_4 then strips trailing
        // zeros to recover the original 5 bytes.
        let mut w0 = [0u8; 32];
        w0[..5].copy_from_slice(b"hello");
        let attrs = vec![Attribute {
            name: ident_b256(b"greeting"),
            valueType: ATTR_STRING,
            value: [
                FixedBytes::from(w0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let out = convert_attributes(&attrs).expect("convert");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, b"greeting".to_vec());
        assert_eq!(out[0].value_type, ATTR_STRING);
        assert_eq!(out[0].value, b"hello".to_vec());
    }

    #[test]
    fn convert_entity_key_attribute_preserves_32_raw_bytes() {
        // Use a key whose last byte is non-zero followed by trailing
        // zeros — must NOT be stripped (real entity keys can end in
        // any byte sequence including zeros).
        let mut key_bytes = [0u8; 32];
        key_bytes[0] = 0xab;
        key_bytes[1] = 0xcd;
        // Rest stays zero — proves we don't strip trailing zeros.
        let attrs = vec![Attribute {
            name: ident_b256(b"linkedTo"),
            valueType: ATTR_ENTITY_KEY,
            value: [
                FixedBytes::from(key_bytes),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let out = convert_attributes(&attrs).expect("convert");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key, b"linkedTo".to_vec());
        assert_eq!(out[0].value_type, ATTR_ENTITY_KEY);
        assert_eq!(out[0].value, key_bytes.to_vec());
        assert_eq!(out[0].value.len(), 32);
    }

    #[test]
    fn convert_attributes_rejects_unknown_value_type() {
        let attrs = vec![Attribute {
            name: ident_b256(b"weird"),
            valueType: 99,
            value: [FixedBytes::ZERO; 4],
        }];
        let err = convert_attributes(&attrs).expect_err("should reject");
        match err {
            ApplyError::Fatal(msg) => assert!(msg.contains("99"), "msg was {msg:?}"),
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    #[test]
    fn convert_attributes_rejects_uint_with_non_zero_upper_words() {
        // ATTR_UINT packs the uint256 into value[0]; value[1..4] must
        // be zero. A non-zero byte in any upper word is malformed —
        // since there is no upstream contract to canonicalize input,
        // the precompile must reject it rather than silently dropping
        // the extra bytes.
        let mut val0 = [0u8; 32];
        val0[31] = 42;
        let mut val2 = [0u8; 32];
        val2[0] = 1;
        let name = ident_b256(b"score");
        let attrs = vec![Attribute {
            name,
            valueType: ATTR_UINT,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::from(val2),
                FixedBytes::ZERO,
            ],
        }];
        let err = convert_attributes(&attrs).expect_err("should reject");
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &AttributeValueMalformed::SELECTOR);
        let decoded = AttributeValueMalformed::abi_decode_raw(&bytes[4..]).expect("decode");
        assert_eq!(decoded.name, name);
        assert_eq!(decoded.valueType, ATTR_UINT);
        assert_eq!(decoded.wordIndex, U256::from(2u32));
    }

    #[test]
    fn convert_attributes_rejects_string_with_embedded_null() {
        // `pack_bytes32_4` strips trailing zeros to recover the
        // string length from the 128-byte buffer — so `"a"` and
        // `"a\0\0"` are indistinguishable on the wire. Reject any
        // non-zero byte that appears after a zero byte so the wire
        // format is unambiguous, mirroring `validate_ident32`.
        let mut val0 = [0u8; 32];
        val0[0] = b'a';
        val0[1] = 0;
        val0[2] = b'b';
        let name = ident_b256(b"greeting");
        let attrs = vec![Attribute {
            name,
            valueType: ATTR_STRING,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let err = convert_attributes(&attrs).expect_err("should reject");
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &AttributeStringInvalidByte::SELECTOR);
        let decoded = AttributeStringInvalidByte::abi_decode_raw(&bytes[4..]).expect("decode");
        assert_eq!(decoded.name, name);
        assert_eq!(decoded.position, U256::from(2u32));
        assert_eq!(decoded.value, FixedBytes::<1>::from([b'b']));
    }

    #[test]
    fn convert_attributes_rejects_string_with_invalid_utf8() {
        // "abc" + lone 0xff (never a valid UTF-8 start byte) +
        // trailing zeros. Report position 3 with value 0xff.
        let mut val0 = [0u8; 32];
        val0[0] = b'a';
        val0[1] = b'b';
        val0[2] = b'c';
        val0[3] = 0xff;
        let name = ident_b256(b"greeting");
        let attrs = vec![Attribute {
            name,
            valueType: ATTR_STRING,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let err = convert_attributes(&attrs).expect_err("should reject");
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &AttributeStringInvalidByte::SELECTOR);
        let decoded = AttributeStringInvalidByte::abi_decode_raw(&bytes[4..]).expect("decode");
        assert_eq!(decoded.name, name);
        assert_eq!(decoded.position, U256::from(3u32));
        assert_eq!(decoded.value, FixedBytes::<1>::from([0xffu8]));
    }

    #[test]
    fn convert_attributes_rejects_entity_key_with_non_zero_upper_words() {
        // ATTR_ENTITY_KEY is fixed-width: the 32-byte key lives in
        // value[0] and value[1..4] must be zero. The precompile is the
        // canonicalization boundary — silently dropping bytes from the
        // upper words would let callers smuggle data past it.
        let mut val0 = [0u8; 32];
        val0[0] = 0xab;
        val0[1] = 0xcd;
        let mut val3 = [0u8; 32];
        val3[15] = 0xff;
        let name = ident_b256(b"linkedTo");
        let attrs = vec![Attribute {
            name,
            valueType: ATTR_ENTITY_KEY,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::from(val3),
            ],
        }];
        let err = convert_attributes(&attrs).expect_err("should reject");
        let bytes = match err {
            ApplyError::Revert(b) => b,
            ApplyError::Fatal(_) => panic!("expected revert"),
        };
        assert_eq!(&bytes[..4], &AttributeValueMalformed::SELECTOR);
        let decoded = AttributeValueMalformed::abi_decode_raw(&bytes[4..]).expect("decode");
        assert_eq!(decoded.name, name);
        assert_eq!(decoded.valueType, ATTR_ENTITY_KEY);
        assert_eq!(decoded.wordIndex, U256::from(3u32));
    }

    #[test]
    fn op_gas_charges_per_op_correctly() {
        let mk = |op_type| Operation {
            operationType: op_type,
            entityKey: B256::ZERO,
            payload: Bytes::new(),
            contentType: Mime128 {
                data: [FixedBytes::ZERO; 4],
            },
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

    #[test]
    fn op_gas_charges_payload_reference_verification() {
        let payload = Bytes::from_static(b"{}");
        let op = Operation {
            operationType: OP_CREATE,
            entityKey: B256::ZERO,
            payload: payload.clone(),
            contentType: mime(std::str::from_utf8(PAYLOAD_REFERENCE_CONTENT_TYPE).unwrap()),
            attributes: vec![],
            btl: 10,
            newOwner: Addr::ZERO,
        };
        assert_eq!(
            op_gas(&op),
            G_CREATE + (payload.len() as u64 * G_BYTE) + G_PAYLOAD_REFERENCE_VERIFY
        );
    }
}
