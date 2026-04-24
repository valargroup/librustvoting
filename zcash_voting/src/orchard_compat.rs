//! Byte-round-trip conversions between upstream orchard and valar-orchard types.
//!
//! valar-orchard 0.12 is a pure superset of upstream orchard 0.12 with the
//! governance-visibility additions, and the `FullViewingKey` / `SpendingKey`
//! wire formats are unchanged all the way back to upstream orchard 0.11 — no
//! key-format-affecting changes landed between 0.11 and 0.12. The conversions
//! here serialize through the public byte APIs and cannot fail for
//! well-formed inputs. Delete this module (and the `orchard_upstream` dep)
//! when the governance-visibility changes land in zcash/orchard upstream.

pub fn fvk_upstream_to_valar(
    upstream: &orchard_upstream::keys::FullViewingKey,
) -> orchard::keys::FullViewingKey {
    let bytes = upstream.to_bytes();
    orchard::keys::FullViewingKey::from_bytes(&bytes)
        .expect("upstream fvk bytes are a valid valar-orchard fvk (identical wire format)")
}

pub fn sk_upstream_to_valar(
    upstream: &orchard_upstream::keys::SpendingKey,
) -> orchard::keys::SpendingKey {
    let bytes = *upstream.to_bytes();
    Option::from(orchard::keys::SpendingKey::from_bytes(bytes))
        .expect("upstream sk bytes are a valid valar-orchard sk (identical wire format)")
}
