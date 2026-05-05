# Changelog
All notable changes to this workspace will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this workspace adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

# Unreleased

## Added
- Split the `zcash_voting` network-facing `client` feature into granular
  `client-pir` and `client-tree-sync` features. The existing `client` feature
  remains as a backwards-compatible aggregate of both.
- Made the PIR proof conversion/validation helper available to downstream
  consumers so SDK FFI layers can validate `pir_client::ImtProofData` without
  enabling vote-commitment-tree sync.

## Changed
- Bumped the `zcash_voting` crate version to `0.4.1` for the additive feature
  split.
