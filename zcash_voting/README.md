# zcash_voting

Client-side library for integrating [Zcash shielded voting](https://github.com/valargroup/vote-sdk) into a wallet. Wraps the Halo 2 ZKPs, hotkey derivation, share construction, and governance-PCZT assembly that a wallet needs to participate in an on-chain voting round.

## Usage

Wallets typically consume this through a language bridge:

- **Rust wallets**: add `zcash_voting = "0.1"` to `Cargo.toml`.
- **iOS wallets**: depend on `valargroup/zcash-swift-wallet-sdk` branch `shielded-vote`, which bundles this crate.

See the [wallet integration guide](https://github.com/valargroup/vote-sdk/blob/main/docs/wallet-integration.md) for the full flow.

## Crate layout

| Crate | Purpose |
|---|---|
| **`zcash_voting`** (this crate) | Top-level API: proof generation, hotkey derivation, share construction, PCZT assembly, round-state storage. |
| [`vote-commitment-tree`](../vote-commitment-tree) | Append-only Poseidon Merkle tree for VANs and vote commitments. |
| [`vote-commitment-tree-client`](../vote-commitment-tree-client) | HTTP client + CLI for syncing the vote commitment tree from a running chain node. |

## Dependency notes

`zcash_voting` tracks the upstream Zcash crates directly:

- **`orchard 0.13`** — upstream [`zcash/orchard`](https://github.com/zcash/orchard), pinned via a `[patch.crates-io]` redirect to the `valargroup/orchard` `valar/0.13-spend-auth-g` branch (tracked by [valargroup/orchard PR #19](https://github.com/valargroup/orchard/pull/19)). That branch carries orchard 0.13.0 plus the `unstable-voting-circuits` feature gate that exposes the governance-visibility APIs, plus cherry-picks of [zcash/orchard #489](https://github.com/zcash/orchard/pull/489) (SpendAuthG fixed-base multiplication) and [zcash/orchard #495](https://github.com/zcash/orchard/pull/495) (`NoteValue::ZERO` public associated constant). Once both upstream PRs land and an `orchard 0.14` ships, this pin will collapse to the published crate.
- **`pczt`, `zcash_keys`, `zcash_primitives`, `zcash_protocol`, `zcash_address`, `zcash_encoding`, `zcash_transparent`** — pinned to a recent commit of upstream [`zcash/librustzcash`](https://github.com/zcash/librustzcash) `main`. The previous `valargroup/librustzcash` fork (with shielded-voting getters in PCZT and friends) has been fully retired now that the relevant PRs (#2281, #2283, #2284) have all merged upstream.

## License

Dual-licensed under MIT or Apache-2.0. See [LICENSE-MIT](../LICENSE-MIT) and [LICENSE-APACHE](../LICENSE-APACHE).
