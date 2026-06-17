//! Custom Ethereum EVM stack that installs the Arkiv precompile on every
//! fresh revm instance.
//!
//! `ArkivEthEvmFactory` wraps the upstream [`EthEvmFactory`], inserts
//! the Arkiv precompile in both canonical and inspector EVMs, and keeps
//! the existing `evm_tx` tracing span around each raw transaction.

use alloy_evm::{
    Database, EthEvmFactory, Evm, EvmEnv, EvmFactory,
    eth::EthEvmContext,
    precompiles::PrecompilesMap,
    revm::{
        Inspector,
        context::{BlockEnv, CfgEnv, TxEnv, result::ResultAndState},
        context_interface::result::{EVMError, HaltReason},
        inspector::NoOpInspector,
        primitives::hardfork::SpecId,
    },
};
use alloy_primitives::{Address, Bytes};
use reth_chainspec::ChainSpec;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm_ethereum::{EthEvm, EthEvmConfig};
use reth_node_builder::{BuilderContext, FullNodeTypes, NodeTypes, components::ExecutorBuilder};

use arkiv_genesis::ARKIV_ADDRESS;

use crate::precompile::arkiv_precompile;

/// EVM factory that defers to the default Ethereum factory and inserts
/// the Arkiv precompile at [`ARKIV_ADDRESS`] on every fresh EVM.
#[derive(Debug, Clone, Default)]
pub struct ArkivEthEvmFactory {
    inner: EthEvmFactory,
}

impl ArkivEthEvmFactory {
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

impl EvmFactory for ArkivEthEvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = ArkivEthEvm<DB, I>;
    type Context<DB: Database> = EthEvmContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let mut evm = self.inner.create_evm(db, input);
        self.install(&mut evm);
        ArkivEthEvm { inner: evm }
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let mut evm = self.inner.create_evm_with_inspector(db, input, inspector);
        self.install(&mut evm);
        ArkivEthEvm { inner: evm }
    }
}

/// Thin wrapper around [`EthEvm`] that spans each raw transaction as
/// `evm_tx` for the direct profiling tests and trace analysis.
pub struct ArkivEthEvm<DB: Database, I> {
    inner: EthEvm<DB, I, PrecompilesMap>,
}

impl<DB, I> Evm for ArkivEthEvm<DB, I>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
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

/// Executor builder that produces an Ethereum EVM config backed by
/// [`ArkivEthEvmFactory`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ArkivEthExecutorBuilder;

impl<N> ExecutorBuilder<N> for ArkivEthExecutorBuilder
where
    N: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = EthPrimitives>>,
{
    type EVM = EthEvmConfig<ChainSpec, ArkivEthEvmFactory>;

    async fn build_evm(self, ctx: &BuilderContext<N>) -> eyre::Result<Self::EVM> {
        Ok(EthEvmConfig::new_with_evm_factory(
            ctx.chain_spec(),
            ArkivEthEvmFactory::new(),
        ))
    }
}
