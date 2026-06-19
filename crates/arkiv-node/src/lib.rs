//! Arkiv node library — reth wrapper with the custom `EvmFactory`
//! (Arkiv precompile) and the `arkiv_*` JSON-RPC namespace.

pub mod evm;
mod install;
pub mod precompile;
pub mod protocol_schedule;
pub mod rpc;
pub mod state_adapter;

pub use evm::{ArkivEthEvmFactory, ArkivEthExecutorBuilder};
pub use install::install;
