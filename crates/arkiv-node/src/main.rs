use arkiv_node::{ArkivEthExecutorBuilder, install, protocol_schedule};
use clap::Parser;
use eyre::Result;
use reth_ethereum_cli::{chainspec::EthereumChainSpecParser, interface::Cli};
use reth_node_ethereum::{EthereumAddOns, EthereumNode};

fn main() -> Result<()> {
    Cli::<EthereumChainSpecParser>::parse().run(|builder, _| async move {
        protocol_schedule::spawn_from_env();

        // The custom executor swaps EthereumNode's EvmFactory for
        // `ArkivEthEvmFactory`, which installs the Arkiv precompile at
        // ARKIV_ADDRESS on every fresh revm instance. `install` adds the
        // `arkiv_*` RPC namespace. The system account that hosts the
        // precompile's storage is materialised lazily on the first op.
        let components = EthereumNode::components().executor(ArkivEthExecutorBuilder);
        let node = install(
            builder
                .with_types::<EthereumNode>()
                .with_components(components)
                .with_add_ons(EthereumAddOns::default()),
        );
        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}
