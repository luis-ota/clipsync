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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub discovery: DiscoveryConfig,
    pub clipboard: ClipboardConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Endereço de bind do servidor WebSocket. Para aceitar conexões
    /// de outros devices na LAN, use `0.0.0.0:8765`.
    pub bind: String,
    /// Nome amigável do PC anunciado via mDNS e exibido no cliente.
    pub name: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8765".into(),
            name: "linux-desktop".into(),
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
            poll_interval_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Restringe pareamento a dispositivos na mesma sub-rede.
    /// Em `local_only = false`, aceita dispositivos de qualquer rede.
    pub local_only: bool,
    /// Timeout de pareamento (segundos).
    pub pairing_timeout_secs: u64,
    /// Nome de rede (SSID) permitido. Vazio = qualquer rede.
    pub allowed_ssid: String,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            local_only: true,
            pairing_timeout_secs: 120,
            allowed_ssid: String::new(),
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

        if !path.exists() {
            let cfg = Config::default();
            cfg.save(&path)?;
            info!(path = %path.display(), "config padrão criada");
            return Ok(cfg);
        }

        let contents = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("falha lendo {path:?}: {e}")))?;
        let cfg: Config = toml::from_str(&contents)
            .map_err(|e| Error::Config(format!("TOML inválido em {path:?}: {e}")))?;
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
        assert_eq!(back.server.bind, cfg.server.bind);
        assert_eq!(back.clipboard.max_image_bytes, cfg.clipboard.max_image_bytes);
    }

    #[test]
    fn load_or_default_creates_file() {
        let dir = std::env::temp_dir().join(format!("clipsync-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);
        let cfg = Config::load_or_default(Some(&path)).unwrap();
        assert!(path.exists());
        assert_eq!(cfg.server.bind, "0.0.0.0:8765");
        let _ = std::fs::remove_file(&path);
    }
}
