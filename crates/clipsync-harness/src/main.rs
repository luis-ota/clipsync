//! `harness` — client WebSocket de referência do protocolo clipsync.
//!
//! Executa o handshake completo (hello → pair_challenge → pair_submit
//! → pair_ok) contra um daemon `clipsyncd run` e entra em modo echo:
//! imprime todo `clipboard_text` recebido e envia o texto digitado no
//! stdin como `clipboard_text`.
//!
//! O PIN de pareamento é exibido pelo daemon (bandeja/tray) e digitado
//! aqui no client — o desafio recebido nunca contém o PIN.
//!
//! Uso:
//!   harness [URL] [--pin PIN] [--name NOME]
//!
//! URL default: `ws://127.0.0.1:8765/ws` (override via env `CLIPSYNC_URL`).

use std::time::Duration;

use clap::Parser;
use clipsync_core::clipboard::MIME_TEXT;
use clipsync_core::protocol::{Capabilities, DeviceInfo, DeviceKind, Message, PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_tungstenite::tungstenite::Message as WsMessage;

const DEFAULT_URL: &str = "ws://127.0.0.1:8765/ws";
const OP_TIMEOUT: Duration = Duration::from_secs(5);

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Parser, Debug)]
#[command(
    name = "harness",
    about = "Client WebSocket de referência do protocolo clipsync"
)]
struct Cli {
    /// URL do daemon (default: ws://127.0.0.1:8765/ws, ou env CLIPSYNC_URL)
    url: Option<String>,
    /// PIN do pareamento (se omitido, lê do terminal)
    #[arg(long)]
    pin: Option<String>,
    /// Nome do device anunciado no hello
    #[arg(long, default_value = "harness")]
    name: String,
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

fn resolve_url(cli: &Cli) -> String {
    cli.url
        .clone()
        .or_else(|| std::env::var("CLIPSYNC_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_owned())
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(run(cli));
}

async fn run(cli: Cli) {
    let url = resolve_url(&cli);
    println!("Conectando em {url} ...");

    let (mut ws, _resp) = match tokio_tungstenite::connect_async(&url).await {
        Ok(x) => x,
        Err(e) => {
            eprintln!("Falha conectando em {url}: {e}");
            std::process::exit(1);
        }
    };

    // hello: device novo (id = None) -> servidor responde pair_challenge.
    let device = DeviceInfo::new(cli.name.clone(), DeviceKind::Linux)
        .with_app_version(format!("clipsync-harness {}", env!("CARGO_PKG_VERSION")))
        .with_capabilities(Capabilities {
            text: true,
            ..Capabilities::default()
        });
    match send(
        &mut ws,
        Message::Hello {
            v: PROTOCOL_VERSION,
            device,
        },
    )
    .await
    {
        Ok(()) => {}
        Err(e) => exit_err("hello", &e),
    }

    // pair_challenge: o client só recebe o desafio (sem PIN). O PIN é
    // exibido pelo daemon na bandeja/tray e digitado abaixo.
    let (challenge_id, nonce) = match recv(&mut ws).await {
        Ok(Message::PairChallenge {
            challenge_id,
            nonce,
            expires_at,
        }) => {
            println!("Desafio de pareamento recebido (expira em {expires_at})");
            (challenge_id, nonce)
        }
        Ok(other) => exit_err("pair_challenge", &format!("recebeu {}", other.type_name())),
        Err(e) => exit_err("pair_challenge", &e),
    };

    // PIN: --pin ou digitado no terminal (exibido pelo daemon).
    let pin = match cli.pin {
        Some(p) => p,
        None => {
            print!("Digite o PIN exibido no daemon: ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                line.clear();
            }
            let typed = line.trim().to_owned();
            if typed.is_empty() {
                eprintln!("PIN não informado; use --pin ou digite o PIN exibido no daemon");
                std::process::exit(1);
            }
            typed
        }
    };

    // pair_submit.
    match send(
        &mut ws,
        Message::PairSubmit {
            challenge_id,
            code: pin,
            nonce,
        },
    )
    .await
    {
        Ok(()) => {}
        Err(e) => exit_err("pair_submit", &e),
    }

    // pair_ok.
    let device_id = match recv(&mut ws).await {
        Ok(Message::PairOk {
            device_id,
            session_id,
            server_name,
            capabilities,
        }) => {
            println!(
                "Pareado! device_id={device_id} session_id={session_id} server={server_name} caps={capabilities:?}"
            );
            device_id
        }
        Ok(Message::PairFail { reason, message }) => {
            eprintln!("Pareamento falhou ({reason}): {message}");
            std::process::exit(1);
        }
        Ok(other) => exit_err("pair_ok", &format!("recebeu {}", other.type_name())),
        Err(e) => exit_err("pair_ok", &e),
    };

    // Modo echo: envia linhas do stdin, imprime o que chega.
    println!("Modo echo: digite texto para enviar (Ctrl+D para sair).");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        let msg = Message::ClipboardText {
                            mime: MIME_TEXT.to_owned(),
                            content: text.clone(),
                            origin: device_id.clone(),
                            sha256: sha256_hex(&text),
                        };
                        match send(&mut ws, msg).await {
                            Ok(()) => println!("SENT {text}"),
                            Err(e) => {
                                eprintln!("Falha enviando: {e}");
                                break;
                            }
                        }
                    }
                    _ => break, // EOF
                }
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        match serde_json::from_str::<Message>(&text) {
                            Ok(Message::ClipboardText { mime, content, .. }) => {
                                println!("RECV [{mime}] {content}");
                            }
                            Ok(Message::Ping { ts }) => {
                                let _ = send(&mut ws, Message::Pong { ts }).await;
                            }
                            Ok(other) => println!("RECV {}", other.type_name()),
                            Err(e) => eprintln!("JSON inválido recebido: {e}"),
                        }
                    }
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = ws.send(WsMessage::Pong(data)).await;
                    }
                    Some(Ok(WsMessage::Pong(_))) | Some(Ok(WsMessage::Frame(_))) => {}
                    Some(Ok(WsMessage::Binary(_))) => println!("RECV <binário>"),
                    Some(Ok(WsMessage::Close(_))) => {
                        println!("Servidor fechou a conexão");
                        break;
                    }
                    Some(Err(e)) => {
                        eprintln!("Erro de websocket: {e}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    let _ = ws.send(WsMessage::Close(None)).await;
    println!("Encerrando.");
}

/// Envia uma mensagem de aplicação com timeout.
async fn send(ws: &mut WsStream, msg: Message) -> Result<(), String> {
    let text = serde_json::to_string(&msg).expect("mensagem serializa");
    tokio::time::timeout(OP_TIMEOUT, ws.send(WsMessage::Text(text)))
        .await
        .map_err(|_| "timeout no send".to_string())?
        .map_err(|e| format!("falha no send: {e}"))
}

/// Recebe a próxima mensagem de aplicação, respondendo a pings e com
/// timeout (nunca pendura).
async fn recv(ws: &mut WsStream) -> Result<Message, String> {
    loop {
        let item = tokio::time::timeout(OP_TIMEOUT, ws.next())
            .await
            .map_err(|_| "timeout esperando mensagem do servidor".to_string())?
            .ok_or_else(|| "conexão fechada pelo servidor".to_string())?
            .map_err(|e| format!("erro de websocket: {e}"))?;
        match item {
            WsMessage::Text(text) => {
                return serde_json::from_str(&text)
                    .map_err(|e| format!("JSON inválido do servidor: {e}"))
            }
            WsMessage::Ping(data) => {
                ws.send(WsMessage::Pong(data))
                    .await
                    .map_err(|e| format!("falha no pong: {e}"))?;
            }
            WsMessage::Pong(_) | WsMessage::Frame(_) => {}
            WsMessage::Binary(_) => return Err("frame binário inesperado".into()),
            WsMessage::Close(_) => return Err("conexão fechada pelo servidor".into()),
        }
    }
}

/// Imprime o erro e sai com código 1.
fn exit_err(phase: &str, e: &str) -> ! {
    eprintln!("Falha em {phase}: {e}");
    std::process::exit(1);
}
