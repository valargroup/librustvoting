# zcash_voting-zakura

Zakura wallet-libraries backend for
[`zcash_voting`](https://crates.io/crates/zcash_voting).

Use the dependency key `zcash_voting` to keep existing Rust imports:

```toml
[dependencies]
zcash_voting = { package = "zcash_voting-zakura", version = "3.1.0-rc.12" }
```

Applications should depend on either `zcash_voting` or
`zcash_voting-zakura`, never both.
