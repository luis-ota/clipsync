use std::sync::Arc;

use clap::Parser;
use clipsync_core::config::Config;
use clipsync_core::protocol::DeviceId;
use clipsync_relay::{AuthError, RelayConfig, RelayIdentity, RelayServer, TokenVerifier};

#[derive(Debug)]
struct EnvironmentVerifier {
    token: String,
    identity: RelayIdentity,
}

#[async_trait::async_trait]
impl TokenVerifier for EnvironmentVerifier {
    async fn verify(&self, opaque_token: &str) -> Result<RelayIdentity, AuthError> {
        (opaque_token == self.token)
            .then(|| self.identity.clone())
            .ok_or(AuthError)
    }
}

#[derive(Debug, Parser)]
#[command(name = "clipsync-relay", about = "TLS WebSocket relay do clipsync")]
struct Cli {
    #[arg(
        long,
        env = "CLIPSYNC_CONFIG",
        default_value = "/etc/clipsync/config.toml"
    )]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let config = match Config::load_or_default_env(Some(&cli.config)) {
        Ok(config) => config,
        Err(error) => exit_with_error(format!("erro carregando configuração: {error}")),
    };
    let relay_config = match RelayConfig::from_config(&config) {
        Ok(config) => config,
        Err(error) => exit_with_error(error),
    };
    let verifier = match verifier_from_environment() {
        Ok(verifier) => verifier,
        Err(error) => exit_with_error(error),
    };
    let server = RelayServer::new(relay_config, Arc::new(verifier));
    if let Err(error) = server.run().await {
        exit_with_error(format!("clipsync-relay: {error}"));
    }
}

fn verifier_from_environment() -> Result<EnvironmentVerifier, String> {
    let required = |name: &str| {
        std::env::var(name).map_err(|_| {
            format!("{name} é obrigatório; relay não inicia sem autenticação configurada")
        })
    };
    Ok(EnvironmentVerifier {
        token: required("CLIPSYNC_RELAY_TOKEN")?,
        identity: RelayIdentity {
            account_id: required("CLIPSYNC_RELAY_ACCOUNT_ID")?,
            device_id: DeviceId::from(required("CLIPSYNC_RELAY_DEVICE_ID")?),
        },
    })
}

fn exit_with_error(error: String) -> ! {
    eprintln!("clipsync-relay: {error}");
    std::process::exit(1);
}
