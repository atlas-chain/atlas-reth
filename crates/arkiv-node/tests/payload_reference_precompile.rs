//! Direct EVM coverage for payload-provider reference validation.

mod common;

use alloy_evm::Evm;
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_sol_types::{SolCall, SolError, sol};
use arkiv_genesis::ARKIV_ADDRESS;
use eyre::Result;
use revm::context::TxEnv;
use revm::context::result::ResultAndState;

use common::{Operation, boot_direct_evm, executeCall, pack_mime};

const OP_CREATE: u8 = 1;
const PAYLOAD_REFERENCE_CONTENT_TYPE: &str = "application/vnd.atlas.payload-reference+json";

sol! {
    error PayloadReferenceMalformed();
}

#[test]
fn execute_accepts_signed_payload_reference_create() -> Result<()> {
    let (mut evm, sender) = boot_direct_evm()?;
    let calldata = executeCall {
        ops: vec![reference_create_op(valid_payload_reference())],
    }
    .abi_encode();

    let ResultAndState { result, .. } = evm.transact(call_tx(sender, calldata, 0))?;

    assert!(result.is_success(), "reference CREATE reverted: {result:?}");
    assert_eq!(
        result.logs().len(),
        1,
        "CREATE should emit one EntityOperation"
    );
    Ok(())
}

#[test]
fn execute_rejects_malformed_payload_reference_create() -> Result<()> {
    let (mut evm, sender) = boot_direct_evm()?;
    let calldata = executeCall {
        ops: vec![reference_create_op(b"{}".to_vec())],
    }
    .abi_encode();

    let ResultAndState { result, .. } = evm.transact(call_tx(sender, calldata, 0))?;

    assert!(
        !result.is_success(),
        "malformed reference unexpectedly succeeded"
    );
    let output = result.output().expect("revert output");
    assert_eq!(&output[..4], &PayloadReferenceMalformed::SELECTOR);
    Ok(())
}

fn reference_create_op(payload: Vec<u8>) -> Operation {
    Operation {
        operationType: OP_CREATE,
        entityKey: B256::ZERO,
        payload: payload.into(),
        contentType: pack_mime(PAYLOAD_REFERENCE_CONTENT_TYPE),
        attributes: vec![],
        btl: 1_000,
        newOwner: Address::ZERO,
    }
}

fn call_tx(sender: Address, calldata: Vec<u8>, nonce: u64) -> TxEnv {
    TxEnv {
        caller: sender,
        gas_limit: 3_000_000,
        gas_price: 0,
        kind: TxKind::Call(ARKIV_ADDRESS),
        value: U256::ZERO,
        data: calldata.into(),
        nonce,
        chain_id: Some(1),
        ..Default::default()
    }
}

fn valid_payload_reference() -> Vec<u8> {
    br#"{"kind":"atlas.payloadReference","version":1,"provider":"atlas-payload-provider","id":"a806b74c6c933e9c0c3cfd7c099c7c6cdbf86bef1a48da310a90bd050c37b4e5","namespace":"atlas.test","contentType":"text/plain","checksum":"sha256:86a4700d6cf4c679fb010312f20e911e86beb1336e5b78ad8b02f1ac6e10c878","sizeBytes":42,"submittedAt":"2026-06-24T15:24:30Z","signature":{"scheme":"eip191","signer":"0xbdd23fd1bab3f4075edef4738d1d78a6bc5c236c","receipt":{"service":"atlas-payload-provider","action":"payloadReceived","payloadId":"a806b74c6c933e9c0c3cfd7c099c7c6cdbf86bef1a48da310a90bd050c37b4e5","namespace":"atlas.test","checksum":"sha256:86a4700d6cf4c679fb010312f20e911e86beb1336e5b78ad8b02f1ac6e10c878","sizeBytes":42,"submittedAt":"2026-06-24T15:24:30Z"},"messageHash":"0x3d89466d4e80c9dfee28158c8802d1750540d670ce2339afb339956718677d1b","signature":"0xddd862a7c78414936141b1279cf05390814534cc67dc9d2cadfd497c557e853b004cb80c547fabd8d6281497c0c4c134b82e34909d91cdfcccfdbc966d0b15051b","r":"0xddd862a7c78414936141b1279cf05390814534cc67dc9d2cadfd497c557e853b","s":"0x004cb80c547fabd8d6281497c0c4c134b82e34909d91cdfcccfdbc966d0b1505","v":27}}"#.to_vec()
}
