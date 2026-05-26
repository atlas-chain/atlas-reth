use arkiv_node::{ArkivExt, ArkivOpNode, install};
use clap::Parser;
use eyre::Result;
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::OpNode;

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let ArkivExt { rollup } = ext;

        // ArkivOpNode swaps OpNode's EvmFactory for `ArkivOpEvmFactory`,
        // which installs the Arkiv precompile at ARKIV_ADDRESS on every
        // fresh revm instance. `install` adds the `arkiv_*` RPC namespace.
        // The system account that hosts the precompile's storage is
        // materialised lazily on the first op — no chainspec dependency.
        let arkiv_node = ArkivOpNode::new(OpNode::new(rollup));
        let node = install(builder.node(arkiv_node));
        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}
