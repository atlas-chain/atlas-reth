use arkiv_node::{ArkivExt, ArkivOpNode, has_arkiv_system_account, install};
use clap::Parser;
use eyre::{Result, bail};
use reth_optimism_cli::{Cli, chainspec::OpChainSpecParser};
use reth_optimism_node::OpNode;

fn main() -> Result<()> {
    Cli::<OpChainSpecParser, ArkivExt>::parse().run(|builder, ext| async move {
        let ArkivExt { rollup } = ext;

        // Hard fail if the system account is missing — v2 has no fallback mode.
        if !has_arkiv_system_account(&builder.config().chain) {
            bail!(
                "Arkiv system account not detected at {} in the loaded chainspec; \
                 arkiv-node currently requires a chainspec with the Arkiv system account",
                arkiv_genesis::SYSTEM_ACCOUNT_ADDRESS,
            );
        }

        // ArkivOpNode swaps OpNode's EvmFactory for `ArkivOpEvmFactory`,
        // which installs the Arkiv precompile at ARKIV_ADDRESS on every
        // fresh revm instance. `install` adds the `arkiv_*` RPC namespace.
        let arkiv_node = ArkivOpNode::new(OpNode::new(rollup));
        let node = install(builder.node(arkiv_node));
        let handle = node.launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}
