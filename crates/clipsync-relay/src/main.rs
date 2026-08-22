use std::sync::Arc;

use clipsync_relay::{AuthError, RelayIdentity, RelayServer, TokenVerifier};

#[derive(Debug)]
struct EnvironmentVerifier;

#[async_trait::async_trait]
impl TokenVerifier for EnvironmentVerifier {
    async fn verify(&self, _opaque_token: &str) -> Result<RelayIdentity, AuthError> {
        Err(AuthError)
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let server = RelayServer::new(
        clipsync_relay::RelayConfig::default(),
        Arc::new(EnvironmentVerifier),
    );
    if let Err(error) = server.run().await {
        eprintln!("clipsync-relay: {error}");
        std::process::exit(1);
    }
}
