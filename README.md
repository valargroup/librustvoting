# librustvoting

Client-side cryptographic library for Zcash shielded voting. Implements proof generation, vote construction, tree synchronization, and FFI bindings for the [Zally governance protocol](https://github.com/z-cale/shielded-vote-book).

## Workspace Crates

| Crate | Description |
|-------|-------------|
| **librustvoting** | Core library: ZKP delegation and vote proofs (Halo2), El Gamal encryption, governance PCZT construction, Merkle witness generation, SQLite round-state persistence |
| **zcash-voting-ffi** | UniFFI bridge exposing librustvoting to Swift (iOS) and Kotlin (Android) |
| **vote-commitment-tree** | Append-only Poseidon Merkle tree for Vote Authority Notes and Vote Commitments |
| **vote-commitment-tree-client** | HTTP client for syncing the vote commitment tree from a chain node |
| **imt-tree** | Indexed Merkle tree primitive used by the vote commitment tree and nullifier infrastructure |
| **pir-export** | Generates PIR tier files from the nullifier indexed Merkle tree |
| **pir-client** | PIR (Private Information Retrieval) query client for private nullifier exclusion proofs |

## Building

```bash
cargo check                    # check all crates
cargo build -p librustvoting   # build just the core library
cargo build -p zcash-voting-ffi --release  # FFI for mobile (release profile)
```

The workspace depends on the private [z-cale/voting-circuits](https://github.com/z-cale/voting-circuits) repo. The `.cargo/config.toml` enables `git-fetch-with-cli` so your local git credentials are used automatically.

## Dependency Strategy

This workspace uses `[patch.crates-io]` (in the root `Cargo.toml`) to override two dependency trees:

- **orchard 0.11** — Resolved from the [z-cale/voting-circuits](https://github.com/z-cale/voting-circuits) repo, which bundles an orchard fork with public visibility for `constants`, `spec`, and a `shared_primitives::spend_authority` gadget. This ensures type compatibility between librustvoting and voting-circuits.

- **librustzcash crates** (pczt, zcash_keys, zcash_client_sqlite, etc.) — Resolved from [z-cale/librustzcash](https://github.com/z-cale/librustzcash) branch `valargroup/pczt-governance-extensions-0.11`, based on `maint/zcash_client_sqlite-0.19.x`. Adds public getters and methods needed for governance PCZT construction and Merkle witness generation.

## Architecture

```
                    ┌──────────────────┐
                    │   zashi-ios /    │
                    │   zashi-android  │
                    └────────┬─────────┘
                             │ UniFFI
                    ┌────────┴─────────┐
                    │ zcash-voting-ffi │
                    └────────┬─────────┘
                             │
                    ┌────────┴─────────┐
              ┌─────┤  librustvoting   ├─────┐
              │     └──────────────────┘     │
              │              │               │
    ┌─────────┴──────┐ ┌────┴─────┐  ┌──────┴──────┐
    │ vote-commitment│ │   pir-   │  │  voting-    │
    │    -tree       │ │  client  │  │  circuits   │
    └───────┬────────┘ └────┬─────┘  └─────────────┘
            │               │
    ┌───────┴────────┐ ┌────┴─────┐
    │    imt-tree    │ │   pir-   │
    └────────────────┘ │  export  │
                       └──────────┘
```

## License

TODO
