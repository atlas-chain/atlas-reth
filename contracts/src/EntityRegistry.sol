// SPDX-License-Identifier: GPL-3.0-or-later
pragma solidity 0.8.25;

// ── Types ────────────────────────────────────────────────────────────────
//
// Inlined verbatim from arkiv-contracts v1 so the external ABI surface
// (function selectors, struct layouts, indexed event fields) matches the
// shape the Rust SDK (`arkiv-bindings`) was generated against.
//
// **There is no implementation contract in v2.** The Arkiv precompile
// sits at `0x44…0044` (the address the SDK already targets) and decodes
// `execute(Operation[])` / `nonces(address)` calldata directly. This file
// only declares the ABI shape so foundry / forge can keep producing
// matching artifacts and so SDK consumers have a canonical reference.

/// @dev Block number encoded as uint32. Kept as a UDVT so v1-shaped
/// fields (`Operation.btl`, `EntityOperation.expiresAt`, …) stay
/// ABI-identical for SDK consumers.
type BlockNumber32 is uint32;

/// @dev Validated lowercase-ASCII identifier (≤32 bytes, left-aligned).
/// v1 UDVT preserved so the SDK's `Ident32` wrapper resolves.
///
/// Charset validation runs **inside the precompile** in v2; it reverts
/// with the v1 `Ident32InvalidByte(position, value)` / `Ident32Empty()`
/// selectors so SDK error decoders keep working.
type Ident32 is bytes32;

error Ident32Empty();
error Ident32InvalidByte(uint256 position, bytes1 value);

/// @dev 128-byte MIME type descriptor, four-word packed. v1 struct
/// preserved so the SDK's `Mime128` resolves. Validation runs in the
/// precompile.
struct Mime128 {
    bytes32[4] data;
}

// ── Entity library ───────────────────────────────────────────────────────
//
// Op-type constants, structs, and errors — names and signatures held
// identical to arkiv-contracts v1.

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

    // Errors — names + arg shapes preserved from v1. The precompile
    // emits these via Solidity-style reverts (selector + abi-encoded
    // args) so SDK error decoders match v1.
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
}

/// @title IEntityRegistry
/// @notice ABI surface implemented by the Arkiv precompile at
///         `0x4400000000000000000000000000000000000044`.
///
/// **Not a deployed contract.** There is no Solidity implementation in
/// v2 — the precompile is invoked directly via normal `CALL` /
/// `STATICCALL` to that address and decodes the calldata itself. This
/// interface exists so SDK / forge consumers have a canonical ABI to
/// generate against; the function selectors and event signatures match
/// the v1 `EntityRegistry` contract exactly.
interface IEntityRegistry {
    /// @notice Per-caller monotonic counter used to mint entity keys.
    /// `STATICCALL`-safe (read-only).
    function nonces(address owner) external view returns (uint32);

    /// @notice Submit a batch of operations atomically. Each op is
    ///         validated, applied, and emits an `EntityOperation`
    ///         event in order. Any revert rolls back the whole batch.
    function execute(Entity.Operation[] calldata ops) external;

    /// @notice Emitted once per validated op. Signature held identical
    /// to v1 for SDK compatibility. The `entityHash` field is always
    /// `bytes32(0)` in v2 — the rolling EIP-712 hash machinery has been
    /// removed.
    event EntityOperation(
        bytes32 indexed entityKey,
        uint8 indexed operationType,
        address indexed owner,
        BlockNumber32 expiresAt,
        bytes32 entityHash
    );
}
