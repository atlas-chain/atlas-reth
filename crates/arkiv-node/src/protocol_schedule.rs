use alloy_eips::eip1559::{
    ARKIV_DEFAULT_MIN_BASE_FEE_PER_GAS, ARKIV_PAYLOAD_PROVIDER_PAYMENT_BPS_DENOMINATOR,
    ArkivPayloadProviderPaymentParams, ArkivProtocolParams, ArkivProtocolScheduleEntry,
    install_arkiv_protocol_schedule,
};
use eyre::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tracing::{debug, info, warn};

const URL_ENV: &str = "ARKIV_PROTOCOL_SCHEDULE_URL";
const PATH_ENV: &str = "ARKIV_PROTOCOL_SCHEDULE_PATH";
const POLL_SECONDS_ENV: &str = "ARKIV_PROTOCOL_SCHEDULE_POLL_SECONDS";
const DEFAULT_PATH: &str = "arkiv-protocol-schedule.json";
const DEFAULT_POLL_SECONDS: u64 = 60;
static LAST_ACCEPTED_VERSION: AtomicU64 = AtomicU64::new(0);

/// Starts the experimental Arkiv protocol-schedule poller if configured.
pub fn spawn_from_env() {
    let Some(url) = env::var(URL_ENV).ok().filter(|url| !url.trim().is_empty()) else {
        debug!(target: "arkiv::protocol_schedule", "protocol schedule poller disabled");
        return;
    };

    let path = env::var(PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PATH));
    let poll_interval = env::var(POLL_SECONDS_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_POLL_SECONDS);

    if let Err(err) = load_persisted_schedule(&path) {
        warn!(
            target: "arkiv::protocol_schedule",
            %err,
            path = %path.display(),
            "failed to load persisted protocol schedule"
        );
    }
    if let Err(err) = ensure_default_schedule_file(&path) {
        warn!(
            target: "arkiv::protocol_schedule",
            %err,
            path = %path.display(),
            "failed to create default protocol schedule file"
        );
    }

    tokio::spawn(async move {
        poll_loop(url, path, Duration::from_secs(poll_interval)).await;
    });
}

async fn poll_loop(url: String, path: PathBuf, poll_interval: Duration) {
    loop {
        if let Err(err) = fetch_store_and_install(&url, &path).await {
            warn!(
                target: "arkiv::protocol_schedule",
                %err,
                url,
                "failed to update protocol schedule"
            );
        }

        tokio::time::sleep(poll_interval).await;
    }
}

async fn fetch_store_and_install(url: &str, path: &Path) -> Result<()> {
    let response = reqwest::get(url)
        .await
        .context("request failed")?
        .error_for_status()?;
    let body = response.text().await.context("read response body")?;
    let schedule = parse_remote_schedule(&body)?;
    install_schedule(&schedule)?;
    fs::write(path, &body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn load_persisted_schedule(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let body = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let schedule = parse_remote_schedule(&body)?;
    install_schedule(&schedule)?;
    info!(
        target: "arkiv::protocol_schedule",
        chain_id = schedule.chain_id,
        version = schedule.version,
        entries = schedule.schedule.len(),
        path = %path.display(),
        "loaded persisted protocol schedule"
    );
    Ok(())
}

fn ensure_default_schedule_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    let default_schedule = RemoteProtocolSchedule::default_file();
    let body = serde_json::to_string_pretty(&default_schedule)?;
    fs::write(path, format!("{body}\n")).with_context(|| format!("write {}", path.display()))?;
    info!(
        target: "arkiv::protocol_schedule",
        path = %path.display(),
        "created default protocol schedule file"
    );
    Ok(())
}

fn install_schedule(schedule: &RemoteProtocolSchedule) -> Result<()> {
    let last_version = LAST_ACCEPTED_VERSION.load(Ordering::Relaxed);
    if last_version != 0 && schedule.version < last_version {
        bail!(
            "schedule version {} is older than last accepted version {}",
            schedule.version,
            last_version
        );
    }

    let selected_entries = selected_entries(schedule);
    let installed_len = selected_entries.len();
    install_arkiv_protocol_schedule(selected_entries);
    LAST_ACCEPTED_VERSION.store(schedule.version, Ordering::Relaxed);
    info!(
        target: "arkiv::protocol_schedule",
        chain_id = schedule.chain_id,
        version = schedule.version,
        current_block = schedule.current_block,
        entries = schedule.schedule.len(),
        installed_entries = installed_len,
        "installed protocol schedule"
    );
    Ok(())
}

fn selected_entries(schedule: &RemoteProtocolSchedule) -> Vec<ArkivProtocolScheduleEntry> {
    let mut entries = schedule.entries();

    if let Some(current_block) = schedule.current_block {
        entries.retain(|entry| entry.activation_block <= current_block);
    }

    if entries.is_empty() {
        entries.extend(schedule.entries().into_iter().take(1));
    }

    entries
}

fn parse_remote_schedule(body: &str) -> Result<RemoteProtocolSchedule> {
    let schedule: RemoteProtocolSchedule = serde_json::from_str(body).context("decode JSON")?;
    schedule.validate()?;
    Ok(schedule)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProtocolSchedule {
    chain_id: u64,
    version: u64,
    #[serde(default)]
    current_block: Option<u64>,
    schedule: Vec<RemoteProtocolScheduleEntry>,
}

impl RemoteProtocolSchedule {
    fn default_file() -> Self {
        Self {
            chain_id: 0,
            version: 1,
            current_block: Some(0),
            schedule: vec![RemoteProtocolScheduleEntry {
                activation_block: 0,
                min_base_fee_per_gas: U64String(ARKIV_DEFAULT_MIN_BASE_FEE_PER_GAS),
                elasticity_multiplier: 2,
                base_fee_max_change_denominator: 8,
                max_block_gas_limit: U64String(30_000_000),
                payload_provider_payment: RemotePayloadProviderPaymentParams {
                    enabled: false,
                    provider_share_bps: 0,
                    minimum_payment: U64String(0),
                },
            }],
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schedule.is_empty() {
            bail!("schedule must not be empty");
        }
        if self.schedule[0].activation_block != 0 {
            bail!("first schedule entry must activate at block 0");
        }

        let mut last_activation = None;
        for entry in &self.schedule {
            if let Some(last_activation) = last_activation
                && entry.activation_block <= last_activation
            {
                bail!("schedule activation blocks must be strictly increasing");
            }
            last_activation = Some(entry.activation_block);
            entry.validate()?;
        }

        Ok(())
    }

    fn entries(&self) -> Vec<ArkivProtocolScheduleEntry> {
        self.schedule
            .iter()
            .map(RemoteProtocolScheduleEntry::to_alloy)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProtocolScheduleEntry {
    activation_block: u64,
    min_base_fee_per_gas: U64String,
    elasticity_multiplier: u128,
    base_fee_max_change_denominator: u128,
    max_block_gas_limit: U64String,
    payload_provider_payment: RemotePayloadProviderPaymentParams,
}

impl RemoteProtocolScheduleEntry {
    fn validate(&self) -> Result<()> {
        if self.elasticity_multiplier == 0 {
            bail!("elasticityMultiplier must be greater than 0");
        }
        if self.base_fee_max_change_denominator == 0 {
            bail!("baseFeeMaxChangeDenominator must be greater than 0");
        }
        if self.max_block_gas_limit.0 < 5_000 {
            bail!("maxBlockGasLimit is below the minimum viable gas limit");
        }
        self.payload_provider_payment.validate()?;
        Ok(())
    }

    fn to_alloy(&self) -> ArkivProtocolScheduleEntry {
        ArkivProtocolScheduleEntry {
            activation_block: self.activation_block,
            params: ArkivProtocolParams {
                min_base_fee_per_gas: self.min_base_fee_per_gas.0,
                elasticity_multiplier: self.elasticity_multiplier,
                base_fee_max_change_denominator: self.base_fee_max_change_denominator,
                max_block_gas_limit: self.max_block_gas_limit.0,
                payload_provider_payment: self.payload_provider_payment.to_alloy(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemotePayloadProviderPaymentParams {
    enabled: bool,
    provider_share_bps: u16,
    minimum_payment: U64String,
}

impl RemotePayloadProviderPaymentParams {
    fn validate(&self) -> Result<()> {
        if self.provider_share_bps > ARKIV_PAYLOAD_PROVIDER_PAYMENT_BPS_DENOMINATOR {
            bail!("payloadProviderPayment.providerShareBps must be <= 10000");
        }
        if self.enabled && self.minimum_payment.0 == 0 {
            bail!("payloadProviderPayment.minimumPayment must be greater than 0 when enabled");
        }
        Ok(())
    }

    fn to_alloy(self) -> ArkivPayloadProviderPaymentParams {
        ArkivPayloadProviderPaymentParams {
            enabled: self.enabled,
            provider_share_bps: self.provider_share_bps,
            minimum_payment: self.minimum_payment.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct U64String(u64);

impl Serialize for U64String {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for U64String {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        parse_u64_string(&raw)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

fn parse_u64_string(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty numeric string");
    }

    if let Some(hex) = raw.strip_prefix("0x") {
        Ok(u64::from_str_radix(hex, 16)?)
    } else {
        Ok(raw.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_and_hex_quantities() {
        assert_eq!(parse_u64_string("440000000").unwrap(), 440_000_000);
        assert_eq!(parse_u64_string("0x1a39de00").unwrap(), 440_000_000);
    }

    #[test]
    fn validates_and_selects_current_entries() {
        let schedule = parse_remote_schedule(
            r#"{
                "chainId": 12345,
                "version": 7,
                "currentBlock": 100,
                "schedule": [
                    {
                        "activationBlock": 0,
                        "minBaseFeePerGas": "440000000",
                        "elasticityMultiplier": 2,
                        "baseFeeMaxChangeDenominator": 8,
                        "maxBlockGasLimit": "30000000",
                        "payloadProviderPayment": {
                            "enabled": true,
                            "providerShareBps": 7000,
                            "minimumPayment": "100000"
                        }
                    },
                    {
                        "activationBlock": 120,
                        "minBaseFeePerGas": "800000000",
                        "elasticityMultiplier": 4,
                        "baseFeeMaxChangeDenominator": 8,
                        "maxBlockGasLimit": "60000000",
                        "payloadProviderPayment": {
                            "enabled": true,
                            "providerShareBps": 8000,
                            "minimumPayment": "200000"
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let entries = selected_entries(&schedule);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].params.min_base_fee_per_gas, 440_000_000);
        assert!(entries[0].params.payload_provider_payment.enabled);
        assert_eq!(
            entries[0]
                .params
                .payload_provider_payment
                .provider_share_bps,
            7000
        );
    }

    #[test]
    fn default_file_round_trips() {
        let schedule = RemoteProtocolSchedule::default_file();
        let body = serde_json::to_string_pretty(&schedule).unwrap();
        let parsed = parse_remote_schedule(&body).unwrap();

        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.current_block, Some(0));
        assert_eq!(parsed.schedule.len(), 1);
        assert_eq!(
            parsed.schedule[0].min_base_fee_per_gas.0,
            ARKIV_DEFAULT_MIN_BASE_FEE_PER_GAS
        );
        assert!(!parsed.schedule[0].payload_provider_payment.enabled);
    }
}
