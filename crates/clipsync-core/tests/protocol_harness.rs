//! Teste de integração do protocolo: client WebSocket de referência
//! contra o server de `clipsync-core`.
//!
//! Sobe o server em porta efêmera (127.0.0.1:0), executa o handshake
//! completo (hello -> pair_challenge -> pair_submit -> pair_ok) e
//! valida o round-trip de `clipboard_text` nas duas direções:
//!
//! 1. evento local (watcher simulado via `broadcast_except`) -> client recebe;
//! 2. client envia -> server publica no canal `local_events` (o daemon
//!    gravaria no clipboard local).
//!
//! Toda operação de rede usa timeout; o teste nunca pendura.

use std::net::SocketAddr;
use std::time::Duration;

use clipsync_core::clipboard::{ClipboardEvent, MIME_TEXT};
use clipsync_core::protocol::{Capabilities, DeviceId, DeviceInfo, DeviceKind, Message};
use clipsync_core::server::{Server, ServerConfig};
use clipsync_core::state::ServerState;
use clipsync_core::PROTOCOL_VERSION;
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Timeout total do teste.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Timeout de cada operação de rede (send/recv).
const OP_TIMEOUT: Duration = Duration::from_secs(5);

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_handshake_and_clipboard_roundtrip() {
    let (state, local_rx) = ServerState::new(ServerConfig::default());
    let state = std::sync::Arc::new(state);
    let server = Server::new(ServerConfig::default(), state.clone());
    let app = server.router();

    // Porta efêmera (127.0.0.1:0), mesmo padrão do teste server_healthz_ok.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let result = tokio::time::timeout(TEST_TIMEOUT, run_client(&addr, &state, local_rx)).await;

    handle.abort();
    let _ = handle.await;

    result.expect("teste excedeu o timeout total");
}

/// Executa o fluxo completo do client de referência.
async fn run_client(
    addr: &SocketAddr,
    state: &std::sync::Arc<ServerState>,
    mut local_rx: mpsc::Receiver<ClipboardEvent>,
) {
    let url = format!("ws://{addr}/ws");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client conectou no ws");

    // 1) hello com device novo (id = None -> dispara pair_challenge) e
    //    pareamento completo até pair_ok.
    let device_id = pair_new_client(&mut ws, state).await;

    assert!(
        state.pairing.lock().await.is_trusted(&device_id),
        "device confiado após o pareamento"
    );
    assert_eq!(state.peer_count().await, 1);

    // 5) Round-trip local -> client: simula o watcher do daemon
    //    (broadcast_except com origin None); o client deve receber.
    let local_content = "texto copiado no PC (evento local)";
    let local_sha = sha256_hex(local_content);
    state
        .broadcast_except(
            Message::ClipboardText {
                mime: MIME_TEXT.to_owned(),
                content: local_content.to_owned(),
                origin: DeviceId::new(),
                sha256: local_sha.clone(),
            },
            None,
        )
        .await;

    match recv_message(&mut ws).await {
        Message::ClipboardText {
            mime,
            content,
            origin,
            sha256,
        } => {
            assert_eq!(content, local_content);
            assert_eq!(sha256, local_sha);
            assert_eq!(mime, MIME_TEXT);
            assert_eq!(origin.as_str().len(), 36, "origin é uuid");
        }
        other => panic!("esperava clipboard_text, recebeu {}", other.type_name()),
    }

    // 6) Round-trip client -> local: o client envia clipboard_text e o
    //    server publica no canal local_events (o daemon gravaria).
    let client_content = "texto copiado no phone (enviado pelo client)";
    let client_sha = sha256_hex(client_content);
    send_json(
        &mut ws,
        Message::ClipboardText {
            mime: MIME_TEXT.to_owned(),
            content: client_content.to_owned(),
            origin: device_id,
            sha256: client_sha.clone(),
        },
    )
    .await;

    let event = tokio::time::timeout(OP_TIMEOUT, local_rx.recv())
        .await
        .expect("server publicou no canal local dentro do timeout")
        .expect("canal local_events não foi fechado");
    match event {
        ClipboardEvent::Changed(snap) => {
            assert_eq!(snap.text(), Some(client_content));
            assert_eq!(snap.sha256, client_sha);
        }
        ClipboardEvent::BackendLost(e) => panic!("BackendLost inesperado: {e}"),
    }
}

/// Executa o handshake completo de um client novo (hello → pair_challenge
/// → pair_submit → pair_ok) e devolve o device_id autenticado.
async fn pair_new_client(ws: &mut WsStream, state: &std::sync::Arc<ServerState>) -> DeviceId {
    let device_name = format!("harness-{}", uuid::Uuid::new_v4());
    let device = DeviceInfo::new(device_name.clone(), DeviceKind::Linux)
        .with_app_version("clipsync-harness 0.1.0")
        .with_capabilities(Capabilities {
            text: true,
            ..Capabilities::default()
        });
    send_json(
        ws,
        Message::Hello {
            v: PROTOCOL_VERSION,
            device,
        },
    )
    .await;

    let (code, nonce) = match recv_message(ws).await {
        Message::PairChallenge { code, nonce, .. } => (code, nonce),
        other => panic!("esperava pair_challenge, recebeu {}", other.type_name()),
    };
    assert_eq!(code.len(), 6, "PIN deve ter 6 dígitos");
    assert!(code.chars().all(|c| c.is_ascii_digit()), "PIN numérico");

    send_json(ws, Message::PairSubmit { code, nonce }).await;

    match recv_message(ws).await {
        Message::PairOk {
            device_id,
            session_id,
            server_name,
            capabilities,
        } => {
            assert!(!session_id.is_empty(), "session_id presente");
            assert_eq!(server_name, "linux-desktop");
            assert!(capabilities.text, "text habilitado no pareamento");
            assert!(
                state.pairing.lock().await.is_trusted(&device_id),
                "device confiado após o pareamento"
            );
            device_id
        }
        other => panic!("esperava pair_ok, recebeu {}", other.type_name()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_overrides_forged_origin_with_authenticated_device_id() {
    let (state, _local_rx) = ServerState::new(ServerConfig::default());
    let state = std::sync::Arc::new(state);
    let server = Server::new(ServerConfig::default(), state.clone());
    let app = server.router();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let result = tokio::time::timeout(TEST_TIMEOUT, run_origin_forgery(&addr, &state)).await;

    handle.abort();
    let _ = handle.await;

    result.expect("teste excedeu o timeout total");
}

/// Dois clients pareados; A envia `clipboard_text` com `origin` forjado
/// e B deve receber o `origin` do device_id autenticado de A.
async fn run_origin_forgery(addr: &SocketAddr, state: &std::sync::Arc<ServerState>) {
    let url = format!("ws://{addr}/ws");

    let (mut ws_a, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client A conectou no ws");
    let device_a = pair_new_client(&mut ws_a, state).await;

    let (mut ws_b, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client B conectou no ws");
    let device_b = pair_new_client(&mut ws_b, state).await;
    assert_ne!(device_a, device_b, "pareamentos geram device_ids distintos");
    assert_eq!(state.peer_count().await, 2);

    // A forja um origin de outro device; o servidor deve sobrepor com
    // o device_id autenticado de A antes de repassar a B.
    let forged = DeviceId::from("origin-forjado-pelo-client");
    let forged_content = "texto com origin forjado";
    send_json(
        &mut ws_a,
        Message::ClipboardText {
            mime: MIME_TEXT.to_owned(),
            content: forged_content.to_owned(),
            origin: forged.clone(),
            sha256: sha256_hex(forged_content),
        },
    )
    .await;

    match recv_message(&mut ws_b).await {
        Message::ClipboardText {
            content, origin, ..
        } => {
            assert_eq!(content, forged_content);
            assert_eq!(
                origin, device_a,
                "origin deve ser o device_id autenticado de A"
            );
            assert_ne!(origin, forged, "origin forjado pelo client é ignorado");
        }
        other => panic!("esperava clipboard_text, recebeu {}", other.type_name()),
    }
}

/// Envia uma mensagem serializada com timeout.
async fn send_json(ws: &mut WsStream, msg: Message) {
    let text = serde_json::to_string(&msg).expect("mensagem serializa");
    tokio::time::timeout(OP_TIMEOUT, ws.send(WsMessage::Text(text)))
        .await
        .expect("send excedeu o timeout")
        .expect("falha no send");
}

/// Recebe a próxima mensagem de aplicação, ignorando pings/pongs e
/// com timeout (o teste nunca pendura).
async fn recv_message(ws: &mut WsStream) -> Message {
    loop {
        let item = tokio::time::timeout(OP_TIMEOUT, ws.next())
            .await
            .expect("recv excedeu o timeout")
            .expect("websocket fechado pelo servidor")
            .expect("erro de websocket");
        match item {
            WsMessage::Text(text) => {
                let msg: Message = serde_json::from_str(&text).expect("JSON válido do servidor");
                match msg {
                    Message::Ping { .. } | Message::Pong { .. } => continue,
                    other => return other,
                }
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            WsMessage::Binary(_) => panic!("frame binário inesperado"),
            WsMessage::Close(_) => panic!("servidor fechou a conexão"),
        }
    }
}
