use std::time::Duration;

use zcash_voting::{prelude::*, HyperTransport};

#[test]
fn helper_transport_method_syntax_remains_available_from_the_prelude() {
    let transport = HyperTransport::new();

    // This is intentionally method syntax: exporting another transport trait
    // with a method named `get` makes existing prelude users fail with E0034,
    // even when the competing method has a different argument count.
    let get = transport.get("https://helper.example", Duration::from_secs(1));
    drop(get);

    let post = transport.post_json(
        "https://helper.example",
        b"{}".to_vec(),
        Duration::from_secs(1),
    );
    drop(post);
}
