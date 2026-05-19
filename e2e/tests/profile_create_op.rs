//! Per-tx timing trace for the CREATE path.
//!
//! Boots once, runs N_CREATE entity creations under a `tracing-chrome`
//! layer that captures the spans installed in:
//!
//!   - `arkiv_node::evm::ArkivOpEvm::transact_raw`  →  `evm_tx`
//!   - `arkiv_node::precompile`                     →  `precompile_call`
//!                                                      `precompile_decode`
//!                                                      `precompile_dispatch`
//!   - `arkiv_entitydb::create`                     →  `entitydb_create`
//!
//! Output:
//!
//!   <workspace>/tmp/arkiv.trace.json   chrome-trace format, loadable
//!                                       in https://ui.perfetto.dev
//!                                       (drag & drop). `tmp/` is in
//!                                       the workspace .gitignore.
//!
//! In Perfetto:
//!   - Sync spans (evm_tx ⊃ precompile_call ⊃ … ⊃ entitydb_create)
//!     nest on a single EVM worker track. Click a parent to see its
//!     duration; click a child for the same.
//!   - Drag-select a region → "Slices" tab shows aggregate counts /
//!     totals per span name (median, sum, count). That's the layer
//!     breakdown.
//!   - Each block carries `payload_bytes` and `n_attrs` as fields
//!     (visible on hover) so outliers are spottable.

use std::time::Instant;

use arkiv_e2e::{CreateOp, WorldOps, boot};
use eyre::Result;
use tracing_chrome::ChromeLayerBuilder;
use tracing_subscriber::{EnvFilter, prelude::*};

const N_CREATE: usize = 100;

// Resolved at compile time from the e2e crate's manifest dir; lands at
// `arkiv-op-reth/tmp/arkiv.trace.json` which is gitignored.
const TRACE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tmp/arkiv.trace.json");

#[tokio::test(flavor = "multi_thread")]
async fn profile_create_op() -> Result<()> {
    if let Some(parent) = std::path::Path::new(TRACE_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Default `Threaded` trace style: spans are emitted on the OS
    // thread they're entered on, so sync spans (evm_tx, precompile_*,
    // entitydb_*) appear *nested* on the EVM worker track in Perfetto
    // — read parent→child top-to-bottom, durations are correct.
    //
    // `Async` style would put each span name on its own track and
    // require manual time-range correlation for the layer breakdown.
    // We only instrument sync code so we don't pay the async-span-
    // fragmentation cost of `Threaded`.
    let (chrome_layer, _flush_guard) = ChromeLayerBuilder::new()
        .file(TRACE_PATH)
        .include_args(true)
        .build();

    // Default to capturing arkiv targets at debug; RUST_LOG overrides.
    // The `evm_tx` span (in `arkiv_node::evm`) wraps each per-tx EVM
    // execution, so contract+precompile+db time = `evm_tx` and block
    // production / RPC / receipt-poll overhead lives outside it.
    //
    // Install BEFORE boot. `boot()` internally calls
    // `reth_tracing::init_test_tracing()` which uses `try_init()` and
    // is a no-op once our global default is set — we lose reth's
    // stderr fmt output but capture every arkiv span into the trace.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "arkiv_entitydb=debug,\
             arkiv_node::evm=debug,\
             arkiv_node::precompile=debug",
        )
    });

    let subscriber = tracing_subscriber::registry()
        .with(chrome_layer)
        .with(filter);
    let _ = tracing::subscriber::set_global_default(subscriber);

    eprintln!("==> tracing-chrome active → {TRACE_PATH}");

    let mut world = boot().await?;

    let t0 = Instant::now();
    for i in 0..N_CREATE {
        world
            .create(
                0,
                CreateOp::new()
                    .payload(format!("create-{i}").into_bytes())
                    .content_type("application/octet-stream")
                    .btl(10_000)
                    .string_attr("workload", "create-bench")
                    .numeric_attr("idx", i as u64),
            )
            .await?;
    }
    eprintln!("==> CREATE  x{N_CREATE} in {:?}", t0.elapsed());

    eprintln!();
    eprintln!("==> wrote {TRACE_PATH}");
    eprintln!("==> open https://ui.perfetto.dev and drag the file onto it");

    // _flush_guard drops here → trace file flushed and closed.
    Ok(())
}
