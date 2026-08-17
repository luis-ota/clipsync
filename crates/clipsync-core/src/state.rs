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
/// Intervalo de keepalive quando há ao menos um peer conectado.
const PEER_PING_INTERVAL_ACTIVE: Duration = Duration::from_secs(30);
/// Intervalo de keepalong quando o daemon está idle (sem peers).
/// Na prática o loop de ping é por-conexão e só roda com um peer
/// ativo, então esse valor documenta o comportamento "pausado":
/// nenhum ping é enviado quando `peer_count == 0`.
const PEER_PING_INTERVAL_IDLE: Duration = Duration::from_secs(120);

/// Intervalo de ping adaptativo: sem peers → idle (loop pausado);
/// com peers → active. Testável sem display (lógica pura).
pub(crate) fn peer_ping_interval_for(peer_count: usize) -> Duration {
    if peer_count == 0 {
        PEER_PING_INTERVAL_IDLE
    } else {
        PEER_PING_INTERVAL_ACTIVE
    }
}

/// Uma conexão de peer ativa (o outro lado do WebSocket).
#[derive(Debug, Clone)]
pub struct PeerHandle {
    pub addr: SocketAddr,
    pub device_id: DeviceId,
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

    /// Registra um peer conectado.
    pub async fn add_peer(
        &self,
        addr: SocketAddr,
        device_id: DeviceId,
        name: String,
        tx: mpsc::Sender<Message>,
    ) {
        let handle = PeerHandle {
            addr,
            device_id: device_id.clone(),
            name,
            connected_at: chrono::Utc::now().timestamp(),
            tx,
        };
        self.peers.write().await.insert(device_id.clone(), handle);
        info!(peer = %addr, device = %device_id, "peer conectado");
    }

    /// Remove um peer da lista de ativos.
    pub async fn remove_peer(&self, device_id: &DeviceId) -> Option<PeerHandle> {
        let removed = self.peers.write().await.remove(device_id);
        if let Some(handle) = &removed {
            info!(peer = %handle.addr, device = %device_id, "peer desconectado");
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

/// Intervalo de ping (re)exportado para o server. Sempre retorna o
/// intervalo ativo, pois o loop de ping só existe com um peer
/// conectado; sem peers, nenhum ping é enviado (loop pausado).
pub(crate) fn peer_ping_interval() -> Duration {
    peer_ping_interval_for(1)
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
                "phone".into(),
                tx,
            )
            .await;
        assert_eq!(state.peer_count().await, 1);
        state.remove_peer(&id).await;
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
            .add_peer("1.1.1.1:1".parse().unwrap(), id_a.clone(), "a".into(), tx_a)
            .await;
        state
            .add_peer("2.2.2.2:2".parse().unwrap(), id_b.clone(), "b".into(), tx_b)
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

    #[test]
    fn ping_interval_adapts_to_peer_count() {
        // Sem peers: intervalo idle (loop na prática pausado).
        assert_eq!(peer_ping_interval_for(0), Duration::from_secs(120));
        // Com 1+ peers: intervalo ativo.
        assert_eq!(peer_ping_interval_for(1), Duration::from_secs(30));
        assert_eq!(peer_ping_interval_for(7), Duration::from_secs(30));
        // O helper pub(crate) usado pelo transporte corresponde ao ativo.
        assert_eq!(peer_ping_interval(), Duration::from_secs(30));
    }
}
