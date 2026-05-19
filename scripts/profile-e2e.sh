#!/usr/bin/env bash
# Run profile_workload e2e test and produce a chrome-trace JSON for
# inspection in https://ui.perfetto.dev.
#
# Output:
#   <workspace>/tmp/arkiv.trace.json   chrome-trace format (tmp/ is gitignored)
#
# How to view:
#   1. Open https://ui.perfetto.dev in any browser.
#   2. Drag tmp/arkiv.trace.json onto the page (or "Open trace file").
#   3. In the timeline:
#        - Each colored block is a span; width = duration.
#        - Click a block to see its details panel.
#        - Cmd-F to search span names (entitydb_create, precompile_dispatch, …).
#        - Select a time range with drag → "Slices" tab shows aggregate
#          duration per span name — that's your layer attribution.
#
# Override RUST_LOG to broaden / narrow what's captured:
#   RUST_LOG="arkiv_entitydb=trace,arkiv_node=debug" ./scripts/profile-e2e.sh
#
# Knobs (env vars):
#   TEST_NAME   test function to run            (default: profile_create_op)
#   TEST_FILE   test file (under e2e/tests/)    (default: profile_create_op)

set -euo pipefail

TEST_NAME="${TEST_NAME:-profile_create_op}"
TEST_FILE="${TEST_FILE:-profile_create_op}"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$WORKSPACE_ROOT"

echo "==> Running $TEST_NAME (release) ..."
echo

cargo test --release \
  -p arkiv-e2e --test "$TEST_FILE" \
  --no-fail-fast \
  -- --exact "$TEST_NAME" --nocapture

TRACE_PATH="$WORKSPACE_ROOT/tmp/arkiv.trace.json"

echo
echo "==> Trace file:"
ls -lh "$TRACE_PATH" 2>/dev/null || echo "    (trace not found at $TRACE_PATH)"
echo
echo "    Load it at: https://ui.perfetto.dev"
echo "    (drag the JSON file onto the page)"
