//! Arkiv node library — op-reth wrapper with the Arkiv system account,
//! custom `EvmFactory` (precompile), and `arkiv_*` JSON-RPC namespace.

mod cli;
pub mod evm;
mod genesis;
mod install;
pub mod precompile;
pub mod rpc;

pub use cli::ArkivExt;
pub use evm::ArkivOpNode;
pub use genesis::has_arkiv_system_account;
pub use install::install;
