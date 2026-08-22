use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use clipsync_core::config::Config;
use clipsync_relay::{FileTokenProvider, RelayConfig, RelayServer};

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
        relay_config,
        Arc::new(provider.clone()),
        provider.authorizer(),
    );
    if let Err(error) = server.run().await {
        exit_with_error(format!("clipsync-relay: {error}"));
    }
}

fn exit_with_error(error: String) -> ! {
    eprintln!("clipsync-relay: {error}");
    std::process::exit(1);
}
