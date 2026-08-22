//! Cliente desktop mínimo e oficial para macOS e Windows.
//!
//! A integração de clipboard usa `arboard`, que chama as APIs nativas de cada
//! sistema. O cliente deliberadamente oferece texto apenas: imagens e tray
//! ainda não fazem parte desta entrega cross-platform.

use std::time::Duration;

use arboard::Clipboard;
use clap::Parser;
use clipsync_core::clipboard::sha256_hex;
use clipsync_core::protocol::{Capabilities, DeviceInfo, DeviceKind, Message, PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

const DEFAULT_URL: &str = "ws://127.0.0.1:8765/ws";

#[derive(Parser, Debug)]
#[command(
    name = "clipsync-client",
    about = "Cliente desktop clipsync para macOS/Windows"
)]
struct Cli {
    /// URL WebSocket do daemon (também aceita CLIPSYNC_URL).
    #[arg(default_value = DEFAULT_URL, env = "CLIPSYNC_URL")]
    url: String,
    /// PIN mostrado localmente pelo daemon/tray.
    #[arg(long, env = "CLIPSYNC_PIN")]
    pin: String,
    /// Nome anunciado no handshake.
    #[arg(long, default_value = "clipsync desktop")]
    name: String,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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
    let (mut ws, _) = tokio_tungstenite::connect_async(&cli.url)
        .await
        .map_err(|e| format!("conexão com {} falhou: {e}", cli.url))?;
    let kind = device_kind();
    let device = DeviceInfo::new(&cli.name, kind)
        .with_app_version(format!("clipsync-client {}", env!("CARGO_PKG_VERSION")))
        .with_capabilities(Capabilities {
            text: true,
            ..Capabilities::default()
        });
    send(
        &mut ws,
        Message::Hello {
            v: PROTOCOL_VERSION,
            device,
        },
    )
    .await?;

    let (challenge_id, nonce) = match receive(&mut ws).await? {
        Message::PairChallenge {
            challenge_id,
            nonce,
            ..
        } => (challenge_id, nonce),
        Message::PairOk { device_id, .. } => return run_clipboard(ws, device_id).await,
        other => return Err(format!("esperava desafio, recebi {}", other.type_name())),
    };
    send(
        &mut ws,
        Message::PairSubmit {
            challenge_id,
            code: cli.pin,
            nonce,
        },
    )
    .await?;
    let device_id = match receive(&mut ws).await? {
        Message::PairOk { device_id, .. } => device_id,
        Message::PairFail { reason, message } => {
            return Err(format!("pareamento {reason}: {message}"))
        }
        other => return Err(format!("esperava pair_ok, recebi {}", other.type_name())),
    };
    run_clipboard(ws, device_id).await
}

async fn run_clipboard(mut ws: Ws, device_id: clipsync_core::DeviceId) -> Result<(), String> {
    let mut clipboard =
        Clipboard::new().map_err(|e| format!("clipboard nativo indisponível: {e}"))?;
    let mut last_hash = clipboard
        .get_text()
        .ok()
        .map(|text| sha256_hex(text.as_bytes()));
    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    eprintln!("pareado como {device_id}; sincronizando texto (Ctrl+C para sair)");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Ok(text) = clipboard.get_text() {
                    let hash = sha256_hex(text.as_bytes());
                    if last_hash.as_deref() != Some(hash.as_str()) {
                        last_hash = Some(hash.clone());
                        send(&mut ws, Message::ClipboardText {
                            mime: "text/plain;charset=utf-8".into(),
                            content: text,
                            origin: device_id.clone(),
                            sha256: hash,
                        }).await?;
                    }
                }
            }
            item = ws.next() => match item {
                Some(Ok(WsMessage::Text(text))) => match serde_json::from_str::<Message>(&text) {
                    Ok(Message::ClipboardText { content, sha256, .. }) => {
                        clipboard.set_text(content).map_err(|e| format!("escrita no clipboard falhou: {e}"))?;
                        last_hash = Some(sha256);
                    }
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

fn device_kind() -> DeviceKind {
    if cfg!(target_os = "macos") {
        DeviceKind::Macos
    } else if cfg!(target_os = "windows") {
        DeviceKind::Windows
    } else {
        DeviceKind::Other
    }
}
