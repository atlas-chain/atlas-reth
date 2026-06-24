# Payload Reference Implementation Summary

This branch implements first-pass contract/precompile support for
Atlas payload-provider references.

## What changed

- `Entity.Operation.payload` can now carry v1 payload-reference JSON
  when `Entity.Operation.contentType` is exactly
  `application/vnd.atlas.payload-reference+json`.
- Normal inline payloads are unchanged. If any other content type is
  used, the precompile stores `payload` bytes exactly as before.
- Reference-backed CREATE and UPDATE operations are validated before
  nonce bumps or entity state writes.
- The precompile verifies the provider receipt with no network calls:
  it reconstructs the canonical receipt JSON, computes the EIP-191
  message hash, checks `messageHash`, checks packed `signature`
  against `r`, `s`, and `v`, recovers the signer, and requires that
  signer to be in the consensus allowlist.
- The current allowlist contains the live Atlas payload-provider
  signer:
  `0xbdd23fd1bab3f4075edef4738d1d78a6bc5c236c`.
- Reference-backed operations pay
  `G_PAYLOAD_REFERENCE_VERIFY = 50_000` in addition to the existing
  CREATE/UPDATE gas formula. The branch is selected only from calldata,
  so gas remains a pure function of transaction input.

## V1 reference shape

```json
{
  "kind": "atlas.payloadReference",
  "version": 1,
  "provider": "atlas-payload-provider",
  "id": "<sha256(namespace || 0x00 || payload)>",
  "namespace": "atlas.test",
  "contentType": "text/plain",
  "checksum": "sha256:<sha256(payload)>",
  "sizeBytes": 42,
  "submittedAt": "2026-06-24T15:24:30Z",
  "signature": {
    "scheme": "eip191",
    "signer": "0x...",
    "receipt": {
      "service": "atlas-payload-provider",
      "action": "payloadReceived",
      "payloadId": "<same as id>",
      "namespace": "<same as namespace>",
      "checksum": "<same as checksum>",
      "sizeBytes": 42,
      "submittedAt": "<same as submittedAt>"
    },
    "messageHash": "0x...",
    "signature": "0x<r><s><v>",
    "r": "0x...",
    "s": "0x...",
    "v": 27
  }
}
```

## Important limitation

The current provider receipt signs payload metadata only. It proves a
trusted provider accepted bytes identified by payload ID, checksum,
namespace, size, and timestamp. It does not yet bind the signature to
Arkiv operation intent such as entity key, attributes, BTL/expiry,
owner, chain ID, or `ARKIV_ADDRESS`.

That is acceptable for this v1 storage/reference step, but the next
provider signing scheme should include full operation intent before
the chain treats the receipt as a complete authorization proof.

## Files touched

- `contracts/src/EntityRegistry.sol`
  - Added the reserved reference content type constant.
  - Added Solidity-style revert selectors for payload-reference errors.
- `crates/arkiv-node/src/precompile.rs`
  - Added v1 reference parsing and validation.
  - Added EIP-191 receipt verification and signer recovery.
  - Added trusted signer allowlist and gas surcharge.
  - Added unit tests for valid fixture, version mismatch, receipt
    mismatch, signature tampering, MIME triggering, and gas accounting.
- `crates/arkiv-node/tests/payload_reference_precompile.rs`
  - Added direct EVM tests for successful signed-reference CREATE and
    malformed-reference revert.
- `docs/2_state-model.md`
  - Documented inline/reference payload semantics, JSON shape, v1 proof
    limitation, and gas model.
- `docs/3_query.md`
  - Documented that query/proof results authenticate reference JSON
    bytes, not the original off-chain payload body.

## Verification

Completed:

```bash
cargo fmt --check
cargo test -p arkiv-node precompile
cargo test -p arkiv-node --test payload_reference_precompile
cargo test -p arkiv-node
cargo test -p arkiv-entitydb
```

Not completed locally:

```bash
forge build
```

`forge`/`solc` is not installed in this environment, so Solidity
artifact generation could not be verified here.
