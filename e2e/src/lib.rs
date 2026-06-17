//! Shared bootstrap + helpers for the `arkiv-reth` e2e tests.
//!
//! Boots an `EthereumNode` with the Arkiv precompile and `arkiv_*`
//! JSON-RPC namespace installed, derives signers from the dev mnemonic, and exposes
//! operation-builder methods (`create` / `update` / `extend` / `transfer` /
//! `delete`) that submit a tx, advance a block, assert the receipt
//! status, and bump nonces.
//!
//! Tests use this via:
//!
//! ```ignore
//! let mut world = arkiv_e2e::boot().await?;
//! let key = world.create(0, CreateOp::new()
//!     .payload(b"hello")
//!     .content_type("text/plain")
//!     .btl(1000)
//!     .string_attr("tag", "music")).await?;
//! let results = world.query(r#"$contentType = "text/plain""#).await?;
//! ```
//!
//! Note on generics: [`World`] is generic over the node component bag
//! because [`NodeTestContext`] is, but call sites never need to name
//! the type — `let mut world = boot().await?;` is enough thanks to
//! inference.

use std::time::Duration;

use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, FixedBytes, TxKind, U256, keccak256};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall, sol};
use arkiv_genesis::{ARKIV_ADDRESS, dev_signers};
use arkiv_node::rpc::{EntityData, QueryResponse};
use arkiv_node::{ArkivEthExecutorBuilder, install};
use eyre::{Result, bail, eyre};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::rpc_params;
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_e2e_test_utils::{node::NodeTestContext, transaction::TransactionTestContext};
use reth_ethereum_primitives::EthPrimitives;
use reth_network_api::test_utils::PeersHandleProvider;
use reth_node_api::{BlockTy, FullNodeComponents, NodeTypes, PayloadTypes};
use reth_node_builder::{NodeBuilder, NodeHandle, rpc::RethRpcAddOns};
use reth_node_core::{args::RpcServerArgs, node_config::NodeConfig};
use reth_node_ethereum::{EthereumAddOns, EthereumNode};
use reth_provider::BlockReader;
use reth_rpc_eth_api::helpers::{EthApiSpec, EthTransactions, TraceExt};

// ── Contract ABI (mirror of EntityRegistry.execute) ──────────────────

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

// Op-type constants — must match `Entity.{CREATE..EXPIRE}` in
// contracts/src/EntityRegistry.sol.
pub const OP_CREATE: u8 = 1;
pub const OP_UPDATE: u8 = 2;
pub const OP_EXTEND: u8 = 3;
pub const OP_TRANSFER: u8 = 4;
pub const OP_DELETE: u8 = 5;
pub const OP_EXPIRE: u8 = 6;

pub const ATTR_UINT: u8 = 1;
pub const ATTR_STRING: u8 = 2;
pub const ATTR_ENTITY_KEY: u8 = 3;

// ── Op builders ──────────────────────────────────────────────────────

/// Builder for a CREATE op submitted via [`World::create`].
#[derive(Debug, Clone, Default)]
pub struct CreateOp {
    payload: Vec<u8>,
    content_type: String,
    btl: u32,
    string_attrs: Vec<(String, String)>,
    numeric_attrs: Vec<(String, U256)>,
    entity_key_attrs: Vec<(String, B256)>,
}

impl CreateOp {
    pub fn new() -> Self {
        Self {
            btl: 1_000,
            ..Self::default()
        }
    }
    pub fn payload(mut self, p: impl Into<Vec<u8>>) -> Self {
        self.payload = p.into();
        self
    }
    pub fn content_type(mut self, s: impl Into<String>) -> Self {
        self.content_type = s.into();
        self
    }
    pub fn btl(mut self, b: u32) -> Self {
        self.btl = b;
        self
    }
    pub fn string_attr(mut self, key: &str, value: &str) -> Self {
        self.string_attrs.push((key.into(), value.into()));
        self
    }
    pub fn numeric_attr(mut self, key: &str, value: u64) -> Self {
        self.numeric_attrs.push((key.into(), U256::from(value)));
        self
    }
    pub fn entity_key_attr(mut self, key: &str, value: B256) -> Self {
        self.entity_key_attrs.push((key.into(), value));
        self
    }
}

/// Builder for an UPDATE op submitted via [`World::update`].
#[derive(Debug, Clone, Default)]
pub struct UpdateOp {
    payload: Vec<u8>,
    content_type: String,
    string_attrs: Vec<(String, String)>,
    numeric_attrs: Vec<(String, U256)>,
}

impl UpdateOp {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn payload(mut self, p: impl Into<Vec<u8>>) -> Self {
        self.payload = p.into();
        self
    }
    pub fn content_type(mut self, s: impl Into<String>) -> Self {
        self.content_type = s.into();
        self
    }
    pub fn string_attr(mut self, key: &str, value: &str) -> Self {
        self.string_attrs.push((key.into(), value.into()));
        self
    }
    pub fn numeric_attr(mut self, key: &str, value: u64) -> Self {
        self.numeric_attrs.push((key.into(), U256::from(value)));
        self
    }
}

// ── Test world ───────────────────────────────────────────────────────

/// Per-test session state + node handle. Hides nonce tracking, ABI
/// encoding, signing, query encoding behind a small method surface.
pub struct World<Node, AddOns>
where
    Node: FullNodeComponents,
    AddOns: RethRpcAddOns<Node>,
{
    pub node: NodeTestContext<Node, AddOns>,
    pub chain_id: u64,
    pub signers: Vec<PrivateKeySigner>,
    /// Tx nonce per signer (incremented on every successful submit, incl. reverts).
    pub eth_nonces: Vec<u64>,
    /// `EntityRegistry.nonces[sender]` per signer (incremented per
    /// successful CREATE).
    pub entity_nonces: Vec<u32>,
}

/// Number of dev signers pre-loaded in the World.
pub const SIGNER_COUNT: usize = 5;

/// Default per-tx gas cap for op submissions.
const DEFAULT_GAS: u64 = 1_500_000;

/// Match the dev funding helper's gas-price assumptions.
const GAS_PRICE: u128 = 20_000_000_000;

/// Boot an `EthereumNode` with the Arkiv precompile and `arkiv_*`
/// namespace installed, derive [`SIGNER_COUNT`] dev signers, and return
/// a ready-to-drive [`World`].
///
/// The return type is a concrete `World<Node, AddOns>` whose generic
/// parameters are inferred at the call site — tests just write
/// `let mut world = arkiv_e2e::boot().await?;`.
pub async fn boot() -> Result<impl impl_world_alias::ArkivWorld> {
    use reth_tasks::Runtime;

    reth_tracing::init_test_tracing();

    let mut genesis: Genesis = serde_json::from_str(include_str!("../../chainspec/dev.base.json"))?;
    for (addr, account) in arkiv_genesis::genesis_alloc()? {
        genesis.alloc.insert(addr, account);
    }
    let chain_spec = ChainSpec::from_genesis(genesis);
    let chain_id = chain_spec.chain.id();

    let runtime = Runtime::test();
    let node_config = NodeConfig::test()
        .with_chain(chain_spec)
        .with_rpc(RpcServerArgs::default().with_http().with_unused_ports());

    let builder = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .with_types::<EthereumNode>()
        .with_components(EthereumNode::components().executor(ArkivEthExecutorBuilder))
        .with_add_ons(EthereumAddOns::default());
    let NodeHandle {
        node,
        node_exit_future: _,
    } = install(builder).launch().await?;

    let node = NodeTestContext::new(node, eth_payload_attributes).await?;

    let signers = dev_signers(SIGNER_COUNT)?;
    Ok(World {
        node,
        chain_id,
        signers,
        eth_nonces: vec![0; SIGNER_COUNT],
        entity_nonces: vec![0; SIGNER_COUNT],
    })
}

/// Trick for inferring the concrete type of [`boot`]'s return value.
/// `impl Trait` in return position requires a named trait; this trait
/// is implemented for any `World<Node, AddOns>` satisfying the bounds
/// our op methods need. Call sites never see this — they just bind
/// the result to `let mut world = ...`.
mod impl_world_alias {
    use super::*;
    pub trait ArkivWorld: WorldOps {}
    impl<W: WorldOps> ArkivWorld for W {}
}

/// Methods exposed on every [`World`] — abstracted via a trait so
/// [`boot`]'s return type can be `impl ArkivWorld`.
#[allow(async_fn_in_trait)]
pub trait WorldOps {
    fn address(&self, i: usize) -> Address;
    async fn head_block(&self) -> Result<u64>;
    async fn create(&mut self, signer: usize, op: CreateOp) -> Result<B256>;
    async fn update(&mut self, signer: usize, key: B256, op: UpdateOp) -> Result<()>;
    async fn extend(&mut self, signer: usize, key: B256, btl: u32) -> Result<()>;
    async fn transfer(&mut self, signer: usize, key: B256, new_owner: Address) -> Result<()>;
    async fn delete(&mut self, signer: usize, key: B256) -> Result<()>;
    async fn submit_expecting_revert(
        &mut self,
        signer: usize,
        op: Operation,
        op_label: &str,
    ) -> Result<()>;
    async fn query(&self, q: &str) -> Result<Vec<EntityData>>;
    async fn query_at(&self, q: &str, block: u64) -> Result<Vec<EntityData>>;
    async fn query_paginated(&self, q: &str, page_size: u64) -> Result<Vec<EntityData>>;
}

impl<Node, Payload, AddOns> WorldOps for World<Node, AddOns>
where
    Node: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec: EthereumHardforks,
                Payload = Payload,
            >,
            Network: PeersHandleProvider,
        >,
    Payload: PayloadTypes<PayloadAttributes = PayloadAttributes>,
    AddOns: RethRpcAddOns<
            Node,
            EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
                        + EthTransactions
                        + TraceExt,
        >,
{
    fn address(&self, i: usize) -> Address {
        self.signers[i].address()
    }

    async fn head_block(&self) -> Result<u64> {
        let rpc = self
            .node
            .rpc_client()
            .ok_or_else(|| eyre!("rpc client unavailable"))?;
        let n: alloy_primitives::U64 = rpc.request("eth_blockNumber", rpc_params![]).await?;
        Ok(n.to::<u64>())
    }

    async fn create(&mut self, signer_idx: usize, op: CreateOp) -> Result<B256> {
        // The precompile derives entityKey from (chainId, ARKIV_ADDRESS,
        // sender, nonce) and bumps the nonce within the same call —
        // mirror that ordering here.
        let sender = self.address(signer_idx);
        let entity_nonce = self.entity_nonces[signer_idx];
        let key = compute_entity_key(self.chain_id, ARKIV_ADDRESS, sender, entity_nonce);

        let attrs = build_attributes(&op.string_attrs, &op.numeric_attrs, &op.entity_key_attrs);
        let abi_op = Operation {
            operationType: OP_CREATE,
            entityKey: B256::ZERO, // contract derives it; this field is ignored on CREATE
            payload: Bytes::from(op.payload),
            contentType: pack_mime(&op.content_type),
            attributes: attrs,
            btl: op.btl,
            newOwner: Address::ZERO,
        };
        self.submit_op(signer_idx, abi_op, "CREATE").await?;
        self.entity_nonces[signer_idx] += 1;
        Ok(key)
    }

    async fn update(&mut self, signer_idx: usize, key: B256, op: UpdateOp) -> Result<()> {
        let attrs = build_attributes(&op.string_attrs, &op.numeric_attrs, &[]);
        let abi_op = Operation {
            operationType: OP_UPDATE,
            entityKey: key,
            payload: Bytes::from(op.payload),
            contentType: pack_mime(&op.content_type),
            attributes: attrs,
            btl: 0,
            newOwner: Address::ZERO,
        };
        self.submit_op(signer_idx, abi_op, "UPDATE").await
    }

    async fn extend(&mut self, signer_idx: usize, key: B256, btl: u32) -> Result<()> {
        let abi_op = Operation {
            operationType: OP_EXTEND,
            entityKey: key,
            payload: Bytes::new(),
            contentType: pack_mime(""),
            attributes: vec![],
            btl,
            newOwner: Address::ZERO,
        };
        self.submit_op(signer_idx, abi_op, "EXTEND").await
    }

    async fn transfer(&mut self, signer_idx: usize, key: B256, new_owner: Address) -> Result<()> {
        let abi_op = Operation {
            operationType: OP_TRANSFER,
            entityKey: key,
            payload: Bytes::new(),
            contentType: pack_mime(""),
            attributes: vec![],
            btl: 0,
            newOwner: new_owner,
        };
        self.submit_op(signer_idx, abi_op, "TRANSFER").await
    }

    async fn delete(&mut self, signer_idx: usize, key: B256) -> Result<()> {
        let abi_op = Operation {
            operationType: OP_DELETE,
            entityKey: key,
            payload: Bytes::new(),
            contentType: pack_mime(""),
            attributes: vec![],
            btl: 0,
            newOwner: Address::ZERO,
        };
        self.submit_op(signer_idx, abi_op, "DELETE").await
    }

    async fn submit_expecting_revert(
        &mut self,
        signer_idx: usize,
        op: Operation,
        op_label: &str,
    ) -> Result<()> {
        let tx_hash = self.send_and_mine(signer_idx, op).await?;
        let receipt = self.fetch_receipt(tx_hash).await?;
        let status = receipt
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("{op_label}: receipt missing status field"))?;
        if status != "0x0" {
            bail!(
                "{op_label}: expected revert (status=0x0), got status={status}; \
                 receipt = {}",
                serde_json::to_string_pretty(&receipt).unwrap_or_default()
            );
        }
        Ok(())
    }

    async fn query(&self, q: &str) -> Result<Vec<EntityData>> {
        self.query_raw(q, serde_json::Value::Null)
            .await
            .map(|r| r.data)
    }

    async fn query_at(&self, q: &str, block: u64) -> Result<Vec<EntityData>> {
        let options = serde_json::json!({ "atBlock": format!("0x{block:x}") });
        self.query_raw(q, options).await.map(|r| r.data)
    }

    async fn query_paginated(&self, q: &str, page_size: u64) -> Result<Vec<EntityData>> {
        let mut acc = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let options = match &cursor {
                None => serde_json::json!({ "resultsPerPage": page_size }),
                Some(c) => serde_json::json!({ "resultsPerPage": page_size, "cursor": c }),
            };
            let page = self.query_raw(q, options).await?;
            acc.extend(page.data);
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(acc),
            }
        }
    }
}

// ── Internal helpers on World ────────────────────────────────────────

impl<Node, Payload, AddOns> World<Node, AddOns>
where
    Node: FullNodeComponents<
            Types: NodeTypes<
                Primitives = EthPrimitives,
                ChainSpec: EthereumHardforks,
                Payload = Payload,
            >,
            Network: PeersHandleProvider,
        >,
    Payload: PayloadTypes<PayloadAttributes = PayloadAttributes>,
    AddOns: RethRpcAddOns<
            Node,
            EthApi: EthApiSpec<Provider: BlockReader<Block = BlockTy<Node::Types>>>
                        + EthTransactions
                        + TraceExt,
        >,
{
    async fn submit_op(&mut self, signer_idx: usize, op: Operation, op_label: &str) -> Result<()> {
        let tx_hash = self.send_and_mine(signer_idx, op).await?;
        let receipt = self.fetch_receipt(tx_hash).await?;
        let status = receipt
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("{op_label}: receipt missing status field"))?;
        if status != "0x1" {
            bail!(
                "{op_label}: tx failed (status={status}); receipt = {}",
                serde_json::to_string_pretty(&receipt).unwrap_or_default()
            );
        }
        Ok(())
    }

    async fn send_and_mine(&mut self, signer_idx: usize, op: Operation) -> Result<B256> {
        let calldata = executeCall { ops: vec![op] }.abi_encode();
        let signer = self.signers[signer_idx].clone();
        let nonce = self.eth_nonces[signer_idx];

        let tx_req = TransactionRequest {
            from: Some(signer.address()),
            to: Some(TxKind::Call(ARKIV_ADDRESS)),
            input: TransactionInput::new(calldata.into()),
            nonce: Some(nonce),
            gas: Some(DEFAULT_GAS),
            max_fee_per_gas: Some(GAS_PRICE),
            max_priority_fee_per_gas: Some(GAS_PRICE),
            chain_id: Some(self.chain_id),
            value: Some(U256::ZERO),
            ..Default::default()
        };
        let signed = TransactionTestContext::sign_tx(signer, tx_req).await;
        let raw_tx: Bytes = signed.encoded_2718().into();
        let tx_hash = self.node.rpc.inject_tx(raw_tx).await?;
        self.eth_nonces[signer_idx] += 1;

        let _payload = self.node.advance_block().await?;
        Ok(tx_hash)
    }

    async fn fetch_receipt(&self, tx_hash: B256) -> Result<serde_json::Value> {
        let rpc = self
            .node
            .rpc_client()
            .ok_or_else(|| eyre!("rpc client unavailable"))?;
        // Tx is mined synchronously inside `advance_block`, but the
        // receipt store occasionally lags a few ms behind. Brief poll.
        for _ in 0..20 {
            let receipt: Option<serde_json::Value> =
                rpc.request("eth_getTransactionReceipt", (tx_hash,)).await?;
            if let Some(r) = receipt {
                return Ok(r);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!("no receipt for tx {tx_hash} after 1s")
    }

    async fn query_raw(&self, q: &str, options: serde_json::Value) -> Result<QueryResponse> {
        let rpc = self
            .node
            .rpc_client()
            .ok_or_else(|| eyre!("rpc client unavailable"))?;
        Ok(rpc
            .request::<QueryResponse, _>("arkiv_query", (q, options))
            .await?)
    }
}

// ── ABI packing helpers ──────────────────────────────────────────────

fn build_attributes(
    string_attrs: &[(String, String)],
    numeric_attrs: &[(String, U256)],
    entity_key_attrs: &[(String, B256)],
) -> Vec<Attribute> {
    let mut out =
        Vec::with_capacity(string_attrs.len() + numeric_attrs.len() + entity_key_attrs.len());
    for (k, v) in string_attrs {
        out.push(pack_string_attr(k, v));
    }
    for (k, v) in numeric_attrs {
        out.push(pack_numeric_attr(k, *v));
    }
    for (k, v) in entity_key_attrs {
        out.push(pack_entity_key_attr(k, *v));
    }
    out
}

fn pack_mime(s: &str) -> Mime128 {
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

fn pack_string_attr(name: &str, value: &str) -> Attribute {
    let name = pack_ident32(name);
    let mut buf = [0u8; 128];
    let v = value.len().min(128);
    buf[..v].copy_from_slice(&value.as_bytes()[..v]);
    Attribute {
        name,
        valueType: ATTR_STRING,
        value: [
            FixedBytes::from_slice(&buf[..32]),
            FixedBytes::from_slice(&buf[32..64]),
            FixedBytes::from_slice(&buf[64..96]),
            FixedBytes::from_slice(&buf[96..128]),
        ],
    }
}

fn pack_numeric_attr(name: &str, value: U256) -> Attribute {
    Attribute {
        name: pack_ident32(name),
        valueType: ATTR_UINT,
        value: [
            FixedBytes::from(value.to_be_bytes::<32>()),
            FixedBytes::ZERO,
            FixedBytes::ZERO,
            FixedBytes::ZERO,
        ],
    }
}

fn pack_entity_key_attr(name: &str, value: B256) -> Attribute {
    Attribute {
        name: pack_ident32(name),
        valueType: ATTR_ENTITY_KEY,
        value: [value, FixedBytes::ZERO, FixedBytes::ZERO, FixedBytes::ZERO],
    }
}

fn pack_ident32(s: &str) -> FixedBytes<32> {
    let mut buf = [0u8; 32];
    let n = s.len().min(32);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
    FixedBytes::from(buf)
}

/// `keccak256(abi.encodePacked(chainId, ARKIV_ADDRESS, sender, nonce))` —
/// the same formula the precompile uses, and the same one the SDK runs
/// locally to predict the key before submitting.
fn compute_entity_key(chain_id: u64, arkiv_addr: Address, sender: Address, nonce: u32) -> B256 {
    let mut buf = Vec::with_capacity(32 + 20 + 20 + 4);
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    buf.extend_from_slice(arkiv_addr.as_slice());
    buf.extend_from_slice(sender.as_slice());
    buf.extend_from_slice(&nonce.to_be_bytes());
    keccak256(&buf)
}

// ── Payload attrs ────────────────────────────────────────────────────

pub const fn eth_payload_attributes(timestamp: u64) -> PayloadAttributes {
    PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(B256::ZERO),
        slot_number: None,
    }
}
