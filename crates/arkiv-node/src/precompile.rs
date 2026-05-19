//! Arkiv precompile — revm-side adapter over [`arkiv_entitydb`].
//!
//! All indexing logic (system counter, ID maps, bitmap deltas, RLP
//! encode/decode, tombstoning) lives in [`arkiv_entitydb`]. This file
//! owns the caller restrictions, calldata decode, gas accounting, and
//! the [`StateAdapter`] impl over revm's [`EvmInternals`].

use alloy_evm::{EvmInternals, precompiles::{DynPrecompile, PrecompileInput}};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::{SolValue, sol};
use arkiv_entitydb::{NumericAnnotation, StateAdapter, StringAnnotation};
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use revm::{
    precompile::{
        PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
    },
    state::Bytecode,
};

pub use arkiv_genesis::ARKIV_PRECOMPILE_ADDRESS;

// Mirror of `EntityRegistry.OpRecord`. Field order / types / names must
// stay in lockstep with `contracts/src/EntityRegistry.sol`.
sol! {
    #[derive(Debug)]
    struct OpRecord {
        uint8 operationType;
        address sender;
        bytes32 entityKey;
        address newOwner;
        uint32 newExpiresAt;     // BlockNumber32 UDVT in Solidity; uint32 on the wire
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
    }

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

// ── Gas model ────────────────────────────────────────────────────────
//
// Pure function of op shape — required for cross-node consensus on the
// returned `gas_used`. Anchored to EVM costs: SSTORE_INIT ≈ 22,100,
// SSTORE_RESET ≈ 5,000, per code byte ≈ 200.

const G_CREATE: u64 = 80_000;
const G_UPDATE: u64 = 30_000;
const G_EXTEND: u64 = 25_000;
const G_TRANSFER: u64 = 25_000;
const G_DELETE: u64 = 50_000;
const G_EXPIRE: u64 = 50_000;
const G_BYTE: u64 = 16;
const G_ANNOTATION: u64 = 5_000;

const PRECOMPILE_NAME: &str = "ARKIV";

pub fn arkiv_precompile() -> DynPrecompile {
    let id = PrecompileId::custom(PRECOMPILE_NAME);
    let call = move |mut input: PrecompileInput<'_>| -> PrecompileResult {
        let _call_span = tracing::debug_span!("precompile_call").entered();

        // Direct CALL only, from the EntityRegistry predeploy.
        if input.target_address != input.bytecode_address {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: DELEGATECALL/CALLCODE not allowed".into(),
            ));
        }
        if input.is_static {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: STATICCALL not allowed".into(),
            ));
        }
        if input.value != U256::ZERO {
            return Err(PrecompileError::Fatal(
                "arkiv precompile: value-bearing call not allowed".into(),
            ));
        }
        if input.caller != ENTITY_REGISTRY_ADDRESS {
            return Err(PrecompileError::Fatal(format!(
                "arkiv precompile: only EntityRegistry ({}) may call; got {}",
                ENTITY_REGISTRY_ADDRESS, input.caller,
            )));
        }

        let records = {
            let _decode_span = tracing::debug_span!("precompile_decode").entered();
            match <Vec<OpRecord> as SolValue>::abi_decode(input.data) {
                Ok(r) => r,
                Err(e) => {
                    return Err(PrecompileError::Fatal(format!(
                        "arkiv precompile: failed to decode OpRecord[]: {e}"
                    )));
                }
            }
        };

        let gas_used = total_gas(&records);
        if gas_used > input.gas {
            return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, input.reservoir));
        }

        let current_block: u64 = input.internals.block_number().saturating_to();
        let mut adapter = RevmStateAdapter::new(&mut input.internals);

        {
            let _dispatch_span =
                tracing::debug_span!("precompile_dispatch", n_ops = records.len()).entered();
            for (i, rec) in records.into_iter().enumerate() {
                if let Err(e) = dispatch(&mut adapter, current_block, &rec) {
                    return Err(PrecompileError::Fatal(format!(
                        "arkiv precompile: op #{i} ({}) failed: {e}",
                        op_name(rec.operationType),
                    )));
                }
            }
        }

        Ok(PrecompileOutput::new(gas_used, Bytes::new(), input.reservoir))
    };
    DynPrecompile::new_stateful(id, call)
}

fn dispatch<S: StateAdapter>(
    state: &mut S,
    current_block: u64,
    rec: &OpRecord,
) -> eyre::Result<()> {
    match rec.operationType {
        OP_CREATE => {
            let (string_annotations, numeric_annotations) = convert_attributes(&rec.attributes)?;
            arkiv_entitydb::create(
                state,
                rec.sender,
                rec.entityKey,
                rec.newExpiresAt as u64,
                current_block,
                rec.payload.to_vec(),
                mime128_to_bytes(&rec.contentType),
                string_annotations,
                numeric_annotations,
            )
        }
        OP_UPDATE => {
            let (string_annotations, numeric_annotations) = convert_attributes(&rec.attributes)?;
            arkiv_entitydb::update(
                state,
                rec.entityKey,
                current_block,
                rec.payload.to_vec(),
                mime128_to_bytes(&rec.contentType),
                string_annotations,
                numeric_annotations,
            )
        }
        OP_EXTEND => arkiv_entitydb::extend(state, rec.entityKey, current_block, rec.newExpiresAt as u64),
        OP_TRANSFER => arkiv_entitydb::transfer(state, rec.entityKey, current_block, rec.newOwner),
        OP_DELETE => arkiv_entitydb::delete(state, rec.entityKey),
        OP_EXPIRE => arkiv_entitydb::expire(state, rec.entityKey),
        t => Err(eyre::eyre!("unknown operationType {t}")),
    }
}

fn op_name(t: u8) -> &'static str {
    match t {
        OP_CREATE => "CREATE",
        OP_UPDATE => "UPDATE",
        OP_EXTEND => "EXTEND",
        OP_TRANSFER => "TRANSFER",
        OP_DELETE => "DELETE",
        OP_EXPIRE => "EXPIRE",
        _ => "UNKNOWN",
    }
}

fn total_gas(records: &[OpRecord]) -> u64 {
    records
        .iter()
        .map(record_gas)
        .fold(0u64, u64::saturating_add)
}

fn record_gas(rec: &OpRecord) -> u64 {
    let base = match rec.operationType {
        OP_CREATE => G_CREATE,
        OP_UPDATE => G_UPDATE,
        OP_EXTEND => G_EXTEND,
        OP_TRANSFER => G_TRANSFER,
        OP_DELETE => G_DELETE,
        OP_EXPIRE => G_EXPIRE,
        // Unknown op-types still get charged so a malformed batch can't
        // dodge gas; dispatch will fatal anyway.
        _ => G_CREATE,
    };

    // EXTEND / TRANSFER / DELETE / EXPIRE don't read payload or
    // attributes from the record — gas is only the fixed base.
    if !matches!(rec.operationType, OP_CREATE | OP_UPDATE) {
        return base;
    }

    let payload_bytes = rec.payload.len() as u64;
    let annotation_count = rec.attributes.len() as u64;
    // Each annotation's name (≤32 bytes) + value (≤128 bytes) lands in
    // both the entity RLP and a pair-account bitmap.
    let annotation_bytes = annotation_count.saturating_mul(32 + 128);

    base.saturating_add(payload_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_bytes.saturating_mul(G_BYTE))
        .saturating_add(annotation_count.saturating_mul(G_ANNOTATION))
}

/// Concatenate a `bytes32[4]` into 128 bytes, then strip trailing
/// `0x00`. Strings written by the SDK are packed left-aligned and
/// zero-padded on the right; trailing `0x00` is banned in pair
/// keys/values so the strip is unambiguous.
fn pack_bytes32_4(words: &[alloy_primitives::FixedBytes<32>; 4]) -> Vec<u8> {
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
) -> eyre::Result<(Vec<StringAnnotation>, Vec<NumericAnnotation>)> {
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
            t => eyre::bail!("unknown attribute valueType {t}"),
        }
    }
    Ok((strings, numerics))
}

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address as Addr, B256, FixedBytes};

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
    fn record_decodes_minimal_create_batch() {
        let rec = OpRecord {
            operationType: OP_CREATE,
            sender: Addr::repeat_byte(0xaa),
            entityKey: B256::repeat_byte(0xbb),
            newOwner: Addr::repeat_byte(0xaa),
            newExpiresAt: 12345,
            payload: vec![0u8; 8].into(),
            contentType: Mime128 { data: [FixedBytes::ZERO; 4] },
            attributes: vec![],
        };
        let encoded = <Vec<OpRecord> as SolValue>::abi_encode(&vec![rec.clone()]);
        let decoded = <Vec<OpRecord> as SolValue>::abi_decode(&encoded)
            .expect("round-trip decode");
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].operationType, OP_CREATE);
        assert_eq!(decoded[0].entityKey, rec.entityKey);
        assert_eq!(decoded[0].newExpiresAt, 12345);
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

    #[test]
    fn convert_uint_attribute_packs_be_word() {
        let mut name = [0u8; 32];
        name[..5].copy_from_slice(b"score");
        let mut val = [0u8; 32];
        val[31] = 42;
        let attrs = vec![Attribute {
            name: FixedBytes::from(name),
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
    fn convert_string_attribute_strips_zeros() {
        let mut name = [0u8; 32];
        name[..3].copy_from_slice(b"tag");
        let mut val0 = [0u8; 32];
        val0[..5].copy_from_slice(b"music");
        let attrs = vec![Attribute {
            name: FixedBytes::from(name),
            valueType: ATTR_STRING,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let (strings, numerics) = convert_attributes(&attrs).expect("convert");
        assert!(numerics.is_empty());
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].key, b"tag".to_vec());
        assert_eq!(strings[0].value, b"music".to_vec());
    }

    #[test]
    fn convert_entity_key_attribute_keeps_full_32_bytes() {
        let mut name = [0u8; 32];
        name[..3].copy_from_slice(b"ref");
        let val0 = [0x42u8; 32];
        let attrs = vec![Attribute {
            name: FixedBytes::from(name),
            valueType: ATTR_ENTITY_KEY,
            value: [
                FixedBytes::from(val0),
                FixedBytes::ZERO,
                FixedBytes::ZERO,
                FixedBytes::ZERO,
            ],
        }];
        let (strings, _) = convert_attributes(&attrs).expect("convert");
        assert_eq!(strings[0].value, vec![0x42u8; 32]);
    }

    #[test]
    fn record_gas_charges_per_op_correctly() {
        let mk = |op| OpRecord {
            operationType: op,
            sender: Addr::ZERO,
            entityKey: B256::ZERO,
            newOwner: Addr::ZERO,
            newExpiresAt: 0,
            payload: Bytes::new(),
            contentType: Mime128 { data: [FixedBytes::ZERO; 4] },
            attributes: vec![],
        };
        assert_eq!(record_gas(&mk(OP_CREATE)), G_CREATE);
        assert_eq!(record_gas(&mk(OP_UPDATE)), G_UPDATE);
        assert_eq!(record_gas(&mk(OP_EXTEND)), G_EXTEND);
        assert_eq!(record_gas(&mk(OP_TRANSFER)), G_TRANSFER);
        assert_eq!(record_gas(&mk(OP_DELETE)), G_DELETE);
        assert_eq!(record_gas(&mk(OP_EXPIRE)), G_EXPIRE);
    }
}
