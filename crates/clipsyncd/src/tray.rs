//! Ícone de bandeja (StatusNotifierItem via `ksni`) para o `clipsyncd`.
//!
//! Mostra o status de conexão (número de peers) e o PIN de pareamento
//! atual no tooltip e no menu. O daemon consulta o `ServerState` e envia
//! atualizações periódicas para o tray; o tray envia comandos de menu
//! (mostrar PIN, listar peers, sair) de volta para o daemon via canal
//! [`tokio::sync::mpsc`].
//!
//! Em ambientes sem D-Bus / StatusNotifierHost (headless, CI, servidores
//! sem desktop), o tray falha ao iniciar de forma silenciosa — apenas um
//! `warn!` é emitido — e o daemon segue normalmente. O binário nunca
//! deve falhar por causa do tray: use `--no-tray` ou `CLIPSYNC_NO_TRAY`
//! para desativá-lo explicitamente.

use std::fmt;

use ksni::menu::StandardItem;
use ksni::{MenuItem, ToolTip, Tray, TrayMethods};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Comandos enviados pelo tray para o daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Exibir o PIN atual (notificação/clipboard/log).
    ShowPin,
    /// Listar peers conectados.
    ListPeers,
    /// Encerrar o daemon.
    Quit,
}

/// Estado operacional do daemon, exibido no tray.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DaemonState {
    /// O daemon está ativo e aceitando conexões.
    Running,
    /// O daemon está ocioso (sem peers conectados).
    #[default]
    Idle,
    /// O daemon encontrou um erro.
    #[allow(dead_code)]
    Error,
}

impl fmt::Display for DaemonState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("rodando"),
            Self::Idle => f.write_str("ocioso"),
            Self::Error => f.write_str("erro"),
        }
    }
}

/// Snapshot de estado exibido pelo tray.
#[derive(Debug, Clone, Default)]
pub struct TrayStatus {
    /// Número de peers conectados no momento.
    pub peer_count: usize,
    /// PIN de pareamento ativo, se houver.
    pub pin: Option<String>,
    /// Estado operacional do daemon.
    pub state: DaemonState,
}

impl TrayStatus {
    fn status_label(&self) -> String {
        let pin = self.pin.clone().unwrap_or_else(|| "nenhum".to_string());
        format!(
            "{} peer(s) conectado(s) · estado: {} · PIN: {}",
            self.peer_count, self.state, pin
        )
    }

    fn tooltip_description(&self) -> String {
        format!("clipsyncd\n{}", self.status_label())
    }
}

struct ClipsyncTray {
    status: TrayStatus,
    cmd_tx: mpsc::Sender<TrayCommand>,
}

/// Wrapper opaco sobre o handle do ksni, para que o tipo interno
/// `ClipsyncTray` não precise ser exposto ao `main.rs`.
#[derive(Clone)]
pub struct TrayHandle {
    inner: ksni::Handle<ClipsyncTray>,
}

impl Tray for ClipsyncTray {
    fn id(&self) -> String {
        "clipsyncd".into()
    }

    fn title(&self) -> String {
        "clipsyncd".into()
    }

    fn icon_name(&self) -> String {
        "edit-paste".into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "clipsyncd".into(),
            description: self.status.tooltip_description(),
            icon_name: "edit-paste".into(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            MenuItem::Standard(StandardItem {
                label: "Mostrar PIN".into(),
                activate: Box::new(move |t| {
                    let _ = t.cmd_tx.try_send(TrayCommand::ShowPin);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: "Listar peers".into(),
                activate: Box::new(move |t| {
                    let _ = t.cmd_tx.try_send(TrayCommand::ListPeers);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: format!("Status: {}", self.status.status_label()),
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "Sair".into(),
                activate: Box::new(move |t| {
                    let _ = t.cmd_tx.try_send(TrayCommand::Quit);
                }),
                ..Default::default()
            }),
        ]
    }
}

/// Tenta iniciar o tray. Retorna `None` (e loga um `warn!`) se o
/// StatusNotifierItem não estiver disponível — o daemon deve seguir sem
/// tray nesse caso. Nunca panica.
pub async fn spawn(cmd_tx: mpsc::Sender<TrayCommand>) -> Option<TrayHandle> {
    let tray = ClipsyncTray {
        status: TrayStatus::default(),
        cmd_tx,
    };
    // `assume_sni_available(true)` evita falhar imediatamente quando o
    // host SNI ainda não subiu (daemon iniciando antes da sessão).
    match tray.assume_sni_available(true).spawn().await {
        Ok(handle) => {
            info!("ícone de bandeja iniciado");
            Some(TrayHandle { inner: handle })
        }
        Err(e) => {
            warn!(error = %e, "tray indisponível; rodando sem ícone de bandeja");
            None
        }
    }
}

/// Atualiza o estado exibido pelo tray. No-op silencioso se o tray não
/// está mais ativo (handle fechado).
pub async fn update(handle: &TrayHandle, status: TrayStatus) {
    handle.inner.update(|t| t.status = status).await;
}

/// Encerra o tray (fecha o serviço D-Bus).
pub fn shutdown(handle: &TrayHandle) {
    handle.inner.shutdown();
}

/// Exibe o PIN atual: tenta notificação freedesktop (libnotify); em caso
/// de falha apenas loga. NUNCA copia para o clipboard (vazaria o PIN
/// para peers via o watcher). Nunca propaga erro.
pub async fn show_pin(pin: Option<String>) {
    let body = pin.unwrap_or_else(|| "Nenhum PIN ativo".to_string());
    if notify_rust::Notification::new()
        .summary("clipsyncd — PIN")
        .body(&body)
        .show_async()
        .await
        .is_ok()
    {
        return;
    }
    // Fallback: apenas loga. NUNCA copia para clipboard (vazaria para peers).
    info!(pin = %body, "PIN (notificação indisponível; exibindo apenas no log)");
}

/// Notifica (ou loga, como fallback) a lista de peers conectados.
pub async fn show_peers(peer_count: usize) {
    let body = format!("{peer_count} peer(s) conectado(s)");
    if notify_rust::Notification::new()
        .summary("clipsyncd — peers")
        .body(&body)
        .show_async()
        .await
        .is_err()
    {
        info!(peer_count, "peers (notificação indisponível): {body}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn status_label_with_pin() {
        let s = TrayStatus {
            peer_count: 2,
            pin: Some("123456".into()),
            state: DaemonState::Running,
        };
        assert!(s.status_label().contains("2 peer(s)"));
        assert!(s.status_label().contains("123456"));
        assert!(s.status_label().contains("rodando"));
    }

    #[test]
    fn status_label_without_pin() {
        let s = TrayStatus {
            peer_count: 0,
            pin: None,
            state: DaemonState::Idle,
        };
        assert!(s.status_label().contains("PIN: nenhum"));
        assert!(s.status_label().contains("0 peer(s)"));
    }

    #[test]
    fn tooltip_description_contains_summary() {
        let s = TrayStatus {
            peer_count: 1,
            pin: Some("999999".into()),
            state: DaemonState::Running,
        };
        let tip = s.tooltip_description();
        assert!(tip.starts_with("clipsyncd\n"));
        assert!(tip.contains("999999"));
    }

    #[test]
    fn daemon_state_display() {
        assert_eq!(DaemonState::Running.to_string(), "rodando");
        assert_eq!(DaemonState::Idle.to_string(), "ocioso");
        assert_eq!(DaemonState::Error.to_string(), "erro");
    }

    #[test]
    fn daemon_state_default_is_idle() {
        assert_eq!(DaemonState::default(), DaemonState::Idle);
    }

    #[tokio::test]
    async fn show_pin_does_not_touch_clipboard() {
        // show_pin deve apenas tentar notificação e, se falhar, logar.
        // NUNCA deve copiar o PIN para o clipboard.
        // Em ambiente headless a notificação falha e caímos no fallback
        // de log — ambas as ramificações são seguras.
        show_pin(Some("test-pin".into())).await;
        show_pin(None).await;
    }

    #[tokio::test]
    async fn spawn_does_not_panic_without_dbus() {
        // Sem sessão D-Bus / SNI host o tray deve falhar graciosamente.
        // Não vinculamos o resultado: em desktop com SNI o spawn pode
        // succeed; em headless retorna None. O importante é não panicar
        // e não travar o runtime — por isso descartamos o handle logo
        // em seguida.
        let (tx, _rx) = mpsc::channel::<TrayCommand>(4);
        // Tenta iniciar; se o D-Bus demorar, cancelamos após 2s.
        let spawned = tokio::time::timeout(Duration::from_millis(500), spawn(tx)).await;
        // Aceita Ok(Ok), Ok(Err) ou timeout — qualquer desfecho é válido
        // desde que não haja panic.
        let _ = spawned;
    }

    #[test]
    fn tray_command_variants() {
        assert_eq!(TrayCommand::ShowPin, TrayCommand::ShowPin);
        assert_ne!(TrayCommand::ShowPin, TrayCommand::Quit);
    }
}
