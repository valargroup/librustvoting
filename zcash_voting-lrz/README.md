# zcash_voting-lrz

Upstream librustzcash facade for
[`zcash_voting`](https://crates.io/crates/zcash_voting). The underlying
package retains the `upstream`/`zakura` feature switch; this facade always
selects `upstream`.

Use the dependency key `zcash_voting` to keep existing Rust imports:

```toml
[dependencies]
zcash_voting = { package = "zcash_voting-lrz", version = "3.1.0-rc.12" }
```

Applications using the Zakura backend should depend directly on
`zcash_voting` with `default-features = false` and `features = ["zakura"]`.
