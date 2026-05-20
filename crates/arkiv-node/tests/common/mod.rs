//! Shared helpers for arkiv-node integration tests.
//!
//! Each test in `tests/*.rs` is compiled as a separate binary; whichever
//! ones add `mod common;` get a copy of these helpers. Items that any
//! given test binary doesn't reference would warn as dead code, hence
//! the crate-attribute below.

#![allow(dead_code)]

use std::path::Path;

use alloy_evm::{EvmEnv, EvmFactory, revm::inspector::NoOpInspector};
use alloy_primitives::{Address, FixedBytes, U256};
use alloy_sol_types::sol;
use arkiv_genesis::{dev_signers, genesis_alloc};
use arkiv_node::evm::{ArkivOpEvm, ArkivOpEvmFactory};
use eyre::Result;
use revm::bytecode::Bytecode;
use revm::database::{CacheDB, EmptyDB};
use revm::state::AccountInfo;
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{EnvFilter, prelude::*};

/// Concrete type returned by [`boot_direct_evm`]. Spelled out so
/// call sites don't have to thread generic bounds through.
pub type DirectEvm = ArkivOpEvm<CacheDB<EmptyDB>, NoOpInspector>;

// Mirror of EntityRegistry.execute(Operation[]) — same shape as
// e2e/src/lib.rs uses. Kept here so tests have no dep on the e2e crate.
sol! {
    #[derive(Debug)]
    struct Mime128 { bytes32[4] data; }

    #[derive(Debug)]
    struct Attribute { bytes32 name; uint8 valueType; bytes32[4] value; }

    #[derive(Debug)]
    struct Operation {
        uint8 operationType;
        bytes32 entityKey;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
        uint32 btl;
        address newOwner;
    }

    function execute(Operation[] ops) external;
}

/// Build a fresh CacheDB-backed EVM with the Arkiv precompile installed
/// (via `ArkivOpEvmFactory::create_evm`) and return it alongside the
/// address of dev signer 0 — the same address `world.address(0)`
/// returns in the e2e harness.
///
/// None of this work emits arkiv spans, so callers should invoke this
/// *before* [`init_tracing`] to keep the trace free of setup noise.
pub fn boot_direct_evm() -> Result<(DirectEvm, Address)> {
    // DB seeded from production genesis_alloc: EntityRegistry contract
    // bytecode + system account at nonce=1 + 100 prefunded dev signers.
    let mut db = CacheDB::new(EmptyDB::default());
    for (addr, account) in genesis_alloc()? {
        let info = AccountInfo {
            balance: account.balance,
            nonce: account.nonce.unwrap_or(0),
            code: account.code.clone().map(Bytecode::new_raw),
            ..Default::default()
        };
        db.insert_account_info(addr, info);
        if let Some(storage) = account.storage {
            for (slot, value) in storage {
                let slot = U256::from_be_bytes(slot.0);
                let value = U256::from_be_bytes(value.0);
                db.insert_account_storage(addr, slot, value)?;
            }
        }
    }

    let factory = ArkivOpEvmFactory::new();
    let env = EvmEnv::default(); // OpSpecId default = JOVIAN; cfg.chain_id = 1
    let evm = factory.create_evm(db, env);

    let sender = dev_signers(1)?[0].address();
    Ok((evm, sender))
}

/// Install a tracing-chrome subscriber writing to `path`. Returns the
/// [`FlushGuard`] whose drop flushes and closes the file — keep it
/// alive for the duration of the recorded section.
pub fn init_tracing(path: &str) -> Result<FlushGuard> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let (chrome_layer, flush_guard) = ChromeLayerBuilder::new()
        .file(path)
        .include_args(true)
        .build();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "arkiv_entitydb=debug,arkiv_node::evm=debug,arkiv_node::precompile=debug",
        )
    });
    let subscriber = tracing_subscriber::registry()
        .with(chrome_layer)
        .with(filter);
    let _ = tracing::subscriber::set_global_default(subscriber);

    eprintln!("==> tracing-chrome active → {path}");
    Ok(flush_guard)
}

/// Pack `s` (≤128 bytes) into the `bytes32[4]` shape EntityRegistry
/// expects for content types.
pub fn pack_mime(s: &str) -> Mime128 {
    let mut buf = [0u8; 128];
    let n = s.len().min(128);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    Mime128 {
        data: [
            FixedBytes::from_slice(&buf[..32]),
            FixedBytes::from_slice(&buf[32..64]),
            FixedBytes::from_slice(&buf[64..96]),
            FixedBytes::from_slice(&buf[96..128]),
        ],
    }
}
