//! System-account detection for Arkiv chainspecs.

use reth_optimism_chainspec::OpChainSpec;

/// Returns `true` iff the chainspec's genesis alloc contains the Arkiv
/// system account at the canonical address with `nonce >= 1`.
///
/// The system account is an empty-coded account whose only job is to
/// host the precompile's consensus storage (per-caller nonces, entity
/// counter, ID maps). `nonce = 1` keeps EIP-161 from pruning it before
/// the precompile writes its first slot. The precompile itself is
/// registered via `EvmFactory`, not via on-chain code.
pub fn has_arkiv_system_account(chain: &OpChainSpec) -> bool {
    let Some(account) = chain
        .inner
        .genesis
        .alloc
        .get(&arkiv_genesis::SYSTEM_ACCOUNT_ADDRESS)
    else {
        return false;
    };
    account.nonce.unwrap_or(0) >= 1
}
