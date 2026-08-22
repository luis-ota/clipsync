//! Transporte: conexão WebSocket individual com um peer.
//!
//! Encapsula o ciclo de vida de uma conexão: handshake HTTP → upgrade
//! WebSocket → estado de pareamento → streaming de mensagens, com
//! uma task de escrita (fila) e uma task de leitura.

use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

use crate::error::{Error, Result};
use crate::pairing::PairingError;
use crate::peer::PeerSession;
use crate::protocol::{DeviceId, DeviceInfo, Message, PROTOCOL_VERSION};
use crate::server::ServerConfig;
use crate::state::SharedState;

/// Limite de tamanho de um frame binário (256 MiB, usado em v0.3
/// para arquivos).
const MAX_BINARY_FRAME: usize = 256 * 1024 * 1024;
/// Timeout para o handshake inicial (hello) e pareamento.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Uma conexão ativa. Duas tasks: `writer` (drena a fila de envio)
/// e o caller `reader` (lê frames do socket).
#[derive(Debug)]
pub struct Connection {
    pub socket: WebSocket,
    pub addr: SocketAddr,
    pub state: SharedState,
    pub config: ServerConfig,
}

impl Connection {
    /// Roda a sessão completa da conexão. Não retorna erro fatal —
    /// erros de um peer são logs e a conexão é fechada.
    pub async fn run(self) {
        let Connection {
            socket,
            addr,
            state,
            config,
        } = self;
        let (mut tx, mut rx) = socket.split();
        let (out_tx, mut out_rx) = mpsc::channel::<Arc<Message>>(128);

        let mut session = PeerSession::new(state.clone(), addr, out_tx.clone());

        // Writer task
        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let payload = msg.wrap().to_string();
                if tx.send(WsMessage::Text(payload)).await.is_err() {
                    break;
                }
            }
        });

        // Reader loop
        let mut conn = ConnectionInner {
            addr,
            state,
            config,
        };
        let result = conn.reader_loop(&mut rx, &mut session).await;

        // Desregistra o peer e encerra o writer.
        let session_id = session.session_id().to_owned();
        conn.state.pairing.lock().await.cancel_session(&session_id);
        session.detach().await;
        // PeerSession also owns a sender. Drop every producer before
        // awaiting the writer, otherwise it waits forever for channel EOF.
        drop(session);
        drop(out_tx);
        let _ = writer.await;
        drop(rx);

        if let Err(e) = result {
            debug!(peer = %addr, error = %e, "sessão encerrada com erro");
        }
    }
}

/// Parte da conexão sem o socket (para pós-split).
#[derive(Debug)]
pub struct ConnectionInner {
    pub addr: SocketAddr,
    pub state: SharedState,
    pub config: ServerConfig,
}

impl ConnectionInner {
    #[tracing::instrument(skip_all, fields(peer = %self.addr))]
    async fn reader_loop<R>(&mut self, rx: &mut R, session: &mut PeerSession) -> Result<()>
    where
        R: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
    {
        // Handshake: espera `hello` dentro do timeout.
        let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.await_hello(rx)).await;
        let (device_info, device_id) = match hello {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                warn!(peer = %self.addr, "handshake timeout; fechando");
                return Ok(());
            }
        };

        // Pareamento: se o device é confiado, aceita direto.
        // Caso contrário, inicia desafio de PIN.
        //
        // Ordem de locks: pairing Mutex primeiro, depois peers RwLock.
        // Isso é consistente com o restante do código (add_peer,
        // remove_peer, broadcast_except) e previne deadlocks.
        let trusted = {
            let pm = self.state.pairing.lock().await;
            device_id
                .as_ref()
                .map(|id| pm.is_trusted(id))
                .unwrap_or(false)
        };

        if trusted {
            // device_id é Some() aqui porque trusted=true só é possível
            // se is_trusted() retornou true, o que requer um id.
            let id = device_id.clone().unwrap();
            let name = {
                let pm = self.state.pairing.lock().await;
                // unwrap_or é seguro: device_name() retorna None apenas se
                // o device não está no HashMap (não é trusted), mas acabamos
                // de confirmar que é. Se retornar None por race, usamos o
                // nome do handshake como fallback.
                pm.device_name(&id).unwrap_or(&device_info.name).to_owned()
            };
            self.state.pairing.lock().await.mark_seen(&id);
            info!(peer = %self.addr, device = %id, name = %name, "device confiado conectado");
            session.attach(id.clone(), name.clone()).await;
            session.send(Message::PairOk {
                device_id: id,
                server_id: Some(self.config.device_id.clone()),
                session_id: session.session_id().to_owned(),
                server_name: self.config.name.clone(),
                capabilities: self.state_caps(&device_info),
            });
        } else {
            // Novo device: fluxo de PIN.
            self.pairing_flow(rx, session, &device_info).await?;
        }

        // Mensagens pós-pareamento.
        self.message_loop(rx, session).await
    }

    /// Espera a primeira mensagem `hello`.
    async fn await_hello<R>(&mut self, rx: &mut R) -> Result<(DeviceInfo, Option<DeviceId>)>
    where
        R: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
    {
        while let Some(item) = rx.next().await {
            let ws_msg = item.map_err(|e| Error::WebSocket(e.to_string()))?;
            match ws_msg {
                WsMessage::Text(text) => {
                    let msg: Message = serde_json::from_str(&text)
                        .map_err(|e| Error::Protocol(format!("JSON inválido: {e}")))?;
                    match msg {
                        Message::Hello { v, device } => {
                            if v != PROTOCOL_VERSION {
                                return Err(Error::Protocol(format!(
                                    "versão incompatível: cliente {v}, servidor {PROTOCOL_VERSION}"
                                )));
                            }
                            return Ok((device.clone(), device.id));
                        }
                        other => {
                            warn!(peer = %self.addr, msg = other.type_name(), "esperava hello, recebeu outra");
                            return Err(Error::Protocol("primeira mensagem deve ser hello".into()));
                        }
                    }
                }
                WsMessage::Close(_) => {
                    return Err(Error::Protocol("fechado antes do hello".into()))
                }
                WsMessage::Binary(_) => {
                    return Err(Error::Protocol(
                        "esperava hello (texto), recebeu binário".into(),
                    ))
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) => {}
            }
        }
        Err(Error::Protocol("conexão fechada durante handshake".into()))
    }

    /// Executa o fluxo de pareamento: envia desafio, espera submissão.
    #[tracing::instrument(skip_all, fields(peer = %self.addr))]
    async fn pairing_flow<R>(
        &mut self,
        rx: &mut R,
        session: &mut PeerSession,
        device_info: &DeviceInfo,
    ) -> Result<()>
    where
        R: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
    {
        let device_name = device_info.name.clone();
        let session_id = session.session_id().to_owned();
        let pairing_timeout = Duration::from_secs(self.config.security.pairing_timeout_secs);
        let pairing_deadline = tokio::time::Instant::now() + pairing_timeout;
        let (challenge_id, nonce) = {
            let mut pm = self.state.pairing.lock().await;
            let ch = pm.start_challenge(&session_id, &device_name, pairing_timeout);
            (ch.challenge_id.clone(), ch.nonce.clone())
        };
        info!(peer = %self.addr, device = %device_name, "novo device: desafio de PIN enviado (PIN exibido no daemon)");
        session.send(Message::PairChallenge {
            challenge_id,
            expires_at: chrono::Utc::now().timestamp()
                + self.config.security.pairing_timeout_secs as i64,
            nonce,
        });

        loop {
            let item = match tokio::time::timeout_at(pairing_deadline, rx.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    self.state.pairing.lock().await.cancel_session(&session_id);
                    return Err(Error::Pairing("tempo de pareamento expirado".into()));
                }
            };
            let ws_msg = item.map_err(|e| Error::WebSocket(e.to_string()))?;
            match ws_msg {
                WsMessage::Text(text) => {
                    let msg: Message = serde_json::from_str(&text)
                        .map_err(|e| Error::Protocol(format!("JSON inválido: {e}")))?;
                    match msg {
                        Message::PairSubmit {
                            code,
                            nonce,
                            challenge_id,
                        } => {
                            let result = self.state.pairing.lock().await.submit(
                                &session_id,
                                &challenge_id,
                                &nonce,
                                &code,
                                &device_info.kind.to_string(),
                            );
                            match result {
                                Ok(id) => {
                                    info!(peer = %self.addr, device = %id, "pareamento concluído");
                                    session.attach(id.clone(), device_name).await;
                                    session.send(Message::PairOk {
                                        device_id: id,
                                        server_id: Some(self.config.device_id.clone()),
                                        session_id: session.session_id().to_owned(),
                                        server_name: self.config.name.clone(),
                                        capabilities: self.state_caps(device_info),
                                    });
                                    return Ok(());
                                }
                                Err(PairingError::Invalid(reason)) => {
                                    session.send(Message::PairFail {
                                        reason,
                                        message: "PIN inválido ou expirado".into(),
                                    });
                                    self.state.pairing.lock().await.cancel_session(&session_id);
                                    return Err(Error::Pairing("PIN incorreto".into()));
                                }
                                Err(PairingError::Store(error)) => return Err(error),
                            }
                        }
                        other => {
                            warn!(
                                peer = %self.addr,
                                msg = other.type_name(),
                                "mensagem inesperada durante pareamento"
                            );
                            return Err(Error::Protocol("pareamento interrompido".into()));
                        }
                    }
                }
                WsMessage::Close(_) => {
                    return Err(Error::Protocol("fechado durante pareamento".into()))
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Binary(_) => continue,
            }
        }
        self.state.pairing.lock().await.cancel_session(&session_id);
        Err(Error::Protocol("conexão fechada durante pareamento".into()))
    }

    /// Calcula capabilities do servidor com base na config e no device.
    fn state_caps(&self, device: &DeviceInfo) -> crate::protocol::Capabilities {
        crate::protocol::Capabilities {
            text: self.config.clipboard.sync_text && device.capabilities.text,
            html: self.config.clipboard.sync_html && device.capabilities.html,
            images: self.config.clipboard.sync_images && device.capabilities.images,
            files: self.config.clipboard.sync_files && device.capabilities.files,
        }
    }

    /// Loop de mensagens pós-pareamento: orquestra keepalive,
    /// idle timer e dispatch de frames. A lógica de cada tipo de
    /// frame vive nos sub-handlers [`Self::handle_frame`] e
    /// [`Self::handle_text_message`].
    #[tracing::instrument(skip_all, fields(peer = %self.addr))]
    async fn message_loop<R>(&mut self, rx: &mut R, session: &mut PeerSession) -> Result<()>
    where
        R: StreamExt<Item = Result<WsMessage, axum::Error>> + Unpin,
    {
        let mut ping_timer = interval(crate::state::peer_ping_interval());
        let idle_timer = tokio::time::sleep(crate::state::peer_idle_timeout());
        tokio::pin!(idle_timer);
        let shutdown = crate::state::shutdown_token(&self.state);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    debug!(peer = %self.addr, "shutdown global; fechando conexão");
                    return Ok(());
                }
                _ = ping_timer.tick() => {
                    session.send(Message::Ping { ts: chrono::Utc::now().timestamp() });
                }
                _ = &mut idle_timer => {
                    warn!(peer = %self.addr, "peer inativo; fechando");
                    return Ok(());
                }
                item = rx.next() => {
                    if self.handle_frame(session, item, &mut idle_timer).await?.is_break() {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Dispatch de um único frame WebSocket. Retorna `Break(())`
    /// quando a conexão deve ser encerrada.
    async fn handle_frame(
        &mut self,
        session: &mut PeerSession,
        item: Option<Result<WsMessage, axum::Error>>,
        idle_timer: &mut std::pin::Pin<&mut tokio::time::Sleep>,
    ) -> Result<ControlFlow<(), ()>> {
        let ws_msg = match item {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                error!(peer = %self.addr, error = %e, "erro de websocket");
                return Err(Error::WebSocket(e.to_string()));
            }
            None => {
                debug!(peer = %self.addr, "conexão fechada pelo peer");
                return Ok(ControlFlow::Break(()));
            }
        };
        let frame_bytes = match &ws_msg {
            WsMessage::Text(text) => text.len(),
            WsMessage::Binary(data) => data.len(),
            WsMessage::Ping(data) | WsMessage::Pong(data) => data.len(),
            WsMessage::Close(_) => 0,
        };
        if !self
            .state
            .admission
            .allow_message(self.addr.ip(), frame_bytes)
            .await
        {
            warn!(peer = %self.addr, "rate limit exceeded; closing connection");
            return Err(Error::Protocol("rate limit exceeded".into()));
        }
        match ws_msg {
            WsMessage::Text(text) => {
                let msg: Message = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(peer = %self.addr, error = %e, "JSON inválido ignorado");
                        return Ok(ControlFlow::Continue(()));
                    }
                };
                match msg {
                    Message::Ping { ts } => {
                        session.send(Message::Pong { ts });
                        Ok(ControlFlow::Continue(()))
                    }
                    Message::Pong { .. } => {
                        idle_timer
                            .as_mut()
                            .reset(tokio::time::Instant::now() + crate::state::peer_idle_timeout());
                        Ok(ControlFlow::Continue(()))
                    }
                    msg => self.handle_text_message(session, msg).await,
                }
            }
            WsMessage::Binary(data) => {
                if data.len() > MAX_BINARY_FRAME {
                    warn!(peer = %self.addr, size = data.len(), "frame binário grande demais");
                    return Err(Error::PayloadTooLarge {
                        size: data.len(),
                        max: MAX_BINARY_FRAME,
                    });
                }
                debug!(peer = %self.addr, size = data.len(), "frame binário recebido (v0.3)");
                Ok(ControlFlow::Continue(()))
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) => Ok(ControlFlow::Continue(())),
            WsMessage::Close(_) => {
                debug!(peer = %self.addr, "peer fechou a conexão");
                Ok(ControlFlow::Break(()))
            }
        }
    }

    /// Processa uma mensagem de texto já parseada (clipboard,
    /// capacidades, broadcast). Retorna `Continue` para manter o
    /// loop ou `Break` para encerrar.
    async fn handle_text_message(
        &mut self,
        session: &mut PeerSession,
        msg: Message,
    ) -> Result<ControlFlow<(), ()>> {
        if let Err(e) = self.check_payload_size(&msg) {
            session.send(Message::Error {
                code: "payload_too_large".into(),
                message: e.to_string(),
            });
            return Ok(ControlFlow::Continue(()));
        }
        if self.enforce_caps(&msg).is_err() {
            session.send(Message::Error {
                code: "capability_disabled".into(),
                message: format!("tipo '{}' não habilitado na config", msg.type_name()),
            });
            return Ok(ControlFlow::Continue(()));
        }
        let origin = session.peer_id().clone();
        // Origin é autoritativo: sobrepõe o campo declarado pelo
        // client com o device_id autenticado (anti-spoof + anti-eco).
        let msg = Arc::new(msg.with_origin(&origin));
        // 1) Repassa para outros peers.
        self.state
            .broadcast_except(Arc::clone(&msg), Some(&origin))
            .await;
        // 2) Publica no canal local p/ o daemon gravar no clipboard.
        if let Some(event) = crate::dispatch::message_to_event(&msg) {
            let _ = self.state.local_events.send(event).await;
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Verifica se o tipo de mensagem está habilitado na config.
    fn enforce_caps(&self, msg: &Message) -> Result<()> {
        let caps = &self.config.clipboard;
        let enabled = match msg {
            Message::ClipboardText { .. } => caps.sync_text,
            Message::ClipboardImage { .. } => caps.sync_images,
            Message::ClipboardHtml { .. } => caps.sync_html,
            _ => true,
        };
        if enabled {
            Ok(())
        } else {
            Err(Error::Protocol("capability não habilitada".into()))
        }
    }

    /// Valida o tamanho do payload de mensagens de clipboard antes de
    /// processar. Previne DoS de memória de peers maliciosos.
    fn check_payload_size(&self, msg: &Message) -> Result<()> {
        let max_text = self.config.clipboard.max_text_bytes as usize;
        let max_image = self.config.clipboard.max_image_bytes as usize;
        // Base64 infla ~33%: 4 bytes base64 para cada 3 bytes binários.
        let max_image_b64 = max_image * 4 / 3;

        match msg {
            Message::ClipboardText { content, .. } if content.len() > max_text => {
                Err(Error::PayloadTooLarge {
                    size: content.len(),
                    max: max_text,
                })
            }
            Message::ClipboardImage { data_b64, .. } if data_b64.len() > max_image_b64 => {
                Err(Error::PayloadTooLarge {
                    size: data_b64.len(),
                    max: max_image_b64,
                })
            }
            Message::ClipboardHtml { html, .. } if html.len() > max_text => {
                Err(Error::PayloadTooLarge {
                    size: html.len(),
                    max: max_text,
                })
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::PairFailReason;

    #[test]
    fn check_payload_size_rejects_oversized_text() {
        let config = crate::server::ServerConfig::default();
        let (state, _rx) = crate::state::ServerState::new(config.clone(), None).unwrap();
        let state = std::sync::Arc::new(state);
        let conn = ConnectionInner {
            addr: "127.0.0.1:0".parse().unwrap(),
            state,
            config,
        };

        let big = "x".repeat(17 * 1024 * 1024); // > 16 MB
        let msg = Message::ClipboardText {
            mime: "text/plain".into(),
            content: big,
            origin: DeviceId::new(),
            sha256: "abc".into(),
        };
        assert!(conn.check_payload_size(&msg).is_err());
    }

    #[test]
    fn pair_fail_reason_display() {
        assert_eq!(PairFailReason::InvalidCode.to_string(), "invalid_code");
        assert_eq!(PairFailReason::Expired.to_string(), "expired");
    }
}
