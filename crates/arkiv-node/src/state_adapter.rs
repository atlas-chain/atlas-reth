//! Production [`StateAdapter`] implementations for `arkiv-entitydb`.
//!
//! Two flavours, one per call path:
//!
//! - [`ReadWriteStateAdapter`] — wraps revm's `&mut EvmInternals`.
//!   Used by the precompile during block execution. All writes go
//!   through revm's journal so reverts roll back cleanly.
//! - [`ReadOnlyStateAdapter`] — wraps a reth `StateProviderBox`
//!   snapshot. Used by the `arkiv_*` RPC handler to drive
//!   `arkiv_entitydb::query::execute` against committed state.
//!   Mutating methods bail — they shouldn't be reached from the
//!   read-only query path.
//!
//! The third [`StateAdapter`] impl, `InMemoryStateAdapter`, lives in
//! `arkiv-entitydb::test_utils` next to its [`InMemoryStateDb`]
//! backing store.

use alloy_evm::EvmInternals;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_entitydb::StateAdapter;
use eyre::Result;
use reth_storage_api::{StateProvider, StateProviderBox};
use revm::state::Bytecode;

// ─── ReadWriteStateAdapter — revm-backed (write path) ────────────────

pub struct ReadWriteStateAdapter<'a, 'b> {
    internals: &'a mut EvmInternals<'b>,
}

impl<'a, 'b> ReadWriteStateAdapter<'a, 'b> {
    pub fn new(internals: &'a mut EvmInternals<'b>) -> Self {
        Self { internals }
    }

    /// `set_code` doesn't bump the nonce; new accounts would land with
    /// `nonce = 0` and EIP-161 would prune them. Force `nonce >= 1`.
    fn ensure_nonce_at_least_one(&mut self, addr: Address) -> Result<()> {
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

impl StateAdapter for ReadWriteStateAdapter<'_, '_> {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
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

    fn set_code(&mut self, addr: &Address, code: Vec<u8>) -> Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::from(code));
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code({addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn tombstone_code(&mut self, addr: &Address) -> Result<()> {
        let bytecode = Bytecode::new_raw(Bytes::new());
        self.internals
            .set_code(*addr, bytecode)
            .map_err(|e| eyre::eyre!("set_code (tombstone, {addr}): {e:?}"))?;
        self.ensure_nonce_at_least_one(*addr)
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
        let key = U256::from_be_bytes(slot.0);
        let load = self
            .internals
            .sload(*addr, key)
            .map_err(|e| eyre::eyre!("sload({addr}, {slot}): {e:?}"))?;
        Ok(B256::from(load.data.to_be_bytes()))
    }

    fn set_storage(&mut self, addr: &Address, slot: B256, value: B256) -> Result<()> {
        let key = U256::from_be_bytes(slot.0);
        let val = U256::from_be_bytes(value.0);
        self.internals
            .sstore(*addr, key, val)
            .map_err(|e| eyre::eyre!("sstore({addr}, {slot}): {e:?}"))?;
        Ok(())
    }

    fn ensure_account_persists(&mut self, addr: &Address) -> Result<()> {
        self.ensure_nonce_at_least_one(*addr)
    }
}

// ─── ReadOnlyStateAdapter — reth-backed (read path) ──────────────────

pub struct ReadOnlyStateAdapter {
    state: StateProviderBox,
}

impl ReadOnlyStateAdapter {
    pub fn new(state: StateProviderBox) -> Self {
        Self { state }
    }
}

impl StateAdapter for ReadOnlyStateAdapter {
    fn code(&mut self, addr: &Address) -> Result<Vec<u8>> {
        Ok(self
            .state
            .account_code(addr)
            .map_err(|e| eyre::eyre!("account_code({addr}): {e}"))?
            .map(|bc| bc.original_bytes().to_vec())
            .unwrap_or_default())
    }

    fn storage(&mut self, addr: &Address, slot: B256) -> Result<B256> {
        let v = self
            .state
            .storage(*addr, slot)
            .map_err(|e| eyre::eyre!("storage({addr}, {slot}): {e}"))?
            .unwrap_or(U256::ZERO);
        Ok(B256::from(v.to_be_bytes()))
    }

    fn set_code(&mut self, _addr: &Address, _code: Vec<u8>) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_code called from query path")
    }

    fn tombstone_code(&mut self, _addr: &Address) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: tombstone_code called from query path")
    }

    fn set_storage(&mut self, _addr: &Address, _slot: B256, _value: B256) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: set_storage called from query path")
    }

    fn ensure_account_persists(&mut self, _addr: &Address) -> Result<()> {
        eyre::bail!("ReadOnlyStateAdapter: ensure_account_persists called from query path")
    }
}
