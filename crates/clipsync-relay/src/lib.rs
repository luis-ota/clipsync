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
use clipsync_core::auth::{GroupAuthorizer, GroupId, RelayEnvelope, ReplayProtector, SessionId};
use clipsync_core::protocol::{DeviceId, Message, PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
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
    pub session_id: SessionId,
    pub group_id: GroupId,
}

#[derive(Debug, serde::Deserialize)]
struct WireEnvelope {
    #[serde(rename = "type")]
    kind: String,
    session_id: SessionId,
    source: DeviceId,
    destination: Option<DeviceId>,
    group: GroupId,
    sequence: u64,
    payload: Message,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("token rejected")]
pub struct AuthError;

/// Boundary for the authentication system owned by issue #75/deployment.
#[async_trait::async_trait]
pub trait TokenVerifier: Send + Sync + 'static {
    async fn verify(&self, opaque_token: &str) -> Result<RelayIdentity, AuthError>;
}

/// Token provider intended for production deployments. The file format is one
/// record per line: `token account device session group`. The session field is
/// retained for file-format compatibility, but a fresh session is assigned for
/// every WebSocket connection.
#[derive(Debug, Clone)]
pub struct FileTokenProvider {
    tokens: Arc<HashMap<[u8; 32], RelayIdentity>>,
}

impl FileTokenProvider {
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let metadata = std::fs::metadata(path).map_err(|e| format!("token file: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("token file must not be readable by group or other".into());
            }
        }
        let contents = std::fs::read_to_string(path).map_err(|e| format!("token file: {e}"))?;
        let mut tokens = HashMap::new();
        for (line_number, line) in contents.lines().enumerate() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 5 {
                return Err(format!(
                    "token file line {}: expected 5 fields",
                    line_number + 1
                ));
            }
            let identity = RelayIdentity {
                account_id: fields[1].to_owned(),
                device_id: DeviceId::from(fields[2]),
                session_id: SessionId::from_string(fields[3])
                    .ok_or_else(|| format!("token file line {}: empty session", line_number + 1))?,
                group_id: GroupId::from_string(fields[4])
                    .ok_or_else(|| format!("token file line {}: empty group", line_number + 1))?,
            };
            let digest: [u8; 32] = Sha256::digest(fields[0].as_bytes()).into();
            if tokens.insert(digest, identity).is_some() {
                return Err(format!(
                    "token file line {}: duplicate token",
                    line_number + 1
                ));
            }
        }
        if tokens.is_empty() {
            return Err("token file contains no credentials".into());
        }
        Ok(Self {
            tokens: Arc::new(tokens),
        })
    }

    pub fn authorizer(&self) -> GroupAuthorizer {
        let mut authorizer = GroupAuthorizer::default();
        for identity in self.tokens.values() {
            authorizer.add_member(identity.group_id.clone(), identity.device_id.clone());
        }
        authorizer
    }
}

#[async_trait::async_trait]
impl TokenVerifier for FileTokenProvider {
    async fn verify(&self, opaque_token: &str) -> Result<RelayIdentity, AuthError> {
        let digest: [u8; 32] = Sha256::digest(opaque_token.as_bytes()).into();
        self.tokens.get(&digest).cloned().ok_or(AuthError)
    }
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
    groups: GroupAuthorizer,
    replay: Mutex<ReplayProtector>,
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
        Self::with_groups_and_limits(
            GroupAuthorizer::default(),
            clipsync_core::config::LimitsConfig::default(),
        )
    }
}

impl RelayState {
    fn with_groups_and_limits(
        groups: GroupAuthorizer,
        limits: clipsync_core::config::LimitsConfig,
    ) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            groups,
            replay: Mutex::new(ReplayProtector::default()),
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
        let session_id = SessionId::new();
        let identity = RelayIdentity {
            session_id: session_id.clone(),
            ..identity
        };
        let mut sessions = self.sessions.write().await;
        let replaced: Vec<_> = sessions
            .iter()
            .filter(|(_, old)| same_device(&old.identity, &identity))
            .map(|(id, _)| id.clone())
            .collect();
        for old_id in replaced {
            if let Some(old) = sessions.remove(&old_id) {
                self.replay.lock().await.forget(&old.identity.session_id);
            }
        }
        sessions.insert(
            session_id.to_string(),
            Session {
                identity,
                session_id: session_id.to_string(),
                tx,
            },
        );
        session_id.to_string()
    }

    pub async fn detach(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.remove(session_id) {
            self.replay
                .lock()
                .await
                .forget(&session.identity.session_id);
        }
    }

    pub async fn route_envelope(
        &self,
        mut envelope: RelayEnvelope,
        authenticated_source: &DeviceId,
    ) -> Result<usize, String> {
        let active = self
            .sessions
            .read()
            .await
            .get(envelope.session_id.as_str())
            .is_some_and(|session| {
                session.identity.device_id == *authenticated_source
                    && session.identity.session_id == envelope.session_id
            });
        if !active {
            return Err("authorization rejected: inactive session".into());
        }
        envelope
            .authorize(
                authenticated_source,
                &self.groups,
                &mut *self.replay.lock().await,
            )
            .map_err(|error| format!("authorization rejected: {error:?}"))?;
        Ok(self
            .route_by_source(
                &envelope.source,
                &envelope.group,
                envelope.destination.as_ref(),
                envelope.payload,
            )
            .await)
    }

    /// Routes only within the authenticated group. A full queue is a
    /// deliberate backpressure signal: the slow session is disconnected.
    #[cfg(test)]
    async fn route(&self, sender: &RelayIdentity, message: Message) -> usize {
        self.route_by_source(&sender.device_id, &sender.group_id, None, message)
            .await
    }

    async fn route_by_source(
        &self,
        source: &DeviceId,
        group: &GroupId,
        destination: Option<&DeviceId>,
        message: Message,
    ) -> usize {
        let message = Arc::new(message.with_origin(source));
        let sessions: Vec<(String, mpsc::Sender<Arc<Message>>)> = self
            .sessions
            .read()
            .await
            .values()
            .filter(|session| {
                session.identity.group_id == *group
                    && destination.map_or(true, |destination| {
                        session.identity.device_id == *destination
                    })
                    && session.identity.device_id != *source
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
                if let Some(session) = map.remove(&id) {
                    self.replay
                        .lock()
                        .await
                        .forget(&session.identity.session_id);
                }
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

fn same_device(left: &RelayIdentity, right: &RelayIdentity) -> bool {
    left.account_id == right.account_id
        && left.device_id == right.device_id
        && left.group_id == right.group_id
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
        Self::new_with_groups(config, verifier, GroupAuthorizer::default())
    }

    pub fn new_with_groups(
        config: RelayConfig,
        verifier: Arc<dyn TokenVerifier>,
        groups: GroupAuthorizer,
    ) -> Self {
        assert!(config.queue_capacity > 0, "queue capacity must be positive");
        let limits = config.limits.clone();
        Self {
            config,
            state: Arc::new(RelayState::with_groups_and_limits(groups, limits)),
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
    let (tx, mut rx) = mpsc::channel::<Arc<Message>>(capacity);
    let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
    let result = relay_loop(
        &mut stream,
        &state,
        &identity,
        &tx,
        &sequence,
        peer,
        &shutdown,
    )
    .await;
    state.release_connection();
    writer.abort();
    if let Ok(Some(session_id)) = result {
        state.detach(&session_id).await;
    }
    let _ = writer.await;
}

async fn relay_loop<S>(
    stream: &mut S,
    state: &Arc<RelayState>,
    identity: &RelayIdentity,
    own_tx: &mpsc::Sender<Arc<Message>>,
    sequence: &Arc<std::sync::atomic::AtomicU64>,
    peer: SocketAddr,
    shutdown: &CancellationToken,
) -> Result<Option<String>, ()>
where
    S: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
{
    let first = tokio::select! {
        _ = shutdown.cancelled() => return Ok(None),
        first = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.next()) => first.map_err(|_| ())?,
    };
    match first {
        Some(frame) => handle_hello(frame.map_err(|_| ())?, identity).await?,
        None => return Ok(None),
    }
    let attached = SessionId::from_string(state.attach(identity.clone(), own_tx.clone()).await)
        .expect("attach always creates a session id");
    info!(%peer, device = %identity.device_id, "relay session connected");
    loop {
        let frame = tokio::select! {
            _ = shutdown.cancelled() => return Ok(Some(attached.to_string())),
            frame = stream.next() => frame,
        };
        let Some(frame) = frame else {
            return Ok(Some(attached.to_string()));
        };
        if handle_frame(
            frame.map_err(|_| ())?,
            state,
            identity,
            own_tx,
            sequence,
            &attached,
            peer.ip(),
        )
        .await
        .is_err()
        {
            return Ok(Some(attached.to_string()));
        }
    }
}

async fn handle_hello(frame: WsMessage, identity: &RelayIdentity) -> Result<(), ()> {
    let WsMessage::Text(text) = frame else {
        return Err(());
    };
    match serde_json::from_str::<Message>(&text) {
        Ok(Message::Hello { v, device }) if v == PROTOCOL_VERSION => {
            if device.id.is_some_and(|id| id != identity.device_id) {
                return Err(());
            }
            Ok(())
        }
        _ => Err(()),
    }
}

async fn handle_frame(
    frame: WsMessage,
    state: &Arc<RelayState>,
    identity: &RelayIdentity,
    own_tx: &mpsc::Sender<Arc<Message>>,
    sequence: &Arc<std::sync::atomic::AtomicU64>,
    session_id: &SessionId,
    peer: std::net::IpAddr,
) -> Result<(), ()> {
    let frame_bytes = match &frame {
        WsMessage::Text(text) => text.len(),
        WsMessage::Binary(bytes) | WsMessage::Ping(bytes) | WsMessage::Pong(bytes) => bytes.len(),
        WsMessage::Close(_) => 0,
    };
    if !state.allow_message(peer, frame_bytes).await {
        return Err(());
    }
    match frame {
        WsMessage::Text(text) => match serde_json::from_str::<Message>(&text) {
            Ok(Message::Hello { .. }) => Err(()),
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
                let sequence = sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let envelope = RelayEnvelope {
                    session_id: session_id.clone(),
                    source: identity.device_id.clone(),
                    destination: None,
                    group: identity.group_id.clone(),
                    sequence,
                    payload: message,
                };
                state
                    .route_envelope(envelope, &identity.device_id)
                    .await
                    .map(|_| ())
                    .map_err(|_| ())
            }
            Err(_) => {
                let envelope: WireEnvelope = serde_json::from_str(&text).map_err(|_| ())?;
                if envelope.kind != "relay_envelope" || envelope.session_id != *session_id {
                    return Err(());
                }
                state
                    .route_envelope(
                        RelayEnvelope {
                            session_id: envelope.session_id,
                            source: envelope.source,
                            destination: envelope.destination,
                            group: envelope.group,
                            sequence: envelope.sequence,
                            payload: envelope.payload,
                        },
                        &identity.device_id,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| ())
            }
        },
        WsMessage::Close(_) => Err(()),
        WsMessage::Ping(_) | WsMessage::Pong(_) => Ok(()),
        WsMessage::Binary(_) => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn identity(account: &str, device: &str) -> RelayIdentity {
        RelayIdentity {
            account_id: account.into(),
            device_id: device.into(),
            session_id: SessionId::from_string(format!("session-{device}")).unwrap(),
            group_id: GroupId::from_string(format!("group-{account}")).unwrap(),
        }
    }

    #[tokio::test]
    async fn file_token_provider_reads_expected_format_without_plaintext_storage() {
        let path =
            std::env::temp_dir().join(format!("clipsync-relay-token-{}", std::process::id()));
        std::fs::write(&path, "smoke-token account device session group\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let provider = FileTokenProvider::from_path(&path).unwrap();
        let verified = provider.verify("smoke-token").await.unwrap();
        assert_eq!(verified.account_id, "account");
        assert!(provider.verify("wrong-token").await.is_err());
        let _ = std::fs::remove_file(path);
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

    #[tokio::test]
    async fn replay_is_rejected_after_reconnect_but_new_session_starts_fresh() {
        let source = identity("account", "source");
        let target = identity("account", "target");
        let mut groups = GroupAuthorizer::default();
        groups.add_member(source.group_id.clone(), source.device_id.clone());
        groups.add_member(target.group_id.clone(), target.device_id.clone());
        let state = RelayState::with_groups_and_limits(
            groups,
            clipsync_core::config::LimitsConfig::default(),
        );
        let (target_tx, mut target_rx) = mpsc::channel(4);
        state.attach(target, target_tx).await;
        let (old_tx, _) = mpsc::channel(4);
        let old_session = state.attach(source.clone(), old_tx).await;
        let old_session = SessionId::from_string(old_session).unwrap();
        let envelope = |session_id: SessionId, sequence| RelayEnvelope {
            session_id,
            source: source.device_id.clone(),
            destination: None,
            group: source.group_id.clone(),
            sequence,
            payload: Message::Ping {
                ts: sequence as i64,
            },
        };

        assert_eq!(
            state
                .route_envelope(envelope(old_session.clone(), 1), &source.device_id)
                .await,
            Ok(1)
        );
        target_rx.recv().await.expect("first message delivered");

        let (new_tx, _) = mpsc::channel(4);
        let new_session =
            SessionId::from_string(state.attach(source.clone(), new_tx).await).unwrap();
        assert_ne!(old_session, new_session);
        assert!(state
            .route_envelope(envelope(old_session, 2), &source.device_id)
            .await
            .is_err());
        assert_eq!(
            state
                .route_envelope(envelope(new_session, 1), &source.device_id)
                .await,
            Ok(1)
        );
    }

    #[tokio::test]
    async fn authenticated_hello_and_clipboard_round_trip_through_relay() {
        let source = identity("account", "source");
        let target = identity("account", "target");
        let mut groups = GroupAuthorizer::default();
        groups.add_member(source.group_id.clone(), source.device_id.clone());
        groups.add_member(target.group_id.clone(), target.device_id.clone());
        let state = Arc::new(RelayState::with_groups_and_limits(
            groups,
            clipsync_core::config::LimitsConfig::default(),
        ));
        let (target_tx, mut target_rx) = mpsc::channel(2);
        state.attach(target.clone(), target_tx).await;
        let (source_tx, _) = mpsc::channel(2);
        let hello = Message::Hello {
            v: PROTOCOL_VERSION,
            device: clipsync_core::protocol::DeviceInfo::new(
                "source",
                clipsync_core::protocol::DeviceKind::Linux,
            )
            .with_capabilities(clipsync_core::protocol::Capabilities {
                text: true,
                ..Default::default()
            }),
        };
        let clipboard = Message::ClipboardText {
            mime: "text/plain".into(),
            content: "end-to-end".into(),
            origin: DeviceId::from("forged"),
            sha256: "hash".into(),
        };
        let mut frames = futures::stream::iter(vec![
            Ok(WsMessage::Text(serde_json::to_string(&hello).unwrap())),
            Ok(WsMessage::Text(serde_json::to_string(&clipboard).unwrap())),
        ]);
        let sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let shutdown = CancellationToken::new();
        relay_loop(
            &mut frames,
            &state,
            &source,
            &source_tx,
            &sequence,
            "127.0.0.1:1".parse().unwrap(),
            &shutdown,
        )
        .await
        .unwrap();
        match target_rx.recv().await.unwrap().as_ref() {
            Message::ClipboardText {
                origin, content, ..
            } => {
                assert_eq!(origin, &source.device_id);
                assert_eq!(content, "end-to-end");
            }
            other => panic!("unexpected message: {}", other.type_name()),
        }
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
        let state = RelayState::with_groups_and_limits(
            GroupAuthorizer::default(),
            clipsync_core::config::LimitsConfig {
                max_connections: 1,
                messages_per_minute: 1,
                bytes_per_minute: 4,
            },
        );
        assert!(state.try_connection());
        assert!(!state.try_connection());
        assert!(state.allow_message("127.0.0.1".parse().unwrap(), 4).await);
        assert!(!state.allow_message("127.0.0.1".parse().unwrap(), 1).await);
        state.release_connection();
        assert!(state.try_connection());
    }
}
