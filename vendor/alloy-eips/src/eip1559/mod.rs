//! [EIP-1559] constants, helpers, and types.
//!
//! [EIP-1559]: https://eips.ethereum.org/EIPS/eip-1559

mod basefee;
pub use basefee::BaseFeeParams;

#[cfg(feature = "std")]
mod arkiv_schedule;
#[cfg(feature = "std")]
pub use arkiv_schedule::{
    arkiv_protocol_params_for_block, arkiv_protocol_params_latest, arkiv_protocol_schedule,
    install_arkiv_protocol_schedule, ArkivProtocolParams, ArkivProtocolScheduleEntry,
    ARKIV_DEFAULT_MIN_BASE_FEE_PER_GAS,
};

mod constants;
pub use constants::*;

mod helpers;
pub use helpers::{
    calc_effective_gas_price, calc_next_block_base_fee, calculate_block_gas_limit,
    Eip1559Estimation,
};
