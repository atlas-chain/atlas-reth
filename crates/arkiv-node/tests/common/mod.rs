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
use alloy_primitives::{Address, B256, FixedBytes, U256, keccak256};
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

// Mirror of EntityRegistry.execute(Operation[]) and the OpRecord shape
// the precompile decodes. Kept here so tests have no dep on the e2e
// crate or on the private sol! block in arkiv_node::precompile.
sol! {
    #[derive(Debug)]
    struct Mime128 { bytes32[4] data; }

    #[derive(Debug)]
    struct Attribute { bytes32 name; uint8 valueType; bytes32[4] value; }

    // What the SDK / EOA sends to EntityRegistry.execute.
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

    // What the EntityRegistry contract forwards to the precompile.
    // The contract derives `sender`, `entityKey` (for CREATE),
    // `newExpiresAt` (= block.number + btl) before passing through.
    #[derive(Debug)]
    struct OpRecord {
        uint8 operationType;
        address sender;
        bytes32 entityKey;
        address newOwner;
        uint32 newExpiresAt;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
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
    let mut env = EvmEnv::default(); // OpSpecId default = JOVIAN; cfg.chain_id = 1
    // Allow tests to set `tx.caller = ENTITY_REGISTRY_ADDRESS` (a
    // code-bearing account) to synthetically pass the precompile's
    // caller check. EIP-3607 would otherwise reject the tx.
    env.cfg_env.disable_eip3607 = true;
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

/// `keccak256(abi.encodePacked(chainId, registry, sender, nonce))` —
/// the formula EntityRegistry.sol uses to derive entity keys for CREATE.
/// Tests that bypass the contract and call the precompile directly must
/// compute this themselves.
pub fn compute_entity_key(chain_id: u64, registry: Address, sender: Address, nonce: u32) -> B256 {
    let mut buf = Vec::with_capacity(32 + 20 + 20 + 4);
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    buf.extend_from_slice(registry.as_slice());
    buf.extend_from_slice(sender.as_slice());
    buf.extend_from_slice(&nonce.to_be_bytes());
    keccak256(&buf)
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
