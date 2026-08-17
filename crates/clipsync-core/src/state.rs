//! Estado compartilhado do servidor: conexões ativas, dispositivos
//! pareados e broadcast para os peers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::pairing::PairingManager;
use crate::protocol::{DeviceId, Message};

/// Capacidade do canal de envio por peer.
const PEER_QUEUE: usize = 128;
/// Timeout para considerar um peer morto (sem pong).
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Intervalo de keepalive. O loop de ping é por-conexão e só existe
/// com um peer conectado, então um único valor basta.
const PEER_PING_INTERVAL: Duration = Duration::from_secs(30);

/// Uma conexão de peer ativa (o outro lado do WebSocket).
#[derive(Debug, Clone)]
pub struct PeerHandle {
    pub addr: SocketAddr,
    pub device_id: DeviceId,
    /// ID único desta sessão de conexão. Distingue handles do mesmo
    /// `device_id`; usado no compare-and-swap do `remove_peer`.
    pub session_id: String,
    pub name: String,
    pub connected_at: i64,
    /// Canal para enviar mensagens ao peer. Enviar = colocar na fila
    /// do writer task.
    pub tx: mpsc::Sender<Message>,
}

impl PeerHandle {
    /// Envia uma mensagem ao peer (fire-and-forget; fila).
    pub fn send(&self, msg: Message) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(e) => {
                debug!(peer = %self.addr, "fila do peer cheia: {e}");
                false
            }
        }
    }
}

/// Estado do servidor, compartilhado entre tasks.
#[derive(Debug)]
pub struct ServerState {
    pub config: crate::server::ServerConfig,
    pub pairing: Mutex<PairingManager>,
    pub peers: RwLock<HashMap<DeviceId, PeerHandle>>,
    /// Broadcast para a task de watcher de clipboard local.
    pub local_events: mpsc::Sender<crate::clipboard::ClipboardEvent>,
    /// Sinal de shutdown global.
    pub shutdown: CancellationToken,
}

impl ServerState {
    pub fn new(
        config: crate::server::ServerConfig,
    ) -> (Self, mpsc::Receiver<crate::clipboard::ClipboardEvent>) {
        let (tx, rx) = mpsc::channel(256);
        let state = Self {
            config,
            pairing: Mutex::new(PairingManager::new()),
            peers: RwLock::new(HashMap::new()),
            local_events: tx,
            shutdown: CancellationToken::new(),
        };
        (state, rx)
    }

    /// Registra um peer conectado. Se o `device_id` já tem uma sessão
    /// ativa com outra `session_id`, a sessão nova substitui a antiga
    /// no mapa e a antiga é notificada via `error` (código
    /// `superseded`). A sessão antiga, ao ser encerrada, não remove a
    /// entrada do sucessor: `remove_peer` faz compare-and-swap pela
    /// `session_id`.
    pub async fn add_peer(
        &self,
        addr: SocketAddr,
        device_id: DeviceId,
        session_id: String,
        name: String,
        tx: mpsc::Sender<Message>,
    ) {
        let handle = PeerHandle {
            addr,
            device_id: device_id.clone(),
            session_id: session_id.clone(),
            name,
            connected_at: chrono::Utc::now().timestamp(),
            tx,
        };
        let mut map = self.peers.write().await;
        if let Some(old) = map.get(&device_id) {
            if old.session_id != session_id {
                warn!(
                    peer = %old.addr,
                    device = %device_id,
                    "device reconectado com nova sessão; notificando a antiga"
                );
                old.send(Message::Error {
                    code: "superseded".into(),
                    message: "sessão substituída por nova conexão deste device".into(),
                });
            }
        }
        map.insert(device_id.clone(), handle);
        info!(peer = %addr, device = %device_id, "peer conectado");
    }

    /// Remove um peer da lista de ativos. Compare-and-swap: só remove
    /// se a entrada do mapa ainda pertence a esta sessão (`session_id`).
    /// Assim, quando um device reconecta e a sessão antiga é substituída,
    /// o detach da sessão antiga não remove o handle do sucessor.
    pub async fn remove_peer(&self, device_id: &DeviceId, session_id: &str) -> Option<PeerHandle> {
        let mut map = self.peers.write().await;
        let removed = if matches!(map.get(device_id), Some(h) if h.session_id == session_id) {
            map.remove(device_id)
        } else {
            None
        };
        if let Some(handle) = &removed {
            info!(peer = %handle.addr, device = %device_id, "peer desconectado");
        } else {
            debug!(device = %device_id, "detach ignorado: sessão já não está ativa");
        }
        removed
    }

    /// Envia uma mensagem para todos os peers conectados, exceto o
    /// originador (para evitar eco).
    pub async fn broadcast_except(&self, msg: Message, origin: Option<&DeviceId>) {
        let peers: Vec<PeerHandle> = {
            let map = self.peers.read().await;
            map.values()
                .filter(|p| match origin {
                    Some(o) => &p.device_id != o,
                    None => true,
                })
                .cloned()
                .collect()
        };
        let mut failed = 0;
        for peer in &peers {
            if !peer.send(msg.clone()) {
                failed += 1;
            }
        }
        if failed > 0 {
            debug!(failed, total = peers.len(), "broadcast parcial");
        }
    }

    /// Número de peers conectados.
    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    /// Lista os peers conectados (para `clipsyncd list-peers`).
    pub async fn peer_list(&self) -> Vec<PeerHandle> {
        let map = self.peers.read().await;
        let mut v: Vec<_> = map.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

/// Wrapper para permitir `ServerState` compartilhado como `Arc`.
pub type SharedState = Arc<ServerState>;

/// Timeout de idle (re)exportado para o server.
pub(crate) fn peer_idle_timeout() -> Duration {
    PEER_IDLE_TIMEOUT
}

/// Intervalo de ping (re)exportado para o server.
pub(crate) fn peer_ping_interval() -> Duration {
    PEER_PING_INTERVAL
}

/// Capacidade padrão da fila de um peer.
pub(crate) fn peer_queue_size() -> usize {
    PEER_QUEUE
}

/// Avisa o shutdown de que um peer saiu.
pub(crate) fn shutdown_token(state: &ServerState) -> CancellationToken {
    state.shutdown.clone()
}

impl Drop for ServerState {
    fn drop(&mut self) {
        warn!("ServerState derrubado; sinalizando shutdown");
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Capabilities, DeviceInfo, DeviceKind};

    fn test_state() -> (
        SharedState,
        mpsc::Receiver<crate::clipboard::ClipboardEvent>,
    ) {
        let (state, rx) = ServerState::new(crate::server::ServerConfig::default());
        (Arc::new(state), rx)
    }

    #[tokio::test]
    async fn add_remove_peer() {
        let (state, _rx) = test_state();
        let (tx, _) = mpsc::channel(10);
        let id = DeviceId::new();
        state
            .add_peer(
                "127.0.0.1:5000".parse().unwrap(),
                id.clone(),
                "sess-1".into(),
                "phone".into(),
                tx,
            )
            .await;
        assert_eq!(state.peer_count().await, 1);
        state.remove_peer(&id, "sess-1").await;
        assert_eq!(state.peer_count().await, 0);
    }

    #[tokio::test]
    async fn reconnect_same_device_keeps_successor() {
        let (state, _rx) = test_state();
        let (tx_old, _) = mpsc::channel(10);
        let (tx_new, _) = mpsc::channel(10);
        let id = DeviceId::new();
        state
            .add_peer(
                "1.1.1.1:1".parse().unwrap(),
                id.clone(),
                "old".into(),
                "phone".into(),
                tx_old,
            )
            .await;
        state
            .add_peer(
                "2.2.2.2:2".parse().unwrap(),
                id.clone(),
                "new".into(),
                "phone".into(),
                tx_new,
            )
            .await;
        assert_eq!(state.peer_count().await, 1, "sucessor substitui a entrada");

        assert!(
            state.remove_peer(&id, "old").await.is_none(),
            "detach da sessão antiga não remove o sucessor"
        );
        assert_eq!(state.peer_count().await, 1);

        assert!(
            state.remove_peer(&id, "new").await.is_some(),
            "detach do sucessor remove a entrada"
        );
        assert_eq!(state.peer_count().await, 0);
    }

    #[tokio::test]
    async fn broadcast_skips_origin() {
        let (state, _rx) = test_state();
        let (tx_a, _) = mpsc::channel(10);
        let (tx_b, mut rx_b) = mpsc::channel(10);
        let id_a = DeviceId::new();
        let id_b = DeviceId::new();
        state
            .add_peer(
                "1.1.1.1:1".parse().unwrap(),
                id_a.clone(),
                "sess-a".into(),
                "a".into(),
                tx_a,
            )
            .await;
        state
            .add_peer(
                "2.2.2.2:2".parse().unwrap(),
                id_b.clone(),
                "sess-b".into(),
                "b".into(),
                tx_b,
            )
            .await;

        let msg = Message::ClipboardText {
            mime: "text/plain".into(),
            content: "hi".into(),
            origin: id_a.clone(),
            sha256: "abc".into(),
        };
        state.broadcast_except(msg, Some(&id_a)).await;

        assert!(rx_b.try_recv().is_ok());
    }

    #[test]
    fn device_info_serializes_without_id() {
        let info = DeviceInfo::new("phone", DeviceKind::Android);
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("\"id\""), "json: {json}");
    }

    #[test]
    fn capabilities_default_false() {
        let caps = Capabilities::default();
        assert!(!caps.files);
        assert!(!caps.text);
    }
}
