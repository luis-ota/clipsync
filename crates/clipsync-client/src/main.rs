//! Cliente desktop cross-platform para macOS e Windows.
//!
//! O cliente não é um daemon: mantém a conexão enquanto está em execução e usa
//! `arboard` para as APIs nativas de texto e imagem. O estado de pareamento é
//! separado por `server_id`, para que vários servidores possam ser usados sem
//! substituir a identidade de outro servidor.

use std::borrow::Cow;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arboard::{Clipboard, ImageData};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::Parser;
use clipsync_core::clipboard::sha256_hex;
use clipsync_core::discovery::Discovery;
use clipsync_core::protocol::{
    Capabilities, DeviceId, DeviceInfo, DeviceKind, Message, PROTOCOL_VERSION,
};
use futures::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::Digest;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

const DEFAULT_URL: &str = "ws://127.0.0.1:8765/ws";
const RECONNECT_MAX: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(
    name = "clipsync-client",
    about = "Cliente desktop clipsync para macOS/Windows"
)]
struct Cli {
    /// Endpoint manual. Se omitido, usa o primeiro serviço mDNS encontrado.
    #[arg(long, env = "CLIPSYNC_URL")]
    url: Option<String>,
    /// PIN mostrado localmente pelo daemon no primeiro pareamento.
    #[arg(long, env = "CLIPSYNC_PIN")]
    pin: Option<String>,
    /// Nome anunciado no handshake.
    #[arg(long, default_value = "clipsync desktop")]
    name: String,
    /// Fingerprint SHA-256 do certificado DER, sem confiança em hostname.
    #[arg(long, env = "CLIPSYNC_TLS_FINGERPRINT")]
    tls_fingerprint: Option<String>,
    /// Bearer token para relay. Nunca é gravado no estado local.
    #[arg(long, env = "CLIPSYNC_RELAY_TOKEN", hide_env_values = true)]
    relay_token: Option<String>,
    /// Não fazer browse mDNS quando --url não for informado.
    #[arg(long)]
    no_mdns: bool,
    /// Seleciona um pairing persistido quando o servidor mudou de endpoint.
    #[arg(long, env = "CLIPSYNC_SERVER_ID")]
    server_id: Option<DeviceId>,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ServerPairing {
    server_id: DeviceId,
    device_id: DeviceId,
    server_name: String,
    endpoint: String,
    #[serde(default)]
    tls_fingerprint: Option<String>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ClientState {
    pairings: Vec<ServerPairing>,
}

impl ClientState {
    fn path() -> Result<PathBuf, String> {
        Ok(clipsync_core::config::config_dir()
            .map_err(|e| e.to_string())?
            .join("desktop-client.json"))
    }

    fn load() -> Result<Self, String> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        serde_json::from_slice(&fs::read(&path).map_err(|e| format!("lendo {path:?}: {e}"))?)
            .map_err(|e| format!("estado inválido em {path:?}: {e}"))
    }

    fn save(&self) -> Result<(), String> {
        let path = Self::path()?;
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("criando {parent:?}: {e}"))?;
        }
        fs::write(&tmp, bytes).map_err(|e| format!("escrevendo {tmp:?}: {e}"))?;
        #[cfg(windows)]
        if path.exists() {
            // Windows não substitui um arquivo aberto com rename; o estado não
            // contém segredos, e esta troca mantém a persistência funcional.
            fs::remove_file(&path).map_err(|e| format!("removendo estado {path:?}: {e}"))?;
        }
        fs::rename(&tmp, &path).map_err(|e| format!("instalando estado {path:?}: {e}"))
    }

    fn pairing_for(&self, endpoint: &str) -> Option<&ServerPairing> {
        self.pairings.iter().find(|p| p.endpoint == endpoint)
    }

    fn remember(&mut self, pairing: ServerPairing) {
        self.pairings.retain(|p| p.server_id != pairing.server_id);
        self.pairings.push(pairing);
    }
}

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(error) = runtime.block_on(run(cli)) {
        eprintln!("clipsync-client: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let mut state = ClientState::load()?;
    let endpoint = resolve_endpoint(&cli).await?;
    let mut delay = Duration::from_secs(1);
    loop {
        match connect(
            &endpoint.url,
            endpoint.fingerprint.as_deref(),
            cli.relay_token.as_deref(),
        )
        .await
        {
            Ok((mut ws, _)) => match handshake(
                &mut ws,
                &cli,
                &mut state,
                &endpoint.url,
                endpoint.fingerprint.clone(),
            )
            .await
            {
                Ok(device_id) => {
                    delay = Duration::from_secs(1);
                    if let Err(error) = run_clipboard(ws, device_id).await {
                        eprintln!("conexão perdida: {error}; reconectando");
                    }
                }
                Err(error) => {
                    if error.contains("PIN") {
                        return Err(error);
                    }
                    eprintln!("handshake falhou: {error}; reconectando");
                }
            },
            Err(error) => eprintln!("conexão com {} falhou: {error}", endpoint.url),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX);
    }
}

#[derive(Debug, Clone)]
struct Endpoint {
    url: String,
    fingerprint: Option<String>,
}

async fn resolve_endpoint(cli: &Cli) -> Result<Endpoint, String> {
    if let Some(url) = &cli.url {
        return Ok(Endpoint {
            url: url.clone(),
            fingerprint: cli.tls_fingerprint.clone(),
        });
    }
    if cli.no_mdns {
        return Ok(Endpoint {
            url: DEFAULT_URL.into(),
            fingerprint: cli.tls_fingerprint.clone(),
        });
    }
    let discovery = Discovery::new().map_err(|e| format!("mDNS indisponível: {e}"))?;
    let service = discovery
        .browse(Duration::from_secs(5), 500)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .next()
        .ok_or_else(|| "nenhum servidor mDNS encontrado; use --url".to_string())?;
    let ip = service
        .addrs
        .first()
        .ok_or_else(|| "serviço mDNS sem endereço".to_string())?;
    let scheme = if service.properties.get("tls").map(String::as_str) == Some("1") {
        "wss"
    } else {
        "ws"
    };
    let host = match ip {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    };
    let url = format!("{scheme}://{host}:{}/ws", service.port);
    let fingerprint = cli.tls_fingerprint.clone().or_else(|| {
        service
            .properties
            .get("tls_fingerprint")
            .cloned()
            .filter(|v| !v.is_empty())
    });
    Ok(Endpoint { url, fingerprint })
}

async fn connect(
    url: &str,
    fingerprint: Option<&str>,
    relay_token: Option<&str>,
) -> Result<
    (
        Ws,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    String,
> {
    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    if let Some(token) = relay_token {
        let value = format!("Bearer {token}")
            .parse()
            .map_err(|_| "Bearer token inválido".to_string())?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    let connector = if url.starts_with("wss://") {
        let fingerprint = fingerprint
            .ok_or_else(|| "wss exige --tls-fingerprint ou fingerprint mDNS".to_string())?;
        Some(Connector::Rustls(Arc::new(pinned_tls(fingerprint)?)))
    } else {
        None
    };
    connect_async_tls_with_config(request, None, false, connector)
        .await
        .map_err(|e| e.to_string())
}

async fn handshake(
    ws: &mut Ws,
    cli: &Cli,
    state: &mut ClientState,
    endpoint: &str,
    fingerprint: Option<String>,
) -> Result<DeviceId, String> {
    let known = cli
        .server_id
        .as_ref()
        .and_then(|id| state.pairings.iter().find(|p| &p.server_id == id))
        .cloned()
        .or_else(|| state.pairing_for(endpoint).cloned());
    let device = DeviceInfo::new(&cli.name, device_kind())
        .with_app_version(format!("clipsync-client {}", env!("CARGO_PKG_VERSION")))
        .with_capabilities(Capabilities {
            text: true,
            images: true,
            html: false,
            ..Default::default()
        });
    let device = if let Some(pairing) = &known {
        DeviceInfo {
            id: Some(pairing.device_id.clone()),
            ..device
        }
    } else {
        device
    };
    send(
        ws,
        Message::Hello {
            v: PROTOCOL_VERSION,
            device,
        },
    )
    .await?;
    let response = receive(ws).await?;
    let device_id = match response {
        Message::PairOk {
            device_id,
            server_id: Some(server_id),
            server_name,
            ..
        } => {
            state.remember(ServerPairing {
                server_id,
                device_id: device_id.clone(),
                server_name,
                endpoint: endpoint.into(),
                tls_fingerprint: fingerprint,
            });
            state.save()?;
            device_id
        }
        Message::PairOk {
            device_id,
            server_id: None,
            ..
        } => device_id,
        Message::PairChallenge {
            challenge_id,
            nonce,
            ..
        } => {
            let pin = cli
                .pin
                .as_deref()
                .ok_or_else(|| "PIN necessário para primeiro pareamento".to_string())?;
            send(
                ws,
                Message::PairSubmit {
                    challenge_id,
                    code: pin.into(),
                    nonce,
                },
            )
            .await?;
            match receive(ws).await? {
                Message::PairOk {
                    device_id,
                    server_id: Some(server_id),
                    server_name,
                    ..
                } => {
                    state.remember(ServerPairing {
                        server_id,
                        device_id: device_id.clone(),
                        server_name,
                        endpoint: endpoint.into(),
                        tls_fingerprint: fingerprint,
                    });
                    state.save()?;
                    device_id
                }
                Message::PairFail { reason, message } => {
                    return Err(format!("PIN ({reason}): {message}"))
                }
                other => return Err(format!("esperava pair_ok, recebi {}", other.type_name())),
            }
        }
        other => {
            return Err(format!(
                "esperava pair_challenge/pair_ok, recebi {}",
                other.type_name()
            ))
        }
    };
    Ok(device_id)
}

async fn run_clipboard(mut ws: Ws, device_id: DeviceId) -> Result<(), String> {
    let mut clipboard =
        Clipboard::new().map_err(|e| format!("clipboard nativo indisponível: {e}"))?;
    let mut last_hash = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    eprintln!("pareado como {device_id}; sincronizando texto/imagem (Ctrl+C para sair)");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Ok(text) = clipboard.get_text() {
                    let hash = sha256_hex(text.as_bytes());
                    if last_hash.as_deref() != Some(hash.as_str()) {
                        last_hash = Some(hash.clone());
                        send(&mut ws, Message::ClipboardText { mime: "text/plain;charset=utf-8".into(), content: text, origin: device_id.clone(), sha256: hash }).await?;
                    }
                } else if let Ok(image) = clipboard.get_image() {
                    let data = png_bytes(&image)?;
                    let hash = sha256_hex(&data);
                    if last_hash.as_deref() != Some(hash.as_str()) {
                        last_hash = Some(hash.clone());
                        send(&mut ws, Message::ClipboardImage { mime: "image/png".into(), data_b64: STANDARD.encode(data), width: Some(image.width as u32), height: Some(image.height as u32), sha256: hash, origin: device_id.clone() }).await?;
                    }
                }
            }
            item = ws.next() => match item {
                Some(Ok(WsMessage::Text(text))) => match serde_json::from_str::<Message>(&text) {
                    Ok(Message::ClipboardText { content, sha256, .. }) => { clipboard.set_text(content).map_err(|e| e.to_string())?; last_hash = Some(sha256); }
                    Ok(Message::ClipboardImage { data_b64, sha256, width, height, .. }) => { let image = png_image(&STANDARD.decode(data_b64).map_err(|e| e.to_string())?, width, height)?; clipboard.set_image(image).map_err(|e| e.to_string())?; last_hash = Some(sha256); }
                    Ok(Message::ClipboardHtml { alt, .. }) => if let Some(text) = alt { clipboard.set_text(text).map_err(|e| e.to_string())?; }
                    Ok(Message::Ping { ts }) => send(&mut ws, Message::Pong { ts }).await?,
                    Ok(Message::Error { code, message }) if code == "superseded" => return Err(message),
                    Ok(_) => {}
                    Err(e) => return Err(format!("JSON inválido: {e}")),
                },
                Some(Ok(WsMessage::Ping(data))) => ws.send(WsMessage::Pong(data)).await.map_err(|e| e.to_string())?,
                Some(Ok(WsMessage::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("websocket: {e}")),
            }
        }
    }
}

fn png_bytes(image: &ImageData<'_>) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut writer = {
        let mut encoder = png::Encoder::new(&mut output, image.width as u32, image.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().map_err(|e| e.to_string())?
    };
    writer
        .write_image_data(&image.bytes)
        .map_err(|e| e.to_string())?;
    drop(writer);
    Ok(output)
}

fn png_image(
    bytes: &[u8],
    width: Option<u32>,
    height: Option<u32>,
) -> Result<ImageData<'static>, String> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba
        || Some(info.width) != width
        || Some(info.height) != height
    {
        return Err("imagem recebida não é RGBA compatível".into());
    }
    Ok(ImageData {
        width: info.width as usize,
        height: info.height as usize,
        bytes: Cow::Owned(data[..info.buffer_size()].to_vec()),
    })
}

async fn send(ws: &mut Ws, message: Message) -> Result<(), String> {
    ws.send(WsMessage::Text(
        serde_json::to_string(&message).map_err(|e| e.to_string())?,
    ))
    .await
    .map_err(|e| e.to_string())
}

async fn receive(ws: &mut Ws) -> Result<Message, String> {
    loop {
        match ws
            .next()
            .await
            .ok_or_else(|| "conexão fechada".to_string())?
            .map_err(|e| e.to_string())?
        {
            WsMessage::Text(text) => return serde_json::from_str(&text).map_err(|e| e.to_string()),
            WsMessage::Ping(data) => ws
                .send(WsMessage::Pong(data))
                .await
                .map_err(|e| e.to_string())?,
            WsMessage::Pong(_) => {}
            WsMessage::Close(_) => return Err("conexão fechada durante handshake".into()),
            WsMessage::Binary(_) | WsMessage::Frame(_) => return Err("frame inesperado".into()),
        }
    }
}

fn pinned_tls(expected: &str) -> Result<rustls::ClientConfig, String> {
    let expected = expected
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .to_lowercase();
    if expected.len() != 64 {
        return Err("fingerprint TLS deve ter 64 dígitos hex".into());
    }
    let verifier = PinnedVerifier {
        expected,
        algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
    };
    Ok(rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

#[derive(Debug)]
struct PinnedVerifier {
    expected: String,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = sha2::Sha256::digest(cert.as_ref())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if actual != self.expected {
            return Err(rustls::Error::General(
                "TLS certificate pin mismatch".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

fn device_kind() -> DeviceKind {
    if cfg!(target_os = "macos") {
        DeviceKind::Macos
    } else if cfg!(target_os = "windows") {
        DeviceKind::Windows
    } else {
        DeviceKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairings_are_keyed_by_server_and_replace_only_that_server() {
        let mut state = ClientState::default();
        state.remember(ServerPairing {
            server_id: DeviceId::from("a"),
            device_id: DeviceId::from("a1"),
            server_name: "A".into(),
            endpoint: "ws://a/ws".into(),
            tls_fingerprint: None,
        });
        state.remember(ServerPairing {
            server_id: DeviceId::from("b"),
            device_id: DeviceId::from("b1"),
            server_name: "B".into(),
            endpoint: "ws://b/ws".into(),
            tls_fingerprint: None,
        });
        state.remember(ServerPairing {
            server_id: DeviceId::from("a"),
            device_id: DeviceId::from("a2"),
            server_name: "A2".into(),
            endpoint: "ws://a2/ws".into(),
            tls_fingerprint: None,
        });
        assert_eq!(state.pairings.len(), 2);
        assert_eq!(
            state.pairing_for("ws://a2/ws").unwrap().device_id,
            DeviceId::from("a2")
        );
        assert_eq!(
            state.pairing_for("ws://b/ws").unwrap().device_id,
            DeviceId::from("b1")
        );
    }
    #[test]
    fn pin_rejects_non_sha256_length() {
        assert!(pinned_tls("00").is_err());
    }
}
