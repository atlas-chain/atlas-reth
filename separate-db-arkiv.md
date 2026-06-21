# Arkiv Separate-DB Commitment Design

Status: draft design note.

Arkiv should move entity payloads, metadata, annotation bitmaps, range indexes,
nonces, and ID maps out of EVM account code/storage. The current trie-backed
model gives state-root commitment semantics, but forces DB writes through revm
journaling, account/code writes, trie updates, historical state, and block
execution. The new split:

```text
Payload service / DB: stores Operation[], materializes indexes, relays signed txs
Blockchain: stores ordering, sender authorization, nonce, and payload hashes
```

The chain is an ordered commitment log; the external Arkiv service is the
high-performance database and query engine.

## Scope

Goals:

- remove entity payload and index writes from EVM state;
- keep blockchain ordering and sender authentication;
- commit every DB mutation request by hash on-chain;
- require centralized payload availability and validation in v1;
- support multiple centralized availability providers, each with its own
  signing key;
- let providers accept already signed blockchain transactions and act as a
  payload-aware mempool/relay;
- allow a later move to decentralized data availability.

Non-goals for v1:

- decentralized payload availability;
- trustless proof that the external DB applied every valid payload;
- full payloads in calldata, blobs, or EVM storage;
- preserving the current "all Arkiv data lives in stateRoot" property.

## Flow

```text
1. Client canonical-encodes Operation[].
2. Client computes payloadHash.
3. Client asks one provider to certify the payload.
4. Provider validates, stores by hash, and signs a certificate.
5. Client signs tx: commit(payloadHash, certificate).
6. Client submits {payload, signedTx} to the provider mempool.
7. Provider verifies the bundle and relays the signed tx.
8. Precompile verifies certificate, bumps Arkiv nonce, emits commitment.
9. Projector watches finalized commitments.
10. Projector fetches payload, verifies hash/envelope, applies ops in chain order.
```

The tx never fetches the payload; it validates only the provider certificate.
That certificate is the centralized v1 claim that the payload exists and passed
validation. The provider mempool makes payload availability part of tx ingress:
only bundles whose signed tx matches the stored payload are relayed.

## On-Chain Surface

```solidity
function commit(bytes32 payloadHash, AvailabilityCertificate cert) external;

event PayloadCommitted(
    address indexed sender,
    uint32 indexed nonce,
    bytes32 indexed payloadHash,
    uint32 providerId
);
```

`AvailabilityCertificate` must carry signed data plus signer identity or a
recoverable signature. The precompile must know the active provider set. V1 can
accept any one active provider signature; each provider has an independent
private key and stable `providerId`.

Precompile validation:

- `providerId` is active;
- signature recovers to the active key for `providerId`;
- `chainId` matches the current chain;
- `registry` equals `ARKIV_ADDRESS`;
- `sender` equals `msg.sender`;
- `nonce` equals the current Arkiv nonce for `msg.sender`;
- `payloadHash` equals the submitted hash;
- certificate version/schema is supported;
- certificate is not expired.

On success, the precompile bumps the sender's Arkiv nonce and emits
`PayloadCommitted`, including `providerId` for auditability.

## Canonical Data

Certificate message fields:

```text
version
providerId
chainId
registry/precompile address
sender
arkiv nonce
payloadHash
payload byte length
payload schema version
expiry block or timestamp
```

Payload envelope fields:

```text
version
chainId
registry/precompile address
sender
arkiv nonce
operations
```

Hashing/signing:

```text
payloadHash = keccak256("arkiv.payload.v1" || canonical_payload_bytes)
certificateSignedBytes = "arkiv.availability.v1" || canonical_certificate_bytes
```

Canonical encoding must be deterministic and specified. EIP-712 typed data is
acceptable; so is a simpler fixed binary encoding. Provider and projector must
recompute `payloadHash`. The certificate binds chain, deployment, account,
nonce, payload hash/length, schema version, expiry, and provider ID to prevent
replay across chains, deployments, accounts, nonces, provider identities, and
future incompatible payload schemas.

## Availability Providers

V1 trusts a configured set of centralized providers. Each provider exposes the
same API, validates payloads independently, stores accepted payloads, and signs
certificates with its own private key. With the initial "any active provider"
policy, clients can choose a provider and one unavailable provider does not
halt writes while another active provider can certify payloads.

Provider-set configuration options:

- compile-time or genesis-configured provider keys for the first devnet;
- chainspec/config-file provider keys loaded by `arkiv-node`;
- future on-chain or operator-managed provider registry.

Provider API:

```text
POST /payload
  request: canonical payload bytes or structured payload envelope
  response: payloadHash + signed AvailabilityCertificate

POST /submit
  request: canonical payload bytes + signed blockchain tx
  response: provider mempool tx id / status

GET /payload/{payloadHash}
  response: canonical payload bytes
```

`POST /payload` behavior:

1. canonicalize or verify canonical payload bytes;
2. recompute `payloadHash`;
3. validate payload schema;
4. validate operation semantics against provider DB/projection state;
5. store payload by hash;
6. return signed certificate.

Validation can initially be centralized and stateful. Invalid payloads are
rejected before the blockchain tx. Providers should use independent private
keys. Key rotation is a provider-set update: add the new key, wait for clients
to switch, then deactivate the old key after outstanding certificates expire.

`POST /submit` behavior:

1. recover the tx sender and decode `commit(payloadHash, certificate)`;
2. verify the tx targets `ARKIV_ADDRESS` on the expected chain;
3. recompute payload hash and verify it matches the tx and certificate;
4. verify the certificate signer/provider/version/expiry;
5. reject if payload semantics are invalid or the payload is not stored;
6. accept into the provider mempool, dedupe by tx hash and payload hash, and
   relay/broadcast the exact signed tx.

The provider can delay, drop, or censor txs, but cannot mutate them. Replacement
requires a new user-signed tx. Public chain ingress that bypasses provider
mempools can still create hash-only commitments unless the precompile requires
valid provider certificates, which this design does.

## Projector

For each finalized `PayloadCommitted` event, the projector:

1. fetches payload bytes by `payloadHash`;
2. recomputes and verifies the hash;
3. verifies the payload envelope matches event fields;
4. applies operations in canonical chain order;
5. records applied/rejected/missing status.

The projector should wait for a configurable finality depth or finality signal.
If it applies before finality, it must support reorg rollback.

## Invalid Payloads

- Before tx: the provider rejects malformed or semantically invalid payloads
  and refuses to issue a certificate.
- During tx: the precompile rejects invalid certificates, wrong provider,
  sender, nonce, chain, registry, hash, version/schema, or expiry.

Because the tx contains only `payloadHash`, the precompile cannot validate the
full operation payload; it validates only the certificate and committed fields.

## Trust And Availability

With centralized availability, the chain proves only:

- sender committed to `payloadHash`;
- an active trusted provider signed for that hash;
- commitment order and nonce are canonical.

It does not independently prove permanent retrievability. V1 trust model:

```text
Blockchain trusts the active provider key set for availability + pre-validation.
DB projection trusts finalized chain order.
Clients trust the provider that certified a payload to keep it retrievable.
```

Multiple centralized providers improve redundancy but are not trustless if one
active signature is enough. Later, the policy can become a quorum certificate,
bonded service, challenge protocol, or external data-availability proof.

## Test Plan

Provider validation:

- valid create payload is accepted, stored, and certified;
- malformed payload is rejected;
- unknown operation type is rejected;
- invalid `Ident32` is rejected;
- zero-BTL create is rejected;
- unauthorized update/delete is rejected when stateful validation is enabled;
- payload hash mismatch is rejected;
- unsupported payload schema version is rejected.

Provider mempool/relay:

- signed tx with matching stored payload is accepted and relayed;
- tx sender mismatch with certificate sender is rejected;
- tx target different from `ARKIV_ADDRESS` is rejected;
- tx calldata hash different from payload hash is rejected;
- unstored payload hash is rejected;
- duplicate tx hash is deduped;
- replacement requires a distinct user-signed tx;
- relayed tx bytes are identical to submitted tx bytes.

Precompile/transaction:

- valid hash plus valid certificate succeeds and emits `PayloadCommitted`;
- certificate signed by wrong key reverts;
- unknown provider ID reverts;
- deactivated provider ID reverts;
- provider A signature labeled as provider B reverts;
- certificate sender different from `msg.sender` reverts;
- certificate nonce different from current Arkiv nonce reverts;
- certificate chain ID different from current chain reverts;
- certificate registry different from `ARKIV_ADDRESS` reverts;
- certificate payload hash different from submitted hash reverts;
- expired certificate reverts;
- reused certificate/nonce reverts.

Projector:

- reads committed events from finalized blocks;
- fetches payload by hash;
- recomputes and verifies payload hash;
- applies operations in chain order;
- does not apply payload before matching commitment;
- records missing payload without applying it;
- handles duplicate commitments according to nonce rules;
- handles reorg rollback or waits for finality before applying.
