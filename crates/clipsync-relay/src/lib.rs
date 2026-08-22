//! TLS-only, in-memory WebSocket relay.
//!
//! Authentication is deliberately a boundary: this crate accepts an opaque
//! token and asks [`TokenVerifier`] for the already authenticated identity.
//! It does not mint, parse, hash, or persist credentials.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use clipsync_core::protocol::{DeviceId, Message};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub const DEFAULT_BIND: &str = "0.0.0.0:9443";
pub const DEFAULT_QUEUE_CAPACITY: usize = 64;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayIdentity {
    pub account_id: String,
    pub device_id: DeviceId,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("token rejected")]
pub struct AuthError;

/// Boundary for the authentication system owned by issue #75/deployment.
#[async_trait::async_trait]
pub trait TokenVerifier: Send + Sync + 'static {
    async fn verify(&self, opaque_token: &str) -> Result<RelayIdentity, AuthError>;
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub bind: String,
    pub security: clipsync_core::config::SecurityConfig,
    pub max_message_bytes: usize,
    pub queue_capacity: usize,
    pub limits: clipsync_core::config::LimitsConfig,
}

impl Default for RelayConfig {
    fn default() -> Self {
        let security = clipsync_core::config::SecurityConfig {
            local_only: false,
            ..Default::default()
        };
        Self {
            bind: DEFAULT_BIND.into(),
            security,
            max_message_bytes: 8 * 1024 * 1024,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            limits: clipsync_core::config::LimitsConfig::default(),
        }
    }
}

impl RelayConfig {
    /// Constrói a configuração do processo relay a partir do mesmo TOML
    /// validado pelo daemon, sem duplicar o contrato de configuração.
    pub fn from_config(config: &clipsync_core::config::Config) -> Result<Self, String> {
        if !matches!(
            config.security.transport,
            clipsync_core::config::Transport::Tls
        ) {
            return Err(
                "relay exige security.transport = \"tls\"; plaintext não é suportado".into(),
            );
        }
        let max_message_bytes = config.clipboard.max_websocket_message_bytes();
        if max_message_bytes == 0 {
            return Err("limite de mensagem do relay não pode ser zero".into());
        }
        Ok(Self {
            bind: config.bind.clone(),
            security: config.security.clone(),
            max_message_bytes,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            limits: config.limits.clone(),
        })
    }
}

#[derive(Debug)]
struct Session {
    identity: RelayIdentity,
    session_id: String,
    tx: mpsc::Sender<Arc<Message>>,
}

#[derive(Debug)]
pub struct RelayState {
    sessions: RwLock<HashMap<String, Session>>,
    admission: RelayAdmission,
}

#[derive(Debug)]
struct RelayAdmission {
    active: std::sync::atomic::AtomicUsize,
    windows: Mutex<HashMap<std::net::IpAddr, Window>>,
    limits: clipsync_core::config::LimitsConfig,
}

#[derive(Debug)]
struct Window {
    started: Instant,
    messages: u32,
    bytes: u64,
}

impl Default for RelayState {
    fn default() -> Self {
        Self::with_limits(clipsync_core::config::LimitsConfig {
            max_connections: 0,
            messages_per_minute: 0,
            bytes_per_minute: 0,
        })
    }
}

impl RelayState {
    fn with_limits(limits: clipsync_core::config::LimitsConfig) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            admission: RelayAdmission {
                active: std::sync::atomic::AtomicUsize::new(0),
                windows: Mutex::new(HashMap::new()),
                limits,
            },
        }
    }

    fn try_connection(&self) -> bool {
        let n = self
            .admission
            .active
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if self.admission.limits.max_connections != 0 && n > self.admission.limits.max_connections {
            self.admission
                .active
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    fn release_connection(&self) {
        self.admission
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    async fn allow_message(&self, ip: std::net::IpAddr, bytes: usize) -> bool {
        let limits = &self.admission.limits;
        if limits.messages_per_minute == 0 && limits.bytes_per_minute == 0 {
            return true;
        }
        let mut windows = self.admission.windows.lock().await;
        let now = Instant::now();
        let window = windows.entry(ip).or_insert(Window {
            started: now,
            messages: 0,
            bytes: 0,
        });
        if now.duration_since(window.started) >= Duration::from_secs(60) {
            *window = Window {
                started: now,
                messages: 0,
                bytes: 0,
            };
        }
        let allowed = (limits.messages_per_minute == 0
            || window.messages < limits.messages_per_minute)
            && (limits.bytes_per_minute == 0
                || window.bytes.saturating_add(bytes as u64) <= limits.bytes_per_minute);
        if allowed {
            window.messages += 1;
            window.bytes = window.bytes.saturating_add(bytes as u64);
        }
        allowed
    }
}

impl RelayState {
    pub async fn attach(&self, identity: RelayIdentity, tx: mpsc::Sender<Arc<Message>>) -> String {
        let session_id = uuid::Uuid::new_v4().to_string();
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, old| old.identity != identity);
        sessions.insert(
            session_id.clone(),
            Session {
                identity,
                session_id: session_id.clone(),
                tx,
            },
        );
        session_id
    }

    pub async fn detach(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.remove(session_id);
    }

    /// Routes only within the authenticated account. A full queue is a
    /// deliberate backpressure signal: the slow session is disconnected.
    pub async fn route(&self, sender: &RelayIdentity, message: Message) -> usize {
        let message = Arc::new(message.with_origin(&sender.device_id));
        let sessions: Vec<(String, mpsc::Sender<Arc<Message>>)> = self
            .sessions
            .read()
            .await
            .values()
            .filter(|session| {
                session.identity.account_id == sender.account_id
                    && session.identity.device_id != sender.device_id
            })
            .map(|session| (session.session_id.clone(), session.tx.clone()))
            .collect();
        let mut delivered = 0;
        let mut saturated = Vec::new();
        for (session_id, tx) in sessions {
            match tx.try_send(Arc::clone(&message)) {
                Ok(()) => delivered += 1,
                Err(mpsc::error::TrySendError::Full(_)) => saturated.push(session_id),
                Err(mpsc::error::TrySendError::Closed(_)) => saturated.push(session_id),
            }
        }
        if !saturated.is_empty() {
            let mut map = self.sessions.write().await;
            for id in saturated {
                map.remove(&id);
            }
        }
        delivered
    }

    pub async fn len(&self) -> usize {
        self.sessions.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.sessions.read().await.is_empty()
    }
}

pub struct RelayServer {
    pub config: RelayConfig,
    pub state: Arc<RelayState>,
    verifier: Arc<dyn TokenVerifier>,
    pub shutdown: CancellationToken,
}

impl std::fmt::Debug for RelayServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayServer")
            .field("config", &self.config)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl RelayServer {
    pub fn new(config: RelayConfig, verifier: Arc<dyn TokenVerifier>) -> Self {
        assert!(config.queue_capacity > 0, "queue capacity must be positive");
        let state = Arc::new(RelayState::with_limits(config.limits.clone()));
        Self {
            config,
            state,
            verifier,
            shutdown: CancellationToken::new(),
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .route("/readyz", get(readyz))
            .route("/ws", get(ws_handler))
            .with_state(Arc::new(self.clone_for_router()))
    }

    fn clone_for_router(&self) -> RouterState {
        RouterState {
            config: self.config.clone(),
            state: self.state.clone(),
            verifier: self.verifier.clone(),
            shutdown: self.shutdown.clone(),
        }
    }

    pub async fn run(&self) -> Result<(), String> {
        if !matches!(
            self.config.security.transport,
            clipsync_core::config::Transport::Tls
        ) {
            return Err("relay exige TLS; plaintext não é suportado".into());
        }
        let addr: SocketAddr = self
            .config
            .bind
            .parse()
            .map_err(|e| format!("bind inválido: {e}"))?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| e.to_string())?;
        let identity = clipsync_core::tls::Identity::load_or_generate(&self.config.security)
            .map_err(|e| e.to_string())?;
        let acceptor =
            tokio_rustls::TlsAcceptor::from(identity.server_config().map_err(|e| e.to_string())?);
        let app = self.router();
        info!(bind = %addr, fingerprint = %identity.fingerprint, "relay TLS escutando");
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => return Ok(()),
                accepted = listener.accept() => {
                    let (stream, peer) = accepted.map_err(|e| e.to_string())?;
                    let acceptor = acceptor.clone();
                    let app = app.clone();
                    tokio::spawn(async move {
                        let tls = match acceptor.accept(stream).await { Ok(s) => s, Err(e) => { debug!(%peer, error = %e, "TLS rejeitado"); return; } };
                        let service = hyper::service::service_fn(move |mut request| {
                            let app = app.clone();
                            request.extensions_mut().insert(ConnectInfo(peer));
                            async move { tower::ServiceExt::oneshot(app, request).await }
                        });
                        let builder = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
                        if let Err(e) = builder.serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), service).await { debug!(%peer, error = %e, "TLS conexão encerrada"); }
                    });
                }
            }
        }
    }
}

#[derive(Clone)]
struct RouterState {
    config: RelayConfig,
    state: Arc<RelayState>,
    verifier: Arc<dyn TokenVerifier>,
    shutdown: CancellationToken,
}

async fn healthz(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    let sessions = state.state.len().await;
    (
        StatusCode::OK,
        format!("clipsync relay ok\nsessions {sessions}\n"),
    )
}

async fn readyz(State(state): State<Arc<RouterState>>) -> impl IntoResponse {
    if state.shutdown.is_cancelled() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "clipsync relay not ready\n",
        )
    } else {
        (StatusCode::OK, "clipsync relay ready\n")
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if !state.state.try_connection() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "connection quota exceeded\n",
        )
            .into_response();
    }
    let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        state.state.release_connection();
        return (StatusCode::UNAUTHORIZED, "authorization required\n").into_response();
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        state.state.release_connection();
        return (StatusCode::UNAUTHORIZED, "bearer token required\n").into_response();
    };
    let identity = match state.verifier.verify(token).await {
        Ok(identity) => identity,
        Err(_) => {
            state.state.release_connection();
            return (StatusCode::UNAUTHORIZED, "token rejected\n").into_response();
        }
    };
    let config = state.config.clone();
    ws.max_message_size(config.max_message_bytes)
        .max_frame_size(config.max_message_bytes)
        .on_upgrade(move |socket| {
            connection(
                socket,
                state.state.clone(),
                identity,
                config.queue_capacity,
                peer,
                state.shutdown.clone(),
            )
        })
        .into_response()
}

async fn connection(
    socket: WebSocket,
    state: Arc<RelayState>,
    identity: RelayIdentity,
    capacity: usize,
    peer: SocketAddr,
    shutdown: CancellationToken,
) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel(capacity);
    let session_id = state.attach(identity.clone(), tx.clone()).await;
    info!(%peer, device = %identity.device_id, "relay session connected");
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            match message.to_json() {
                Ok(json) => {
                    if sink.send(WsMessage::Text(json)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let _ = relay_loop(&mut stream, &state, &identity, &tx, &shutdown, peer.ip()).await;
    state.detach(&session_id).await;
    state.release_connection();
    writer.abort();
    let _ = writer.await;
}

async fn relay_loop<S>(
    stream: &mut S,
    state: &Arc<RelayState>,
    identity: &RelayIdentity,
    own_tx: &mpsc::Sender<Arc<Message>>,
    shutdown: &CancellationToken,
    peer: std::net::IpAddr,
) -> Result<(), ()>
where
    S: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
{
    let first = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        first = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.next()) => first.map_err(|_| ())?,
    };
    if let Some(frame) = first {
        handle_frame(frame.map_err(|_| ())?, state, identity, own_tx, peer).await?;
    } else {
        return Ok(());
    }
    loop {
        let frame = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            frame = stream.next() => frame,
        };
        let Some(frame) = frame else {
            return Ok(());
        };
        handle_frame(frame.map_err(|_| ())?, state, identity, own_tx, peer).await?;
    }
}

async fn handle_frame(
    frame: WsMessage,
    state: &Arc<RelayState>,
    identity: &RelayIdentity,
    own_tx: &mpsc::Sender<Arc<Message>>,
    peer: std::net::IpAddr,
) -> Result<(), ()> {
    match frame {
        WsMessage::Text(text) => match serde_json::from_str::<Message>(&text) {
            Ok(Message::Hello { .. }) => Ok(()),
            Ok(Message::PairChallenge { .. })
            | Ok(Message::PairSubmit { .. })
            | Ok(Message::PairOk { .. })
            | Ok(Message::PairFail { .. }) => Err(()),
            Ok(Message::Ping { ts }) => {
                let _ = own_tx.try_send(Arc::new(Message::Pong { ts }));
                Ok(())
            }
            Ok(Message::Pong { .. }) => Ok(()),
            Ok(message) => {
                let bytes = text.len();
                if !state.allow_message(peer, bytes).await {
                    return Err(());
                }
                state.route(identity, message).await;
                Ok(())
            }
            Err(_) => Err(()),
        },
        WsMessage::Close(_) => Err(()),
        WsMessage::Ping(_) | WsMessage::Pong(_) => Ok(()),
        WsMessage::Binary(_) => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(account: &str, device: &str) -> RelayIdentity {
        RelayIdentity {
            account_id: account.into(),
            device_id: device.into(),
        }
    }

    #[tokio::test]
    async fn two_clients_route_and_offline_is_dropped() {
        let state = RelayState::default();
        let (a, _) = mpsc::channel(4);
        let (b, mut b_rx) = mpsc::channel(4);
        state.attach(identity("account", "a"), a).await;
        let b_id = state.attach(identity("account", "b"), b).await;
        let message = Message::ClipboardText {
            mime: "text/plain".into(),
            content: "hello".into(),
            origin: "spoofed".into(),
            sha256: "hash".into(),
        };
        assert_eq!(state.route(&identity("account", "a"), message).await, 1);
        match &*b_rx.recv().await.unwrap() {
            Message::ClipboardText { origin, .. } => assert_eq!(origin.as_str(), "a"),
            other => panic!("unexpected message: {}", other.type_name()),
        }
        state.detach(&b_id).await;
        assert_eq!(
            state
                .route(&identity("account", "a"), Message::Ping { ts: 2 })
                .await,
            0
        );
    }

    #[tokio::test]
    async fn full_queue_evicts_slow_session() {
        let state = RelayState::default();
        let (sender, _) = mpsc::channel(1);
        let (slow, mut slow_rx) = mpsc::channel(1);
        state.attach(identity("account", "a"), sender).await;
        let slow_id = state.attach(identity("account", "b"), slow).await;
        slow_rx.try_recv().ok();
        let message = Message::ClipboardText {
            mime: "text/plain".into(),
            content: "one".into(),
            origin: "a".into(),
            sha256: "one".into(),
        };
        assert_eq!(
            state
                .route(&identity("account", "a"), message.clone())
                .await,
            1
        );
        assert_eq!(state.route(&identity("account", "a"), message).await, 0);
        assert_eq!(state.len().await, 1);
        assert!(state.sessions.read().await.get(&slow_id).is_none());
    }

    #[tokio::test]
    async fn reconnect_replaces_old_session_without_old_detach_removing_new() {
        let state = RelayState::default();
        let (old, _) = mpsc::channel(1);
        let (new, _) = mpsc::channel(1);
        let id = identity("account", "device");
        let old_id = state.attach(id.clone(), old).await;
        let new_id = state.attach(id, new).await;
        state.detach(&old_id).await;
        assert_eq!(state.len().await, 1);
        state.detach(&new_id).await;
        assert_eq!(state.len().await, 0);
    }

    #[test]
    fn relay_config_deserializes_limits_and_derives_message_limit() {
        let mut config = clipsync_core::config::Config::default();
        config.limits.max_connections = 7;
        config.limits.messages_per_minute = 11;
        config.limits.bytes_per_minute = 1234;
        let relay = RelayConfig::from_config(&config).unwrap();
        assert_eq!(relay.limits.max_connections, 7);
        assert_eq!(relay.limits.messages_per_minute, 11);
        assert_eq!(relay.limits.bytes_per_minute, 1234);
        assert_eq!(
            relay.max_message_bytes,
            config.clipboard.max_websocket_message_bytes()
        );
    }

    #[tokio::test]
    async fn admission_enforces_connection_and_message_limits() {
        let state = RelayState::with_limits(clipsync_core::config::LimitsConfig {
            max_connections: 1,
            messages_per_minute: 1,
            bytes_per_minute: 4,
        });
        assert!(state.try_connection());
        assert!(!state.try_connection());
        assert!(state.allow_message("127.0.0.1".parse().unwrap(), 4).await);
        assert!(!state.allow_message("127.0.0.1".parse().unwrap(), 1).await);
        state.release_connection();
        assert!(state.try_connection());
    }
}
