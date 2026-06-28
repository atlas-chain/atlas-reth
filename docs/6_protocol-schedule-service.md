# Arkiv Protocol Schedule Service

This document specifies the HTTP service that publishes Arkiv's
experimental protocol schedule. `arkiv-node` consumes this service when
`ARKIV_PROTOCOL_SCHEDULE_URL` is set.

The schedule controls protocol-level EIP-1559 parameters carried by
Arkiv's patched `alloy-eips` dependency:

- minimum base fee per gas;
- elasticity multiplier;
- base fee max-change denominator;
- payload-builder gas-limit cap.

These values affect consensus header validation. Every execution node
on a given chain must converge on the same accepted schedule.

## 1. Scope

The service is intentionally small. It serves one JSON document over
HTTP(S). It does not need to know individual node identities, push
updates, stream events, or expose mutation APIs to nodes.

Required service responsibilities:

- Publish a full schedule document for one chain.
- Keep `version` monotonic.
- Publish syntactically and semantically valid JSON.
- Serve the document with low latency and high availability.
- Preserve older valid versions long enough for rollback diagnostics.

Out of scope:

- Chain hardfork governance.
- Signing or threshold approval. This may be added later, but current
  `arkiv-node` releases do not verify signatures.
- Per-node schedules. All nodes for the same chain should read the same
  schedule URL.

## 2. Endpoint

The service must expose an HTTP `GET` endpoint:

```text
GET /arkiv-protocol-schedule.json
```

Recommended production URL shape:

```text
https://schedule.<network>.arkiv.network/arkiv-protocol-schedule.json
```

Local Docker Compose deployments may serve the same document from a
sidecar:

```text
http://protocol-schedule:8080/arkiv-protocol-schedule.json
```

The response body is the complete schedule JSON. `arkiv-node` currently
uses a plain `GET` with no custom headers, no authentication, and no
conditional request handling.

Required response behavior:

- Return `200 OK` with the JSON document for healthy reads.
- Set `Content-Type: application/json`.
- Avoid redirects for node-facing URLs.
- Keep the response small; the expected body is well under 64 KiB.
- Do not require cookies, bearer tokens, mTLS, or IP-specific logic.

Recommended headers:

```text
Content-Type: application/json
Cache-Control: no-cache
```

`Cache-Control: no-cache` allows intermediaries to store the object but
requires revalidation. This keeps polling behavior predictable when a
schedule changes. If the service is behind a CDN, configure the CDN to
revalidate frequently or bypass cache for this path.

## 3. JSON Schema

The document is a JSON object:

```json
{
  "chainId": 42069,
  "version": 1,
  "currentBlock": 0,
  "schedule": [
    {
      "activationBlock": 0,
      "minBaseFeePerGas": "440000000",
      "elasticityMultiplier": 2,
      "baseFeeMaxChangeDenominator": 8,
      "maxBlockGasLimit": "30000000",
      "payloadProviderPayment": {
        "enabled": false,
        "providerShareBps": 0,
        "minimumPayment": "0"
      }
    }
  ]
}
```

Fields:

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `chainId` | integer | yes | EVM chain ID this schedule is intended for. |
| `version` | integer | yes | Monotonic schedule version. Nodes reject versions lower than their last accepted version. |
| `currentBlock` | integer or absent | no | Service-side chain head used to gate future entries. |
| `schedule` | array | yes | Ordered list of protocol parameter entries. |

Schedule entry fields:

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `activationBlock` | integer | yes | First block at which this entry may apply. |
| `minBaseFeePerGas` | string | yes | Minimum base fee in wei per gas. Decimal or `0x` hex string. |
| `elasticityMultiplier` | integer | yes | EIP-1559 elasticity multiplier. Must be greater than zero. |
| `baseFeeMaxChangeDenominator` | integer | yes | EIP-1559 max-change denominator. Must be greater than zero. |
| `maxBlockGasLimit` | string | yes | Payload-builder gas-limit cap. Decimal or `0x` hex string. |
| `payloadProviderPayment` | object | yes | Native-token side effect applied to signed payload-reference create/update operations. |

`payloadProviderPayment` fields:

| Field | Type | Required | Meaning |
|---|---:|---:|---|
| `enabled` | boolean | yes | Enables caller debit, provider payout, and burn for signed payload-reference payments. |
| `providerShareBps` | integer | yes | Basis points of `payment` transferred to the recovered trusted provider signer. The remainder is burned. |
| `minimumPayment` | string | yes | Minimum accepted signed payment. Decimal or `0x` hex string. |

Validation rules enforced by current `arkiv-node`:

- `schedule` must not be empty.
- `schedule[0].activationBlock` must be `0`.
- `activationBlock` values must be strictly increasing.
- `elasticityMultiplier` must be greater than `0`.
- `baseFeeMaxChangeDenominator` must be greater than `0`.
- `maxBlockGasLimit` must be at least `5000`.
- `payloadProviderPayment.providerShareBps` must be `<= 10000`.
- `payloadProviderPayment.minimumPayment` must be greater than `0`
  when `payloadProviderPayment.enabled` is true.
- Decimal and `0x`-prefixed hex strings are accepted for
  string-encoded quantity fields.

Operational validation the service should enforce before publishing:

- `chainId` must match the target network.
- `version` must be greater than or equal to the last published version.
- Prefer increasing `version` for every content change, including
  changes only to `currentBlock`.
- Numeric values must fit in unsigned 64-bit quantities where
  `arkiv-node` expects string-encoded gas quantities.
- The JSON should be canonicalized or pretty-printed consistently so
  operators can diff releases.

## 4. Selection Semantics

When `currentBlock` is present, `arkiv-node` installs only schedule
entries whose `activationBlock <= currentBlock`.

When `currentBlock` is absent, `arkiv-node` installs the full schedule.
The low-level patched helpers then use the latest applicable entry.

This means the service can safely publish future schedule entries
without activating them early by keeping `currentBlock` below the
future activation block. The service should update `currentBlock` as it
observes the chain head.

If all entries are filtered out by `currentBlock`, the node falls back
to installing the first schedule entry. Because the first entry must
activate at block `0`, this is the safe baseline.

## 5. Versioning

`version` is the service's monotonic safety rail.

Node behavior:

- Initial `last accepted version` is `0`.
- A schedule with `version >= last accepted version` may be accepted.
- A schedule with `version < last accepted version` is rejected.
- The last accepted version is process-local; after node restart, the
  persisted schedule is loaded first and establishes the local version.

Service rules:

- Never intentionally publish a lower version to the primary URL.
- Increment `version` for every deliberate schedule change.
- If a bad schedule version was accepted by nodes, publish a corrected
  document with a higher version. Do not try to roll back by lowering
  `version`.
- Keep a release log mapping versions to operator intent, author, time,
  diff, and validation result.

## 6. Failure Handling

`arkiv-node` is fail-last-good:

- On startup, it loads the persisted local schedule file if present.
- If no persisted file exists, it writes a default schedule file using
  compiled defaults.
- During polling, if the HTTP request fails, the response is not `2xx`,
  the body is invalid JSON, or validation fails, the node keeps using
  the last accepted local schedule.
- After a successful fetch and install, the node writes the accepted
  body to `ARKIV_PROTOCOL_SCHEDULE_PATH`.

Service outage guidance:

- Returning no response is safer than returning invalid or partially
  generated JSON.
- Do not serve maintenance HTML at the schedule URL.
- If the backing store is unavailable, prefer `503 Service Unavailable`
  over a stale document whose provenance is unknown.
- Stale-but-valid JSON is acceptable when it is the intended last known
  schedule.

## 7. Publishing Workflow

Recommended release pipeline:

1. Build a candidate JSON document from source-controlled schedule data.
2. Validate the JSON against the rules in this document.
3. Verify `chainId` and monotonic `version`.
4. Publish the candidate to a staging URL.
5. Run an `arkiv-node` canary against the staging URL and confirm logs
   show `installed protocol schedule`.
6. Promote the exact same JSON bytes to the production URL.
7. Monitor node logs, HTTP status, and chain progression.

Minimum pre-publish checks:

```bash
jq . arkiv-protocol-schedule.json >/dev/null
```

The service implementation should also have unit tests for:

- empty schedule rejection;
- first activation block not equal to `0`;
- non-increasing activation blocks;
- version regression;
- invalid decimal or hex quantity strings;
- per-chain schedule selection.

## 8. Deployment Model

The service can be implemented as any static JSON host or small API
service. A static host is preferred unless dynamic `currentBlock`
updates are required.

Acceptable implementations:

- Object storage plus CDN, with strict cache revalidation.
- Nginx or Caddy serving a mounted JSON file.
- A small API service backed by a database or source-controlled config.
- A Docker Compose sidecar serving a local bind-mounted JSON file for
  testnets.

The service should expose health checks separately from the schedule
path:

```text
GET /healthz
```

Recommended `healthz` response:

```json
{
  "ok": true,
  "chainId": 42069,
  "version": 1
}
```

`arkiv-node` does not call `/healthz`; it is for operators and load
balancers.

## 9. Observability

The service should emit structured logs for every publish and every
served version:

- timestamp;
- path;
- status code;
- chain ID;
- version;
- current block if present;
- response hash;
- request latency.

Recommended metrics:

- `schedule_http_requests_total{status}`;
- `schedule_http_request_duration_seconds`;
- `schedule_current_version{chain_id}`;
- `schedule_current_block{chain_id}`;
- `schedule_response_hash_info{chain_id,version,hash}`;
- `schedule_validation_failures_total{reason}`.

Alert on:

- non-`2xx` responses from the node-facing URL;
- invalid JSON at the node-facing URL;
- version regression in the published document;
- response hash changing without a version increase;
- `currentBlock` lagging the chain head beyond the expected polling
  window.

## 10. Security

Because current nodes do not verify signatures, protect the publishing
path operationally:

- Restrict write access to the backing store.
- Require review for production schedule changes.
- Keep immutable history of published documents.
- Use HTTPS for any cross-host or public network deployment.
- Avoid shared mutable volumes on production hosts unless filesystem
  permissions and audit logging are in place.
- Treat DNS, CDN, and object-store permissions as consensus-sensitive.

The read endpoint may be public. The write path must not be reachable
from the public internet.

## 11. Node Configuration

`arkiv-node` reads three environment variables:

| Variable | Required | Default | Meaning |
|---|---:|---|---|
| `ARKIV_PROTOCOL_SCHEDULE_URL` | no | unset | Enables polling when non-empty. |
| `ARKIV_PROTOCOL_SCHEDULE_PATH` | no | `arkiv-protocol-schedule.json` | Local persisted schedule path. |
| `ARKIV_PROTOCOL_SCHEDULE_POLL_SECONDS` | no | `60` | Poll interval. Values less than or equal to zero fall back to `60`. |

Example:

```bash
export ARKIV_PROTOCOL_SCHEDULE_URL=https://schedule.testnet.arkiv.network/arkiv-protocol-schedule.json
export ARKIV_PROTOCOL_SCHEDULE_PATH=/var/lib/arkiv-node/arkiv-protocol-schedule.json
export ARKIV_PROTOCOL_SCHEDULE_POLL_SECONDS=60
```

The service should assume nodes may poll at the configured interval
without jitter. Size infrastructure accordingly or add caching at the
edge.

## 12. Compatibility Notes

The JSON schema in this document matches the current Rust consumer in
`crates/arkiv-node/src/protocol_schedule.rs`.

Future schema changes should be additive:

- Add optional fields first.
- Keep existing field names and numeric encodings stable.
- Continue serving the current schema until all deployed nodes are
  upgraded.
- Use a new endpoint path only for breaking changes.
