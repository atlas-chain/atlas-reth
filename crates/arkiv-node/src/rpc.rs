//! `arkiv_*` JSON-RPC namespace.
//!
//! Thin adapter: owns the JSON-RPC plumbing, wire-format types, and
//! the [`StateProvider`] snapshot selection. Query execution lives in
//! [`arkiv_entitydb::query::execute`].

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Bytes, U256};
use arkiv_entitydb::query::{Page, PageParams, execute};
use arkiv_entitydb::{ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, EntityRlp, all_entities};
use async_trait::async_trait;
use eyre::Result;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::error::{ErrorObject, ErrorObjectOwned, INTERNAL_ERROR_CODE};
use reth_storage_api::{HeaderProvider, StateProviderBox, StateProviderFactory};
use serde::{Deserialize, Serialize};

use crate::state_adapter::ReadOnlyStateAdapter;

const DEFAULT_PAGE_SIZE: u64 = 100;
const MAX_PAGE_SIZE: u64 = 200;

#[rpc(server, namespace = "arkiv")]
pub trait ArkivApi {
    /// Evaluate a query and return matching entities. Pagination is
    /// descending by entity ID (newest first). When more results
    /// remain, `cursor` in the response is the ID of the last entry —
    /// pass it back as `options.cursor` to fetch the next page.
    #[method(name = "query")]
    async fn query(&self, q: String, options: Option<QueryOptions>) -> RpcResult<QueryResponse>;

    /// Number of live entities at the head (`$all` bitmap cardinality).
    #[method(name = "getEntityCount")]
    async fn get_entity_count(&self) -> RpcResult<u64>;

    /// Head block number, head block timestamp, and the duration
    /// (seconds) between the head and its parent.
    #[method(name = "getBlockTiming")]
    async fn get_block_timing(&self) -> RpcResult<BlockTiming>;
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryOptions {
    /// Block to evaluate against. `None` or `"latest"` reads head; a
    /// hex number (`"0x1a"`) reads historical state. Tags other than
    /// `latest` are rejected.
    pub at_block: Option<BlockNumberOrTag>,
    /// Page size; clamped to `[1, MAX_PAGE_SIZE]`. Accepts either a
    /// JSON number or a hex string (`numberToHex` from the JS SDK).
    #[serde(default, deserialize_with = "de_u64_flexible")]
    pub results_per_page: Option<u64>,
    /// Cursor for the next page (the last ID of the previous page,
    /// hex-encoded). Results in this call have ID strictly less than
    /// this value.
    pub cursor: Option<String>,
    /// Per-field projection. `None` → include everything. `Some` →
    /// each missing field defaults to `false`.
    pub include_data: Option<IncludeData>,
}

/// Field-selection options for `arkiv_query`. Boolean per top-level
/// field; missing fields default to `false` when the struct itself is
/// present. `key` is always returned regardless of this struct —
/// callers always need an identifier, and the SDK constructs its
/// `Entity` with `key` as a required parameter.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncludeData {
    pub key: Option<bool>,
    pub payload: Option<bool>,
    pub attributes: Option<bool>,
    pub content_type: Option<bool>,
    pub expiration: Option<bool>,
    pub owner: Option<bool>,
    pub creator: Option<bool>,
    pub created_at_block: Option<bool>,
    pub last_modified_at_block: Option<bool>,
    pub transaction_index_in_block: Option<bool>,
    pub operation_index_in_transaction: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub data: Vec<EntityData>,
    /// Block number at which the query was evaluated.
    #[serde(serialize_with = "ser_u64_hex", deserialize_with = "de_u64_hex")]
    pub block_number: u64,
    /// Cursor for the next page, or `None` if this is the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Per-entity payload in `arkiv_query` responses. Every metadata
/// field is optional and skip-serialized when None, so opting out via
/// `includeData` omits the field on the wire. `key` is always present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityData {
    pub key: B256,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Bytes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<Address>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<Address>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_u64_hex",
        deserialize_with = "de_u64_flexible"
    )]
    pub created_at_block: Option<u64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_u64_hex",
        deserialize_with = "de_u64_flexible"
    )]
    pub last_modified_at_block: Option<u64>,
    /// Always 0 — reth's revm context doesn't expose the
    /// tx-index-in-block during precompile execution. Kept in the wire
    /// shape for SDK parity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_index_in_block: Option<u64>,
    /// Always 0 — same caveat as `transaction_index_in_block`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_index_in_transaction: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub attributes: Vec<Attribute>,
}

/// Discriminated attribute on the wire. `value`'s encoding depends on
/// `value_type`:
///   - `ATTR_UINT` → decimal U256 string (e.g. `"42"`).
///   - `ATTR_STRING` → UTF-8 string.
///   - `ATTR_ENTITY_KEY` → `0x`-prefixed hex of the 32-byte key.
///
/// Mirrors the create/update request shape so the SDK uses one type
/// for both directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attribute {
    pub key: String,
    pub value_type: u8,
    pub value: String,
}

/// Response shape for `arkiv_getBlockTiming`. Snake_case wire field
/// names match what the SDK reads.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BlockTiming {
    pub current_block: u64,
    pub current_block_time: u64,
    pub duration: u64,
}

pub struct ArkivRpc<P> {
    provider: P,
}

impl<P> ArkivRpc<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl<P> ArkivApiServer for ArkivRpc<P>
where
    P: StateProviderFactory + HeaderProvider + Clone + Send + Sync + 'static,
{
    async fn query(&self, q: String, options: Option<QueryOptions>) -> RpcResult<QueryResponse> {
        let provider = self.provider.clone();
        let options = options.unwrap_or_default();
        // MDBX state reads are sync I/O — keep them off the tokio runtime.
        tokio::task::spawn_blocking(move || run_query(provider, &q, &options))
            .await
            .map_err(|e| internal_err(format!("blocking task join: {e}")))?
            .map_err(|e| internal_err(format!("{e}")))
    }

    async fn get_entity_count(&self) -> RpcResult<u64> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let mut adapter = ReadOnlyStateAdapter::new(provider.latest()?);
            Ok(all_entities(&mut adapter)?.len())
        })
        .await
        .map_err(|e| internal_err(format!("blocking task join: {e}")))?
        .map_err(|e| internal_err(format!("{e}")))
    }

    async fn get_block_timing(&self) -> RpcResult<BlockTiming> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || -> Result<BlockTiming> {
            let current_block = provider.best_block_number()?;
            let head = provider
                .header_by_number(current_block)?
                .ok_or_else(|| eyre::eyre!("head header missing for block {current_block}"))?;
            let current_block_time = head.timestamp();
            let duration = if current_block == 0 {
                0
            } else {
                let parent = provider
                    .header_by_number(current_block - 1)?
                    .ok_or_else(|| {
                        eyre::eyre!("parent header missing for block {}", current_block - 1)
                    })?;
                current_block_time.saturating_sub(parent.timestamp())
            };
            Ok(BlockTiming {
                current_block,
                current_block_time,
                duration,
            })
        })
        .await
        .map_err(|e| internal_err(format!("blocking task join: {e}")))?
        .map_err(|e| internal_err(format!("{e}")))
    }
}

fn run_query<P: StateProviderFactory>(
    provider: P,
    q: &str,
    options: &QueryOptions,
) -> Result<QueryResponse> {
    let (state, block_number) = snapshot_for(&provider, options.at_block)?;
    let mut adapter = ReadOnlyStateAdapter::new(state);

    let params = PageParams {
        page_size: options
            .results_per_page
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE),
        cursor: parse_cursor(options.cursor.as_deref())?,
    };
    let Page {
        entries,
        next_cursor,
    } = execute(&mut adapter, q, params)?;
    let include = ResolvedIncludeData::from_options(options.include_data.as_ref());

    Ok(QueryResponse {
        data: entries
            .into_iter()
            .map(|e| entity_data_from(e, &include))
            .collect(),
        block_number,
        cursor: next_cursor.map(|id| format!("0x{id:x}")),
    })
}

fn snapshot_for<P: StateProviderFactory>(
    provider: &P,
    at_block: Option<BlockNumberOrTag>,
) -> Result<(StateProviderBox, u64)> {
    match at_block {
        None | Some(BlockNumberOrTag::Latest) => {
            // best_block_number / latest race is benign — both observe
            // the canonical head.
            let n = provider.best_block_number()?;
            Ok((provider.latest()?, n))
        }
        Some(BlockNumberOrTag::Number(n)) => {
            let state = provider.history_by_block_number(n)?;
            Ok((state, n))
        }
        Some(other) => {
            eyre::bail!("atBlock tag {other:?} not supported; pass a hex block number or 'latest'")
        }
    }
}

/// Resolved per-field projection. `None` includeData → every field
/// included; `Some` → each unset field defaults to false.
#[derive(Debug, Clone, Copy)]
struct ResolvedIncludeData {
    payload: bool,
    attributes: bool,
    content_type: bool,
    expiration: bool,
    owner: bool,
    creator: bool,
    created_at_block: bool,
    last_modified_at_block: bool,
    transaction_index_in_block: bool,
    operation_index_in_transaction: bool,
}

impl ResolvedIncludeData {
    fn all() -> Self {
        Self {
            payload: true,
            attributes: true,
            content_type: true,
            expiration: true,
            owner: true,
            creator: true,
            created_at_block: true,
            last_modified_at_block: true,
            transaction_index_in_block: true,
            operation_index_in_transaction: true,
        }
    }

    fn from_options(opt: Option<&IncludeData>) -> Self {
        match opt {
            None => Self::all(),
            Some(id) => Self {
                payload: id.payload.unwrap_or(false),
                attributes: id.attributes.unwrap_or(false),
                content_type: id.content_type.unwrap_or(false),
                expiration: id.expiration.unwrap_or(false),
                owner: id.owner.unwrap_or(false),
                creator: id.creator.unwrap_or(false),
                created_at_block: id.created_at_block.unwrap_or(false),
                last_modified_at_block: id.last_modified_at_block.unwrap_or(false),
                transaction_index_in_block: id.transaction_index_in_block.unwrap_or(false),
                operation_index_in_transaction: id.operation_index_in_transaction.unwrap_or(false),
            },
        }
    }
}

fn entity_data_from(e: EntityRlp, inc: &ResolvedIncludeData) -> EntityData {
    let attributes = if inc.attributes {
        e.attributes
            .into_iter()
            .map(|a| Attribute {
                key: String::from_utf8_lossy(&a.key).into_owned(),
                value_type: a.value_type,
                value: format_attribute_value(a.value_type, &a.value),
            })
            .collect()
    } else {
        Vec::new()
    };

    EntityData {
        key: e.key,
        value: inc.payload.then(|| Bytes::from(e.payload)),
        content_type: inc
            .content_type
            .then(|| String::from_utf8_lossy(&e.content_type).into_owned()),
        expires_at: inc.expiration.then_some(e.expires_at),
        owner: inc.owner.then_some(e.owner),
        creator: inc.creator.then_some(e.creator),
        created_at_block: inc.created_at_block.then_some(e.created_at_block),
        last_modified_at_block: inc
            .last_modified_at_block
            .then_some(e.last_modified_at_block),
        // Not yet tracked through the precompile path — see field doc.
        transaction_index_in_block: inc.transaction_index_in_block.then_some(0),
        operation_index_in_transaction: inc.operation_index_in_transaction.then_some(0),
        attributes,
    }
}

fn format_attribute_value(value_type: u8, bytes: &[u8]) -> String {
    match value_type {
        ATTR_UINT => U256::from_be_slice(bytes).to_string(),
        ATTR_STRING => String::from_utf8_lossy(bytes).into_owned(),
        ATTR_ENTITY_KEY => alloy_primitives::hex::encode_prefixed(bytes),
        _ => String::new(),
    }
}

fn ser_u64_hex<S: serde::Serializer>(v: &u64, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0x{v:x}"))
}

fn ser_opt_u64_hex<S: serde::Serializer>(
    v: &Option<u64>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match v {
        Some(n) => s.serialize_str(&format!("0x{n:x}")),
        None => s.serialize_none(),
    }
}

fn de_u64_hex<'de, D>(de: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Num(u64),
        Hex(String),
    }
    match Either::deserialize(de)? {
        Either::Num(n) => Ok(n),
        Either::Hex(s) => {
            let stripped = s
                .strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .unwrap_or(&s);
            u64::from_str_radix(stripped, 16)
                .map_err(|e| D::Error::custom(format!("invalid hex u64 {s:?}: {e}")))
        }
    }
}

/// Accept either a JSON number or a hex string for an `Option<u64>`
/// field. The JS SDK encodes integers as hex strings (`numberToHex`)
/// over the wire; our own e2e helpers pass plain numbers. Supporting
/// both keeps both ergonomic.
fn de_u64_flexible<'de, D>(de: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Num(u64),
        Hex(String),
    }
    let opt: Option<Either> = Option::deserialize(de)?;
    match opt {
        None => Ok(None),
        Some(Either::Num(n)) => Ok(Some(n)),
        Some(Either::Hex(s)) => {
            let stripped = s
                .strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .unwrap_or(&s);
            u64::from_str_radix(stripped, 16)
                .map(Some)
                .map_err(|e| D::Error::custom(format!("invalid hex u64 {s:?}: {e}")))
        }
    }
}

fn parse_cursor(s: Option<&str>) -> Result<Option<u64>> {
    match s {
        None => Ok(None),
        Some(c) => {
            let stripped = c.strip_prefix("0x").unwrap_or(c);
            let n = u64::from_str_radix(stripped, 16)
                .map_err(|e| eyre::eyre!("invalid cursor {c:?}: {e}"))?;
            Ok(Some(n))
        }
    }
}

fn internal_err(msg: String) -> ErrorObjectOwned {
    ErrorObject::owned(INTERNAL_ERROR_CODE, msg, None::<()>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uint_value_is_decimal_string() {
        // 32-byte big-endian for U256(123456789). SDK reads as decimal.
        let bytes = U256::from(123_456_789u64).to_be_bytes::<32>();
        assert_eq!(
            format_attribute_value(ATTR_UINT, &bytes),
            "123456789".to_string()
        );
    }

    #[test]
    fn format_string_value_is_utf8() {
        assert_eq!(
            format_attribute_value(ATTR_STRING, b"hello world"),
            "hello world".to_string()
        );
    }

    #[test]
    fn format_entity_key_value_is_prefixed_hex() {
        // 32 raw bytes — must be emitted as 0x + 64 hex chars,
        // including any trailing zeros.
        let mut k = [0u8; 32];
        k[0] = 0xab;
        k[1] = 0xcd;
        assert_eq!(
            format_attribute_value(ATTR_ENTITY_KEY, &k),
            "0xabcd000000000000000000000000000000000000000000000000000000000000".to_string(),
        );
    }

    #[test]
    fn attribute_serializes_with_camel_case_value_type() {
        // The wire shape must mirror the create/update request:
        // `key`, `valueType`, `value`. The SDK matches on `valueType`.
        let attr = Attribute {
            key: "score".to_string(),
            value_type: ATTR_UINT,
            value: "42".to_string(),
        };
        let json = serde_json::to_value(&attr).expect("serialize");
        assert_eq!(json["key"], "score");
        assert_eq!(json["valueType"], 1);
        assert_eq!(json["value"], "42");
        assert!(json.get("value_type").is_none(), "snake_case leaked");
    }
}
