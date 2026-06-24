# New Contract / Precompile Requirements

This document captures the requirements the Arkiv contract surface must meet
when we move from today's inline payload model to payload-provider-backed entity
payloads.

In this repo, "contract" means the ABI in
`contracts/src/EntityRegistry.sol` plus the Rust precompile implementation at
`ARKIV_ADDRESS`. There is no deployed Solidity bytecode. Any ABI change must keep
the Solidity interface and the precompile `sol!`/decoder definitions in lockstep.

## Current SDK Version: No Contract Change Required

The first SDK integration is a dual-write flow:

1. SDK receives `createEntity`, `updateEntity`, or `mutateEntities` input.
2. SDK uploads every create/update payload to the Atlas Payload Provider.
3. Provider stores the bytes and returns payload metadata plus an EIP-191 receipt
   signature.
4. SDK verifies the provider metadata/signature.
5. SDK sends the existing `execute(Operation[])` transaction to Arkiv RPC with
   the original full payload bytes inline.
6. SDK returns normal transaction data plus optional provider receipt data.

The current precompile can remain unchanged for that version. The chain still
stores and serves inline payload bytes exactly as it does today.

## Future Contract Goal

Later, create/update operations should be able to store a compact detached
payload reference instead of the full payload bytes.

```text
Original payload bytes
   |
   |--> Payload Provider: stored bytes + signed receipt
   |
   |--> Arkiv precompile: provider reference + signature proof
```

The precompile must never perform HTTP calls or depend on live provider
availability. All proof material needed for consensus validation must be in
calldata and/or chain configuration/state.

## Payload Reference Requirements

A reference-mode create/update operation must provide enough data for the
precompile to verify that a trusted provider signed a receipt for the detached
payload.

Version 1 can carry the full provider receipt/signature. Later versions can
compress the same proof.

Minimum version 1 reference fields:

- Reference marker, for example `kind = "atlas.payload.reference"`.
- Reference format version, initially `1`.
- Provider identity, either a URL or a compact `providerId`.
- Payload id: SHA-256 over `namespace || 0x00 || payload_bytes`, lowercase hex.
- Namespace, defaulting to `arkiv.entities`.
- Payload checksum: `sha256:<hex>` over raw payload bytes.
- Payload size in bytes.
- Payload content type.
- Provider `submittedAt` timestamp from the signed receipt.
- Caller-scoped one-time `nonce` as a nonzero 32-byte hex value.
- Simple numeric `payment` gas amount, currently set by the SDK to
  `100000`.
- Full provider signature:
  - `scheme`
  - `signer`
  - signed `receipt`
  - `messageHash`
  - packed `signature`
  - `r`
  - `s`
  - `v`

The first reference encoding may be canonical JSON for SDK simplicity, but the
precompile should prefer a deterministic binary/ABI encoding before activation if
we want smaller calldata and simpler consensus parsing. If JSON is used in
consensus, it must be canonical: UTF-8, compact, sorted object keys, no duplicate
keys, no insignificant whitespace, fixed field names, and exact integer ranges.

## Provider Receipt Verification

For each reference-mode create/update, the precompile must validate:

- `scheme == "eip191"`.
- The embedded receipt matches the reference metadata.
- The receipt action is `payloadReceived`.
- The canonical receipt bytes match the payload-provider server format:

```json
{"service":"atlas-payload-provider","action":"payloadReceived","payloadId":"<id>","namespace":"<namespace>","checksum":"sha256:<hex>","sizeBytes":123,"submittedAt":"<iso>","nonce":"0x<bytes32>","payment":100000}
```

- `messageHash == keccak256("\x19Ethereum Signed Message:\n" + len(receipt_bytes) + receipt_bytes)`.
- `signature`, `r`, `s`, and `v` are internally consistent.
- `v` is 27 or 28.
- Recovered signer address equals `signature.signer`.
- Recovered signer is trusted for the declared provider.
- The signed nonce has not already been consumed by the caller.
- The precompile charges the signed `payment` value as extra gas in
  addition to fixed reference verification gas.

The provider trust check must be consensus-defined. Acceptable approaches:

- A hardcoded chain configuration mapping `providerId -> signer address`.
- A precompile-managed registry in state.
- A governance-controlled allowlist.

Do not accept arbitrary signer addresses from calldata. Without an allowlist or
registry, the signature only proves that some key signed the receipt, not that a
trusted provider accepted the payload.

## Important Binding Limitation

The current payload-provider receipt signs payload metadata plus
nonce/payment:

- service
- action
- payload id
- namespace
- checksum
- size
- submitted timestamp
- nonce
- payment

It does not sign the Arkiv entity key, attributes, expiration, owner, or content
type outside the payload metadata.

If the future contract must prove that a provider accepted a payload for a
specific Arkiv entity operation, the provider API must be extended so the signed
claim also includes Arkiv context, such as:

- operation type: create/update
- entity key
- content type
- attributes hash
- expires/btl
- owner or submitter
- chain id
- Arkiv registry address

Until that extension exists, the precompile can only verify that a trusted
provider signed receipt for the payload bytes. It cannot verify that the provider
signed the full Arkiv operation intent.

## ABI / Operation Shape Options

There are two viable activation paths.

### Option A: Reuse `Operation.payload`

Keep `execute(Entity.Operation[] ops)` unchanged. Reference-mode operations put
the encoded reference into `Operation.payload` and use a reserved content type,
for example:

```text
application/vnd.atlas.payload-reference+json
```

Pros:

- No ABI break for `execute`.
- Existing batching, nonces, entity keys, events, and SDK call shape remain.

Cons:

- The precompile must inspect `contentType` and parse `payload` differently.
- Query clients must distinguish inline entity bytes from reference bytes.

### Option B: Add Explicit Reference Operation Shape

Add a new ABI struct or function for detached payload references.

Pros:

- Clear typed boundary between inline payloads and detached references.
- Easier to optimize reference fields later.

Cons:

- SDK and ABI codegen changes are larger.
- Precompile selector dispatch expands.

Unless there is a strong reason to break ABI, Option A is the preferred first
contract change.

## State And Query Semantics

The precompile must support both inline and reference-backed entities.

Requirements:

- Existing inline entities remain valid and readable.
- A create may create either inline or reference-backed content.
- An update may transition inline -> reference, reference -> inline, or
  reference -> reference.
- Owner, creator, expiration, attributes, entity key derivation, nonce handling,
  and authorization semantics remain unchanged.
- The entity state committed in the state root must include the exact stored
  reference bytes or a canonical internal reference representation.
- `eth_getProof` + `eth_getCode` must still prove the entity record that Arkiv
  stores. For reference-backed entities, the proof authenticates the reference,
  not the raw off-chain payload bytes.
- `arkiv_query` must expose enough information for SDKs to resolve detached
  payloads from a provider.
- Query/RPC must not pretend a reference is the raw payload. Either return the
  reference payload as stored, or add an explicit response shape/metadata flag
  that tells clients the payload is detached.

SDKs can then fetch `GET /payloads/{id}/raw` from the provider and verify the
checksum client-side.

## Event Requirements

`EntityOperation` emission must remain one event per validated operation, in the
same order as today.

If the reserved `entityHash` field starts carrying a real commitment for
reference-backed payloads, update all of these together:

- `contracts/src/EntityRegistry.sol`
- precompile ABI definitions
- event emission logic
- SDK event types
- `docs/2_state-model.md`

Until then, keep current event compatibility.

## Gas Requirements

Gas must remain a pure function of calldata.

The precompile must not charge based on:

- provider availability,
- provider response time,
- existing chain state beyond normal op validation,
- or any external network condition.

Reference validation gas should account for:

- encoded reference byte length,
- receipt parsing/validation,
- EIP-191 hash reconstruction,
- secp256k1 recovery,
- provider allowlist lookup if applicable,
- and normal entity create/update indexing costs.

Malformed references should revert deterministically before state changes.

## Error Requirements

Add Solidity-style errors to the ABI for reference-mode failures. Suggested
errors:

```solidity
error PayloadReferenceMalformed(bytes32 entityKey);
error PayloadReferenceUnsupportedVersion(uint256 version);
error PayloadProviderUnknown(bytes32 providerId);
error PayloadProviderSignerNotAllowed(address signer);
error PayloadProviderReceiptMismatch(bytes32 entityKey);
error PayloadProviderSignatureInvalid(bytes32 entityKey);
error PayloadReferenceContentTypeInvalid(bytes32 entityKey);
```

Final names can change, but errors must be ABI-declared so SDK decoders can show
clear revert reasons.

## Provider Reference Optimization Path

Version 1 may include the full signature object. Later versions can reduce
calldata by replacing repeated fields with compact ids:

- `providerId` instead of URL.
- signer implied by provider registry.
- compact payload id bytes instead of hex string.
- checksum bytes instead of `sha256:<hex>` string.
- `messageHash` plus packed signature only, if receipt reconstruction is
  deterministic from compact fields.
- optional omission of `r`, `s`, `v` when packed signature is enough.

Any optimized format must remain reconstructable into the same signed receipt
message, or the provider signing scheme must be versioned explicitly.

## Activation / Compatibility

Do not flip SDKs to reference mode until the chain exposes a reliable capability
signal. Options:

- Chain id plus known activation block.
- RPC capability method.
- Contract/precompile version method.
- SDK chain config flag.

Before activation, SDKs must keep `transactionPayload: "inline"` behavior.

After activation, SDKs may support:

```ts
payloadProvider: {
  url: "https://payload.atlas.arkiv-global.net",
  bearerKey: "...",
  transactionPayload: "reference"
}
```

The chain must continue accepting inline payloads unless a later migration
explicitly disables them.

## Test Checklist

Precompile / entitydb tests:

- Valid reference create stores the canonical reference.
- Valid reference update replaces previous content.
- Inline create/update behavior remains unchanged.
- Inline -> reference and reference -> inline transitions work.
- Malformed reference reverts before state changes.
- Unsupported reference version reverts.
- Unknown provider reverts.
- Disallowed signer reverts.
- Invalid message hash reverts.
- Invalid signature recovery reverts.
- Receipt metadata mismatch reverts.
- Batch atomicity: one bad reference rolls back all operations.
- Gas is deterministic for identical calldata across different pre-states.
- Query returns enough metadata for SDK resolution.

SDK / e2e tests:

- SDK uploads payload before transaction.
- SDK fails before RPC submission if provider upload/signature verification fails.
- Reference-mode transaction succeeds against activated node.
- SDK can resolve reference-backed query results through the provider.
- Raw provider bytes verify against checksum.

## Files To Update When Implementing

- `contracts/src/EntityRegistry.sol`
- `crates/arkiv-node/src/precompile.rs`
- `crates/arkiv-entitydb/src/lib.rs`
- `crates/arkiv-node/src/rpc.rs`
- `docs/2_state-model.md`
- `docs/3_query.md` if query responses change
- SDK ABI/types and payload-provider reference encoder
