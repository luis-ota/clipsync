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

use clipsync_core::clipboard::{ClipboardEvent, ClipboardManager, WriteOrigin, MIME_HTML};
use clipsync_core::config::Config;
use clipsync_core::discovery::Discovery;
use clipsync_core::protocol::{DeviceId, Message};
use clipsync_core::server::{Server, ServerConfig};
use clipsync_core::state::ServerState;

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
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Roda o daemon: server + watcher de clipboard + mDNS.
async fn cmd_run(config: Config, no_tray: bool) -> Result<(), Box<dyn std::error::Error>> {
    let server_config = ServerConfig::from_config(&config);
    let (state, mut peer_events_rx) = ServerState::new(server_config.clone());
    let state = std::sync::Arc::new(state);

    // Clipboard manager
    let clipboard = ClipboardManager::new()?;
    clipboard.check_tools().ok();

    // mDNS announce
    let discovery = Discovery::new()?;
    let port = config
        .server
        .bind
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8765);
    let _ = discovery.announce(&config.server.name, port);

    // Watcher: clipboard local → peers (broadcast)
    let watcher_rx = clipboard.watch(Duration::from_millis(config.clipboard.poll_interval_ms));
    let state_watcher = state.clone();
    let sync_text = config.clipboard.sync_text;
    let sync_images = config.clipboard.sync_images;
    let sync_html = config.clipboard.sync_html;
    tokio::spawn(async move {
        let mut rx = watcher_rx;
        while let Some(evt) = rx.recv().await {
            match evt {
                ClipboardEvent::Changed(snap) => {
                    let msg = match &snap.mime[..] {
                        m if m.starts_with("text/") && sync_text => Message::ClipboardText {
                            mime: m.to_owned(),
                            content: String::from_utf8_lossy(&snap.bytes).into_owned(),
                            origin: DeviceId::new(),
                            sha256: snap.sha256,
                        },
                        m if m.starts_with("image/") && sync_images => Message::ClipboardImage {
                            mime: m.to_owned(),
                            data_b64: {
                                use base64::Engine;
                                base64::engine::general_purpose::STANDARD.encode(&snap.bytes)
                            },
                            width: None,
                            height: None,
                            sha256: snap.sha256,
                            origin: DeviceId::new(),
                        },
                        m if m == MIME_HTML && sync_html => Message::ClipboardHtml {
                            html: String::from_utf8_lossy(&snap.bytes).into_owned(),
                            alt: None,
                            sha256: snap.sha256,
                            origin: DeviceId::new(),
                        },
                        _ => continue,
                    };
                    state_watcher.broadcast_except(msg, None).await;
                }
                ClipboardEvent::BackendLost(e) => {
                    warn!(error = %e, "backend de clipboard perdido");
                }
            }
        }
    });

    // Peers → clipboard local (grava com origem Remote para anti-eco)
    tokio::spawn(async move {
        let mut cm = ClipboardManager::new().unwrap_or_else(|_| ClipboardManager::headless());
        while let Some(evt) = peer_events_rx.recv().await {
            match evt {
                ClipboardEvent::Changed(snap) => {
                    if snap.mime.starts_with("text/") {
                        let _ = cm.write_text(snap.text().unwrap_or_default(), WriteOrigin::Remote);
                    } else if snap.mime.starts_with("image/") {
                        let _ = cm.write_image(&snap.mime, &snap.bytes, WriteOrigin::Remote);
                    }
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
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<tray::TrayCommand>(16);
        match tray::spawn(cmd_tx).await {
            Some(handle) => {
                let handle_for_updater = handle.clone();
                let state_for_updater = state.clone();
                // Atualiza periodicamente o tooltip/menu do tray com
                // PIN e contagem de peers.
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(Duration::from_secs(2));
                    ticker.tick().await; // primeiro tick imediato
                    loop {
                        ticker.tick().await;
                        let peer_count = state_for_updater.peer_count().await;
                        let pin = state_for_updater.pairing.lock().await.active_pin();
                        let status = tray::TrayStatus {
                            peer_count,
                            pin,
                            state: "rodando".to_string(),
                        };
                        tray::update(&handle_for_updater, status).await;
                    }
                });

                // Lida com comandos vindos do menu do tray.
                let state_for_cmds = state.clone();
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
            None => None,
        }
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
    if let Some(pin) = read_current_pin() {
        println!("PIN atual: {pin}");
    } else {
        eprintln!("Nenhum PIN disponível. Rode 'clipsyncd run' para gerar um.");
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

/// Lê o PIN corrente do file de runtime se existir.
fn read_current_pin() -> Option<String> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let pin_path = std::path::Path::new(&runtime_dir).join("clipsync-pin");
    if pin_path.exists() {
        std::fs::read_to_string(&pin_path)
            .ok()
            .map(|s| s.trim().to_owned())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Run { no_tray }) => {
            let config = Config::load_or_default(None).unwrap_or_default();
            if let Err(e) = cmd_run(config, no_tray).await {
                eprintln!("Erro: {e}");
                std::process::exit(1);
            }
        }
        Some(Commands::ShowPin) => cmd_show_pin(),
        Some(Commands::ListPeers) => {
            eprintln!("ListPeers requer estado do daemon; disponível em versão futura (v0.2)");
        }
        Some(Commands::Untrust { device }) => {
            eprintln!(
                "Untrust ({device}) requer estado do daemon; disponível em versão futura (v0.2)"
            );
        }
        Some(Commands::ShowAddress) => cmd_show_address(),
        Some(Commands::ServiceInstall) => cmd_service_install(),
        Some(Commands::Discover { timeout }) => {
            if let Err(e) = cmd_discover(timeout).await {
                eprintln!("Erro: {e}");
                std::process::exit(1);
            }
        }
        None => {
            cmd_run(Config::default(), false).await.ok();
        }
    }
}
