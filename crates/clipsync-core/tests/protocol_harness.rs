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
//! Também cobre o fluxo seguro de pareamento: o desafio NUNCA contém o
//! PIN (ele é lido do `PairingManager` local, como o daemon faria via
//! `active_pin()`), e submissão com PIN errado falha com `invalid_code`.
//!
//! Toda operação de rede usa timeout; o teste nunca pendura.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clipsync_core::clipboard::{sha256_hex, ClipboardEvent, MIME_TEXT};
use clipsync_core::protocol::{
    Capabilities, DeviceId, DeviceInfo, DeviceKind, Message, PairFailReason,
};
use clipsync_core::server::{Server, ServerConfig};
use clipsync_core::state::ServerState;
use clipsync_core::PROTOCOL_VERSION;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Timeout total do teste.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Timeout de cada operação de rede (send/recv).
const OP_TIMEOUT: Duration = Duration::from_secs(5);

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Sobe um server em porta efêmera e devolve (addr, handle) para o
/// caller abortar no fim do teste.
async fn spawn_server(state: Arc<ServerState>) -> (SocketAddr, JoinHandle<()>) {
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
    (addr, handle)
}

/// Conecta um device novo (id = None) e envia o `hello`; o server
/// responde `pair_challenge`.
async fn connect_and_hello(addr: &SocketAddr, device_name: &str) -> WsStream {
    let url = format!("ws://{addr}/ws");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client conectou no ws");
    let device = DeviceInfo::new(device_name.to_owned(), DeviceKind::Linux)
        .with_app_version("clipsync-harness 0.1.0")
        .with_capabilities(Capabilities {
            text: true,
            ..Capabilities::default()
        });
    send_json(
        &mut ws,
        Message::Hello {
            v: PROTOCOL_VERSION,
            device,
        },
    )
    .await;
    ws
}

/// Roda um corpo de teste com timeout total e aborta o server no fim.
async fn run_test<F, Fut>(body: F)
where
    F: FnOnce(SocketAddr, Arc<ServerState>, mpsc::Receiver<ClipboardEvent>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (state, local_rx) = ServerState::new(ServerConfig::default(), None).unwrap();
    let state = Arc::new(state);
    let (addr, handle) = spawn_server(state.clone()).await;
    let result = tokio::time::timeout(TEST_TIMEOUT, body(addr, state, local_rx)).await;
    handle.abort();
    let _ = handle.await;
    result.expect("teste excedeu o timeout total");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_handshake_and_clipboard_roundtrip() {
    run_test(|addr, state, mut local_rx| async move {
        // 1) hello com device novo (id = None -> dispara pair_challenge).
        let device_name = format!("harness-integration-{}", uuid::Uuid::new_v4());
        let mut ws = connect_and_hello(&addr, &device_name).await;

        // 2) pair_challenge: o client só recebe o desafio (sem PIN).
        let (challenge_id, nonce) = match recv_message(&mut ws).await {
            Message::PairChallenge {
                challenge_id,
                nonce,
                ..
            } => (challenge_id, nonce),
            other => panic!("esperava pair_challenge, recebeu {}", other.type_name()),
        };

        // O PIN é exibido no daemon: o harness lê do PairingManager local
        // (mesmo caminho do `clipsyncd --show-pin` / tray).
        let code = {
            let pm = state.pairing.lock().await;
            pm.active_pin().expect("PIN ativo após pair_challenge")
        };
        assert_eq!(code.len(), 6, "PIN deve ter 6 dígitos");
        assert!(code.chars().all(|c| c.is_ascii_digit()), "PIN numérico");

        // 3) pair_submit ecoando o PIN (digitado) + challenge_id + nonce.
        send_json(
            &mut ws,
            Message::PairSubmit {
                challenge_id,
                code,
                nonce,
            },
        )
        .await;

        // 4) pair_ok com device_id estável.
        let (device_id, server_name) = match recv_message(&mut ws).await {
            Message::PairOk {
                device_id,
                server_id,
                session_id,
                server_name,
                capabilities,
            } => {
                assert!(!session_id.is_empty(), "session_id presente");
                assert_eq!(server_id, Some(state.config.device_id.clone()));
                assert!(capabilities.text, "text habilitado no pareamento");
                (device_id, server_name)
            }
            other => panic!("esperava pair_ok, recebeu {}", other.type_name()),
        };
        assert_eq!(server_name, "linux-desktop");
        assert!(
            state.pairing.lock().await.is_trusted(&device_id),
            "device confiado após o pareamento"
        );
        assert_eq!(state.peer_count().await, 1);

        // 5) Round-trip local -> client: simula o watcher do daemon
        //    (broadcast_except com origin None); o client deve receber.
        let local_content = "texto copiado no PC (evento local)";
        let local_sha = sha256_hex(local_content.as_bytes());
        state
            .broadcast_except(
                Arc::new(Message::ClipboardText {
                    mime: MIME_TEXT.to_owned(),
                    content: local_content.to_owned(),
                    origin: DeviceId::new(),
                    sha256: local_sha.clone(),
                }),
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
        let client_sha = sha256_hex(client_content.as_bytes());
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
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_challenge_never_leaks_pin_on_wire() {
    run_test(|addr, state, _local_rx| async move {
        let device_name = format!("harness-leak-{}", uuid::Uuid::new_v4());
        let mut ws = connect_and_hello(&addr, &device_name).await;

        // Captura o JSON cru do pair_challenge: o PIN não pode aparecer
        // em lugar nenhum da resposta.
        let raw = recv_raw_text(&mut ws).await;
        let value: serde_json::Value = serde_json::from_str(&raw).expect("JSON válido");
        assert_eq!(value["type"], "pair_challenge");
        assert!(
            value.get("code").is_none(),
            "pair_challenge não deve conter campo 'code'"
        );

        let pin = {
            let pm = state.pairing.lock().await;
            pm.active_pin().expect("PIN ativo no servidor")
        };
        assert!(
            !raw.contains(&pin),
            "o PIN ativo não pode aparecer no frame do challenge"
        );

        // Fecha o pareamento para não deixar estado pendurado.
        let challenge_id = value["challenge_id"]
            .as_str()
            .expect("challenge_id presente")
            .to_owned();
        let nonce = value["nonce"].as_str().expect("nonce presente").to_owned();
        send_json(
            &mut ws,
            Message::PairSubmit {
                challenge_id,
                code: pin,
                nonce,
            },
        )
        .await;
        match recv_message(&mut ws).await {
            Message::PairOk { .. } => {}
            other => panic!("esperava pair_ok, recebeu {}", other.type_name()),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_submit_wrong_pin_fails_with_invalid() {
    run_test(|addr, _state, _local_rx| async move {
        let device_name = format!("harness-wrong-{}", uuid::Uuid::new_v4());
        let mut ws = connect_and_hello(&addr, &device_name).await;

        let (challenge_id, nonce) = match recv_message(&mut ws).await {
            Message::PairChallenge {
                challenge_id,
                nonce,
                ..
            } => (challenge_id, nonce),
            other => panic!("esperava pair_challenge, recebeu {}", other.type_name()),
        };

        // Submete um PIN errado (ex.: o "000000" nunca é gerado).
        send_json(
            &mut ws,
            Message::PairSubmit {
                challenge_id,
                code: "000000".into(),
                nonce,
            },
        )
        .await;

        match recv_message(&mut ws).await {
            Message::PairFail { reason, .. } => {
                assert_eq!(reason, PairFailReason::InvalidCode);
            }
            other => panic!("esperava pair_fail, recebeu {}", other.type_name()),
        }
    })
    .await;
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

    let (challenge_id, nonce) = match recv_message(ws).await {
        Message::PairChallenge {
            challenge_id,
            nonce,
            ..
        } => (challenge_id, nonce),
        other => panic!("esperava pair_challenge, recebeu {}", other.type_name()),
    };

    // O PIN é exibido no daemon: o harness lê do PairingManager local
    // (mesmo caminho do `clipsyncd --show-pin` / tray).
    let code = {
        let pm = state.pairing.lock().await;
        pm.active_pin().expect("PIN ativo após pair_challenge")
    };
    assert_eq!(code.len(), 6, "PIN deve ter 6 dígitos");
    assert!(code.chars().all(|c| c.is_ascii_digit()), "PIN numérico");

    send_json(
        ws,
        Message::PairSubmit {
            challenge_id,
            code,
            nonce,
        },
    )
    .await;

    match recv_message(ws).await {
        Message::PairOk {
            device_id,
            server_id,
            session_id,
            server_name,
            capabilities,
        } => {
            assert!(!session_id.is_empty(), "session_id presente");
            assert_eq!(server_id, Some(state.config.device_id.clone()));
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
    let (state, _local_rx) = ServerState::new(ServerConfig::default(), None).unwrap();
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
            sha256: sha256_hex(forged_content.as_bytes()),
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

/// Recebe o texto cru do próximo frame de aplicação, ignorando pings.
async fn recv_raw_text(ws: &mut WsStream) -> String {
    loop {
        let item = tokio::time::timeout(OP_TIMEOUT, ws.next())
            .await
            .expect("recv excedeu o timeout")
            .expect("websocket fechado pelo servidor")
            .expect("erro de websocket");
        match item {
            WsMessage::Text(text) => return text,
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            WsMessage::Binary(_) => panic!("frame binário inesperado"),
            WsMessage::Close(_) => panic!("servidor fechou a conexão"),
        }
    }
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

// ============================================================
// Novos helpers para os cenários adicionados (#44)
// ============================================================

/// Conecta como device confiado (com `device_id` conhecido) e envia
/// `hello`. O servidor deve responder `PairOk` direto, sem passar
/// pelo fluxo de PIN.
async fn connect_as_trusted(addr: &SocketAddr, device_id: &DeviceId, name: &str) -> WsStream {
    let url = format!("ws://{addr}/ws");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("client conectou no ws");
    let mut device = DeviceInfo::new(name.to_owned(), DeviceKind::Linux)
        .with_app_version("clipsync-harness 0.1.0")
        .with_capabilities(Capabilities {
            text: true,
            ..Capabilities::default()
        });
    device.id = Some(device_id.clone());
    send_json(
        &mut ws,
        Message::Hello {
            v: PROTOCOL_VERSION,
            device,
        },
    )
    .await;
    ws
}

/// Recebe a próxima mensagem de aplicação ou `None` se a conexão
/// for fechada (timeout, EOF, erro ou frame Close). Útil para
/// cenários onde o servidor fecha a conexão e queremos detectar
/// sem panic.
async fn recv_message_or_close(ws: &mut WsStream) -> Option<Message> {
    loop {
        let result = tokio::time::timeout(OP_TIMEOUT, ws.next()).await;
        let item = match result {
            Ok(Some(item)) => item,
            Ok(None) => return None,
            Err(_) => return None,
        };
        let ws_msg = match item {
            Ok(m) => m,
            Err(_) => return None,
        };
        match ws_msg {
            WsMessage::Text(text) => {
                let msg: Message = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(_) => return None,
                };
                match msg {
                    Message::Ping { .. } | Message::Pong { .. } => continue,
                    other => return Some(other),
                }
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            WsMessage::Close(_) | WsMessage::Binary(_) => return None,
        }
    }
}

// ============================================================
// #44 — Cenário 1: PIN errado → PairFail + InvalidCode
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_pin_returns_pair_fail_with_invalid_code() {
    run_test(|addr, state, _local_rx| async move {
        let device_name = format!("harness-wrong-pin-{}", uuid::Uuid::new_v4());
        let mut ws = connect_and_hello(&addr, &device_name).await;

        let (challenge_id, nonce) = match recv_message(&mut ws).await {
            Message::PairChallenge {
                challenge_id,
                nonce,
                ..
            } => (challenge_id, nonce),
            other => panic!("esperava pair_challenge, recebeu {}", other.type_name()),
        };

        // Submete PIN errado ("000000" — impossível; gerador usa 100_000..=999_999).
        send_json(
            &mut ws,
            Message::PairSubmit {
                challenge_id,
                code: "000000".into(),
                nonce,
            },
        )
        .await;

        match recv_message(&mut ws).await {
            Message::PairFail { reason, message } => {
                assert_eq!(reason, PairFailReason::InvalidCode);
                assert!(!message.is_empty(), "PairFail message não pode ser vazia");
            }
            other => panic!("esperava pair_fail, recebeu {}", other.type_name()),
        }

        // O servidor NÃO deve ter registrado peer após falha de pareamento.
        assert_eq!(
            state.peer_count().await,
            0,
            "nenhum peer após pareamento falho"
        );
    })
    .await;
}

// ============================================================
// #44 — Cenário 2: Device confiado pula pareamento
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_device_skips_pairing() {
    run_test(|addr, state, _local_rx| async move {
        // 1) Primeira conexão: handshake completo → obtém device_id.
        let url = format!("ws://{addr}/ws");
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("client conectou no ws");
        let device_id = pair_new_client(&mut ws1, &state).await;
        assert_eq!(state.peer_count().await, 1);

        // 2) Fecha a primeira conexão e aguarda remoção determinística.
        drop(ws1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while state.peer_count().await > 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timeout aguardando peer_count() chegar a 0"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // 3) Reconecta como device confiado: deve receber PairOk direto.
        let mut ws2 = connect_as_trusted(&addr, &device_id, "trusted-reconnect").await;
        match recv_message(&mut ws2).await {
            Message::PairOk {
                device_id: did,
                server_id: _,
                session_id,
                server_name,
                capabilities,
            } => {
                assert_eq!(did, device_id, "mesmo device_id no reconnect confiado");
                assert!(!session_id.is_empty(), "session_id presente");
                assert_eq!(server_name, "linux-desktop");
                assert!(capabilities.text, "text habilitado");
            }
            other => panic!(
                "esperava pair_ok no reconnect confiado, recebeu {}",
                other.type_name()
            ),
        }

        assert_eq!(state.peer_count().await, 1);
        assert!(
            state.pairing.lock().await.is_trusted(&device_id),
            "device permanece confiado após reconnect"
        );
    })
    .await;
}

// ============================================================
// #44 — Cenário 3: Anti-eco com 2 clients
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_anti_echo() {
    run_test(|addr, state, _local_rx| async move {
        // Pareia dois clients.
        let url = format!("ws://{addr}/ws");
        let (mut ws_a, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("client A conectou");
        let device_a = pair_new_client(&mut ws_a, &state).await;

        let (mut ws_b, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("client B conectou");
        let device_b = pair_new_client(&mut ws_b, &state).await;

        assert_ne!(device_a, device_b, "pareamentos geram ids distintos");
        assert_eq!(state.peer_count().await, 2);

        // Client A envia clipboard_text.
        let content = "texto anti-eco de A";
        let sha = sha256_hex(content.as_bytes());
        send_json(
            &mut ws_a,
            Message::ClipboardText {
                mime: MIME_TEXT.to_owned(),
                content: content.to_owned(),
                origin: device_a.clone(),
                sha256: sha.clone(),
            },
        )
        .await;

        // Client B DEVE receber a mensagem.
        match recv_message(&mut ws_b).await {
            Message::ClipboardText {
                content: c,
                origin,
                sha256,
                ..
            } => {
                assert_eq!(c, content);
                assert_eq!(
                    origin, device_a,
                    "origin deve ser o device_id autenticado de A"
                );
                assert_eq!(sha256, sha);
            }
            other => panic!("B esperava clipboard_text, recebeu {}", other.type_name()),
        }

        // Client A NÃO deve receber sua própria mensagem de volta (anti-eco).
        let a_echo = tokio::time::timeout(Duration::from_secs(1), recv_message(&mut ws_a)).await;
        assert!(
            a_echo.is_err(),
            "A não deve receber seu próprio clipboard_text de volta (anti-eco)"
        );
    })
    .await;
}

// ============================================================
// #44 — Cenário 4: Frames malformados
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_frames_handled_gracefully() {
    // 4a: JSON inválido antes do hello → servidor fecha conexão (sem panic).
    run_test(|addr, _state, _local_rx| async move {
        let url = format!("ws://{addr}/ws");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("conectou");

        ws.send(WsMessage::Text("not valid json!!!".into()))
            .await
            .expect("enviou lixo");

        // Servidor deve fechar a conexão graciosamente.
        let result = recv_message_or_close(&mut ws).await;
        assert!(
            result.is_none(),
            "conexão deve fechar após JSON inválido antes do hello"
        );
    })
    .await;

    // 4b: Tipo de mensagem errado antes do hello → servidor fecha conexão.
    run_test(|addr, _state, _local_rx| async move {
        let url = format!("ws://{addr}/ws");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("conectou");

        // Envia clipboard_text antes do hello (tipo errado no estágio errado).
        send_json(
            &mut ws,
            Message::ClipboardText {
                mime: MIME_TEXT.to_owned(),
                content: "too early".into(),
                origin: DeviceId::new(),
                sha256: "abc".into(),
            },
        )
        .await;

        let result = recv_message_or_close(&mut ws).await;
        assert!(
            result.is_none(),
            "conexão deve fechar quando tipo errado enviado antes do hello"
        );
    })
    .await;

    // 4c: JSON inválido durante message loop → servidor ignora e continua.
    run_test(|addr, state, mut local_rx| async move {
        let url = format!("ws://{addr}/ws");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("conectou");
        let _device_id = pair_new_client(&mut ws, &state).await;

        // Envia JSON inválido.
        ws.send(WsMessage::Text("{broken json".into()))
            .await
            .expect("enviou lixo");

        // Servidor deve ignorar o frame ruim e continuar processando.
        // Envia um clipboard_text válido; se o servidor não crashou,
        // ele publica no canal local_events.
        let content = "still working after bad json";
        let sha = sha256_hex(content.as_bytes());
        send_json(
            &mut ws,
            Message::ClipboardText {
                mime: MIME_TEXT.to_owned(),
                content: content.to_owned(),
                origin: DeviceId::new(),
                sha256: sha.clone(),
            },
        )
        .await;

        let event = tokio::time::timeout(OP_TIMEOUT, local_rx.recv())
            .await
            .expect("server publicou no canal local dentro do timeout")
            .expect("canal local_events não foi fechado");
        match event {
            ClipboardEvent::Changed(snap) => {
                assert_eq!(snap.text(), Some(content));
                assert_eq!(snap.sha256, sha);
            }
            ClipboardEvent::BackendLost(e) => panic!("BackendLost inesperado: {e}"),
        }
    })
    .await;
}

// ============================================================
// #44 — Cenário 5: Reconexão substitui sessão antiga
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_supersedes_old_session() {
    run_test(|addr, state, _local_rx| async move {
        // 1) Conecta e pareia client A.
        let url = format!("ws://{addr}/ws");
        let (mut ws_a, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("client A conectou");
        let device_id = pair_new_client(&mut ws_a, &state).await;
        assert_eq!(state.peer_count().await, 1);

        // 2) Spawn task para ler o erro "superseded" de ws_a.
        let a_handle = tokio::spawn(async move { recv_message(&mut ws_a).await });
        tokio::task::yield_now().await;

        // 3) Conecta client B com o MESMO device_id (trusted).
        let mut ws_b = connect_as_trusted(&addr, &device_id, "reconnect-B").await;

        // 4) ws_a deve receber Error { code: "superseded" }.
        let a_result = tokio::time::timeout(TEST_TIMEOUT, a_handle)
            .await
            .expect("ws_a task excedeu timeout")
            .expect("ws_a task paniquei");
        match a_result {
            Message::Error { code, message } => {
                assert_eq!(code, "superseded");
                assert!(
                    !message.is_empty(),
                    "mensagem de superseded não pode ser vazia"
                );
            }
            other => panic!(
                "esperava superseded na sessão antiga, recebeu {}",
                other.type_name()
            ),
        }

        // 5) ws_b deve receber PairOk e funcionar normalmente.
        match recv_message(&mut ws_b).await {
            Message::PairOk {
                device_id: did,
                session_id,
                ..
            } => {
                assert_eq!(did, device_id, "mesmo device_id na nova sessão");
                assert!(!session_id.is_empty(), "session_id presente");
            }
            other => panic!(
                "esperava pair_ok no reconnect, recebeu {}",
                other.type_name()
            ),
        }

        // Apenas a nova sessão deve estar registrada.
        assert_eq!(state.peer_count().await, 1);
    })
    .await;
}
