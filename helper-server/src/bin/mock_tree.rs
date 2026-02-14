//! Mock tree dev server binary.
//!
//! Wraps an in-memory TreeServer behind the same REST API as the real chain,
//! so that `HttpTreeSyncApi`, the helper server, and the iOS client can all
//! sync against it during development.
//!
//! Usage:
//!   cargo run --bin mock-tree -- --port 8080

use clap::Parser;

// Access the mock_tree module from the library crate.
use helper_server::mock_tree;

#[derive(Parser)]
#[command(name = "mock-tree", about = "Mock vote commitment tree dev server")]
struct Cli {
    /// Port to listen on.
    #[arg(long, default_value = "8080")]
    port: u16,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mock_tree=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let app = mock_tree::router();

    let addr = format!("0.0.0.0:{}", cli.port);
    tracing::info!(addr = %addr, "mock tree dev server listening");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
