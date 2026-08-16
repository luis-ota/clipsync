//! Sessão de peer: estado por-conexão e fila de envio.

use std::net::SocketAddr;

use tokio::sync::mpsc;
use tracing::debug;

use crate::protocol::{DeviceId, Message};
use crate::state::SharedState;

/// Sessão de um peer conectado. Mantém o `device_id` (atribuído após
/// pareamento ou vindo do `hello`) e a fila de envio para a task de
/// escrita da conexão.
#[derive(Debug, Clone)]
pub struct PeerSession {
    pub state: SharedState,
    pub addr: SocketAddr,
    /// Dispositivo atribuído (após pareamento).
    pub device_id: Option<DeviceId>,
    /// Nome amigável do peer.
    pub name: String,
    tx: mpsc::Sender<Message>,
}

impl PeerSession {
    pub fn new(state: SharedState, addr: SocketAddr, tx: mpsc::Sender<Message>) -> Self {
        Self {
            state,
            addr,
            device_id: None,
            name: String::new(),
            tx,
        }
    }

    /// Associa um device à sessão (após pareamento ou trust).
    pub fn attach(&mut self, device_id: DeviceId, name: String) {
        self.device_id = Some(device_id);
        self.name = name;
    }

    pub fn peer_id(&self) -> &DeviceId {
        self.device_id
            .as_ref()
            .expect("peer_id chamado antes do attach")
    }

    /// Envia uma mensagem ao peer pela fila.
    pub fn send(&self, msg: Message) {
        if let Err(e) = self.tx.try_send(msg) {
            debug!(peer = %self.addr, error = %e, "falha enviando ao peer");
        }
    }
}

/// Re-export para conveniência de callers.
pub fn peer_queue_size() -> usize {
    crate::state::peer_queue_size()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ServerState;

    #[tokio::test]
    async fn attach_and_send() {
        let (state, _rx) = ServerState::new(crate::server::ServerConfig::default());
        let state = std::sync::Arc::new(state);
        let (tx, mut rx) = mpsc::channel(16);
        let mut session = PeerSession::new(state, "127.0.0.1:1".parse().unwrap(), tx);
        let id = DeviceId::new();
        session.attach(id, "phone".into());
        assert_eq!(session.peer_id().0, session.peer_id().0);

        session.send(Message::Ping { ts: 1 });
        assert!(rx.try_recv().is_ok());
    }
}
