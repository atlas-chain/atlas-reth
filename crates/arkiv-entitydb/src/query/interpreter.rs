//! Tree-walking evaluator for the parsed query AST + the top-level
//! [`execute`] entry point that callers (RPC, tests) should use.
//!
//! `Query::evaluate` recursively turns a [`Query`] into a [`Bitmap`] of
//! matching entity IDs by issuing point reads against a
//! [`StateAdapter`]. No normalization pass — `Not` is evaluated as
//! `$all \ eval(inner)`, which means each `Not` (or `!=` / `NOT IN`)
//! costs one extra `$all` read. Acceptable for the small queries we
//! expect; can revisit if profiling says otherwise.
//!
//! [`execute`] is the convenience wrapper that combines parse →
//! evaluate → paginate → resolve. The RPC layer calls only this; the
//! lower-level `parse` / `Query::evaluate` / `resolve_id` remain
//! public for tests and advanced consumers.
//!
//! Integration tests live in `crates/arkiv-entitydb/tests/query_eval.rs`.

use eyre::Result;

use super::parser::{AnnotKey, AnnotVal, Query, parse};
use crate::{Bitmap, EntityRlp, StateAdapter, all_entities, read_index_tree, read_pair_bitmap, resolve_id};

impl Query {
    /// Evaluate the AST against `state`, returning the bitmap of
    /// matching entity IDs.
    pub fn evaluate<S: StateAdapter>(&self, state: &mut S) -> Result<Bitmap> {
        eval(self, state)
    }
}

fn eval<S: StateAdapter>(query: &Query, state: &mut S) -> Result<Bitmap> {
    match query {
        Query::All => all_entities(state),

        Query::Eq { key, value } => read_eq(state, key, value),
        Query::Neq { key, value } => {
            let mut all = all_entities(state)?;
            let hit = read_eq(state, key, value)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::In { key, values } => read_in(state, key, values),
        Query::NotIn { key, values } => {
            let mut all = all_entities(state)?;
            let hit = read_in(state, key, values)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::And(left, right) => {
            let mut l = eval(left, state)?;
            // Short-circuit: AND with empty is empty, no need to load
            // the right side.
            if l.is_empty() {
                return Ok(l);
            }
            let r = eval(right, state)?;
            l.intersect_with(&r);
            Ok(l)
        }
        Query::Or(left, right) => {
            let mut l = eval(left, state)?;
            let r = eval(right, state)?;
            l.union_with(&r);
            Ok(l)
        }
        Query::Not(inner) => {
            let mut all = all_entities(state)?;
            let hit = eval(inner, state)?;
            all.subtract(&hit);
            Ok(all)
        }

        Query::Gt { key, value } => {
            let attr_key = key.pair_key_bytes();
            let tree = read_index_tree(state, attr_key)?;
            let mut result = Bitmap::new();
            for val in tree.iter_gt(&value.0) {
                result.union_with(&read_pair_bitmap(state, attr_key, &val)?);
            }
            Ok(result)
        }
        Query::Gte { key, value } => {
            let attr_key = key.pair_key_bytes();
            let tree = read_index_tree(state, attr_key)?;
            let mut result = Bitmap::new();
            for val in tree.iter_gte(&value.0) {
                result.union_with(&read_pair_bitmap(state, attr_key, &val)?);
            }
            Ok(result)
        }
        Query::Lt { key, value } => {
            let attr_key = key.pair_key_bytes();
            let tree = read_index_tree(state, attr_key)?;
            let mut result = Bitmap::new();
            for val in tree.iter_lt(&value.0) {
                result.union_with(&read_pair_bitmap(state, attr_key, &val)?);
            }
            Ok(result)
        }
        Query::Lte { key, value } => {
            let attr_key = key.pair_key_bytes();
            let tree = read_index_tree(state, attr_key)?;
            let mut result = Bitmap::new();
            for val in tree.iter_lte(&value.0) {
                result.union_with(&read_pair_bitmap(state, attr_key, &val)?);
            }
            Ok(result)
        }
        Query::Glob { key, value } => {
            let attr_key = key.pair_key_bytes();
            let tree = read_index_tree(state, attr_key)?;
            let mut result = Bitmap::new();
            for val in tree.iter_prefix(&value.0) {
                result.union_with(&read_pair_bitmap(state, attr_key, &val)?);
            }
            Ok(result)
        }
        Query::NotGlob { key, value } => {
            let mut all = all_entities(state)?;
            let hit = eval(&Query::Glob { key: key.clone(), value: value.clone() }, state)?;
            all.subtract(&hit);
            Ok(all)
        }
    }
}

fn read_eq<S: StateAdapter>(state: &mut S, key: &AnnotKey, value: &AnnotVal) -> Result<Bitmap> {
    read_pair_bitmap(state, key.pair_key_bytes(), &value.0)
}

/// OR-union of the bitmaps for each value in an `IN (...)` list.
fn read_in<S: StateAdapter>(
    state: &mut S,
    key: &AnnotKey,
    values: &[AnnotVal],
) -> Result<Bitmap> {
    let mut acc = Bitmap::new();
    for v in values {
        let bm = read_pair_bitmap(state, key.pair_key_bytes(), &v.0)?;
        acc.union_with(&bm);
    }
    Ok(acc)
}

// ── Top-level entry point ────────────────────────────────────────────

/// Parameters for paginated query execution.
///
/// Cursors are raw entity IDs — the RPC layer is responsible for any
/// hex (or other) encoding on the wire.
#[derive(Debug, Clone, Copy)]
pub struct PageParams {
    /// Maximum number of entities to return in this page. Must be > 0.
    pub page_size: u64,
    /// If `Some(c)`, only return IDs strictly less than `c`. Use the
    /// `next_cursor` from the previous page to walk through results.
    pub cursor: Option<u64>,
}

/// One page of query results, ordered descending by entity ID
/// (newest first).
#[derive(Debug, Clone)]
pub struct Page {
    /// The matching entities — already resolved through the
    /// `id_to_addr` system slot + `EntityRlp::decode_from_code`.
    pub entries: Vec<EntityRlp>,
    /// Set when more pages remain. Pass it as the next call's
    /// `cursor` to walk forward.
    pub next_cursor: Option<u64>,
}

/// Parse → evaluate → paginate → resolve, in one call.
///
/// This is the entry point the RPC layer (and any other caller that
/// just wants matching entities) should use. The lower-level
/// [`parse`], [`Query::evaluate`], and [`resolve_id`] remain public
/// for tests and advanced consumers.
///
/// IDs that fail to resolve (e.g. entity tombstoned between the
/// bitmap read and the resolve — possible under concurrent writes)
/// are skipped silently and don't count toward `page_size`.
pub fn execute<S: StateAdapter>(
    state: &mut S,
    query: &str,
    params: PageParams,
) -> Result<Page> {
    eyre::ensure!(params.page_size > 0, "page_size must be > 0");

    let parsed = parse(query)?;
    let bitmap = parsed.evaluate(state)?;

    // Collect ascending so we can pop ids >= cursor off the tail
    // cheaply, then iterate in reverse for newest-first output. Page
    // sizes are small (typical RPC cap is 200) so materializing the
    // full Vec doesn't matter even when the bitmap is large.
    let mut ids: Vec<u64> = bitmap.iter().collect();
    ids.sort_unstable();
    if let Some(c) = params.cursor {
        while ids.last().is_some_and(|id| *id >= c) {
            ids.pop();
        }
    }

    let page_size = params.page_size as usize;
    let mut entries = Vec::with_capacity(page_size.min(ids.len()));
    let mut last_returned_id: Option<u64> = None;
    let mut has_more = false;

    for &id in ids.iter().rev() {
        if entries.len() >= page_size {
            has_more = true;
            break;
        }
        if let Some(entity) = resolve_id(state, id)? {
            entries.push(entity);
            last_returned_id = Some(id);
        }
    }

    let next_cursor = if has_more { last_returned_id } else { None };
    Ok(Page { entries, next_cursor })
}
