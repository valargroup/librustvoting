# zcash_voting-lrz

LRZ (upstream librustzcash) facade for
[`zcash_voting`](https://crates.io/crates/zcash_voting). The underlying
package exposes `lrz` as its non-default backend feature; this facade exposes
only that backend and selects it by default.

Use the dependency key `zcash_voting` to keep existing Rust imports:

```toml
[dependencies]
zcash_voting = {
    package = "zcash_voting-lrz",
    version = "3.1.0-rc.12",
    default-features = false,
    features = ["lrz"],
}
```

Applications using the Zakura backend should depend directly on
`zcash_voting`; Zakura is its default backend.
