//! Shared helpers for arkiv-node integration tests.
//!
//! Each test in `tests/*.rs` is compiled as a separate binary; whichever
//! ones add `mod common;` get a copy of these helpers. Items that any
//! given test binary doesn't reference would warn as dead code, hence
//! the crate-attribute below.

#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use alloy_evm::{EvmEnv, EvmFactory, revm::inspector::NoOpInspector};
use alloy_primitives::{Address, FixedBytes, U256};
use alloy_sol_types::sol;
use arkiv_genesis::{dev_signers, genesis_alloc};
use arkiv_node::evm::{ArkivEthEvm, ArkivEthEvmFactory};
use eyre::Result;
use revm::bytecode::Bytecode;
use revm::database::{CacheDB, EmptyDB};
use revm::state::AccountInfo;
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{EnvFilter, prelude::*};

/// Concrete type returned by [`boot_direct_evm`]. Spelled out so
/// call sites don't have to thread generic bounds through.
pub type DirectEvm = ArkivEthEvm<CacheDB<EmptyDB>, NoOpInspector>;

// Mirror of the `execute(Operation[])` ABI the precompile decodes
// (see contracts/src/EntityRegistry.sol). Kept here so tests have no
// dep on the e2e crate or on the private sol! block in
// arkiv_node::precompile.
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
/// (via `ArkivEthEvmFactory::create_evm`) and return it alongside the
/// address of dev signer 0 — the same address `world.address(0)`
/// returns in the e2e harness.
///
/// None of this work emits arkiv spans, so callers should invoke this
/// *before* [`init_tracing`] to keep the trace free of setup noise.
pub fn boot_direct_evm() -> Result<(DirectEvm, Address)> {
    boot_direct_evm_with_chain_id(1)
}

pub fn boot_direct_evm_with_chain_id(chain_id: u64) -> Result<(DirectEvm, Address)> {
    boot_direct_evm_with_chain_id_and_base_fee(chain_id, 0)
}

pub fn boot_direct_evm_with_chain_id_and_base_fee(
    chain_id: u64,
    base_fee_per_gas: u64,
) -> Result<(DirectEvm, Address)> {
    // DB seeded from production genesis_alloc: 100 prefunded dev
    // signers. The system account is materialised lazily on the first
    // op, so nothing else needs seeding here.
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

    let factory = ArkivEthEvmFactory::new();
    let mut env: EvmEnv = EvmEnv::default(); // SpecId default with cfg.chain_id = 1
    env.cfg_env.chain_id = chain_id;
    env.block_env.basefee = base_fee_per_gas;
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
        EnvFilter::new("arkiv_entitydb=debug,arkiv_node::evm=debug,arkiv_node::precompile=debug")
    });
    let subscriber = tracing_subscriber::registry()
        .with(chrome_layer)
        .with(filter);
    let _ = tracing::subscriber::set_global_default(subscriber);

    eprintln!("==> tracing-chrome active → {path}");
    Ok(flush_guard)
}

/// Pack `s` (≤128 bytes) into the `bytes32[4]` shape the precompile
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

/// Print median / p95 / p99 of a duration sample, in microseconds. The
/// slice is sorted in place. Used by the profile tests to surface a
/// stable per-tx number alongside the chrome trace.
pub fn print_timing(samples: &mut [Duration], label: &str) {
    samples.sort();
    let n = samples.len();
    let pct = |p: usize| samples[((n * p) / 100).min(n - 1)];
    let us = |d: Duration| d.as_nanos() as f64 / 1_000.0;
    eprintln!(
        "==> {label:<28} N={n:<5}  median={:>7.2}µs  p95={:>7.2}µs  p99={:>7.2}µs",
        us(samples[n / 2]),
        us(pct(95)),
        us(pct(99)),
    );
}
