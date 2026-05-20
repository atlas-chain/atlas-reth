//! Direct revm profile of N CREATE ops via `ArkivOpEvmFactory::create_evm`.
//! Single thread, no tokio, no block production, no RPC.
//!
//! The chrome trace contains only the per-tx workload — setup runs
//! before `init_tracing` is called.
//!
//! Output: <workspace>/tmp/arkiv.create.trace.json
//! (loadable at https://ui.perfetto.dev).

mod common;

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_sol_types::SolCall;
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use eyre::Result;
use op_revm::{OpTransaction, transaction::deposit::DepositTransactionParts};
use revm::context::TxEnv;

use common::{Operation, boot_direct_evm, executeCall, init_tracing, pack_mime};

const N_CREATE: usize = 10;
const OP_CREATE: u8 = 1;

const TRACE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tmp/arkiv.create.trace.json"
);

#[test]
fn profile_create_op_direct() -> Result<()> {
    let (mut evm, sender) = boot_direct_evm()?;
    let txs: Vec<_> = (0..N_CREATE)
        .map(|i| build_create_tx(sender, i as u64))
        .collect();

    // Start recording AFTER setup so the trace captures only the loop.
    let _flush_guard = init_tracing(TRACE_PATH)?;

    // `transact_commit` calls `transact_raw` (emits `evm_tx`) and writes
    // the state diff back, so each iteration sees the previous nonce
    // bump and new entity / pair accounts.
    for (i, tx) in txs.into_iter().enumerate() {
        let result = evm.transact_commit(tx)?;
        if !result.is_success() {
            eyre::bail!("tx #{i} did not succeed: {result:?}");
        }
    }

    eprintln!("==> ran {N_CREATE} direct CREATEs");
    eprintln!("==> trace at {TRACE_PATH}");

    Ok(())
}

/// Caller-as-recovered OpTx for one CREATE targeting EntityRegistry.
/// Skips signature recovery — `caller` is taken as authoritative.
fn build_create_tx(sender: Address, idx: u64) -> OpTx {
    let calldata = executeCall {
        ops: vec![Operation {
            operationType: OP_CREATE,
            entityKey: B256::ZERO,
            payload: format!("direct-{idx}").into_bytes().into(),
            contentType: pack_mime("application/octet-stream"),
            attributes: vec![],
            btl: 10_000,
            newOwner: Address::ZERO,
        }],
    }
    .abi_encode();

    let tx_env = TxEnv {
        caller: sender,
        gas_limit: 1_500_000,
        gas_price: 0,
        kind: TxKind::Call(ENTITY_REGISTRY_ADDRESS),
        value: U256::ZERO,
        data: calldata.into(),
        nonce: idx,
        chain_id: Some(1), // matches CfgEnv::new_with_spec default
        ..Default::default()
    };

    OpTx(OpTransaction {
        base: tx_env,
        enveloped_tx: Some(vec![0u8].into()),
        deposit: DepositTransactionParts::default(),
    })
}
