use std::path::PathBuf;
use std::sync::Arc;

use clipsync_relay::{FileTokenProvider, RelayServer};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let path = std::env::var_os("CLIPSYNC_RELAY_TOKEN_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/clipsync/relay.tokens"));
    let provider = match FileTokenProvider::from_path(&path) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("clipsync-relay: unable to load token provider: {error}");
            std::process::exit(1);
        }
    };
    let server = RelayServer::new_with_groups(
        clipsync_relay::RelayConfig::default(),
        Arc::new(provider.clone()),
        provider.authorizer(),
    );
    if let Err(error) = server.run().await {
        eprintln!("clipsync-relay: {error}");
        std::process::exit(1);
    }
}
