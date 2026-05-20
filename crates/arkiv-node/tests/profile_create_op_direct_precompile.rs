//! Direct revm profile of N CREATE ops **bypassing the EntityRegistry
//! contract** — txs target the precompile address directly. Used to
//! isolate the contract's wall-clock contribution: the difference
//! against `profile_create_op_direct` is the wall-clock cost
//! `EntityRegistry.execute` adds on top of the precompile + entitydb.
//!
//! Synthetic shortcut: the precompile rejects calls where
//! `input.caller != ENTITY_REGISTRY_ADDRESS`, so the test stamps
//! `tx_env.caller = ENTITY_REGISTRY_ADDRESS` to satisfy that check.
//! In a "drop the contract" production design that check would be
//! relaxed; the wall-clock work measured here is the same either way.
//!
//! Output: <workspace>/tmp/arkiv.create.precompile.trace.json

mod common;

use std::time::{Duration, Instant};

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, TxKind, U256};
use alloy_sol_types::SolValue;
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use arkiv_node::precompile::ARKIV_PRECOMPILE_ADDRESS;
use eyre::Result;
use op_revm::{OpTransaction, transaction::deposit::DepositTransactionParts};
use revm::DatabaseCommit;
use revm::context::TxEnv;
use revm::context::result::ResultAndState;

use common::{
    Attribute, OpRecord, boot_direct_evm, compute_entity_key, init_tracing, pack_mime,
    print_timing,
};

const N_CREATE: usize = 100;
const OP_CREATE: u8 = 1;
const CHAIN_ID: u64 = 1; // matches CfgEnv::new_with_spec default in boot_direct_evm
const BTL: u32 = 10_000;

const TRACE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tmp/arkiv.create.precompile.trace.json"
);

#[test]
fn profile_create_op_direct_precompile() -> Result<()> {
    let (mut evm, sender) = boot_direct_evm()?;
    let txs: Vec<_> = (0..N_CREATE)
        .map(|i| build_direct_precompile_tx(sender, i as u32))
        .collect();

    let _flush_guard = init_tracing(TRACE_PATH)?;

    let mut samples: Vec<Duration> = Vec::with_capacity(N_CREATE);
    for (i, tx) in txs.into_iter().enumerate() {
        let t0 = Instant::now();
        let ResultAndState { result, state } = evm.transact(tx)?;
        samples.push(t0.elapsed());
        if !result.is_success() {
            eyre::bail!("tx #{i} did not succeed: {result:?}");
        }
        evm.db_mut().commit(state);
    }

    print_timing(&mut samples, "direct precompile CREATE");
    eprintln!("==> trace at {TRACE_PATH}");

    Ok(())
}

/// Build a tx whose `to` is the precompile address and whose calldata
/// is an ABI-encoded `Vec<OpRecord>` — the exact shape the precompile
/// decodes. Bypasses the EntityRegistry contract entirely.
///
/// `caller` is set to `ENTITY_REGISTRY_ADDRESS` so the precompile's
/// caller restriction passes synthetically; see file-level docs.
fn build_direct_precompile_tx(sender: Address, idx: u32) -> OpTx {
    let entity_key =
        compute_entity_key(CHAIN_ID, ENTITY_REGISTRY_ADDRESS, sender, idx);

    // The contract would normally compute newExpiresAt = block.number + btl.
    // Default EvmEnv has block.number = 0, so newExpiresAt = BTL.
    let new_expires_at: u32 = BTL;

    let record = OpRecord {
        operationType: OP_CREATE,
        sender,
        entityKey: entity_key,
        newOwner: Address::ZERO,
        newExpiresAt: new_expires_at,
        payload: format!("direct-precompile-{idx}").into_bytes().into(),
        contentType: pack_mime("application/octet-stream"),
        attributes: Vec::<Attribute>::new(),
    };
    let calldata = <Vec<OpRecord> as SolValue>::abi_encode(&vec![record]);

    let tx_env = TxEnv {
        caller: ENTITY_REGISTRY_ADDRESS, // synthetic bypass of precompile's caller check
        gas_limit: 1_500_000,
        gas_price: 0,
        kind: TxKind::Call(ARKIV_PRECOMPILE_ADDRESS),
        value: U256::ZERO,
        data: calldata.into(),
        nonce: idx as u64,
        chain_id: Some(CHAIN_ID),
        ..Default::default()
    };

    OpTx(OpTransaction {
        base: tx_env,
        enveloped_tx: Some(vec![0u8].into()),
        deposit: DepositTransactionParts::default(),
    })
}
