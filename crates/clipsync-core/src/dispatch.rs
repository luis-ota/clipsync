//! Dispatch: conversão entre snapshots de clipboard e mensagens do
//! protocolo.
//!
//! As três funções públicas deste módulo extraem a lógica de dispatch
//! que antes vivia inline no `cmd_run` do daemon e no `transport`:
//!
//! - [`event_to_message`]: snapshot local → `Message` para broadcast
//!   aos peers (watcher → rede).
//! - [`message_to_event`]: `Message` de clipboard recebida de um peer
//!   → [`ClipboardEvent`] para o canal local (rede → clipboard).
//! - [`apply_peer_snapshot`]: snapshot recebido de um peer → escrita
//!   no clipboard local (rede → clipboard).
//!
//! `event_to_message` e `message_to_event` são puras e testáveis sem
//! I/O. `apply_peer_snapshot` escreve no clipboard local (via
//! `ClipboardManager`) mas é testável em modo headless sem
//! dependência de rede.

use base64::Engine;
use tracing::warn;

use crate::clipboard::{
    is_supported_image_mime, ClipboardEvent, ClipboardManager, ClipboardSnapshot, WriteOrigin,
    MIME_HTML,
};
use crate::config::ClipboardConfig;
use crate::protocol::{DeviceId, Message};

/// Converte um snapshot de clipboard local em uma [`Message`] para
/// broadcast aos peers, respeitando as flags de sincronização da
/// config.
///
/// Retorna `None` quando o tipo de conteúdo está desabilitado na
/// config ou quando o snapshot não contém conteúdo transmissível
/// (ex: rich text sem HTML quando `sync_html` está desligado).
///
/// Ordem de precedência:
/// 1. Imagem (`sync_images`)
/// 2. Rich text / HTML (`sync_html`) — exige `snap.rich`
/// 3. Texto plain (`sync_text`)
pub fn event_to_message(
    snap: &ClipboardSnapshot,
    cfg: &ClipboardConfig,
    origin: &DeviceId,
) -> Option<Message> {
    if is_supported_image_mime(&snap.mime)
        && cfg.sync_images
        && snap.bytes.len() <= cfg.max_image_bytes as usize
    {
        use base64::Engine;
        Some(Message::ClipboardImage {
            mime: snap.mime.clone(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(&snap.bytes),
            width: None,
            height: None,
            sha256: snap.sha256.clone(),
            origin: origin.clone(),
        })
    } else if cfg.sync_html {
        let rich = snap.rich.as_ref()?;
        let alt = snap.text().map(|t| t.to_owned());
        Some(Message::ClipboardHtml {
            sha256: rich.sha256.clone(),
            html: rich.html.clone(),
            alt,
            origin: origin.clone(),
        })
    } else if snap.mime.starts_with("text/")
        && cfg.sync_text
        && snap.bytes.len() <= cfg.max_text_bytes as usize
    {
        Some(Message::ClipboardText {
            mime: snap.mime.clone(),
            content: String::from_utf8_lossy(&snap.bytes).into_owned(),
            origin: origin.clone(),
            sha256: snap.sha256.clone(),
        })
    } else {
        None
    }
}

/// Aplica um snapshot recebido de um peer ao clipboard local.
///
/// Lida com fallback: se a escrita de HTML falhar (ex: backend sem
/// suporte a MIME seletivo), cai para texto plain.
pub async fn apply_peer_snapshot(snap: &ClipboardSnapshot, cm: &mut ClipboardManager) {
    if snap.mime == MIME_HTML {
        if let Some(rich) = &snap.rich {
            if cm
                .write_html(&rich.html, WriteOrigin::Remote)
                .await
                .is_err()
            {
                let fallback = snap.text().unwrap_or(&rich.html);
                let _ = cm.write_text(fallback, WriteOrigin::Remote).await;
            }
        }
    } else if snap.mime.starts_with("text/") {
        let _ = cm
            .write_text(snap.text().unwrap_or_default(), WriteOrigin::Remote)
            .await;
    } else if snap.mime.starts_with("image/") {
        let _ = cm
            .write_image(&snap.mime, &snap.bytes, WriteOrigin::Remote)
            .await;
    }
}

/// Converte uma [`Message`] de clipboard recebida de um peer em um
/// [`ClipboardEvent`] para o canal local.
///
/// Retorna `None` para mensagens não-clipboard ou quando o decode de
/// base64 falha. É a operação inversa de [`event_to_message`].
pub fn message_to_event(msg: &Message) -> Option<ClipboardEvent> {
    match msg {
        Message::ClipboardText { content, .. } => Some(ClipboardEvent::Changed(Box::new(
            ClipboardSnapshot::new_text(
                crate::clipboard::MIME_TEXT,
                content.as_bytes().to_vec(),
                sha_of(msg),
            ),
        ))),
        Message::ClipboardImage { data_b64, mime, .. } => {
            if !is_supported_image_mime(mime) {
                warn!(mime, "MIME de imagem não suportado; ignorando");
                return None;
            }
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "base64 inválido em clipboard_image; ignorando");
                    return None;
                }
            };
            Some(ClipboardEvent::Changed(Box::new(
                ClipboardSnapshot::new_image(mime, bytes, sha_of(msg)),
            )))
        }
        Message::ClipboardHtml { html, alt, .. } => Some(ClipboardEvent::Changed(Box::new(
            ClipboardSnapshot::new_html(html.clone(), alt.clone(), sha_of(msg)),
        ))),
        _ => None,
    }
}

fn sha_of(msg: &Message) -> String {
    match msg {
        Message::ClipboardText { sha256, .. }
        | Message::ClipboardImage { sha256, .. }
        | Message::ClipboardHtml { sha256, .. } => sha256.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::sha256_hex;
    use crate::clipboard::{ClipboardSnapshot, MIME_HTML, MIME_JPEG, MIME_PNG, MIME_TEXT};
    use crate::protocol::DeviceId;

    fn default_clipboard_cfg() -> ClipboardConfig {
        ClipboardConfig::default()
    }

    fn origin() -> DeviceId {
        DeviceId::from("test-origin")
    }

    // ---- event_to_message tests ----

    #[test]
    fn text_snapshot_produces_clipboard_text() {
        let snap = ClipboardSnapshot::new_text(
            MIME_TEXT,
            b"hello world".to_vec(),
            sha256_hex(b"hello world"),
        );
        let cfg = default_clipboard_cfg();
        let msg = event_to_message(&snap, &cfg, &origin()).expect("esperava Some");
        match msg {
            Message::ClipboardText {
                mime,
                content,
                origin,
                sha256,
            } => {
                assert_eq!(mime, MIME_TEXT);
                assert_eq!(content, "hello world");
                assert_eq!(origin, DeviceId::from("test-origin"));
                assert!(!sha256.is_empty());
            }
            other => panic!("esperava ClipboardText, recebeu {}", other.type_name()),
        }
    }

    #[test]
    fn image_snapshot_produces_clipboard_image() {
        let img_bytes = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic
        let snap =
            ClipboardSnapshot::new_image(MIME_PNG, img_bytes.clone(), sha256_hex(&img_bytes));
        let cfg = default_clipboard_cfg();
        let msg = event_to_message(&snap, &cfg, &origin()).expect("esperava Some");
        match msg {
            Message::ClipboardImage {
                mime,
                data_b64,
                origin,
                ..
            } => {
                assert_eq!(mime, MIME_PNG);
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&data_b64)
                    .unwrap();
                assert_eq!(decoded, img_bytes);
                assert_eq!(origin, DeviceId::from("test-origin"));
            }
            other => panic!("esperava ClipboardImage, recebeu {}", other.type_name()),
        }
    }

    #[test]
    fn html_snapshot_produces_clipboard_html_when_sync_html_enabled() {
        let html = "<b>bold</b>";
        let snap = ClipboardSnapshot::new_html(
            html.into(),
            Some("bold".into()),
            sha256_hex(html.as_bytes()),
        );
        let mut cfg = default_clipboard_cfg();
        cfg.sync_html = true;
        let msg = event_to_message(&snap, &cfg, &origin()).expect("esperava Some");
        match msg {
            Message::ClipboardHtml {
                html: h,
                alt,
                origin,
                ..
            } => {
                assert_eq!(h, "<b>bold</b>");
                assert_eq!(alt, Some("bold".into()));
                assert_eq!(origin, DeviceId::from("test-origin"));
            }
            other => panic!("esperava ClipboardHtml, recebeu {}", other.type_name()),
        }
    }

    #[test]
    fn rich_text_without_html_returns_none_when_html_enabled() {
        // Snapshot de texto plain (sem rich text) com sync_html=true:
        // quando HTML está habilitado, a função assume modo HTML-first
        // e retorna None para snapshots sem rich text (equivale ao
        // `continue` do cmd_run original).
        let snap = ClipboardSnapshot::new_text(MIME_TEXT, b"plain only".to_vec(), "hash".into());
        let mut cfg = default_clipboard_cfg();
        cfg.sync_html = true;
        cfg.sync_text = true;
        assert!(
            event_to_message(&snap, &cfg, &origin()).is_none(),
            "sync_html=true sem rich text deve retornar None"
        );
    }

    #[test]
    fn text_disabled_returns_none() {
        let snap = ClipboardSnapshot::new_text(MIME_TEXT, b"hi".to_vec(), "hash".into());
        let mut cfg = default_clipboard_cfg();
        cfg.sync_text = false;
        assert!(event_to_message(&snap, &cfg, &origin()).is_none());
    }

    #[test]
    fn image_disabled_returns_none() {
        let snap = ClipboardSnapshot::new_image(MIME_PNG, vec![0x89], "hash".into());
        let mut cfg = default_clipboard_cfg();
        cfg.sync_images = false;
        assert!(event_to_message(&snap, &cfg, &origin()).is_none());
    }

    #[test]
    fn unsupported_image_mime_is_not_published() {
        let snap = ClipboardSnapshot::new_image("image/gif", vec![1], "hash".into());
        assert!(event_to_message(&snap, &default_clipboard_cfg(), &origin()).is_none());
    }

    #[test]
    fn oversized_image_is_not_published() {
        let mut cfg = default_clipboard_cfg();
        cfg.max_image_bytes = 1;
        let snap = ClipboardSnapshot::new_image(MIME_PNG, vec![1, 2], "hash".into());
        assert!(event_to_message(&snap, &cfg, &origin()).is_none());
    }

    #[test]
    fn html_disabled_returns_none_for_html_snapshot() {
        let snap = ClipboardSnapshot::new_html("<b>x</b>".into(), Some("x".into()), "hash".into());
        let mut cfg = default_clipboard_cfg();
        cfg.sync_html = false;
        cfg.sync_text = false;
        assert!(event_to_message(&snap, &cfg, &origin()).is_none());
    }

    #[test]
    fn html_enabled_but_no_rich_text_returns_none() {
        // sync_html=true mas snap não tem rich text → None (falha no ?)
        let snap = ClipboardSnapshot::new_text(MIME_TEXT, b"hi".to_vec(), "hash".into());
        let mut cfg = default_clipboard_cfg();
        cfg.sync_html = true;
        cfg.sync_text = false;
        assert!(event_to_message(&snap, &cfg, &origin()).is_none());
    }

    #[test]
    fn jpeg_image_produces_clipboard_image() {
        let snap = ClipboardSnapshot::new_image(MIME_JPEG, vec![0xFF, 0xD8], "hash".into());
        let cfg = default_clipboard_cfg();
        let msg = event_to_message(&snap, &cfg, &origin()).expect("esperava Some");
        match msg {
            Message::ClipboardImage { mime, .. } => assert_eq!(mime, MIME_JPEG),
            other => panic!("esperava ClipboardImage, recebeu {}", other.type_name()),
        }
    }

    // ---- apply_peer_snapshot tests ----

    #[tokio::test]
    async fn apply_text_snapshot_writes_to_clipboard() {
        let snap = ClipboardSnapshot::new_text(MIME_TEXT, b"peer text".to_vec(), "hash".into());
        let mut cm = ClipboardManager::headless();
        apply_peer_snapshot(&snap, &mut cm).await;
        // Headless: write é no-op, mas não deve panicar.
    }

    #[tokio::test]
    async fn apply_image_snapshot_writes_to_clipboard() {
        let snap = ClipboardSnapshot::new_image(MIME_PNG, vec![0x89, 0x50], "hash".into());
        let mut cm = ClipboardManager::headless();
        apply_peer_snapshot(&snap, &mut cm).await;
    }

    #[tokio::test]
    async fn apply_html_snapshot_writes_to_clipboard() {
        let snap =
            ClipboardSnapshot::new_html("<p>html</p>".into(), Some("html".into()), "hash".into());
        let mut cm = ClipboardManager::headless();
        apply_peer_snapshot(&snap, &mut cm).await;
    }

    #[tokio::test]
    async fn apply_html_without_rich_text_does_nothing() {
        // MIME_HTML mas sem rich: não deve panicar.
        let snap = ClipboardSnapshot {
            mime: MIME_HTML.to_owned(),
            bytes: b"fallback".to_vec(),
            sha256: "hash".into(),
            rich: None,
        };
        let mut cm = ClipboardManager::headless();
        apply_peer_snapshot(&snap, &mut cm).await;
    }

    #[tokio::test]
    async fn apply_unknown_mime_does_nothing() {
        let snap = ClipboardSnapshot {
            mime: "application/octet-stream".into(),
            bytes: vec![0x00],
            sha256: "hash".into(),
            rich: None,
        };
        let mut cm = ClipboardManager::headless();
        apply_peer_snapshot(&snap, &mut cm).await;
    }

    // ---- message_to_event tests ----

    #[test]
    fn message_to_event_builds_snapshot() {
        let msg = Message::ClipboardText {
            mime: "text/plain".into(),
            content: "hello".into(),
            origin: DeviceId::new(),
            sha256: "abc".into(),
        };
        match message_to_event(&msg) {
            Some(ClipboardEvent::Changed(snap)) => {
                assert_eq!(snap.text(), Some("hello"));
            }
            _ => panic!("esperava Some(Changed)"),
        }
    }

    #[test]
    fn message_to_event_returns_none_for_non_clipboard() {
        let msg = Message::Ping { ts: 123 };
        assert!(message_to_event(&msg).is_none(), "ping não gera snapshot");
    }

    #[test]
    fn message_to_event_returns_none_for_invalid_base64() {
        let msg = Message::ClipboardImage {
            mime: "image/png".into(),
            data_b64: "!!!invalid-base64!!!".into(),
            width: None,
            height: None,
            sha256: "abc".into(),
            origin: DeviceId::new(),
        };
        assert!(
            message_to_event(&msg).is_none(),
            "base64 inválido deve retornar None"
        );
    }
}
