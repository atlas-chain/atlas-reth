use crate::eip1559::BaseFeeParams;
use alloc::vec::Vec;
use std::sync::{OnceLock, RwLock};

/// Default Arkiv testnet base-fee floor: 0.44 gwei.
pub const ARKIV_DEFAULT_MIN_BASE_FEE_PER_GAS: u64 = 440_000_000;

/// Basis-point denominator for payload-provider payment splits.
pub const ARKIV_PAYLOAD_PROVIDER_PAYMENT_BPS_DENOMINATOR: u16 = 10_000;

/// Runtime payload-provider payment knobs supplied by Arkiv's schedule poller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArkivPayloadProviderPaymentParams {
    /// Whether signed payload-reference payments are applied as balance side effects.
    pub enabled: bool,
    /// Share of the signed payment transferred to the recovered provider signer.
    pub provider_share_bps: u16,
    /// Minimum signed payment accepted by the precompile.
    pub minimum_payment: u64,
}

/// Runtime protocol knobs supplied by Arkiv's experimental schedule poller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArkivProtocolParams {
    /// Minimum EIP-1559 base fee per gas.
    pub min_base_fee_per_gas: u64,
    /// EIP-1559 elasticity multiplier.
    pub elasticity_multiplier: u128,
    /// EIP-1559 max-change denominator.
    pub base_fee_max_change_denominator: u128,
    /// Desired payload-builder gas-limit cap.
    pub max_block_gas_limit: u64,
    /// Payload-provider payment split parameters.
    pub payload_provider_payment: ArkivPayloadProviderPaymentParams,
}

impl Default for ArkivProtocolParams {
    fn default() -> Self {
        let base_fee_params = BaseFeeParams::ethereum();
        Self {
            min_base_fee_per_gas: ARKIV_DEFAULT_MIN_BASE_FEE_PER_GAS,
            elasticity_multiplier: base_fee_params.elasticity_multiplier,
            base_fee_max_change_denominator: base_fee_params.max_change_denominator,
            max_block_gas_limit: u64::MAX,
            payload_provider_payment: ArkivPayloadProviderPaymentParams::default(),
        }
    }
}

impl ArkivProtocolParams {
    /// Converts the Arkiv schedule entry into Alloy's EIP-1559 parameters.
    pub const fn base_fee_params(self) -> BaseFeeParams {
        BaseFeeParams::new(
            self.base_fee_max_change_denominator,
            self.elasticity_multiplier,
        )
    }
}

/// One block-numbered Arkiv protocol schedule entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArkivProtocolScheduleEntry {
    /// First block where this entry should apply.
    pub activation_block: u64,
    /// Protocol knobs active at and after `activation_block`.
    pub params: ArkivProtocolParams,
}

fn schedule() -> &'static RwLock<Vec<ArkivProtocolScheduleEntry>> {
    static SCHEDULE: OnceLock<RwLock<Vec<ArkivProtocolScheduleEntry>>> = OnceLock::new();
    SCHEDULE.get_or_init(|| RwLock::new(Vec::new()))
}

/// Installs a validated Arkiv protocol schedule snapshot.
pub fn install_arkiv_protocol_schedule(entries: Vec<ArkivProtocolScheduleEntry>) {
    if let Ok(mut schedule) = schedule().write() {
        *schedule = entries;
    }
}

/// Returns the currently installed schedule.
pub fn arkiv_protocol_schedule() -> Vec<ArkivProtocolScheduleEntry> {
    schedule()
        .read()
        .map(|schedule| schedule.clone())
        .unwrap_or_default()
}

/// Returns the Arkiv protocol parameters for `block_number`.
pub fn arkiv_protocol_params_for_block(block_number: u64) -> ArkivProtocolParams {
    schedule()
        .read()
        .ok()
        .and_then(|schedule| {
            schedule
                .iter()
                .rev()
                .find(|entry| entry.activation_block <= block_number)
                .copied()
        })
        .map(|entry| entry.params)
        .unwrap_or_default()
}

/// Returns the latest installed Arkiv protocol parameters.
pub fn arkiv_protocol_params_latest() -> ArkivProtocolParams {
    schedule()
        .read()
        .ok()
        .and_then(|schedule| schedule.last().copied())
        .map(|entry| entry.params)
        .unwrap_or_default()
}
