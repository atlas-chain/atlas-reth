# contracts

Solidity ABI surface for Arkiv. **No deployed contracts.**
[`src/EntityRegistry.sol`](src/EntityRegistry.sol) declares the
`IEntityRegistry` interface, `Entity` library structs / constants /
errors, and the `EntityOperation` event. The Arkiv precompile at
`0x4400000000000000000000000000000000000044` implements this ABI —
EOAs and SDKs `CALL` that address with the same calldata they would
send to a Solidity contract.

This file exists so SDK / forge consumers have a canonical ABI to
codegen against.

## Build (optional, for SDK codegen)

```
forge build
```

Produces `out/EntityRegistry.sol/EntityRegistry.json`. Nothing in
`arkiv-op-reth` reads the build output — `arkiv-genesis` does not
bake contract bytecode into the binary.
