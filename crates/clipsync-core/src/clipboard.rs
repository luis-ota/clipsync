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
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};

/// MIME types suportados por este daemon.
pub const MIME_TEXT: &str = "text/plain;charset=utf-8";
pub const MIME_TEXT_PLAIN: &str = "text/plain";
pub const MIME_PNG: &str = "image/png";
pub const MIME_JPEG: &str = "image/jpeg";
pub const MIME_HTML: &str = "text/html";

/// Janela de debounce: mudanças dentro desse intervalo são
/// coalescidas em um único evento (processa apenas a última).
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Snapshot do clipboard num dado momento.
///
/// O campo `html` carrega o conteúdo rich text (text/html) quando
/// disponível no clipboard, além do conteúdo primário em `bytes`.
/// Em Wayland isso vem de `wl-paste --type text/html`; em X11 e
/// headless fica sempre `None` (sem suporte confiável a MIME seletivo).
#[derive(Debug, Clone)]
pub struct ClipboardSnapshot {
    pub mime: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    /// Conteúdo HTML quando o clipboard oferece `text/html` além do
    /// texto plain. `None` em backends sem suporte a MIME seletivo.
    pub html: Option<String>,
    /// SHA-256 (hex) do conteúdo em `html`, quando presente. Usado
    /// para dedup/anti-eco do rich text independente do texto plain.
    pub html_sha256: Option<String>,
}

impl ClipboardSnapshot {
    pub fn text(&self) -> Option<&str> {
        if self.mime.starts_with("text/") {
            std::str::from_utf8(&self.bytes).ok()
        } else {
            None
        }
    }

    /// Conteúdo HTML quando disponível.
    pub fn html(&self) -> Option<&str> {
        self.html.as_deref()
    }

    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Assinatura usada pelo watcher para detectar mudanças. Combina
    /// o hash do conteúdo primário com o hash do HTML (se houver),
    /// de forma que alterações só de formatação também disparam.
    pub fn fingerprint(&self) -> String {
        match &self.html_sha256 {
            Some(h) => format!("{}|{}", self.sha256, h),
            None => self.sha256.clone(),
        }
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
    ///
    /// Em Wayland, quando o conteúdo primário é texto plain, também
    /// tenta ler `text/html` via `wl-paste --type text/html` e anexa
    /// ao snapshot em [`ClipboardSnapshot::html`]. Em X11/headless o
    /// HTML fica sempre `None` (logado em debug).
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
                            let mut snap = Self::snapshot(mime, o.stdout);
                            Self::attach_html(&mut snap);
                            return Ok(Some(snap));
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
                            let snap = Self::snapshot(MIME_TEXT, o.stdout);
                            // xclip não expõe MIME seletivo de forma
                            // confiável: não lemos HTML aqui.
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

    /// Anexa o conteúdo `text/html` ao snapshot quando disponível.
    /// Se o mime primário já for HTML, espelha `bytes` em `html`.
    fn attach_html(snap: &mut ClipboardSnapshot) {
        if snap.mime == MIME_HTML {
            let html = String::from_utf8_lossy(&snap.bytes).into_owned();
            snap.html_sha256 = Some(snap.sha256.clone());
            snap.html = Some(html);
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
                snap.html_sha256 = Some(hex::encode(Sha256::digest(html.as_bytes())));
                snap.html = Some(html);
            }
            _ => debug!("sem conteúdo text/html no clipboard"),
        }
    }

    fn snapshot(mime: &str, bytes: Vec<u8>) -> ClipboardSnapshot {
        let sha256 = hex::encode(Sha256::digest(&bytes));
        ClipboardSnapshot {
            mime: mime.to_owned(),
            bytes,
            sha256,
            html: None,
            html_sha256: None,
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

    /// Escreve rich text (HTML) no clipboard via `wl-copy -t text/html`
    /// (Wayland). Em X11, grava como texto (xclip não suporta MIME
    /// seletivo). Em headless, é no-op (log debug).
    pub fn write_html(&mut self, html: &str, origin: WriteOrigin) -> Result<()> {
        self.write(MIME_HTML, html.as_bytes(), origin)
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
    ///
    /// Em Wayland, preferiu-se o modo event-driven via
    /// `wl-paste --watch` (subprocesso bloqueante que emite uma
    /// notificação por mudança), eliminando o polling ativo. X11 e
    /// headless usam polling (com `interval`) como fallback. Se
    /// `wl-paste --watch` falhar ao subir, cai para polling com aviso.
    ///
    /// Múltiplas mudanças dentro de [`DEBOUNCE`] são coalescidas em
    /// um único [`ClipboardEvent::Changed`] com o último snapshot.
    pub fn watch(self, interval: Duration) -> mpsc::Receiver<ClipboardEvent> {
        let (tx, rx) = mpsc::channel(64);
        let backend = self.backend;

        tokio::spawn(async move {
            let mut me = self;
            if backend == BackendKind::Wayland && wl_paste_exists() {
                match run_event_driven(&mut me, tx.clone()).await {
                    Ok(()) => return,
                    Err(e) => {
                        warn!(error = %e, "wl-paste --watch falhou; caindo para polling");
                        let _ = tx.send(ClipboardEvent::BackendLost(e.to_string())).await;
                    }
                }
            }
            run_polling(&mut me, tx, interval).await;
        });

        rx
    }
}

/// Verifica se o binário `wl-paste` está disponível no PATH.
fn wl_paste_exists() -> bool {
    Command::new("which")
        .arg("wl-paste")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Lê o clipboard e devolve `Some(snapshot)` apenas quando o
/// conteúdo é novo (ou seja, diferente do último visto e não é
/// eco de uma escrita nossa). Atualiza `last_seen`/`last_self_write`
/// conforme necessário. É a lógica compartilhada entre os modos
/// event-driven e polling.
fn read_for_emit(me: &mut ClipboardManager) -> Result<Option<ClipboardSnapshot>> {
    let snapshot = me.read(&[MIME_TEXT, MIME_PNG, MIME_JPEG, MIME_HTML])?;
    let Some(snap) = snapshot else {
        me.last_seen = None;
        me.last_self_write = None;
        return Ok(None);
    };

    // Anti-eco: se o conteúdo atual é exatamente o que acabamos de
    // escrever (porque veio de um peer remoto), absorvemos e não
    // emitimos.
    if me.last_self_write.as_deref() == Some(snap.sha256.as_str()) {
        debug!(sha256 = %snap.sha256, "anti-echo: ignorando escrita própria");
        me.last_self_write = None;
        me.last_seen = Some(snap.sha256);
        return Ok(None);
    }

    // Conteúdo igual ao último visto: nada novo.
    if me.last_seen.as_deref() == Some(snap.sha256.as_str()) {
        return Ok(None);
    }

    me.last_seen = Some(snap.sha256.clone());
    Ok(Some(snap))
}

/// Debouncer puro: coalescea uma rajada de snapshots em um único
/// evento, retendo apenas o último. A janela é [`DEBOUNCE`].
///
/// Independente de tokio/display — testável com lógica pura.
#[derive(Debug, Default)]
struct Debouncer {
    pending: Option<ClipboardSnapshot>,
    deadline: Option<Instant>,
}

impl Debouncer {
    /// Registra/reescreve o snapshot pendente e rearma a janela.
    fn feed_at(&mut self, snap: ClipboardSnapshot, now: Instant) {
        self.pending = Some(snap);
        self.deadline = Some(now + DEBOUNCE);
    }

    /// Devolve o snapshot pendente se a janela já expirou; caso
    /// contrário, mantém o estado e retorna `None`.
    fn fire_at(&mut self, now: Instant) -> Option<ClipboardSnapshot> {
        match (self.pending.take(), self.deadline) {
            (Some(snap), Some(dl)) if now >= dl => {
                self.deadline = None;
                Some(snap)
            }
            (snap, _) => {
                self.pending = snap;
                None
            }
        }
    }

    fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

/// Watcher event-driven para Wayland: spawn `wl-paste --watch cat`
/// (subprocesso bloqueante que escreve no stdout a cada mudança de
/// clipboard) e processa cada notificação através do debouncer.
async fn run_event_driven(
    me: &mut ClipboardManager,
    tx: mpsc::Sender<ClipboardEvent>,
) -> Result<()> {
    let mut child = tokio::process::Command::new("wl-paste")
        .arg("--watch")
        .arg("cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::Clipboard(format!("falha spawn wl-paste --watch: {e}")))?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Clipboard("wl-paste --watch sem stdout".into()))?;

    let mut buf = [0u8; 4096];
    let mut debouncer = Debouncer::default();
    // Sleep re-armável: quando há pending, expira em `DEBOUNCE`.
    let debounce_timer = tokio::time::sleep(DEBOUNCE);
    tokio::pin!(debounce_timer);
    let mut timer_armed = false;

    loop {
        tokio::select! {
            r = stdout.read(&mut buf) => {
                match r {
                    Ok(0) => {
                        let _ = child.start_kill();
                        return Err(Error::Clipboard(
                            "wl-paste --watch encerrou (EOF)".into(),
                        ));
                    }
                    Ok(_) => {
                        match read_for_emit(me) {
                            Ok(Some(snap)) => {
                                debouncer.feed_at(snap, Instant::now());
                                debounce_timer
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + DEBOUNCE);
                                timer_armed = true;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(error = %e, "falha lendo clipboard (event-driven)");
                                let _ = tx
                                    .send(ClipboardEvent::BackendLost(e.to_string()))
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = child.start_kill();
                        return Err(Error::Clipboard(format!(
                            "wl-paste --watch read: {e}"
                        )));
                    }
                }
            }
            _ = &mut debounce_timer, if timer_armed && debouncer.has_pending() => {
                if let Some(snap) = debouncer.fire_at(Instant::now()) {
                    timer_armed = false;
                    if tx
                        .send(ClipboardEvent::Changed(Box::new(snap)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }

    let _ = child.start_kill();
    Ok(())
}

/// Watcher por polling (fallback X11/headless ou falha do
/// `wl-paste --watch`). Mesmo comportamento de antes, agora com
/// debounce de rajadas.
async fn run_polling(
    me: &mut ClipboardManager,
    tx: mpsc::Sender<ClipboardEvent>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // tick inicial imediato (comportamento de tokio::interval).
    let mut debouncer = Debouncer::default();
    let debounce_timer = tokio::time::sleep(DEBOUNCE);
    tokio::pin!(debounce_timer);
    let mut timer_armed = false;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match read_for_emit(me) {
                    Ok(s) => {
                        if let Some(snap) = s {
                            debouncer.feed_at(snap, Instant::now());
                            debounce_timer
                                .as_mut()
                                .reset(tokio::time::Instant::now() + DEBOUNCE);
                            timer_armed = true;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "falha lendo clipboard");
                        let _ = tx.send(ClipboardEvent::BackendLost(e.to_string())).await;
                    }
                }
            }
            _ = &mut debounce_timer, if timer_armed && debouncer.has_pending() => {
                if let Some(snap) = debouncer.fire_at(Instant::now()) {
                    timer_armed = false;
                    if tx
                        .send(ClipboardEvent::Changed(Box::new(snap)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
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
        // Snapshots sem HTML têm fingerprint igual ao sha256.
        assert_eq!(s1.fingerprint(), s1.sha256);
        assert!(s1.html.is_none());
        assert!(s1.html_sha256.is_none());
    }

    #[test]
    fn fingerprint_combines_text_and_html() {
        let mut s = ClipboardManager::snapshot("text/plain", b"hello".to_vec());
        s.html = Some("<b>hello</b>".into());
        s.html_sha256 = Some(hex::encode(Sha256::digest(b"<b>hello</b>")));
        let fp = s.fingerprint();
        assert!(fp.starts_with(&s.sha256));
        assert!(fp.contains('|'));
        assert_ne!(fp, s.sha256);
    }

    #[test]
    fn html_accessor_returns_content() {
        let mut s = ClipboardManager::snapshot("text/html", b"<b>x</b>".to_vec());
        s.html = Some("<b>x</b>".into());
        s.html_sha256 = Some(s.sha256.clone());
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
    fn debouncer_coalesces_bursts_to_latest() {
        let mut d = Debouncer::default();
        let t0 = Instant::now();
        let s1 = ClipboardManager::snapshot("text/plain", b"a".to_vec());
        let s2 = ClipboardManager::snapshot("text/plain", b"ab".to_vec());
        let s3 = ClipboardManager::snapshot("text/plain", b"abc".to_vec());

        // Rajada: 3 mudanças dentro de <300ms.
        d.feed_at(s1, t0);
        d.feed_at(s2, t0 + Duration::from_millis(100));
        d.feed_at(s3, t0 + Duration::from_millis(200));

        // Antes da janela expirar (300ms após a última feed): nada.
        assert!(d.fire_at(t0 + Duration::from_millis(499)).is_none());
        assert!(d.has_pending());

        // Exatamente após a janela: emite apenas o último snapshot.
        let emitted = d
            .fire_at(t0 + Duration::from_millis(500))
            .expect("emite o último snapshot da rajada");
        assert_eq!(emitted.text(), Some("abc"));

        // Após emitir, fica vazio.
        assert!(!d.has_pending());
        assert!(d.fire_at(t0 + Duration::from_millis(9999)).is_none());
    }

    #[test]
    fn debouncer_rearms_on_new_feed() {
        let mut d = Debouncer::default();
        let t0 = Instant::now();
        let s1 = ClipboardManager::snapshot("text/plain", b"first".to_vec());
        let s2 = ClipboardManager::snapshot("text/plain", b"second".to_vec());

        d.feed_at(s1, t0);
        // Segunda feed bem depois reescreve o pending e rearma.
        d.feed_at(s2, t0 + Duration::from_millis(400));
        // Ainda não (300ms após a segunda feed).
        assert!(d.fire_at(t0 + Duration::from_millis(699)).is_none());
        let emitted = d
            .fire_at(t0 + Duration::from_millis(700))
            .expect("emite o segundo snapshot");
        assert_eq!(emitted.text(), Some("second"));
    }

    #[test]
    fn read_for_emit_dedups_in_headless() {
        let mut m = ClipboardManager::headless();
        // Headless sempre lê None: nenhum evento a emitir.
        assert!(read_for_emit(&mut m).unwrap().is_none());
        assert!(read_for_emit(&mut m).unwrap().is_none());
    }
}
