//! `clipsyncd` — daemon do server para sincronização de clipboard.
//!
//! O daemon escuta conexões WebSocket de clients Android, anuncia-se via
//! mDNS e sincroniza clipboard text/image entre dispositivos.
//! Zero cloud, zero servidor externo.

use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::signal;
use tokio::sync::mpsc;
use tracing::{info, warn};

use clipsync_core::clipboard::{ClipboardEvent, ClipboardManager};
use clipsync_core::config::Config;
use clipsync_core::discovery::Discovery;
use clipsync_core::dispatch;
use clipsync_core::server::{Server, ServerConfig};
use clipsync_core::state::ServerState;

use std::sync::Arc;

mod tray;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "clipsyncd")]
#[command(about = "Desktop daemon para clipboard universal Linux ↔ Android", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Inicia o daemon (modo foreground com logs e PIN).
    Run {
        /// Arquivo TOML de configuração (ou CLIPSYNC_CONFIG).
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
        /// Desativa o ícone de bandeja (modo headless).
        /// Também pode ser ativado via CLIPSYNC_NO_TRAY=1.
        #[arg(long, env = "CLIPSYNC_NO_TRAY")]
        no_tray: bool,
    },
    /// Mostra o PIN de pareamento atual.
    ShowPin,
    /// Lista devices pareados e confiados.
    ListPeers,
    /// Remove um device da lista de confiados.
    Untrust { device: String },
    /// Mostra o endereço mDNS e porta do serviço.
    ShowAddress,
    /// Instala um unit do systemd --user.
    ServiceInstall,
    /// Descobre daemons clipsync na rede local via mDNS.
    Discover {
        /// Tempo máximo de espera pela descoberta, em segundos.
        #[arg(long, default_value_t = 5)]
        timeout: u64,
    },
    /// Valida a configuração sem iniciar o daemon.
    ValidateConfig {
        #[arg(long, value_name = "PATH")]
        config: Option<std::path::PathBuf>,
    },
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Roda o daemon: server + watcher de clipboard + mDNS.
async fn cmd_run(config: Config, no_tray: bool) -> Result<(), Box<dyn std::error::Error>> {
    let server_config = ServerConfig::from_config(&config);
    let trusted_path = clipsync_core::config::trusted_devices_path()?;
    let (state, mut peer_events_rx) = ServerState::new(server_config.clone(), Some(&trusted_path))?;
    let state = std::sync::Arc::new(state);

    // Clipboard manager
    let clipboard = ClipboardManager::new()?;
    if let Err(e) = clipboard.check_tools().await {
        warn!(error = %e, "ferramentas de clipboard ausentes; modo headless");
    }
    // O caminho peer→local compartilha o rastro de escrita própria com
    // o watcher: sem isso, o watcher veria a própria escrita como
    // mudança externa e ecoaria para todos os peers.
    let clipboard_writer = clipboard.share_self_write();

    // mDNS announce
    let discovery = if config.discovery.enable_mdns {
        Some(Discovery::new()?)
    } else {
        None
    };
    let port = server_config.port();
    let daemon_id = server_config.device_id.clone();
    let tls_identity = if matches!(
        server_config.security.transport,
        clipsync_core::config::Transport::Tls
    ) {
        Some(clipsync_core::tls::Identity::load_or_generate(
            &server_config.security,
        )?)
    } else {
        None
    };
    if let Some(discovery) = discovery.as_ref() {
        if let Err(e) = discovery.announce(
            &server_config.name,
            port,
            &daemon_id,
            tls_identity.as_ref().map(|i| i.fingerprint.as_str()),
        ) {
            warn!(error = %e, "falha anunciando serviço mDNS");
        }
    } else {
        info!("mDNS desativado pela configuração");
    }

    // Watcher: clipboard local → peers (broadcast)
    // `origin` é o device_id persistido do daemon (estável por sessão),
    // nunca um UUID novo por frame: o dedup last_origin+last_seq dos
    // clients só funciona com origin estável.
    let clipboard_cfg = server_config.clipboard.clone();
    let watcher_rx = clipboard.watch(Duration::from_millis(clipboard_cfg.poll_interval_ms));
    let state_watcher = state.clone();
    tokio::spawn(async move {
        let mut rx = watcher_rx;
        while let Some(evt) = rx.recv().await {
            match evt {
                ClipboardEvent::Changed(snap) => {
                    if let Some(msg) = dispatch::event_to_message(&snap, &clipboard_cfg, &daemon_id)
                    {
                        state_watcher.broadcast_except(Arc::new(msg), None).await;
                    }
                }
                ClipboardEvent::BackendLost(e) => {
                    warn!(error = %e, "backend de clipboard perdido");
                }
            }
        }
    });

    // Peers → clipboard local (grava com origem Remote para anti-eco)
    tokio::spawn(async move {
        let mut cm = clipboard_writer;
        while let Some(evt) = peer_events_rx.recv().await {
            match evt {
                ClipboardEvent::Changed(snap) => {
                    dispatch::apply_peer_snapshot(&snap, &mut cm).await;
                }
                ClipboardEvent::BackendLost(e) => {
                    warn!(error = %e, "backend perdido no fluxo peer→local");
                }
            }
        }
    });

    // Server
    let server = Server::new(server_config, state.clone());
    info!(
        name = %server.config.name,
        bind = %server.config.bind,
        "clipsyncd em execução"
    );

    // Ícone de bandeja (opcional). Iniciado DEPOIS do server estar de pé
    // para que o daemon funcione mesmo em ambientes sem D-Bus/SNI host.
    // Falhas no tray NUNCA derrubam o daemon.
    let tray_handle = if no_tray {
        info!("tray desativado (--no-tray)");
        None
    } else {
        setup_tray(state.clone()).await
    };

    tokio::select! {
        res = server.run() => {
            if let Err(e) = res {
                eprintln!("Erro do servidor: {e}");
                if let Some(h) = tray_handle.as_ref() {
                    tray::shutdown(h);
                }
                return Err(e.to_string().into());
            }
        }
        _ = async {
            signal::ctrl_c().await.ok();
            info!("interrupção solicitada");
        } => {
            info!("encerrando");
        }
        _ = state.shutdown.cancelled() => {
            info!("encerrando (solicitado pelo tray)");
        }
    }

    if let Some(h) = tray_handle.as_ref() {
        tray::shutdown(h);
    }
    let _ = discovery;
    Ok(())
}

fn load_config_or_exit(path: Option<&std::path::Path>) -> Config {
    match Config::load_or_default_env(path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Erro carregando configuração: {error}");
            std::process::exit(1);
        }
    }
}

/// Inicia o ícone de bandeja e suas tasks de atualização e comando.
/// Retorna `None` se o tray não pôde ser iniciado (D-Bus indisponível).
async fn setup_tray(state: clipsync_core::state::SharedState) -> Option<tray::TrayHandle> {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<tray::TrayCommand>(16);
    let handle = tray::spawn(cmd_tx).await?;

    // Atualiza periodicamente o tooltip/menu do tray com PIN e peers.
    let handle_for_updater = handle.clone();
    let state_for_updater = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let peer_count = state_for_updater.peer_count().await;
            let pin = state_for_updater.pairing.lock().await.active_pin();
            let status = tray::TrayStatus {
                peer_count,
                pin,
                state: tray::DaemonState::Running,
            };
            tray::update(&handle_for_updater, status).await;
        }
    });

    // Lida com comandos vindos do menu do tray.
    let state_for_cmds = state;
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                tray::TrayCommand::ShowPin => {
                    let pin = state_for_cmds.pairing.lock().await.active_pin();
                    tray::show_pin(pin).await;
                }
                tray::TrayCommand::ListPeers => {
                    let peers = state_for_cmds.peer_list().await;
                    let n = peers.len();
                    info!(peer_count = n, "peers conectados:");
                    for p in &peers {
                        info!(name = %p.name, addr = %p.addr, "  peer");
                    }
                    tray::show_peers(n).await;
                }
                tray::TrayCommand::Quit => {
                    info!("encerrando via tray");
                    state_for_cmds.shutdown.cancel();
                }
            }
        }
    });

    Some(handle)
}

/// Descobre daemons na rede local via mDNS e imprime os serviços
/// encontrados (nome, endereços, porta e propriedades TXT).
async fn cmd_discover(timeout_secs: u64) -> Result<(), Box<dyn std::error::Error>> {
    let discovery = Discovery::new()?;
    let timeout = Duration::from_secs(timeout_secs);
    info!(timeout_secs, "mDNS: buscando serviços na rede local");

    let services = discovery.browse(timeout, 500).await?;

    if services.is_empty() {
        println!("Nenhum daemon clipsync encontrado na rede local.");
        return Ok(());
    }

    println!("Daemons clipsync encontrados ({}):", services.len());
    for svc in &services {
        let addrs: Vec<String> = svc.addrs.iter().map(|a| a.to_string()).collect();
        println!("  {}", svc.instance);
        println!("    Endereços: {}", addrs.join(", "));
        println!("    Porta: {}", svc.port);
        let mut properties: Vec<_> = svc.properties.iter().collect();
        properties.sort();
        for (key, value) in properties {
            println!("    {key}: {value}");
        }
    }
    Ok(())
}

fn cmd_show_pin() {
    // O PIN vive em memória no PairingManager dentro do daemon em
    // execução. Offline não é possível consultá-lo. O tray (menu de
    // bandeja) consegue exibir o PIN diretamente do daemon via canal
    // interno — use-o ou rode `clipsyncd run` e observe o log.
    eprintln!(
        "O PIN só está disponível enquanto o daemon está rodando.\n\
         Use o menu de bandeja (clique direito no ícone → Mostrar PIN)\n\
         ou rode 'clipsyncd run' e observe o log do PIN no startup."
    );
}

/// Path do arquivo de devices confiados, ou encerra com erro.
fn trusted_path_or_exit() -> std::path::PathBuf {
    match clipsync_core::config::trusted_devices_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Erro localizando trusted devices: {e}");
            std::process::exit(1);
        }
    }
}

/// Lista os devices confiados persistidos em `path` (trusted.toml).
/// Opera offline, sem precisar do daemon rodando.
fn list_trusted(path: &std::path::Path) -> clipsync_core::Result<()> {
    let store = clipsync_core::pairing::TrustedStore::load(path)?;
    if store.devices.is_empty() {
        println!("Nenhum device pareado ainda.");
        return Ok(());
    }
    let devices = store.trusted_devices();
    println!("Devices confiados ({}):", devices.len());
    for d in devices {
        println!("  {}", d.name);
        println!("    ID: {}", d.id);
        println!("    Tipo: {}", d.kind);
        println!("    Último visto: {}", format_last_seen(d.last_seen));
    }
    Ok(())
}

/// Remove o device `device_id` do store persistido em `path`.
/// Retorna true se o device existia e foi removido (e salvo em disco).
fn untrust_device(path: &std::path::Path, device_id: &str) -> clipsync_core::Result<bool> {
    let _store_lock = clipsync_core::pairing::TrustedStoreLock::try_acquire(path)?;
    let mut store = clipsync_core::pairing::TrustedStore::load(path)?;
    if store.remove(device_id) {
        store.save(path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Formata um Unix timestamp como data/hora UTC legível.
fn format_last_seen(ts: i64) -> String {
    if ts == 0 {
        return "nunca".into();
    }
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn cmd_list_peers() {
    let path = trusted_path_or_exit();
    if let Err(e) = list_trusted(&path) {
        eprintln!("Erro lendo trusted devices: {e}");
        std::process::exit(1);
    }
}

fn cmd_untrust(device: &str) {
    let path = trusted_path_or_exit();
    match untrust_device(&path, device) {
        Ok(true) => println!("Device removido: {device}"),
        Ok(false) => {
            eprintln!("Device não encontrado: {device}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Erro removendo device: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_show_address() {
    println!("Serviço mDNS: _clipsync._tcp.local");
    println!("Porta padrão: 8765 (configurável em config.toml)");
}

fn cmd_service_install() {
    let unit = r#"# clipsyncd.service
[Unit]
Description=Clipboard sync daemon (Linux <-> Android)
After=graphical-session.target
Wants=graphical-session.target

[Service]
Type=simple
ExecStart=%h/.cargo/bin/clipsyncd run
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"#;
    let path = std::path::Path::new("clipsyncd.service");
    if let Err(e) = std::fs::write(path, unit) {
        eprintln!("Falha gravando {path:?}: {e}");
        return;
    }
    println!("Unit gerado em {path:?}");
    println!("Instale com:");
    println!("  mkdir -p ~/.config/systemd/user");
    println!("  cp clipsyncd.service ~/.config/systemd/user/");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable --now clipsyncd");
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Run { no_tray, config }) => {
            let config = load_config_or_exit(config.as_deref());
            if let Err(e) = cmd_run(config, no_tray).await {
                eprintln!("Erro: {e}");
                std::process::exit(1);
            }
        }
        Some(Commands::ShowPin) => cmd_show_pin(),
        Some(Commands::ListPeers) => cmd_list_peers(),
        Some(Commands::Untrust { device }) => cmd_untrust(&device),
        Some(Commands::ShowAddress) => cmd_show_address(),
        Some(Commands::ServiceInstall) => cmd_service_install(),
        Some(Commands::Discover { timeout }) => {
            if let Err(e) = cmd_discover(timeout).await {
                eprintln!("Erro: {e}");
                std::process::exit(1);
            }
        }
        Some(Commands::ValidateConfig { config }) => {
            let config = load_config_or_exit(config.as_deref());
            println!(
                "configuração válida: bind={} name={}",
                config.bind, config.name
            );
        }
        None => {
            if let Err(e) = cmd_run(load_config_or_exit(None), false).await {
                eprintln!("Erro: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipsync_core::pairing::{TrustedDevice, TrustedStore};
    use clipsync_core::DeviceId;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("clipsync-cli-{name}-{}", std::process::id()))
    }

    fn sample_store() -> TrustedStore {
        TrustedStore {
            devices: vec![TrustedDevice {
                id: DeviceId::from("abc-123"),
                name: "Pixel 8".into(),
                kind: "android".into(),
                last_seen: 1_700_000_000,
                paired_at: 1_690_000_000,
                trusted: true,
            }],
        }
    }

    #[test]
    fn untrust_removes_device_from_file() {
        let path = temp_path("untrust");
        let _ = std::fs::remove_file(&path);
        sample_store().save(&path).unwrap();
        assert!(untrust_device(&path, "abc-123").unwrap());
        assert!(TrustedStore::load(&path).unwrap().devices.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn untrust_missing_device_returns_false() {
        let path = temp_path("untrust-missing");
        let _ = std::fs::remove_file(&path);
        sample_store().save(&path).unwrap();
        assert!(!untrust_device(&path, "nonexistent").unwrap());
        assert_eq!(TrustedStore::load(&path).unwrap().devices.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn untrust_without_file_returns_false() {
        let path = temp_path("untrust-no-file");
        let _ = std::fs::remove_file(&path);
        assert!(!untrust_device(&path, "abc-123").unwrap());
    }

    #[test]
    fn untrust_refuses_store_owned_by_daemon() {
        let path = temp_path("untrust-busy");
        let _ = std::fs::remove_file(&path);
        let _owner = clipsync_core::pairing::PairingManager::new_with_store(&path).unwrap();

        assert!(matches!(
            untrust_device(&path, "abc-123"),
            Err(clipsync_core::Error::StoreBusy(busy_path)) if busy_path == path
        ));
    }

    #[test]
    fn list_trusted_empty_store_is_empty() {
        let path = temp_path("list-empty");
        let _ = std::fs::remove_file(&path);
        assert!(list_trusted(&path).is_ok());
    }
}
