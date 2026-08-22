//! Servidor WebSocket do daemon.
//!
//! Expõe um endpoint `/ws` que aceita conexões de clients (apps
//! Android) e serve um health-check em `/healthz`.

use std::net::SocketAddr;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tracing::{debug, info};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::protocol::DeviceId;
use crate::state::SharedState;

/// Endereço de bind padrão do servidor.
pub(crate) const DEFAULT_BIND: &str = "0.0.0.0:8765";
/// Nome amigável padrão do servidor.
pub(crate) const DEFAULT_NAME: &str = "linux-desktop";

/// Configuração runtime do servidor, construída uma única vez a
/// partir do [`Config`] no boot e compartilhada por referência em
/// todo o runtime (state, transport, message loop).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: String,
    pub name: String,
    pub clipboard: crate::config::ClipboardConfig,
    pub security: crate::config::SecurityConfig,
    /// ID próprio do daemon (persistido no TOML). Usado como
    /// `origin` estável no anti-eco do watcher.
    pub device_id: DeviceId,
}

impl ServerConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            bind: cfg.bind.clone(),
            name: cfg.name.clone(),
            clipboard: cfg.clipboard.clone(),
            security: cfg.security.clone(),
            device_id: cfg.device_id.clone().unwrap_or_default(),
        }
    }

    /// Porta extraída do `bind`. Fallback: 8765.
    pub fn port(&self) -> u16 {
        self.bind
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8765)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.into(),
            name: DEFAULT_NAME.into(),
            clipboard: crate::config::ClipboardConfig::default(),
            security: crate::config::SecurityConfig::default(),
            device_id: DeviceId::new(),
        }
    }
}

/// O servidor WebSocket.
#[derive(Debug)]
pub struct Server {
    pub config: ServerConfig,
    pub state: SharedState,
}

impl Server {
    pub fn new(config: ServerConfig, state: SharedState) -> Self {
        Self { config, state }
    }

    /// Sobe o servidor e aguarda até o shutdown (Ctrl+C ou sinal).
    pub async fn run(&self) -> Result<()> {
        let app = self.router();

        let addr: SocketAddr = self
            .config
            .bind
            .parse()
            .map_err(|e| Error::Config(format!("bind inválido {}: {e}", self.config.bind)))?;

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(Error::Io)?;
        info!(bind = %addr, name = %self.config.name, "servidor WebSocket escutando");

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(self.state.shutdown.clone().cancelled_owned())
        .await
        .map_err(|e| Error::Http(e.to_string()))
    }

    /// Monta o Router axum (endpoints `/ws` e `/healthz`).
    /// Público para permitir embedding e testes de integração.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/ws", get(ws_handler))
            .route("/healthz", get(healthz))
            .with_state(self.state.clone())
    }
}

/// Endpoint WebSocket principal.
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    if state.config.security.local_only && !is_local_address(peer_addr.ip()) {
        return (StatusCode::FORBIDDEN, "conexão remota bloqueada\n").into_response();
    }
    ws.on_upgrade(move |socket| {
        let addr = peer_addr.to_string();
        debug!(peer = %addr, "WebSocket upgrade aceito");
        let conn = crate::transport::Connection {
            socket,
            addr: peer_addr,
            state: state.clone(),
            config: state.config.clone(),
        };
        async move {
            conn.run().await;
        }
    })
}

/// Filtro de rede local implementável sem APIs de plataforma. Não valida
/// SSID nem prova que o peer está na mesma sub-rede.
fn is_local_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || (ip.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Health-check simples.
async fn healthz() -> impl IntoResponse {
    "clipsync daemon ok\n"
}

#[cfg(test)]
/// Parsea um endereço "ip:port" em SocketAddr, com fallback.
fn parse_addr(s: &str) -> SocketAddr {
    s.parse()
        .unwrap_or_else(|_| "0.0.0.0:0".parse().expect("addr estático"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ServerState;

    #[test]
    fn parse_addr_fallback() {
        let a = parse_addr("127.0.0.1:9000");
        assert_eq!(a.port(), 9000);
        let b = parse_addr("garbage");
        assert_eq!(b.port(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn server_healthz_ok() {
        let (state, _rx) = ServerState::new(ServerConfig::default(), None).unwrap();
        let state = std::sync::Arc::new(state);
        let server = Server::new(ServerConfig::default(), state.clone());
        let app = server.router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Health check via HTTP/1.1 cru.
        let resp = raw_get(&addr, "/healthz").await;
        assert!(resp.contains("clipsync"), "healthz respondeu: {resp}");
        handle.abort();
    }

    /// GET cru sem dependência de cliente HTTP (async, com timeout).
    async fn raw_get(addr: &SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .expect("write");
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            stream.read_to_end(&mut buf),
        )
        .await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn config_default_ok() {
        let cfg = Config::default();
        let sc = ServerConfig::from_config(&cfg);
        assert_eq!(sc.bind, "0.0.0.0:8765");
        assert_eq!(sc.name, "linux-desktop");
    }

    #[test]
    fn local_only_address_filter_is_explicit() {
        assert!(is_local_address("127.0.0.1".parse().unwrap()));
        assert!(is_local_address("192.168.1.10".parse().unwrap()));
        assert!(is_local_address("fe80::1".parse().unwrap()));
        assert!(is_local_address("fd00::1".parse().unwrap()));
        assert!(!is_local_address("8.8.8.8".parse().unwrap()));
        assert!(!is_local_address("2001:4860:4860::8888".parse().unwrap()));
    }
}
