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

use std::process::{Command, Stdio};
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

/// Snapshot do clipboard num dado momento.
#[derive(Debug, Clone)]
pub struct ClipboardSnapshot {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

impl ClipboardSnapshot {
    pub fn text(&self) -> Option<&str> {
        if self.mime.starts_with("text/") {
            std::str::from_utf8(&self.bytes).ok()
        } else {
            None
        }
    }

    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }
}

/// Origem de uma escrita no clipboard local. Usado para anti-eco:
/// quando o daemon escreve algo vindo de um peer remoto, ele
/// marca o evento para que o watcher não reenvie aos outros peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOrigin {
    /// Escrita originada de um peer remoto. Não retransmitir.
    Remote,
    /// Escrita originada localmente (CLI, tray, etc). Transmitir.
    Local,
}

/// Eventos emitidos pelo watcher.
#[derive(Debug, Clone)]
pub enum ClipboardEvent {
    /// Conteúdo mudou (não veio de uma escrita nossa recente).
    Changed(Box<ClipboardSnapshot>),
    /// Backend de clipboard indisponível; o watcher está em modo
    /// passivo. Aplicações ainda podem chamar `write_*` manualmente.
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

/// Wrapper de alto nível sobre o backend.
#[derive(Debug)]
pub struct ClipboardManager {
    backend: BackendKind,
    /// SHA-256 do último conteúdo escrito por nós. Usado para
    /// suprimir eco no watcher.
    last_self_write: Option<String>,
    /// SHA-256 do último conteúdo visto pelo watcher. Usado para
    /// detectar mudanças.
    last_seen: Option<String>,
}

impl ClipboardManager {
    /// Constrói um novo manager, selecionando o melhor backend.
    pub fn new() -> Result<Self> {
        let backend = Self::pick_backend();
        info!(backend = backend.name(), "clipboard backend selected");
        Ok(Self {
            backend,
            last_self_write: None,
            last_seen: None,
        })
    }

    /// Constrói um manager em modo headless (sem display).
    /// Útil para testes e ambientes sem servidor gráfico.
    pub fn headless() -> Self {
        Self {
            backend: BackendKind::Headless,
            last_self_write: None,
            last_seen: None,
        }
    }

    fn pick_backend() -> BackendKind {
        // 1) Wayland (preferido)
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("WAYLAND_SOCKET").is_some()
        {
            return BackendKind::Wayland;
        }

        // 2) X11
        if std::env::var_os("DISPLAY").is_some() {
            return BackendKind::X11;
        }

        // 3) Headless
        BackendKind::Headless
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.backend
    }

    /// Verifica se as ferramentas externas existem.
    pub fn check_tools(&self) -> Result<()> {
        match self.backend {
            BackendKind::Wayland => {
                for tool in ["wl-paste", "wl-copy"] {
                    if Command::new("which").arg(tool).output().is_err() {
                        return Err(Error::Clipboard(format!(
                            "{tool} não encontrado. Instale com: sudo pacman -S wl-clipboard"
                        )));
                    }
                }
            }
            BackendKind::X11 => {
                for tool in ["xclip", "xsel"] {
                    if Command::new("which").arg(tool).output().is_err() {
                        return Err(Error::Clipboard(format!(
                            "{tool} não encontrado. Instale com: sudo pacman -S xclip"
                        )));
                    }
                }
            }
            BackendKind::Headless => {}
        }
        Ok(())
    }

    /// Lê o conteúdo atual do clipboard. Retorna `None` se vazio.
    pub fn read(&mut self, preferred_mimes: &[&str]) -> Result<Option<ClipboardSnapshot>> {
        match self.backend {
            BackendKind::Wayland => {
                // Tenta cada mime em ordem de preferência.
                for mime in preferred_mimes {
                    let out = Command::new("wl-paste")
                        .arg("--no-newline")
                        .arg("--type")
                        .arg(mime)
                        .output();
                    match out {
                        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                            return Ok(Some(Self::snapshot(mime, o.stdout)));
                        }
                        Ok(o) if o.status.success() => continue, // vazio
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
                            return Ok(Some(Self::snapshot(MIME_TEXT, o.stdout)));
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

    fn snapshot(mime: &str, bytes: Vec<u8>) -> ClipboardSnapshot {
        let sha256 = hex::encode(Sha256::digest(&bytes));
        ClipboardSnapshot {
            mime: mime.to_owned(),
            bytes,
            sha256,
        }
    }

    /// Escreve texto no clipboard.
    pub fn write_text(&mut self, text: &str, origin: WriteOrigin) -> Result<()> {
        self.write(MIME_TEXT, text.as_bytes(), origin)
    }

    /// Escreve imagem no clipboard. `mime` deve ser `image/png` ou
    /// `image/jpeg`.
    pub fn write_image(&mut self, mime: &str, bytes: &[u8], origin: WriteOrigin) -> Result<()> {
        if !mime.starts_with("image/") {
            return Err(Error::Protocol(format!("mime de imagem inválido: {mime}")));
        }
        self.write(mime, bytes, origin)
    }

    fn write(&mut self, mime: &str, bytes: &[u8], origin: WriteOrigin) -> Result<()> {
        match self.backend {
            BackendKind::Wayland => {
                let mut child = Command::new("wl-copy")
                    .arg("--type")
                    .arg(mime)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| Error::Clipboard(format!("falha spawn wl-copy: {e}")))?;

                use std::io::Write;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin
                        .write_all(bytes)
                        .map_err(|e| Error::Clipboard(format!("falha escrevendo stdin: {e}")))?;
                }
                let out = child
                    .wait_with_output()
                    .map_err(|e| Error::Clipboard(format!("falha wait wl-copy: {e}")))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(Error::Clipboard(format!(
                        "wl-copy falhou: {}",
                        stderr.trim()
                    )));
                }
            }
            BackendKind::X11 => {
                let mut child = Command::new("xclip")
                    .args(["-selection", "clipboard", "-i"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|e| Error::Clipboard(format!("falha spawn xclip: {e}")))?;

                use std::io::Write;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin
                        .write_all(bytes)
                        .map_err(|e| Error::Clipboard(format!("falha escrevendo stdin: {e}")))?;
                }
                let out = child
                    .wait_with_output()
                    .map_err(|e| Error::Clipboard(format!("falha wait xclip: {e}")))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(Error::Clipboard(format!("xclip falhou: {}", stderr.trim())));
                }
            }
            BackendKind::Headless => {
                debug!("headless: ignorando write de {} bytes", bytes.len());
            }
        }

        if origin == WriteOrigin::Remote {
            // Marca como escrita remota: o watcher deve suprimir.
            let sha = hex::encode(Sha256::digest(bytes));
            self.last_self_write = Some(sha);
        }
        Ok(())
    }

    /// Inicia um watcher que emite eventos de mudança. O canal é
    /// fechado quando o manager é droppado ou o backend falha.
    pub fn watch(self, interval: Duration) -> mpsc::Receiver<ClipboardEvent> {
        let (tx, rx) = mpsc::channel(64);
        let mut me = self;

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                ticker.tick().await;

                let snapshot = match me.read(&[MIME_TEXT, MIME_PNG, MIME_JPEG, MIME_HTML]) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "falha lendo clipboard");
                        let _ = tx.send(ClipboardEvent::BackendLost(e.to_string())).await;
                        continue;
                    }
                };

                let Some(snap) = snapshot else {
                    me.last_seen = None;
                    me.last_self_write = None;
                    continue;
                };

                // Anti-eco: se o conteúdo atual é exatamente o que
                // acabamos de escrever (porque veio de um peer remoto),
                // absorvemos e não emitimos.
                if me.last_self_write.as_deref() == Some(snap.sha256.as_str()) {
                    debug!(sha256 = %snap.sha256, "anti-echo: ignorando escrita própria");
                    me.last_self_write = None;
                    me.last_seen = Some(snap.sha256);
                    continue;
                }

                // Conteúdo igual ao último visto: nada novo.
                if me.last_seen.as_deref() == Some(snap.sha256.as_str()) {
                    continue;
                }

                me.last_seen = Some(snap.sha256.clone());
                if tx
                    .send(ClipboardEvent::Changed(Box::new(snap)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_hashes_deterministically() {
        let s1 = ClipboardManager::snapshot("text/plain", b"hello".to_vec());
        let s2 = ClipboardManager::snapshot("text/plain", b"hello".to_vec());
        let s3 = ClipboardManager::snapshot("text/plain", b"hellp".to_vec());
        assert_eq!(s1.sha256, s2.sha256);
        assert_ne!(s1.sha256, s3.sha256);
    }

    #[test]
    fn headless_works_without_display() {
        let mut m = ClipboardManager::headless();
        assert_eq!(m.backend_kind(), BackendKind::Headless);
        assert!(m.read(&[MIME_TEXT]).unwrap().is_none());
        m.write_text("hello", WriteOrigin::Local).unwrap();
    }
}
