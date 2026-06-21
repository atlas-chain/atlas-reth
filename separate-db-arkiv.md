# Arkiv Separate-DB Commitment Design

Status: draft design note.

This document describes the intended direction for separating Arkiv's
database workload from Ethereum execution state. The current implementation
stores entity payloads, entity metadata, annotation bitmaps, range indexes,
nonces, and ID maps directly in EVM account code/storage. That gives strong
state-root commitment semantics, but it also routes every DB mutation through
the slowest part of the execution stack: revm journaling, account/code writes,
trie updates, historical state, and block execution.

The desired architecture is:

```text
Payload service / DB
  stores full operations and materializes query indexes

Blockchain
  stores ordering, sender authorization, nonce, and payload hash commitments
```

The chain becomes an ordered commitment log. The external Arkiv service
becomes the high-performance database and query engine.

## Goals

- Remove entity payload and index writes from EVM state.
- Keep blockchain ordering and sender authentication.
- Commit every DB mutation request by hash on-chain.
- Require centralized payload availability and validation for the first
  version.
- Leave room to replace the centralized availability service with a
  decentralized data-availability solution later.

## Non-goals for v1

- Decentralized payload availability.
- Trustless proof that the external DB applied every valid payload.
- Storing full operation payloads in calldata, blobs, or EVM storage.
- Preserving the current "all Arkiv data lives in stateRoot" property.

## High-Level Flow

```text
1. Client canonical-encodes Operation[].
2. Client computes payloadHash.
3. Client uploads the payload to the centralized availability service.
4. Availability service validates the payload and stores it by hash.
5. Availability service signs an availability/validation certificate.
6. Client sends a blockchain tx containing payloadHash + certificate.
7. Arkiv precompile verifies the certificate and emits a commitment event.
8. DB projector watches finalized chain commitments.
9. DB projector fetches payload by hash, verifies the hash, and applies ops
   in finalized chain order.
```

The transaction must not fetch the payload from the service. The transaction
only verifies the signed certificate. The certificate is the centralized v1
claim that the payload exists and has passed validation.

## On-Chain Surface

The minimal precompile/ABI surface should be a commitment entry point:

```solidity
function commit(bytes32 payloadHash, AvailabilityCertificate cert) external;

event PayloadCommitted(
    address indexed sender,
    uint32 indexed nonce,
    bytes32 indexed payloadHash
);
```

The exact Solidity type for `AvailabilityCertificate` can be chosen during
implementation. At minimum it needs to carry signed data and the signer
identity or recoverable signature.

The precompile should validate:

- certificate signature recovers to the configured availability-service key;
- certificate `chainId` equals the current chain ID;
- certificate `registry` equals `ARKIV_ADDRESS`;
- certificate `sender` equals `msg.sender`;
- certificate `nonce` equals the current Arkiv nonce for `msg.sender`;
- certificate `payloadHash` equals the submitted `payloadHash`;
- certificate schema/version is supported;
- certificate has not expired.

If validation succeeds, the precompile bumps the sender's Arkiv nonce and
emits `PayloadCommitted`.

## Certificate Payload

The signed certificate message should bind:

```text
version
chainId
registry/precompile address
sender
arkiv nonce
payloadHash
payload byte length
payload schema version
expiry block or timestamp
```

This prevents replay across chains, deployments, accounts, nonces, and
future incompatible payload schemas.

The signed bytes should be domain separated. For example:

```text
arkiv.availability.v1 || canonical_certificate_bytes
```

Implementation can use EIP-712 typed data or a simpler fixed canonical
encoding, but the encoding must be deterministic and specified.

## Payload Hashing

The payload hash must be computed over canonical bytes, not ad hoc JSON text.

Recommended payload envelope:

```text
version
chainId
registry/precompile address
sender
arkiv nonce
operations
```

Then:

```text
payloadHash = keccak256("arkiv.payload.v1" || canonical_payload_bytes)
```

The availability service and DB projector must recompute this hash before
accepting or applying a payload.

## Availability Service

For v1, Arkiv trusts one centralized service key for availability and
pre-validation.

The service should expose at least:

```text
POST /payload
  request: canonical payload bytes or structured payload envelope
  response: payloadHash + signed AvailabilityCertificate

GET /payload/{payloadHash}
  response: canonical payload bytes
```

On `POST /payload`, the service:

1. canonicalizes or verifies the submitted canonical payload;
2. recomputes `payloadHash`;
3. validates payload schema;
4. validates operation semantics against its current DB/projection state;
5. stores the payload by hash;
6. returns a signed certificate.

Validation can initially be centralized and stateful. Invalid payloads are
rejected before the client sends the blockchain transaction.

## DB Projector

The projector is responsible for turning finalized on-chain commitments into
database mutations.

For each finalized `PayloadCommitted` event, the projector:

1. fetches payload bytes by `payloadHash`;
2. recomputes and verifies the hash;
3. verifies the payload envelope matches event fields;
4. applies operations in canonical chain order;
5. records applied/rejected/missing status.

The projector should wait for a configurable finality depth or finality signal
before applying mutations. If it applies before finality, it must support reorg
rollback.

## Invalid Payloads

There are two classes of invalid payload handling:

- Before tx: the centralized availability service rejects malformed or
  semantically invalid payloads and refuses to issue a certificate.
- During tx: the precompile rejects invalid certificates, wrong sender, wrong
  nonce, wrong chain, wrong registry, hash mismatch, unsupported version, or
  expiry.

The precompile cannot validate the full operation payload if the tx only
contains `payloadHash`. It can only validate the certificate and fields
committed in the tx.

## Availability Risk

With centralized availability, the chain only proves:

- a sender committed to a payload hash;
- the trusted service signed a certificate for that hash;
- the commitment order and nonce are canonical.

The chain does not independently prove that the payload remains retrievable
forever. The v1 trust model is therefore:

```text
Blockchain trusts one service key for availability + pre-validation.
DB projection trusts finalized chain order.
Clients trust the service to keep payloads retrievable.
```

Later, the single service signature can be replaced by a quorum certificate,
bonded service, challenge protocol, or external data-availability proof.

## Test Plan

Service validation tests:

- valid create payload is accepted, stored, and certified;
- malformed payload is rejected;
- unknown operation type is rejected;
- invalid `Ident32` is rejected;
- zero-BTL create is rejected;
- unauthorized update/delete is rejected when stateful validation is enabled;
- payload hash mismatch is rejected;
- unsupported payload schema version is rejected.

Precompile/transaction tests:

- valid hash plus valid certificate succeeds and emits `PayloadCommitted`;
- certificate signed by wrong key reverts;
- certificate sender different from `msg.sender` reverts;
- certificate nonce different from current Arkiv nonce reverts;
- certificate chain ID different from current chain reverts;
- certificate registry different from `ARKIV_ADDRESS` reverts;
- certificate payload hash different from submitted hash reverts;
- expired certificate reverts;
- reused certificate/nonce reverts.

Projector tests:

- reads committed events from finalized blocks;
- fetches payload by hash;
- recomputes and verifies payload hash;
- applies operations in chain order;
- does not apply payload before matching commitment;
- records missing payload without applying it;
- handles duplicate commitments according to nonce rules;
- handles reorg rollback or waits for finality before applying.

## Migration Direction

The safest implementation path is additive:

1. Add `commit(bytes32, certificate)` beside the existing
   `execute(Operation[])` path.
2. Implement the centralized availability service and projector.
3. Move CLI and SDK write flows to payload upload + commitment tx.
4. Keep the old trie-backed execution path only for compatibility during
   migration.
5. Remove or disable direct state-trie entity writes once the separate DB path
   is proven.

