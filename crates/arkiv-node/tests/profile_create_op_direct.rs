//! Direct revm profile of N CREATE ops via `ArkivOpEvmFactory::create_evm`.
//! Single thread, no tokio, no block production, no RPC.
//!
//! The chrome trace contains only the per-tx workload — setup runs
//! before `init_tracing` is called.
//!
//! Output: <workspace>/tmp/arkiv.create.trace.json
//! (loadable at https://ui.perfetto.dev).

mod common;

use std::time::{Duration, Instant};

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_sol_types::SolCall;
use arkiv_genesis::ENTITY_REGISTRY_ADDRESS;
use eyre::Result;
use op_revm::{OpTransaction, transaction::deposit::DepositTransactionParts};
use revm::DatabaseCommit;
use revm::context::TxEnv;
use revm::context::result::ResultAndState;

use common::{Operation, boot_direct_evm, executeCall, init_tracing, pack_mime, print_timing};

const N_CREATE: usize = 100;
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

    // Time each `evm.transact` (the per-tx EVM-internal work — same
    // thing wrapped by the `evm_tx` span). State commit happens
    // separately so it doesn't contaminate the sample.
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

    print_timing(&mut samples, "EntityRegistry CREATE");
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
