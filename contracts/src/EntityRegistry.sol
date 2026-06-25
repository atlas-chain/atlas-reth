// SPDX-License-Identifier: GPL-3.0-or-later
pragma solidity 0.8.25;

// ── Types ────────────────────────────────────────────────────────────────
//
// **There is no implementation contract.** The Arkiv precompile sits at
// `0x44…0044` (the address the SDK targets) and decodes
// `execute(Operation[])` / `nonces(address)` calldata directly. This file
// only declares the ABI shape so foundry / forge can produce matching
// artifacts and so SDK consumers have a canonical reference.

/// @dev Block number encoded as uint32. Kept as a UDVT so fields like
/// `Operation.btl` and `EntityOperation.expiresAt` carry the
/// distinct shape SDK wrappers depend on.
type BlockNumber32 is uint32;

/// @dev Validated lowercase-ASCII identifier (≤32 bytes, left-aligned).
///
/// Charset validation runs **inside the precompile**; it reverts with
/// the `Ident32InvalidByte(position, value)` / `Ident32Empty()`
/// selectors declared below.
type Ident32 is bytes32;

error Ident32Empty();
error Ident32InvalidByte(uint256 position, bytes1 value);

/// @dev 128-byte MIME type descriptor, four-word packed. Validation
/// runs in the precompile.
struct Mime128 {
    bytes32[4] data;
}

// ── Entity library ───────────────────────────────────────────────────────

library Entity {
    uint8 internal constant UNINITIALIZED = 0;
    uint8 internal constant CREATE = 1;
    uint8 internal constant UPDATE = 2;
    uint8 internal constant EXTEND = 3;
    uint8 internal constant TRANSFER = 4;
    uint8 internal constant DELETE = 5;
    uint8 internal constant EXPIRE = 6;

    uint8 internal constant ATTR_UINT = 1;
    uint8 internal constant ATTR_STRING = 2;
    uint8 internal constant ATTR_ENTITY_KEY = 3;

    /// @dev `Operation.contentType` value that tells the precompile to
    /// interpret `Operation.payload` as an Atlas payload-provider reference
    /// JSON document instead of inline entity bytes.
    string internal constant PAYLOAD_REFERENCE_CONTENT_TYPE =
        "application/vnd.atlas.payload-reference+json";

    struct Attribute {
        Ident32 name;
        uint8 valueType;
        bytes32[4] value;
    }

    struct Operation {
        uint8 operationType;
        bytes32 entityKey;
        bytes payload;
        Mime128 contentType;
        Attribute[] attributes;
        BlockNumber32 btl;
        address newOwner;
    }

    // Errors emitted by the precompile via Solidity-style reverts
    // (selector + abi-encoded args) so SDK error decoders resolve them.
    error EmptyBatch();
    error InvalidOpType(uint8 operationType);
    error ZeroBtl();
    error EntityNotFound(bytes32 entityKey);
    error NotOwner(bytes32 entityKey, address caller, address owner);
    error EntityExpired(bytes32 entityKey, BlockNumber32 expiresAt);
    error ExpiryNotExtended(
        bytes32 entityKey,
        BlockNumber32 newExpiresAt,
        BlockNumber32 currentExpiresAt
    );
    error TransferToZeroAddress(bytes32 entityKey);
    error TransferToSelf(bytes32 entityKey);
    error EntityNotExpired(bytes32 entityKey, BlockNumber32 expiresAt);

    /// @dev An attribute value carries non-canonical bytes for its
    /// declared `valueType`. For fixed-width types (`ATTR_UINT`,
    /// `ATTR_ENTITY_KEY`) the SDK packs into `value[0]` only and the
    /// remaining words must be zero; `wordIndex` is the 1-based index
    /// of the first offending word.
    error AttributeValueMalformed(bytes32 name, uint8 valueType, uint256 wordIndex);

    /// @dev An `ATTR_STRING` value has an embedded null byte: a
    /// non-zero byte was found after a zero byte within the 128-byte
    /// value buffer. `position` is the index of the offending non-zero
    /// byte (0..127) and `value` is the byte itself. Mirrors
    /// `Ident32InvalidByte`'s shape.
    error AttributeStringInvalidByte(bytes32 name, uint256 position, bytes1 value);

    /// @dev Payload-reference JSON is not valid v1 reference data.
    error PayloadReferenceMalformed();

    /// @dev Payload-reference version is not supported by this
    /// precompile revision.
    error PayloadReferenceUnsupportedVersion(uint256 version);

    /// @dev The declared provider/service is not a consensus-trusted
    /// provider identity.
    error PayloadProviderUnknown(string provider);

    /// @dev The recovered provider signer is not in the consensus
    /// signer allowlist.
    error PayloadProviderSignerNotAllowed(address signer);

    /// @dev The signed receipt does not match the outer reference
    /// metadata.
    error PayloadProviderReceiptMismatch();

    /// @dev The provider signature is malformed, internally
    /// inconsistent, has a bad message hash, or does not recover to
    /// the declared signer.
    error PayloadProviderSignatureInvalid();

    /// @dev Reserved for callers that try to submit a payload reference
    /// with a non-reference content type.
    error PayloadReferenceContentTypeInvalid(bytes contentType);

    /// @dev The caller already consumed this signed payload-reference
    /// nonce.
    error PayloadReferenceNonceUsed(bytes32 nonce);

    /// @dev The signed payload-reference payment amount is invalid.
    error PayloadReferencePaymentInvalid(uint256 payment);

    /// @dev Create/update operations must use the payload-reference
    /// content type. Inline payload bytes are no longer accepted.
    error PayloadReferenceRequired(bytes contentType);
}

/// @title IEntityRegistry
/// @notice ABI surface implemented by the Arkiv precompile at
///         `0x4400000000000000000000000000000000000044`.
///
/// **Not a deployed contract.** The precompile is invoked directly via
/// normal `CALL` / `STATICCALL` to that address and decodes the
/// calldata itself. This interface exists so SDK / forge consumers
/// have a canonical ABI to generate against.
interface IEntityRegistry {
    /// @notice Per-caller monotonic counter used to mint entity keys.
    /// `STATICCALL`-safe (read-only).
    function nonces(address owner) external view returns (uint32);

    /// @notice Submit a batch of operations atomically. Each op is
    ///         validated, applied, and emits an `EntityOperation`
    ///         event in order. Any revert rolls back the whole batch.
    function execute(Entity.Operation[] calldata ops) external;

    /// @notice Emitted once per validated op. The `entityHash` field
    /// is always `bytes32(0)` — the field is reserved for a future
    /// rolling-hash extension.
    event EntityOperation(
        bytes32 indexed entityKey,
        uint8 indexed operationType,
        address indexed owner,
        BlockNumber32 expiresAt,
        bytes32 entityHash
    );
}
