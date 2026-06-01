//! Custom EVM stack that installs the Arkiv precompile on every fresh
//! revm instance.
//!
//! Layering (each layer exists for a specific reason — keep the
//! comments below in mind before flattening):
//!
//! - [`ArkivOpEvmFactory`] — wraps [`OpEvmFactory<OpTx>`] and inserts
//!   the precompile in `create_evm` / `create_evm_with_inspector`. Both
//!   methods must agree, otherwise tracing diverges from execution.
//! - [`ArkivOpEvmConfig`] — local newtype around `OpEvmConfig` so we
//!   can satisfy the orphan rule when implementing
//!   `ConfigureEngineEvm<OpExecData>` (upstream only impls it for the
//!   default `OpEvmFactory<OpTx>` variant of `OpEvmConfig`, so our
//!   custom-factory variant gets no impl from the open OP crates).
//!   `ConfigureEvm` is a thin passthrough; `ConfigureEngineEvm`
//!   delegates to a stored default-factory `OpEvmConfig` (whose
//!   implementation body doesn't read the factory).
//! - [`ArkivOpExecutorBuilder`] — replaces `OpExecutorBuilder` in the
//!   node's component bundle so the factory is the one used at runtime.
//! - [`ArkivOpNode`] — replaces `OpNode` so `add_ons()` matches the
//!   swapped Components type. Forwards everything else to the inner
//!   `OpNode`.
//! - [`ArkivLocalPayloadAttributesBuilder`] — verbatim copy of
//!   op-reth's private `OpLocalPayloadAttributesBuilder`, required by
//!   `DebugNode::local_payload_attributes_builder`.

use std::sync::Arc;

// `revm` types come via `alloy_evm::revm` — that re-exports the matching
// revm 38 line. The workspace also declares `revm 38.0.0` directly (used
// by `precompile.rs`); going through the alloy_evm re-export guarantees
// we pick up the same line as `alloy-op-evm` and avoids silent type
// mismatches across the two `revm` editions if they ever drift.
use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    block::BlockExecutor,
    precompiles::PrecompilesMap,
    revm::{
        Inspector,
        context::{BlockEnv, CfgEnv, result::ResultAndState},
        context_interface::result::EVMError,
        database::State,
        inspector::NoOpInspector,
    },
};
use alloy_op_evm::{
    OpBlockExecutorFactory, OpEvm, OpEvmContext, OpEvmFactory, OpTxError,
    post_exec::{
        PostExecEvmFactoryAdapter, PostExecEvmFactoryHooks, PostExecExecutedTx,
        PostExecExecutorExt, PostExecTxContext,
    },
};
use alloy_primitives::{Address, Bytes};
use op_revm::{OpHaltReason, OpSpecId};

use alloy_consensus::Header;
use alloy_eips::eip1559::BaseFeeParams;
use alloy_hardforks::EthereumHardforks;
use op_alloy_consensus::EIP1559ParamError;
use reth_evm::{EvmEnvFor, ExecutionCtxFor, execute::BlockBuilder};
use reth_node_api::PayloadAttributesBuilder;
use reth_node_builder::{
    BuilderContext, ConfigureEngineEvm, ConfigureEvm, DebugNode, FullNodeComponents, FullNodeTypes,
    Node, NodeAdapter, NodeComponentsBuilder, NodeTypes,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder},
    rpc::BasicEngineValidatorBuilder,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_node::{
    ConfigurePostExecEvm, OpAddOns, OpBlockAssembler, OpEngineApiBuilder, OpEngineTypes,
    OpEvmConfig, OpNextBlockEnvAttributes, OpNode, OpRethReceiptBuilder, OpStorage, OpTx,
    PostExecMode,
    node::{
        OpConsensusBuilder, OpEngineValidatorBuilder, OpNetworkBuilder, OpPayloadBuilder,
        OpPoolBuilder,
    },
    payload::{OpExecData, OpPayloadAttributes, OpPayloadAttrs},
    rpc::OpEthApiBuilder,
};
use reth_optimism_primitives::{OpBlock, OpPrimitives};
use reth_primitives_traits::{NodePrimitives, SealedBlock, SealedHeader};

use arkiv_genesis::ARKIV_ADDRESS;

use crate::precompile::arkiv_precompile;

// ─────────────────────────────────────────────────────────────────────
// ArkivOpEvmFactory — wraps OpEvmFactory<OpTx>; installs the precompile
// ─────────────────────────────────────────────────────────────────────

/// EVM factory that defers to the default OP factory and inserts the
/// Arkiv precompile at [`ARKIV_ADDRESS`] on every fresh EVM
/// (both canonical execution and inspector-instrumented contexts).
#[derive(Debug, Clone, Default)]
pub struct ArkivOpEvmFactory {
    inner: OpEvmFactory<OpTx>,
}

impl ArkivOpEvmFactory {
    pub fn new() -> Self {
        Self::default()
    }

    fn install<E>(&self, evm: &mut E)
    where
        E: Evm<Precompiles = PrecompilesMap>,
    {
        let precompile = arkiv_precompile();
        evm.precompiles_mut()
            .apply_precompile(&ARKIV_ADDRESS, |_existing| Some(precompile));
    }
}

impl EvmFactory for ArkivOpEvmFactory {
    // Mirror `OpEvmFactory<OpTx>`'s associated types concretely. Forwarding
    // through `<OpEvmFactory<OpTx> as EvmFactory>::X` projections compiles
    // here but leaves bounds like `Self::Tx: FromRecoveredTx<_>` unresolved
    // in downstream `ConfigureEvm` / `OpAddOns` impls, because the compiler
    // does not always normalise nested projections through trait bounds.
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = ArkivOpEvm<DB, I>;
    type Context<DB: Database> = OpEvmContext<DB>;
    type Tx = OpTx;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError, OpTxError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let mut evm = self.inner.create_evm(db, input);
        self.install(&mut evm);
        ArkivOpEvm { inner: evm }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let mut evm = self.inner.create_evm_with_inspector(db, input, inspector);
        self.install(&mut evm);
        ArkivOpEvm { inner: evm }
    }
}

// `OpBlockExecutorFactory` only implements `BlockExecutorFactory` for the
// default `OpEvmFactory` or a custom factory wrapped in
// `PostExecEvmFactoryAdapter`, so the adapter requires our factory to
// expose the SDM post-exec hooks. We delegate to the inner `OpEvm`, which
// carries the real implementation.
impl PostExecEvmFactoryHooks for ArkivOpEvmFactory {
    fn begin_post_exec_tx<DB, I>(evm: &mut Self::Evm<DB, I>, ctx: PostExecTxContext)
    where
        DB: Database,
        I: Inspector<Self::Context<DB>>,
    {
        evm.inner.begin_post_exec_tx(ctx);
    }

    fn take_last_post_exec_tx_result<DB, I>(evm: &mut Self::Evm<DB, I>) -> PostExecExecutedTx
    where
        DB: Database,
        I: Inspector<Self::Context<DB>>,
    {
        evm.inner.take_last_post_exec_tx_result()
    }
}

// ─────────────────────────────────────────────────────────────────────
// ArkivOpEvm — newtype wrapper around OpEvm that spans `transact_raw`
// ─────────────────────────────────────────────────────────────────────
//
// Wraps the OP EVM produced by `OpEvmFactory` so each per-tx
// `transact_raw` call is enclosed in an `evm_tx` tracing span. The
// span boundary captures *only* the EVM-internal execution of a
// transaction (Solidity bytecode + nested precompile call). Everything
// outside — block assembly, payload building, state-root, sealing, RPC,
// receipt polling — runs outside this span, so subtracting children
// (e.g. `precompile_call`) from `evm_tx` yields a clean
// contract-execution slice.
//
// Every `Evm` trait method that isn't `transact_raw` is a pass-through.

pub struct ArkivOpEvm<DB: Database, I> {
    inner: OpEvm<DB, I, PrecompilesMap, OpTx>,
}

impl<DB, I> Evm for ArkivOpEvm<DB, I>
where
    DB: Database,
    I: Inspector<OpEvmContext<DB>>,
{
    type DB = DB;
    type Tx = OpTx;
    type Error = EVMError<DB::Error, OpTxError>;
    type HaltReason = OpHaltReason;
    type Spec = OpSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &Self::BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &CfgEnv<Self::Spec> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let _span = tracing::debug_span!("evm_tx").entered();
        self.inner.transact_raw(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        self.inner.components()
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        self.inner.components_mut()
    }
}

// ─────────────────────────────────────────────────────────────────────
// ArkivOpEvmConfig — local newtype wrapper around OpEvmConfig
// ─────────────────────────────────────────────────────────────────────
//
// Required because the orphan rule does not let us impl the foreign
// trait `ConfigureEngineEvm<OpExecData>` directly on the foreign type
// `OpEvmConfig<_, _, _, ArkivOpEvmFactory>` — the local type only
// appears as a generic argument inside a foreign type, which is not
// enough to satisfy E0117. Wrapping in a local newtype lifts the local
// type to the head position and unblocks the impls.
//
// `ConfigureEvm` is a pure passthrough; `ConfigureEngineEvm<OpExecData>`
// delegates to a temporary default-factory `OpEvmConfig` built from the
// same chain spec, since the upstream impl body does not actually
// depend on the EVM factory — it only reads `chain_spec()` and
// constructs values from the payload data.

type InnerEvmConfig = OpEvmConfig<
    OpChainSpec,
    OpPrimitives,
    OpRethReceiptBuilder,
    PostExecEvmFactoryAdapter<ArkivOpEvmFactory>,
>;
type DefaultEvmConfig = OpEvmConfig<OpChainSpec, OpPrimitives, OpRethReceiptBuilder>;

#[derive(Debug, Clone)]
pub struct ArkivOpEvmConfig {
    inner: InnerEvmConfig,
    /// Default-factory `OpEvmConfig` sharing the same chain spec. Used by
    /// the `ConfigureEngineEvm<OpExecData>` shim (whose upstream impl body
    /// does not depend on the EVM factory). Stored as a field rather than
    /// constructed on the fly so the iterator returned by
    /// `tx_iterator_for_payload` can outlive the call.
    inner_default: DefaultEvmConfig,
}

impl ArkivOpEvmConfig {
    pub fn new(chain_spec: Arc<OpChainSpec>) -> Self {
        let executor_factory = OpBlockExecutorFactory::new(
            OpRethReceiptBuilder::default(),
            chain_spec.clone(),
            PostExecEvmFactoryAdapter::new(ArkivOpEvmFactory::new()),
        );
        let inner = OpEvmConfig {
            block_assembler: OpBlockAssembler::new(chain_spec.clone()),
            executor_factory,
            sdm_enabled: false,
            _pd: core::marker::PhantomData,
        };
        let inner_default = OpEvmConfig::new(chain_spec, OpRethReceiptBuilder::default());
        Self {
            inner,
            inner_default,
        }
    }
}

impl ConfigureEvm for ArkivOpEvmConfig {
    // Concrete types (rather than `<InnerEvmConfig as ConfigureEvm>::X`
    // projections) so downstream bounds like
    // `<Self as ConfigureEvm>::NextBlockEnvCtx: BuildNextEnv<...>` in
    // `OpAddOns: NodeAddOns` can be checked without normalising through
    // `OpEvmConfig`'s blanket `ConfigureEvm` impl. Rust's trait solver is
    // sometimes unable to do that normalisation under nested bounds.
    type Primitives = OpPrimitives;
    type Error = EIP1559ParamError;
    type NextBlockEnvCtx = OpNextBlockEnvAttributes;
    type BlockExecutorFactory = OpBlockExecutorFactory<
        OpRethReceiptBuilder,
        Arc<OpChainSpec>,
        PostExecEvmFactoryAdapter<ArkivOpEvmFactory>,
    >;
    type BlockAssembler = OpBlockAssembler<OpChainSpec>;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self.inner.block_executor_factory()
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self.inner.block_assembler()
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<OpBlock>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        self.inner.context_for_block(block)
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<Header>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
    }
}

impl ConfigureEngineEvm<OpExecData> for ArkivOpEvmConfig {
    fn evm_env_for_payload(
        &self,
        payload: &OpExecData,
    ) -> Result<EvmEnvFor<Self>, <Self as ConfigureEvm>::Error> {
        self.inner_default.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a OpExecData,
    ) -> Result<ExecutionCtxFor<'a, Self>, <Self as ConfigureEvm>::Error> {
        self.inner_default.context_for_payload(payload)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &OpExecData,
    ) -> Result<impl reth_evm::ExecutableTxIterator<Self>, <Self as ConfigureEvm>::Error> {
        self.inner_default.tx_iterator_for_payload(payload)
    }
}

// `OpPayloadBuilder` now requires the EVM config to implement
// `ConfigurePostExecEvm` (SDM post-exec support). Upstream impls it for
// `OpEvmConfig<.., PostExecEvmFactoryAdapter<F>>`, which is exactly our
// `inner` — so forward both methods to it.
impl ConfigurePostExecEvm for ArkivOpEvmConfig {
    fn post_exec_executor_for_block<'a, DB: Database>(
        &'a self,
        db: &'a mut State<DB>,
        block: &'a SealedBlock<<Self::Primitives as NodePrimitives>::Block>,
        post_exec_mode: PostExecMode,
    ) -> Result<
        impl BlockExecutor<
            Transaction = <Self::Primitives as NodePrimitives>::SignedTx,
            Receipt = <Self::Primitives as NodePrimitives>::Receipt,
        > + PostExecExecutorExt
        + 'a,
        Self::Error,
    > {
        self.inner
            .post_exec_executor_for_block(db, block, post_exec_mode)
    }

    fn post_exec_builder_for_next_block<'a, DB: Database + 'a>(
        &'a self,
        db: &'a mut State<DB>,
        parent: &'a SealedHeader<<Self::Primitives as NodePrimitives>::BlockHeader>,
        attributes: Self::NextBlockEnvCtx,
        post_exec_mode: PostExecMode,
    ) -> Result<
        impl BlockBuilder<Primitives = Self::Primitives, Executor: PostExecExecutorExt> + 'a,
        Self::Error,
    > {
        self.inner
            .post_exec_builder_for_next_block(db, parent, attributes, post_exec_mode)
    }
}

// ─────────────────────────────────────────────────────────────────────
// ArkivOpExecutorBuilder
// ─────────────────────────────────────────────────────────────────────

/// Drop-in replacement for `OpExecutorBuilder` that produces an
/// [`ArkivOpEvmConfig`] (which uses [`ArkivOpEvmFactory`] internally).
#[derive(Debug, Clone, Default)]
pub struct ArkivOpExecutorBuilder;

impl<N> ExecutorBuilder<N> for ArkivOpExecutorBuilder
where
    N: FullNodeTypes<Types: NodeTypes<ChainSpec = OpChainSpec, Primitives = OpPrimitives>>,
{
    type EVM = ArkivOpEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<N>) -> eyre::Result<Self::EVM> {
        Ok(ArkivOpEvmConfig::new(ctx.chain_spec()))
    }
}

// ─────────────────────────────────────────────────────────────────────
// ArkivOpNode — thin wrapper around `OpNode` that swaps the executor
// ─────────────────────────────────────────────────────────────────────
//
// Required because `OpNode::add_ons()` returns `OpAddOns<NodeAdapter<N,
// OpDefaultComponents>, ...>` — the AddOns is hardcoded to the default
// component bundle (with `OpEvmConfig<.., OpEvmFactory<OpTx>>`). When we
// swap in `ArkivOpExecutorBuilder` the `Components` type changes, so
// `op_node.add_ons()` no longer matches the components we built and
// `with_add_ons` rejects it.
//
// The fix mirrors the upstream `examples/custom-node` pattern: define a
// local `Node` impl whose `ComponentsBuilder` and `AddOns` are typed
// consistently against `ArkivOpExecutorBuilder`, and forward to the
// inner `OpNode` for everything else.

#[derive(Debug, Clone, Default)]
pub struct ArkivOpNode {
    inner: OpNode,
}

impl ArkivOpNode {
    pub fn new(inner: OpNode) -> Self {
        Self { inner }
    }
}

impl NodeTypes for ArkivOpNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type Storage = OpStorage;
    type Payload = OpEngineTypes;
}

impl<N> Node<N> for ArkivOpNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        OpPoolBuilder,
        BasicPayloadServiceBuilder<OpPayloadBuilder>,
        OpNetworkBuilder,
        ArkivOpExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns = OpAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        OpEthApiBuilder,
        OpEngineValidatorBuilder,
        OpEngineApiBuilder<OpEngineValidatorBuilder>,
        BasicEngineValidatorBuilder<OpEngineValidatorBuilder>,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.inner.components().executor(ArkivOpExecutorBuilder)
    }

    fn add_ons(&self) -> Self::AddOns {
        self.inner.add_ons_builder().build()
    }
}

// Required for `launch_with_debug_capabilities()`. Body is a faithful
// copy of upstream `OpNode`'s impl (op-reth/crates/node/src/node.rs:337
// and the private `OpLocalPayloadAttributesBuilder` it constructs at
// :74-137); the type lives in the op-reth `node.rs` module privately so
// we cannot reuse it directly.
impl<N> DebugNode<N> for ArkivOpNode
where
    N: FullNodeComponents<Types = Self>,
{
    type RpcBlock = alloy_rpc_types_eth::Block<op_alloy_consensus::OpTxEnvelope>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_node_api::BlockTy<Self> {
        rpc_block.into_consensus()
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as reth_node_api::PayloadTypes>::PayloadAttributes>
    {
        ArkivLocalPayloadAttributesBuilder {
            chain_spec: Arc::new(chain_spec.clone()),
        }
    }
}

/// Local-mining payload attributes builder. Verbatim copy of
/// `OpLocalPayloadAttributesBuilder` from op-reth's private module; kept
/// in sync with `op-reth/v2.2.5`.
struct ArkivLocalPayloadAttributesBuilder {
    chain_spec: Arc<OpChainSpec>,
}

impl PayloadAttributesBuilder<OpPayloadAttrs> for ArkivLocalPayloadAttributesBuilder {
    fn build(
        &self,
        parent: &reth_primitives_traits::SealedHeader<alloy_consensus::Header>,
    ) -> OpPayloadAttrs {
        use alloy_consensus::BlockHeader;
        use alloy_primitives::{Address, B64};

        let timestamp = std::cmp::max(
            parent.timestamp().saturating_add(1),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );

        let eth_attrs = alloy_rpc_types_engine::PayloadAttributes {
            timestamp,
            prev_randao: alloy_primitives::B256::random(),
            suggested_fee_recipient: Address::random(),
            withdrawals: self
                .chain_spec
                .is_shanghai_active_at_timestamp(timestamp)
                .then(Default::default),
            parent_beacon_block_root: self
                .chain_spec
                .is_cancun_active_at_timestamp(timestamp)
                .then(alloy_primitives::B256::random),
            slot_number: None,
        };

        // OP Mainnet `setL1BlockValuesEcotone` system tx at index 0 of
        // block 124665056. Hard-coded for dev mode so blocks pass the
        // OP "first tx must be a deposit" rule.
        const TX_SET_L1_BLOCK: [u8; 251] = alloy_primitives::hex!(
            "7ef8f8a0683079df94aa5b9cf86687d739a60a9b4f0835e520ec4d664e2e415dca17a6df94deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e200000146b000f79c500000000000000040000000066d052e700000000013ad8a3000000000000000000000000000000000000000000000000000000003ef1278700000000000000000000000000000000000000000000000000000000000000012fdf87b89884a61e74b322bbcf60386f543bfae7827725efaaf0ab1de2294a590000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985"
        );

        let default_params = BaseFeeParams::optimism();
        let denominator = std::env::var("OP_DEV_EIP1559_DENOMINATOR")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_params.max_change_denominator as u32);
        let elasticity = std::env::var("OP_DEV_EIP1559_ELASTICITY")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(default_params.elasticity_multiplier as u32);
        let gas_limit = std::env::var("OP_DEV_GAS_LIMIT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());

        let mut eip1559_bytes = [0u8; 8];
        eip1559_bytes[0..4].copy_from_slice(&denominator.to_be_bytes());
        eip1559_bytes[4..8].copy_from_slice(&elasticity.to_be_bytes());

        OpPayloadAttrs(OpPayloadAttributes {
            payload_attributes: eth_attrs,
            transactions: Some(vec![TX_SET_L1_BLOCK.into()]),
            no_tx_pool: None,
            gas_limit,
            eip_1559_params: Some(B64::from(eip1559_bytes)),
            min_base_fee: Some(0),
        })
    }
}
