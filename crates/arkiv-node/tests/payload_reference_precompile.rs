//! Direct EVM coverage for payload-provider reference validation.

mod common;

use alloy_evm::Evm;
use alloy_primitives::{Address, B256, TxKind, U256};
use alloy_sol_types::{SolCall, SolError, sol};
use arkiv_genesis::ARKIV_ADDRESS;
use eyre::Result;
use revm::DatabaseCommit;
use revm::context::TxEnv;
use revm::context::result::ResultAndState;

use common::{Operation, boot_direct_evm_with_chain_id, executeCall, pack_mime};

const OP_CREATE: u8 = 1;
const PAYLOAD_REFERENCE_CONTENT_TYPE: &str = "application/vnd.atlas.payload-reference+json";

sol! {
    error PayloadReferenceMalformed();
    error PayloadReferenceNonceUsed(bytes32 nonce);
}

#[test]
fn execute_accepts_signed_payload_reference_create() -> Result<()> {
    let (mut evm, sender) = boot_direct_evm_with_chain_id(1337)?;
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
    let (mut evm, sender) = boot_direct_evm_with_chain_id(1337)?;
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

#[test]
fn execute_rejects_replayed_payload_reference_nonce() -> Result<()> {
    let (mut evm, sender) = boot_direct_evm_with_chain_id(1337)?;
    let calldata = executeCall {
        ops: vec![reference_create_op(valid_payload_reference())],
    }
    .abi_encode();

    let ResultAndState { result, state } = evm.transact(call_tx(sender, calldata.clone(), 0))?;
    assert!(
        result.is_success(),
        "first reference CREATE reverted: {result:?}"
    );
    evm.db_mut().commit(state);

    let ResultAndState { result, .. } = evm.transact(call_tx(sender, calldata, 1))?;
    assert!(
        !result.is_success(),
        "replayed reference unexpectedly succeeded"
    );
    let output = result.output().expect("revert output");
    assert_eq!(&output[..4], &PayloadReferenceNonceUsed::SELECTOR);
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
        chain_id: Some(1337),
        ..Default::default()
    }
}

fn valid_payload_reference() -> Vec<u8> {
    br#"{"kind":"atlas.payloadReference","version":1,"provider":"atlas-payload-provider","id":"a806b74c6c933e9c0c3cfd7c099c7c6cdbf86bef1a48da310a90bd050c37b4e5","namespace":"atlas.test","contentType":"text/plain","checksum":"sha256:86a4700d6cf4c679fb010312f20e911e86beb1336e5b78ad8b02f1ac6e10c878","sizeBytes":42,"submittedAt":"2026-06-24T15:24:30Z","nonce":"0x0000000000000000000000000000000000000000000000000000000000000001","payment":100000,"signature":{"scheme":"eip191","signer":"0x7e5f4552091a69125d5dfcb7b8c2659029395bdf","receipt":{"service":"atlas-payload-provider","action":"payloadReceived","payloadId":"a806b74c6c933e9c0c3cfd7c099c7c6cdbf86bef1a48da310a90bd050c37b4e5","namespace":"atlas.test","checksum":"sha256:86a4700d6cf4c679fb010312f20e911e86beb1336e5b78ad8b02f1ac6e10c878","sizeBytes":42,"submittedAt":"2026-06-24T15:24:30Z","nonce":"0x0000000000000000000000000000000000000000000000000000000000000001","payment":100000},"messageHash":"0xc26441853fe5760f4b5621649c8c0a2a7645b81793c3b367eb7f69f936736080","signature":"0x175505ad691cf7c80733ab39c0158d850182176090fc1365e71a13f61b2dadaa66e455ba88196d2a1570c326c3813cbc8e3b417ef79891db2ed934bdb4d687061b","r":"0x175505ad691cf7c80733ab39c0158d850182176090fc1365e71a13f61b2dadaa","s":"0x66e455ba88196d2a1570c326c3813cbc8e3b417ef79891db2ed934bdb4d68706","v":27}}"#.to_vec()
}
