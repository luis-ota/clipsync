//! Watcher subsystem for clipboard changes.
//!
//! Provides two monitoring strategies:
//!
//! - **Event-driven**: spawns `wl-paste --watch cat` (Wayland only).
//! - **Polling**: periodic reads at a fixed interval (X11/headless
//!   fallback).
//!
//! Both strategies share a [`DebounceEmitter`] that coalesces rapid
//! changes within [`DEBOUNCE`] into a single [`ClipboardEvent`].

use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{
    ClipboardEvent, ClipboardManager, ClipboardSnapshot, MIME_HTML, MIME_JPEG, MIME_PNG, MIME_TEXT,
};
use crate::error::{Error, Result};

/// Janela de debounce: mudanças dentro desse intervalo são
/// coalescidas em um único evento (processa apenas a última).
const DEBOUNCE: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Verifica se o binário `wl-paste` está disponível no PATH.
/// Delega para [`super::detect_clipboard_tools`] — fonte única de verdade.
pub(super) fn wl_paste_exists() -> bool {
    super::detect_clipboard_tools().wl_paste
}

/// Lê o clipboard e devolve `Some(snapshot)` apenas quando o
/// conteúdo é novo (ou seja, diferente do último visto e não é
/// eco de uma escrita nossa). Atualiza `last_seen`/`last_self_write`
/// conforme necessário. É a lógica compartilhada entre os modos
/// event-driven e polling.
pub(super) fn read_for_emit(me: &mut ClipboardManager) -> Result<Option<ClipboardSnapshot>> {
    let snapshot = me.read(&[MIME_TEXT, MIME_PNG, MIME_JPEG, MIME_HTML])?;
    let Some(snap) = snapshot else {
        me.last_seen = None;
        me.last_self_write.clear();
        return Ok(None);
    };

    // Anti-eco: se o conteúdo atual é exatamente o que acabamos de
    // escrever (porque veio de um peer remoto), absorvemos e não
    // emitimos.
    if me.last_self_write.matches(&snap.sha256) {
        debug!(sha256 = %snap.sha256, "anti-echo: ignorando escrita própria");
        me.last_self_write.clear();
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

// ---------------------------------------------------------------------------
// Debouncer + shared emitter
// ---------------------------------------------------------------------------

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

/// Encapsula o estado de debounce compartilhado entre os watchers
/// event-driven e polling.
///
/// Agrupa o [`Debouncer`] e a flag `timer_armed`, expondo uma
/// interface de alto nível que elimina a duplicação dos loops
/// de `tokio::select!`:
///
/// - [`feed`](Self::feed) — alimenta o debouncer e rearma o timer.
/// - [`is_ready`](Self::is_ready) — guarda do branch de debounce no
///   `select!`.
/// - [`try_fire`](Self::try_fire) — emite o snapshot quando a janela
///   expira.
struct DebounceEmitter {
    debouncer: Debouncer,
    timer_armed: bool,
}

impl DebounceEmitter {
    fn new() -> Self {
        Self {
            debouncer: Debouncer::default(),
            timer_armed: false,
        }
    }

    /// Alimenta o debouncer com um novo snapshot e rearma o timer
    /// de debounce.
    fn feed(&mut self, snap: ClipboardSnapshot, timer: Pin<&mut tokio::time::Sleep>) {
        self.debouncer.feed_at(snap, Instant::now());
        timer.reset(tokio::time::Instant::now() + DEBOUNCE);
        self.timer_armed = true;
    }

    /// `true` quando o branch de debounce do `select!` deve estar
    /// ativo (timer armado e há snapshot pendente).
    fn is_ready(&self) -> bool {
        self.timer_armed && self.debouncer.has_pending()
    }

    /// Dispara o debouncer se a janela expirou. Retorna o snapshot a
    /// emitir, ou `None` se ainda não expirou.
    fn try_fire(&mut self) -> Option<ClipboardSnapshot> {
        if let Some(snap) = self.debouncer.fire_at(Instant::now()) {
            self.timer_armed = false;
            Some(snap)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Watchers
// ---------------------------------------------------------------------------

/// Watcher event-driven para Wayland: spawn `wl-paste --watch cat`
/// (subprocesso bloqueante que escreve no stdout a cada mudança de
/// clipboard) e processa cada notificação através do debouncer.
pub(super) async fn run_event_driven(
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
    let mut emitter = DebounceEmitter::new();
    let debounce_timer = tokio::time::sleep(DEBOUNCE);
    tokio::pin!(debounce_timer);

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
                                emitter.feed(snap, debounce_timer.as_mut());
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
            _ = &mut debounce_timer, if emitter.is_ready() => {
                if let Some(snap) = emitter.try_fire() {
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
pub(super) async fn run_polling(
    me: &mut ClipboardManager,
    tx: mpsc::Sender<ClipboardEvent>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // tick inicial imediato (comportamento de tokio::interval).
    let mut emitter = DebounceEmitter::new();
    let debounce_timer = tokio::time::sleep(DEBOUNCE);
    tokio::pin!(debounce_timer);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match read_for_emit(me) {
                    Ok(s) => {
                        if let Some(snap) = s {
                            emitter.feed(snap, debounce_timer.as_mut());
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "falha lendo clipboard");
                        let _ = tx.send(ClipboardEvent::BackendLost(e.to_string())).await;
                    }
                }
            }
            _ = &mut debounce_timer, if emitter.is_ready() => {
                if let Some(snap) = emitter.try_fire() {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
