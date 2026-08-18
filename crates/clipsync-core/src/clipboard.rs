//! Abstração de clipboard com suporte a Wayland e X11.
//!
//! Em Wayland, delega para `wl-copy`/`wl-paste` (ferramentas padrão
//! de `wl-clipboard`, pacote Arch `wl-clipboard`). Em X11, usa
//! `arboard` (que delega para `xclip`/`xsel`).
//!
//! O backend é escolhido automaticamente na construção:
//!
//! 1. Se `WAYLAND_DISPLAY` está setado, usa Wayland.
//! 2. Caso contrário, tenta X11 via `$DISPLAY`.
//! 3. Em último caso, opera em modo `headless` (apenas relay entre
//!    peers — útil para testes e CI sem display).

mod watch;

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

/// MIME types suportados por este daemon.
pub const MIME_TEXT: &str = "text/plain;charset=utf-8";
pub const MIME_TEXT_PLAIN: &str = "text/plain";
pub const MIME_PNG: &str = "image/png";
pub const MIME_JPEG: &str = "image/jpeg";
pub const MIME_HTML: &str = "text/html";

/// Conteúdo rich text (HTML) associado a um snapshot.
#[derive(Debug, Clone)]
pub struct RichText {
    pub html: String,
    pub sha256: String,
}

/// Snapshot do clipboard num dado momento.
///
/// O campo `rich` carrega o conteúdo rich text (text/html) quando
/// disponível no clipboard, além do conteúdo primário em `bytes`.
#[derive(Debug, Clone)]
pub struct ClipboardSnapshot {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub rich: Option<RichText>,
}

impl ClipboardSnapshot {
    fn new_basic(mime: &str, bytes: Vec<u8>, sha256: String) -> Self {
        Self {
            mime: mime.to_owned(),
            bytes,
            sha256,
            rich: None,
        }
    }

    /// Snapshot de texto plain.
    pub fn new_text(mime: &str, bytes: Vec<u8>, sha256: String) -> Self {
        Self::new_basic(mime, bytes, sha256)
    }

    /// Snapshot de imagem.
    pub fn new_image(mime: &str, bytes: Vec<u8>, sha256: String) -> Self {
        Self::new_basic(mime, bytes, sha256)
    }

    /// Snapshot de rich text (HTML). `bytes` é o fallback plain text;
    /// `sha256` é o hash do HTML, armazenado em `rich.sha256`.
    pub fn new_html(html: String, alt: Option<String>, sha256: String) -> Self {
        let bytes = alt
            .map(|a| a.into_bytes())
            .unwrap_or_else(|| html.as_bytes().to_vec());
        Self {
            mime: MIME_HTML.to_owned(),
            bytes,
            sha256: sha256.clone(),
            rich: Some(RichText { html, sha256 }),
        }
    }

    pub fn text(&self) -> Option<&str> {
        if self.mime.starts_with("text/") {
            std::str::from_utf8(&self.bytes).ok()
        } else {
            None
        }
    }

    /// Conteúdo HTML quando disponível.
    pub fn html(&self) -> Option<&str> {
        self.rich.as_ref().map(|r| r.html.as_str())
    }

    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }
}

/// Origem de uma escrita no clipboard local. Usado para anti-eco.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOrigin {
    Remote,
    Local,
}

/// Eventos emitidos pelo watcher.
#[derive(Debug, Clone)]
pub enum ClipboardEvent {
    Changed(Box<ClipboardSnapshot>),
    BackendLost(String),
}

/// Backend de clipboard escolhido.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Wayland,
    X11,
    Headless,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Wayland => "wayland",
            Self::X11 => "x11",
            Self::Headless => "headless",
        }
    }
}

/// Rastro compartilhado da última escrita remota (anti-eco).
#[derive(Debug, Clone, Default)]
struct SelfWriteTracker(Arc<Mutex<Option<String>>>);

impl SelfWriteTracker {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }
    fn set(&self, sha256: String) {
        *self.0.lock().unwrap() = Some(sha256);
    }
    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
    fn matches(&self, sha256: &str) -> bool {
        self.0.lock().unwrap().as_deref() == Some(sha256)
    }
}

/// Wrapper de alto nível sobre o backend de clipboard.
#[derive(Debug)]
pub struct ClipboardManager {
    backend: BackendKind,
    last_self_write: SelfWriteTracker,
    last_seen: Option<String>,
}

impl ClipboardManager {
    /// Constrói um novo manager, selecionando o melhor backend.
    pub fn new() -> Result<Self> {
        let backend = Self::pick_backend();
        info!(backend = backend.name(), "clipboard backend selected");
        Ok(Self {
            backend,
            last_self_write: SelfWriteTracker::new(),
            last_seen: None,
        })
    }

    /// Constrói um manager em modo headless (sem display).
    pub fn headless() -> Self {
        Self {
            backend: BackendKind::Headless,
            last_self_write: SelfWriteTracker::new(),
            last_seen: None,
        }
    }

    /// Cópia que compartilha o rastro de escrita própria (anti-eco).
    pub fn share_self_write(&self) -> Self {
        Self {
            backend: self.backend,
            last_self_write: self.last_self_write.clone(),
            last_seen: None,
        }
    }

    fn pick_backend() -> BackendKind {
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            return BackendKind::Wayland;
        }
        if std::env::var_os("DISPLAY").is_some() {
            return BackendKind::X11;
        }
        BackendKind::Headless
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.backend
    }

    /// Verifica se as ferramentas externas necessárias existem.
    pub fn check_tools(&self) -> Result<()> {
        match self.backend {
            BackendKind::Wayland => {
                Self::check_tool("wl-paste", "wl-clipboard")?;
                Self::check_tool("wl-copy", "wl-clipboard")?;
            }
            BackendKind::X11 => {
                Self::check_tool("xclip", "xclip")?;
                Self::check_tool("xsel", "xclip")?;
            }
            BackendKind::Headless => {}
        }
        Ok(())
    }

    fn check_tool(tool: &str, pkg: &str) -> Result<()> {
        if Command::new("which").arg(tool).output().is_err() {
            return Err(Error::Clipboard(format!(
                "{tool} não encontrado. Instale com: sudo pacman -S {pkg}"
            )));
        }
        Ok(())
    }

    /// Lê o conteúdo atual do clipboard. Retorna `None` se vazio.
    ///
    /// Em Wayland, tenta `text/html` como complemento ao texto plain.
    pub fn read(&mut self, preferred_mimes: &[&str]) -> Result<Option<ClipboardSnapshot>> {
        match self.backend {
            BackendKind::Wayland => {
                for mime in preferred_mimes {
                    let out = Command::new("wl-paste")
                        .arg("--no-newline")
                        .arg("--type")
                        .arg(mime)
                        .output();
                    match out {
                        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                            let mut snap = Self::snapshot(mime, o.stdout);
                            Self::attach_html(&mut snap);
                            return Ok(Some(snap));
                        }
                        Ok(o) if o.status.success() => continue,
                        Ok(_) => continue,
                        Err(e) => {
                            debug!(mime, error = %e, "wl-paste falhou");
                            return Err(Error::Clipboard(e.to_string()));
                        }
                    }
                }
                Ok(None)
            }
            BackendKind::X11 => {
                if preferred_mimes.iter().any(|m| m.starts_with("text/")) {
                    let out = Command::new("xclip")
                        .arg("-selection")
                        .arg("clipboard")
                        .arg("-o")
                        .output();
                    match out {
                        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                            let snap = Self::snapshot(MIME_TEXT, o.stdout);
                            debug!("x11: leitura de HTML não suportada");
                            return Ok(Some(snap));
                        }
                        Ok(_) => {}
                        Err(e) => return Err(Error::Clipboard(e.to_string())),
                    }
                }
                Ok(None)
            }
            BackendKind::Headless => Ok(None),
        }
    }

    /// Anexa `text/html` ao snapshot quando disponível.
    fn attach_html(snap: &mut ClipboardSnapshot) {
        if snap.mime == MIME_HTML {
            let html = String::from_utf8_lossy(&snap.bytes).into_owned();
            snap.rich = Some(RichText {
                html,
                sha256: snap.sha256.clone(),
            });
            return;
        }
        if !snap.mime.starts_with("text/") {
            return;
        }
        let out = Command::new("wl-paste")
            .arg("--no-newline")
            .arg("--type")
            .arg(MIME_HTML)
            .output();
        match out {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                let html = String::from_utf8_lossy(&o.stdout).into_owned();
                let sha256 = hex::encode(Sha256::digest(html.as_bytes()));
                snap.rich = Some(RichText { html, sha256 });
            }
            _ => debug!("sem conteúdo text/html no clipboard"),
        }
    }

    fn snapshot(mime: &str, bytes: Vec<u8>) -> ClipboardSnapshot {
        let sha256 = hex::encode(Sha256::digest(&bytes));
        if mime.starts_with("image/") {
            ClipboardSnapshot::new_image(mime, bytes, sha256)
        } else {
            ClipboardSnapshot::new_text(mime, bytes, sha256)
        }
    }

    /// Escreve texto no clipboard.
    pub fn write_text(&mut self, text: &str, origin: WriteOrigin) -> Result<()> {
        self.write(MIME_TEXT, text.as_bytes(), origin)
    }

    /// Escreve imagem no clipboard.
    pub fn write_image(&mut self, mime: &str, bytes: &[u8], origin: WriteOrigin) -> Result<()> {
        if !mime.starts_with("image/") {
            return Err(Error::Protocol(format!("mime de imagem inválido: {mime}")));
        }
        self.write(mime, bytes, origin)
    }

    /// Escreve rich text (HTML) no clipboard.
    pub fn write_html(&mut self, html: &str, origin: WriteOrigin) -> Result<()> {
        self.write(MIME_HTML, html.as_bytes(), origin)
    }

    fn write(&mut self, mime: &str, bytes: &[u8], origin: WriteOrigin) -> Result<()> {
        match self.backend {
            BackendKind::Wayland => {
                Self::spawn_and_write("wl-copy", &["--type", mime], bytes)?;
            }
            BackendKind::X11 => {
                Self::spawn_and_write("xclip", &["-selection", "clipboard", "-i"], bytes)?;
            }
            BackendKind::Headless => {
                debug!("headless: ignorando write de {} bytes", bytes.len());
            }
        }
        if origin == WriteOrigin::Remote {
            let sha = hex::encode(Sha256::digest(bytes));
            self.last_self_write.set(sha);
        }
        Ok(())
    }

    fn spawn_and_write(cmd: &str, args: &[&str], bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Clipboard(format!("falha spawn {cmd}: {e}")))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(bytes)
                .map_err(|e| Error::Clipboard(format!("falha escrevendo stdin: {e}")))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| Error::Clipboard(format!("falha wait {cmd}: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Clipboard(format!("{cmd} falhou: {}", stderr.trim())));
        }
        Ok(())
    }

    /// Inicia um watcher que emite [`ClipboardEvent::Changed`].
    ///
    /// Wayland usa `wl-paste --watch`; X11/headless faz polling.
    /// Mudanças rápidas são coalescidas via debounce.
    pub fn watch(self, interval: Duration) -> mpsc::Receiver<ClipboardEvent> {
        let (tx, rx) = mpsc::channel(64);
        let backend = self.backend;

        tokio::spawn(async move {
            let mut me = self;
            if backend == BackendKind::Wayland && watch::wl_paste_exists() {
                match watch::run_event_driven(&mut me, tx.clone()).await {
                    Ok(()) => return,
                    Err(e) => {
                        warn!(error = %e, "wl-paste --watch falhou; caindo para polling");
                        let _ = tx.send(ClipboardEvent::BackendLost(e.to_string())).await;
                    }
                }
            }
            watch::run_polling(&mut me, tx, interval).await;
        });

        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_constructor_uses_alt_as_bytes() {
        let s = ClipboardSnapshot::new_html("<b>oi</b>".into(), Some("oi".into()), "sha123".into());
        assert_eq!(s.mime, MIME_HTML);
        assert_eq!(s.bytes, b"oi".to_vec());
        assert_eq!(s.html(), Some("<b>oi</b>"));
        assert_eq!(s.sha256, "sha123");
        let rich = s.rich.as_ref().expect("rich text presente");
        assert_eq!(rich.sha256, "sha123");
    }

    #[test]
    fn html_constructor_falls_back_to_html_bytes() {
        let s = ClipboardSnapshot::new_html("<b>oi</b>".into(), None, "sha123".into());
        assert_eq!(s.bytes, b"<b>oi</b>".to_vec());
        let rich = s.rich.as_ref().expect("rich text presente");
        assert_eq!(rich.sha256, "sha123");
    }

    #[test]
    fn snapshot_hashes_deterministically() {
        let s1 = ClipboardManager::snapshot("text/plain", b"hello".to_vec());
        let s2 = ClipboardManager::snapshot("text/plain", b"hello".to_vec());
        let s3 = ClipboardManager::snapshot("text/plain", b"hellp".to_vec());
        assert_eq!(s1.sha256, s2.sha256);
        assert_ne!(s1.sha256, s3.sha256);
        assert!(s1.rich.is_none());
    }

    #[test]
    fn rich_text_hash_differs_from_plain_text_hash() {
        let s = ClipboardSnapshot::new_html(
            "<b>hello</b>".into(),
            Some("hello".into()),
            hex::encode(Sha256::digest(b"<b>hello</b>")),
        );
        let plain = ClipboardManager::snapshot("text/plain", b"hello".to_vec());
        assert_ne!(s.sha256, plain.sha256);
        let rich = s.rich.as_ref().expect("rich text presente");
        assert_eq!(rich.sha256, s.sha256);
    }

    #[test]
    fn html_accessor_returns_content() {
        let s = ClipboardSnapshot::new_html(
            "<b>x</b>".into(),
            None,
            ClipboardManager::snapshot("text/html", b"<b>x</b>".to_vec()).sha256,
        );
        assert_eq!(s.html(), Some("<b>x</b>"));
        assert_eq!(s.text(), Some("<b>x</b>"));
    }

    #[test]
    fn headless_works_without_display() {
        let mut m = ClipboardManager::headless();
        assert_eq!(m.backend_kind(), BackendKind::Headless);
        assert!(m.read(&[MIME_TEXT]).unwrap().is_none());
        m.write_text("hello", WriteOrigin::Local).unwrap();
        m.write_html("<b>hello</b>", WriteOrigin::Local).unwrap();
    }

    #[test]
    fn shared_self_write_tracker_marks_across_managers() {
        let watcher = ClipboardManager::headless();
        let mut writer = watcher.share_self_write();
        let sha = hex::encode(Sha256::digest(b"eco"));

        writer.write_text("eco", WriteOrigin::Remote).unwrap();
        assert!(watcher.last_self_write.matches(&sha));

        writer.write_text("outro", WriteOrigin::Local).unwrap();
        assert!(watcher.last_self_write.matches(&sha));
        watcher.last_self_write.clear();
        assert!(!watcher.last_self_write.matches(&sha));
    }
}
