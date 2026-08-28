# zcash-voting-impl

Shared implementation for the published `zcash_voting` and
`zcash_voting-zakura` facade crates.

This crate is an implementation detail. Applications should depend on exactly
one facade:

- `zcash_voting` for the upstream librustzcash family;
- `zcash_voting-zakura` for the Zakura wallet-libraries family.

The `upstream` and `zakura` features are mutually exclusive.
