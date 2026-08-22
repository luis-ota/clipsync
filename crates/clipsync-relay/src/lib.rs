//! TLS-only, in-memory WebSocket relay.
//!
//! Authentication is deliberately a boundary: this crate accepts an opaque
//! token and asks [`TokenVerifier`] for the already authenticated identity.
//! It does not mint, parse, hash, or persist credentials.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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
use tokio::sync::{mpsc, RwLock};
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
        }
    }
}

#[derive(Debug)]
struct Session {
    identity: RelayIdentity,
    session_id: String,
    tx: mpsc::Sender<Arc<Message>>,
}

#[derive(Debug, Default)]
pub struct RelayState {
    sessions: RwLock<HashMap<String, Session>>,
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
        Self {
            config,
            state: Arc::new(RelayState::default()),
            verifier,
            shutdown: CancellationToken::new(),
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
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

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return (StatusCode::UNAUTHORIZED, "authorization required\n").into_response();
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return (StatusCode::UNAUTHORIZED, "bearer token required\n").into_response();
    };
    let identity = match state.verifier.verify(token).await {
        Ok(identity) => identity,
        Err(_) => return (StatusCode::UNAUTHORIZED, "token rejected\n").into_response(),
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
    let _ = relay_loop(&mut stream, &state, &identity, &tx, &shutdown).await;
    state.detach(&session_id).await;
    writer.abort();
    let _ = writer.await;
}

async fn relay_loop<S>(
    stream: &mut S,
    state: &Arc<RelayState>,
    identity: &RelayIdentity,
    own_tx: &mpsc::Sender<Arc<Message>>,
    shutdown: &CancellationToken,
) -> Result<(), ()>
where
    S: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
{
    let first = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        first = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.next()) => first.map_err(|_| ())?,
    };
    if let Some(frame) = first {
        handle_frame(frame.map_err(|_| ())?, state, identity, own_tx).await?;
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
        handle_frame(frame.map_err(|_| ())?, state, identity, own_tx).await?;
    }
}

async fn handle_frame(
    frame: WsMessage,
    state: &Arc<RelayState>,
    identity: &RelayIdentity,
    own_tx: &mpsc::Sender<Arc<Message>>,
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
}
