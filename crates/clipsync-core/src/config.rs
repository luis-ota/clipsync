//! Configuração persistente do daemon e da crate core.
//!
//! Formato: TOML. Localização padrão:
//!
//! * Linux: `$XDG_CONFIG_HOME/clipsync/config.toml`
//!   (geralmente `~/.config/clipsync/config.toml`)
//! * Fallback: `~/.config/clipsync/config.toml`

use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::{Error, Result};
use crate::protocol::DeviceId;
use crate::server::{DEFAULT_BIND, DEFAULT_NAME};
use crate::SERVICE_TYPE;

/// Qual projeto as pastas de config usam.
fn qualifier() -> String {
    "dev".into()
}

/// Diretório base de config (~/.config/clipsync).
pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(&qualifier(), "clipsync", "clipsync")
        .ok_or_else(|| Error::Config("não foi possível localizar diretório de config".into()))?;
    Ok(dirs.config_dir().to_path_buf())
}

/// Path do arquivo de config padrão.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Path do arquivo de devices confiados.
pub fn trusted_devices_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("trusted.toml"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Endereço de bind do servidor WebSocket. Para aceitar conexões
    /// de outros devices na LAN, use `0.0.0.0:8765`.
    pub bind: String,
    /// Nome amigável do PC anunciado via mDNS e exibido no cliente.
    pub name: String,
    /// Device_id próprio do daemon. Gerado e persistido no primeiro
    /// `load_or_default`; usado como `origin` estável (anti-eco) nas
    /// mensagens emitidas pelo watcher. `None` apenas em configs
    /// construídas diretamente sem passar por `load_or_default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    pub discovery: DiscoveryConfig,
    pub clipboard: ClipboardConfig,
    pub security: SecurityConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.into(),
            name: DEFAULT_NAME.into(),
            device_id: None,
            discovery: DiscoveryConfig::default(),
            clipboard: ClipboardConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Habilita anúncio mDNS.
    pub enable_mdns: bool,
    /// Tipo de serviço DNS-SD.
    pub service_type: String,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enable_mdns: true,
            service_type: SERVICE_TYPE.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Sincronizar texto plain.
    pub sync_text: bool,
    /// Sincronizar imagens (png/jpeg).
    pub sync_images: bool,
    /// Sincronizar rich text (html).
    pub sync_html: bool,
    /// Sincronizar arquivos (v0.3).
    pub sync_files: bool,
    /// Backend: "auto" | "wayland" | "x11".
    pub backend: String,
    /// Limite máximo de tamanho (bytes) para imagens.
    pub max_image_bytes: u64,
    /// Limite máximo de tamanho (bytes) para texto e HTML.
    pub max_text_bytes: u64,
    /// Intervalo de poll do watcher (ms).
    pub poll_interval_ms: u64,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            sync_text: true,
            sync_images: true,
            sync_html: false,
            sync_files: false,
            backend: "auto".into(),
            max_image_bytes: 25 * 1024 * 1024,
            max_text_bytes: 16 * 1024 * 1024,
            poll_interval_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Aceita apenas conexões cujo endereço de origem seja local (loopback,
    /// privado ou link-local). Isto não prova a mesma sub-rede nem inspeciona
    /// SSID.
    pub local_only: bool,
    /// Timeout de pareamento (segundos).
    pub pairing_timeout_secs: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            local_only: true,
            pairing_timeout_secs: 120,
        }
    }
}

impl Config {
    /// Carrega a config de um path, criando um default e salvando
    /// se o arquivo não existir.
    pub fn load_or_default(path: Option<&std::path::Path>) -> Result<Self> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };

        let mut cfg = if !path.exists() {
            let cfg = Config::default();
            cfg.save(&path)?;
            info!(path = %path.display(), "config padrão criada");
            cfg
        } else {
            let contents = std::fs::read_to_string(&path)
                .map_err(|e| Error::Config(format!("falha lendo {path:?}: {e}")))?;
            toml::from_str(&contents)
                .map_err(|e| Error::Config(format!("TOML inválido em {path:?}: {e}")))?
        };

        // O daemon precisa de um device_id próprio estável (origin do
        // anti-eco). Gera e persiste se ausente (config antiga).
        if cfg.device_id.is_none() {
            cfg.device_id = Some(DeviceId::new());
            cfg.save(&path)?;
            info!(path = %path.display(), "device_id do daemon gerado e persistido");
        }
        Ok(cfg)
    }

    /// Salva a config em disco.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("falha criando {parent:?}: {e}")))?;
        }
        let toml_str = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("falha serializando config: {e}")))?;
        std::fs::write(path, toml_str)
            .map_err(|e| Error::Config(format!("falha salvando {path:?}: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_toml() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.bind, cfg.bind);
        assert_eq!(
            back.clipboard.max_image_bytes,
            cfg.clipboard.max_image_bytes
        );
    }

    #[test]
    fn load_or_default_creates_file() {
        let dir = std::env::temp_dir().join(format!("clipsync-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        let cfg = Config::load_or_default(Some(&path)).unwrap();
        assert!(path.exists());
        assert_eq!(cfg.bind, "0.0.0.0:8765");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_or_default_persists_stable_daemon_device_id() {
        let dir = std::env::temp_dir().join(format!("clipsync-test-{}", std::process::id()));
        let path = dir.join("config-device-id.toml");
        let _ = std::fs::remove_file(&path);

        let first = Config::load_or_default(Some(&path)).unwrap();
        let id = first
            .device_id
            .clone()
            .expect("device_id gerado no primeiro load");
        assert_eq!(id.as_str().len(), 36, "device_id é uuid");

        // Recarregar não pode gerar um id novo: origin deve ser estável.
        let second = Config::load_or_default(Some(&path)).unwrap();
        assert_eq!(second.device_id, Some(id));
        let _ = std::fs::remove_file(&path);
    }
}
