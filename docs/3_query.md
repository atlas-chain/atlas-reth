# Arkiv Query Language and Reads

This document covers the `arkiv_*` JSON-RPC namespace, the query
grammar it accepts, how each query shape is evaluated against the
trie, and how clients can independently verify query results.

For the underlying state model — what pair / index / entity accounts
look like and how their `codeHash` is set — see
[`2_state-model.md`](2_state-model.md). For higher-level context see
[`1_overview.md`](1_overview.md).

## Contents

- [1. The `arkiv_*` RPC namespace](#1-the-arkiv_-rpc-namespace)
- [2. Query language](#2-query-language)
- [3. Evaluation](#3-evaluation)
  - [Equality, Inclusion, Boolean](#equality-inclusion-boolean)
  - [Range and Prefix-Glob](#range-and-prefix-glob)
  - [Historical queries](#historical-queries)
- [4. Verification recipes](#4-verification-recipes)

---

## 1. The `arkiv_*` RPC namespace

Local-only. Registered via the standard `extend_rpc_modules` hook.

| Method | Returns |
|---|---|
| `arkiv_query(query, [options])` | Page of matching entities. Pagination is descending by entity ID. Options carry `atBlock`, `resultsPerPage`, `cursor`, and per-field `includeData` projection. |
| `arkiv_getEntityCount()` | Cardinality of the `$all` bitmap at head. |
| `arkiv_getBlockTiming()` | Head block number, head block timestamp, and seconds since the parent block. |

The namespace registers on every transport the operator has enabled
(`--http`, `--ws`, `--ipc`).

All queries are evaluated by reading the trie. Every read is a
standard `eth_call` / `eth_getStorageAt` / `eth_getCode` against
reth's `StateProvider`.

The query grammar (lexer + parser in
`crates/arkiv-entitydb/src/query/`) and the tree-walking interpreter
live in `arkiv-entitydb`. The RPC layer (`crates/arkiv-node/src/rpc.rs`)
is a thin shell: take a `StateProvider` snapshot, wrap it in
`ReadOnlyStateAdapter`, call `arkiv_entitydb::query::execute`, render
matching entities to wire-format `EntityData`, apply pagination.

---

## 2. Query language

Implemented today:

- Top-level: `*` and `$all` (every live entity).
- Equality and inequality: `k = v`, `k != v`.
- Inclusion: `k IN (v1 v2 …)`, `k NOT IN (…)`.
- Range: `k < v`, `k <= v`, `k > v`, `k >= v`.
- Prefix-glob: `k ~ "prefix*"`, `k !~ "prefix*"`.
- Boolean combinators: `&&` / `AND`, `||` / `OR`, `NOT (…)`, `!(…)`.
- Built-ins: `$owner`, `$creator`, `$key`, `$expiration`,
  `$contentType`, `$createdAtBlock`.
- Value literals: hex addresses (`0x…40hex`), entity keys (`0x…64hex`,
  optionally quoted), decimal numbers, double-quoted strings.

Range and prefix-glob evaluate against the Tier-2 ART index account
for the queried key (`keccak256("arkiv.index" || k)[:20]`). The
glob operator is **prefix-only** — mid-pattern wildcards
(`"img/*/large"`) are not supported.

**Not implemented:**

- `$sequence` built-in.
- Arbitrary-pattern glob (only prefix-glob).

---

## 3. Evaluation

### Equality, Inclusion, Boolean

```
Query: $contentType = "image/png" && tag = "approved"

1. Derive pair_addr_1 = keccak256("arkiv.pair" || "$contentType" || 0x00 || "image/png")[:20].
2. Derive pair_addr_2 = keccak256("arkiv.pair" || "tag"          || 0x00 || "approved")[:20].
3. Read pair_addr_1.code → bitmap_1; pair_addr_2.code → bitmap_2.
4. Deserialize both bitmaps; compute intersection in memory.
5. Apply cursor / page-size limit.
6. For each uint64_id in the result: read system.slot[keccak256("id_to_addr", id)] → entity_address.
7. eth_getCode(entity_address) → decode RLP, project per includeData.
```

Operators:

- `*` and `$all` — every live entity (reads the `$all` bitmap).
- `k = v`, `k != v` — point reads; `!=` subtracts from `$all`.
- `k IN (v1 v2 …)`, `k NOT IN (…)` — OR of per-value reads; `NOT IN`
  subtracts from `$all`.
- `&&` / `AND`, `||` / `OR` — intersect / union of sub-evaluations.
- `NOT (…)`, `!(…)` — `$all \ eval(inner)`.

Built-in keys (`$owner`, `$creator`, `$key`, `$expiration`,
`$contentType`, `$createdAtBlock`) and user-defined annotation keys
both follow the same path; the only difference is which pair-account
address gets derived for a given `(k, v)`.

### Range and Prefix-Glob

Range (`<`, `<=`, `>`, `>=`) and prefix-glob (`~`, `!~`) operators
evaluate against the Tier-2 index account for the queried key.

```
Query: price > 100 AND price < 500

1. Encode bounds as 32-byte big-endian UINT:
     lo_key = [0×24 zeros, 0,0,0,0,0,0,0,100]
     hi_key = [0×24 zeros, 0,0,0,0,0,0,1,244]

2. index_addr = keccak256("arkiv.index" || "price")[:20].
3. eth_getCode(index_addr) → deserialise ART.
4. ART.iter_gt(lo_key) and ART.iter_lt(hi_key) → enumerate matching v_i.
5. For each v_i:
     pair_addr_i = keccak256("arkiv.pair" || "price" || 0x00 || v_i)[:20]
     bitmap_i    = deserialise(eth_getCode(pair_addr_i))
6. Union all bitmap_i → result bitmap.
7. Compose with other sub-expression bitmaps via the standard
   &&/||/NOT pipeline; apply cursor / page-size; resolve IDs to
   entity addresses via the system account.
```

Prefix-glob (`tag ~ "image/*"`) uses `ART.iter_prefix(prefix_bytes)`
on the index for `tag`; the rest of the pipeline is identical. The
glob operator is **prefix-only** — wildcards in the middle of a
pattern (`"img/*/large"`) are not supported by the underlying ART
scan. The grammar accepts `"prefix*"` and treats the bytes preceding
the `*` as the prefix; `!~` is `$all \ eval(~)`.

Range operators are well-defined against any key whose ART encoding
gives lex-order ≡ semantic-order — UINT user attrs, `$expiration`,
and `$createdAtBlock` are the obvious targets. The interpreter does
not type-check the predicate against the key's actual encoding; a
range query on `$owner` will evaluate against the lex order of
20-byte addresses, which is rarely meaningful but is well-defined.

Range and prefix-glob compose with the equality family via the
standard `&&` / `||` / `NOT` combinators; each leaf produces a bitmap
and the combinators run at the bitmap layer.

### Historical queries

The RPC handler takes an optional `atBlock` (hex number) and routes
to `provider.history_by_block_number(n)` instead of `provider.latest()`.
The resulting `StateProvider` is read by `ReadOnlyStateAdapter` exactly
as for the head state. Reth's `Bytecodes` table retains old bitmap
bytes and old ART bytes keyed by hash, so equality and range queries
at any retained block resolve cleanly.

The response's `block_number` field reports the block the query was
evaluated against (the explicit `atBlock`, or the head if absent).

---

## 4. Verification recipes

Every bitmap and every ART is content-addressed in the trie — the
account's `codeHash` **is** the keccak hash of its content. Every
ID-to-address mapping is a trie-committed system-account slot. From
these primitives, a client can verify any query result by re-running
the query logic locally on cryptographically verified bytes.

For an **entity payload**:

```
eth_getProof(entity_address, [], blockN)  →  proves codeHash against stateRoot_N
eth_getCode (entity_address, blockN)       →  returns RLP bytes
verify keccak256(0xFE || rlp_bytes) == codeHash
```

For an **equality query result** (per-term):

```
eth_getProof(pair_address, [], blockN)                          →  proves bitmap codeHash
eth_getCode (pair_address, blockN)                              →  returns bitmap bytes
verify keccak256(bytes) == codeHash
decode bitmap; for each id:
  eth_getProof(system_account, [slot[keccak256("id_to_addr", id)]], blockN)  →  proves id → entity_address
```

**Multi-condition equality (`AND` / `OR` / `NOT` / `IN`).** Repeat
per term; combine bitmaps locally with the same logic the server
ran; one ID-resolution proof per surviving ID.

For a **range or prefix-glob query result**:

```
eth_getProof(index_address, [], blockN)                          →  proves ART codeHash
eth_getCode (index_address, blockN)                              →  returns ART bytes
verify keccak256(bytes) == codeHash
deserialise ART; walk the requested bound or prefix locally; for each yielded value v_i
  derive pair_address(k, v_i); verify each bitmap as for equality (above)
```

The ART for the queried key is content-addressed in the trie via the
index account's `codeHash`. After verifying it, the client knows the
exact set of values the server's range or prefix scan must have
considered; the result is complete iff every returned ID corresponds
to a value the local ART scan also yields.

For an **ownership / lifetime check**:

```
eth_getProof(entity_address, [], blockN)  →  proves codeHash against stateRoot_N
eth_getCode (entity_address, blockN)       →  returns RLP bytes
decode RLP; the `owner` and `expires_at` fields are authoritative
```

Owner and expiry live in the entity RLP — the same single
`eth_getCode` that anchors the payload also anchors ownership and
lifetime. There is no separate contract-side mapping to prove against.

Each of the proofs above is a single-level proof against the block's
`stateRoot`.
