//! Cliente outbound do daemon Linux para peers LAN e relay.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{HeaderValue, AUTHORIZATION};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{client_async_tls_with_config, Connector, WebSocketStream};
use tracing::{debug, warn};
use url::Url;

use crate::clipboard::ClipboardEvent;
use crate::config::{Config, EndpointConfig, EndpointScope, OutboundRoute, Transport};
use crate::dispatch;
use crate::protocol::{Capabilities, DeviceInfo, DeviceKind, Message, PROTOCOL_VERSION};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const RETRY_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct OutboundManager;

pub async fn test_endpoint(endpoint: &EndpointConfig) -> Result<(), String> {
    let _ = connect(endpoint).await?;
    Ok(())
}

impl OutboundManager {
    /// Inicia uma sessão outbound failover. A fila permanece ativa mesmo sem
    /// endpoint disponível, portanto endpoints adicionados no próximo boot
    /// não exigem mudanças no watcher.
    pub fn spawn(
        config: Config,
        local_events: mpsc::Sender<ClipboardEvent>,
    ) -> mpsc::Sender<Arc<Message>> {
        let (tx, rx) = mpsc::channel(128);
        tokio::spawn(run(config, local_events, rx));
        tx
    }
}

async fn run(
    config: Config,
    local_events: mpsc::Sender<ClipboardEvent>,
    mut outbound: mpsc::Receiver<Arc<Message>>,
) {
    let endpoints = ordered_endpoints(&config);
    if endpoints.is_empty() {
        debug!("nenhum endpoint outbound configurado");
        return;
    }
    loop {
        let mut connected = false;
        for endpoint in &endpoints {
            match connect(endpoint).await {
                Ok(ws) => {
                    connected = true;
                    if let Err(error) = session(
                        ws,
                        endpoint,
                        config.device_id.clone().unwrap_or_default(),
                        config.name.clone(),
                        config.clipboard.clone(),
                        local_events.clone(),
                        &mut outbound,
                    )
                    .await
                    {
                        warn!(endpoint = %endpoint.name, error = %error, "sessão outbound encerrada");
                    }
                    break;
                }
                Err(error) => {
                    warn!(endpoint = %endpoint.name, error = %error, "endpoint outbound indisponível")
                }
            }
        }
        if !connected {
            tokio::time::sleep(RETRY_DELAY).await;
        }
    }
}

fn ordered_endpoints(config: &Config) -> Vec<EndpointConfig> {
    let mut endpoints: Vec<_> = config
        .endpoints
        .iter()
        .filter(|endpoint| match config.security.outbound_route {
            OutboundRoute::Lan => endpoint.scope == EndpointScope::Lan,
            OutboundRoute::Relay => endpoint.scope == EndpointScope::Relay,
            OutboundRoute::Auto => true,
        })
        .cloned()
        .collect();
    if matches!(config.security.outbound_route, OutboundRoute::Auto) {
        endpoints.sort_by_key(|endpoint| endpoint.scope == EndpointScope::Relay);
    }
    endpoints
}

async fn connect(
    endpoint: &EndpointConfig,
) -> Result<WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>, String> {
    endpoint.validate().map_err(|e| e.to_string())?;
    let url = Url::parse(&endpoint.url).map_err(|e| format!("URL inválida: {e}"))?;
    let host = url.host_str().ok_or("endpoint sem host")?;
    let port = url.port_or_known_default().ok_or("endpoint sem porta")?;
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| "timeout conectando TCP".to_owned())?
        .map_err(|e| e.to_string())?;
    let mut request = endpoint
        .url
        .clone()
        .into_client_request()
        .map_err(|e| format!("request WebSocket inválido: {e}"))?;
    if endpoint.scope == EndpointScope::Relay {
        let token = credential(endpoint)?;
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "bearer contém caracteres inválidos".to_owned())?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    let connector = match endpoint.transport {
        Transport::PlaintextLegacy => Connector::Plain,
        Transport::Tls => {
            let pin = endpoint
                .tls_fingerprint
                .as_deref()
                .ok_or("TLS outbound exige pin SHA-256")?;
            let verifier = PinVerifier {
                pin: pin.to_ascii_lowercase().replace(':', ""),
            };
            let provider = rustls::crypto::ring::default_provider();
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth();
            Connector::Rustls(Arc::new(tls))
        }
    };
    client_async_tls_with_config(request, tcp, None, Some(connector))
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

fn credential(endpoint: &EndpointConfig) -> Result<String, String> {
    let reference = endpoint
        .credential_ref
        .as_deref()
        .ok_or("credential_ref ausente")?;
    if let Some(path) = reference.strip_prefix("file:") {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|e| format!("falha lendo bearer: {e}"))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err("arquivo bearer deve ter modo 0600".into());
            }
        }
        return std::fs::read_to_string(path)
            .map(|value| value.trim().to_owned())
            .map_err(|e| format!("falha lendo bearer: {e}"));
    }
    std::env::var(reference).map_err(|_| format!("variável bearer não definida: {reference}"))
}

async fn session<S>(
    mut ws: WebSocketStream<S>,
    endpoint: &EndpointConfig,
    device_id: crate::protocol::DeviceId,
    name: String,
    clipboard: crate::config::ClipboardConfig,
    local_events: mpsc::Sender<ClipboardEvent>,
    outbound: &mut mpsc::Receiver<Arc<Message>>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut device = DeviceInfo::new(name, DeviceKind::Linux)
        .with_app_version(format!("clipsyncd {}", env!("CARGO_PKG_VERSION")))
        .with_capabilities(Capabilities {
            text: clipboard.sync_text,
            html: clipboard.sync_html,
            images: clipboard.sync_images,
            files: clipboard.sync_files,
        });
    device.id = Some(device_id);
    let hello = Message::Hello {
        v: PROTOCOL_VERSION,
        device,
    };
    ws.send(WsMessage::Text(serde_json::to_string(&hello).unwrap()))
        .await
        .map_err(|e| e.to_string())?;
    if endpoint.scope == EndpointScope::Lan {
        loop {
            match next_message(&mut ws).await? {
                Message::PairOk { .. } => break,
                Message::Ping { ts } => ws
                    .send(WsMessage::Text(
                        serde_json::to_string(&Message::Pong { ts }).unwrap(),
                    ))
                    .await
                    .map_err(|e| e.to_string())?,
                other => return Err(format!("handshake LAN inesperado: {}", other.type_name())),
            }
        }
    }
    loop {
        tokio::select! {
            message = outbound.recv() => {
                let Some(message) = message else { return Ok(()) };
                ws.send(WsMessage::Text(message.to_json().map_err(|e| e.to_string())?)).await.map_err(|e| e.to_string())?;
            }
            item = ws.next() => {
                let Some(item) = item else { return Err("peer fechou conexão".into()) };
                match item.map_err(|e| e.to_string())? {
                    WsMessage::Text(text) => match serde_json::from_str::<Message>(&text).map_err(|e| e.to_string())? {
                        Message::Ping { ts } => ws.send(WsMessage::Text(serde_json::to_string(&Message::Pong { ts }).unwrap())).await.map_err(|e| e.to_string())?,
                        Message::Pong { .. } | Message::PairOk { .. } => {},
                        message => if let Some(event) = dispatch::message_to_event(&message) { let _ = local_events.send(event).await; },
                    },
                    WsMessage::Ping(data) => ws.send(WsMessage::Pong(data)).await.map_err(|e| e.to_string())?,
                    WsMessage::Close(_) => return Err("peer fechou conexão".into()),
                    WsMessage::Pong(_) | WsMessage::Binary(_) | WsMessage::Frame(_) => {},
                }
            }
        }
    }
}

async fn next_message<S>(ws: &mut WebSocketStream<S>) -> Result<Message, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match ws
            .next()
            .await
            .ok_or("conexão fechada")?
            .map_err(|e| e.to_string())?
        {
            WsMessage::Text(text) => return serde_json::from_str(&text).map_err(|e| e.to_string()),
            WsMessage::Ping(data) => ws
                .send(WsMessage::Pong(data))
                .await
                .map_err(|e| e.to_string())?,
            WsMessage::Pong(_) => {}
            WsMessage::Close(_) => return Err("conexão fechada".into()),
            WsMessage::Binary(_) => return Err("frame binário inesperado".into()),
            WsMessage::Frame(_) => {}
        }
    }
}

#[derive(Debug)]
struct PinVerifier {
    pin: String,
}

impl rustls::client::danger::ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fingerprint = crate::tls::fingerprint(cert.as_ref());
        if fingerprint == self.pin {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "TLS fingerprint não corresponde ao pin".into(),
            ))
        }
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EndpointConfig, EndpointScope, Transport};

    #[test]
    fn auto_prefers_lan_before_relay() {
        let config = Config {
            endpoints: vec![
                EndpointConfig {
                    name: "relay".into(),
                    url: "wss://relay/ws".into(),
                    transport: Transport::Tls,
                    tls_fingerprint: Some("a".repeat(64)),
                    credential_ref: Some("TOKEN".into()),
                    scope: EndpointScope::Relay,
                },
                EndpointConfig {
                    name: "lan".into(),
                    url: "wss://lan/ws".into(),
                    transport: Transport::Tls,
                    tls_fingerprint: Some("b".repeat(64)),
                    credential_ref: None,
                    scope: EndpointScope::Lan,
                },
            ],
            ..Config::default()
        };
        assert_eq!(ordered_endpoints(&config)[0].scope, EndpointScope::Lan);
    }
}
