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
    pub limits: LimitsConfig,
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
            limits: LimitsConfig::default(),
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

impl ClipboardConfig {
    /// Maior mensagem JSON que pode conter um payload de clipboard válido.
    ///
    /// O limite considera a inflação do base64 de imagens e o pior caso de
    /// escaping de strings JSON (até seis bytes por byte de entrada). Os
    /// campos de envelope não têm limites próprios, portanto reservamos uma
    /// margem fixa para metadados do protocolo.
    pub fn max_websocket_message_bytes(&self) -> usize {
        const JSON_ENVELOPE_OVERHEAD: u128 = 8 * 1024;
        let max_usize = usize::MAX as u128;
        let max_text_json = (self.max_text_bytes as u128)
            .saturating_mul(6)
            .saturating_add(JSON_ENVELOPE_OVERHEAD)
            .min(max_usize);
        let max_image_json = ((self.max_image_bytes as u128).saturating_add(2) / 3)
            .saturating_mul(4)
            .saturating_add(JSON_ENVELOPE_OVERHEAD)
            .min(max_usize);

        max_text_json.max(max_image_json) as usize
    }
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

/// Limites de admissao por endereco de origem. Zero desabilita o limite.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_connections: usize,
    pub messages_per_minute: u32,
    pub bytes_per_minute: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 256,
            messages_per_minute: 120,
            bytes_per_minute: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Transporte obrigatório por padrão. `plaintext_legacy` existe apenas
    /// para interoperabilidade explícita com clients v0.1.
    pub transport: Transport,
    /// Aceita apenas conexões cujo endereço de origem seja local (loopback,
    /// privado ou link-local). Isto não prova a mesma sub-rede nem inspeciona
    /// SSID.
    pub local_only: bool,
    /// Timeout de pareamento (segundos).
    pub pairing_timeout_secs: u64,
    /// Paths opcionais da identidade TLS persistente.
    pub tls_cert_path: Option<String>,
    pub tls_key_path: Option<String>,
    /// Se informado, impede iniciar com outro certificado.
    pub tls_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Tls,
    PlaintextLegacy,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            transport: Transport::Tls,
            local_only: true,
            pairing_timeout_secs: 120,
            tls_cert_path: None,
            tls_key_path: None,
            tls_fingerprint: None,
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

        let (mut cfg, mut needs_save) = if !path.exists() {
            (Config::default(), true)
        } else {
            let contents = std::fs::read_to_string(&path)
                .map_err(|e| Error::Config(format!("falha lendo {path:?}: {e}")))?;
            (
                toml::from_str(&contents)
                    .map_err(|e| Error::Config(format!("TOML inválido em {path:?}: {e}")))?,
                false,
            )
        };

        // O daemon precisa de um device_id próprio estável (origin do
        // anti-eco). Gera e persiste se ausente (config antiga).
        if cfg.device_id.is_none() {
            cfg.device_id = Some(DeviceId::new());
            needs_save = true;
        }
        if needs_save {
            cfg.save(&path)?;
            info!(path = %path.display(), "config criada ou atualizada atomicamente");
        }
        Ok(cfg)
    }

    /// Carrega o arquivo e aplica apenas variaveis operacionais documentadas.
    /// O arquivo continua sendo a fonte de defaults, o que torna o ambiente
    /// adequado para containers sem aceitar TOML arbitrario via env.
    pub fn load_or_default_env(path: Option<&std::path::Path>) -> Result<Self> {
        let env_path = std::env::var_os("CLIPSYNC_CONFIG");
        let path = path.or_else(|| env_path.as_deref().map(std::path::Path::new));
        let mut cfg = Self::load_or_default(path)?;
        apply_env(&mut cfg)?;
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
        crate::persistence::atomic_write(path, toml_str.as_bytes())
            .map_err(|e| Error::Config(format!("falha salvando {path:?}: {e}")))?;
        Ok(())
    }
}

fn apply_env(cfg: &mut Config) -> Result<()> {
    if let Some(value) = std::env::var_os("CLIPSYNC_BIND") {
        cfg.bind = value
            .into_string()
            .map_err(|_| Error::Config("CLIPSYNC_BIND nao e UTF-8".into()))?;
    }
    if let Some(value) = std::env::var_os("CLIPSYNC_NAME") {
        cfg.name = value
            .into_string()
            .map_err(|_| Error::Config("CLIPSYNC_NAME nao e UTF-8".into()))?;
    }
    if let Some(value) = std::env::var_os("CLIPSYNC_DISCOVERY_ENABLE_MDNS") {
        cfg.discovery.enable_mdns = parse_env(&value, "CLIPSYNC_DISCOVERY_ENABLE_MDNS")?;
    }
    if let Some(value) = std::env::var_os("CLIPSYNC_SECURITY_TRANSPORT") {
        cfg.security.transport = match value.to_str() {
            Some("tls") => crate::config::Transport::Tls,
            Some("plaintext_legacy") => crate::config::Transport::PlaintextLegacy,
            _ => {
                return Err(Error::Config(
                    "CLIPSYNC_SECURITY_TRANSPORT deve ser tls ou plaintext_legacy".into(),
                ))
            }
        };
    }
    if let Some(value) = std::env::var_os("CLIPSYNC_SECURITY_LOCAL_ONLY") {
        cfg.security.local_only = parse_env(&value, "CLIPSYNC_SECURITY_LOCAL_ONLY")?;
    }
    if let Some(value) = std::env::var_os("CLIPSYNC_LIMITS_MAX_CONNECTIONS") {
        cfg.limits.max_connections = parse_env(&value, "CLIPSYNC_LIMITS_MAX_CONNECTIONS")?;
    }
    if let Some(value) = std::env::var_os("CLIPSYNC_LIMITS_MESSAGES_PER_MINUTE") {
        cfg.limits.messages_per_minute = parse_env(&value, "CLIPSYNC_LIMITS_MESSAGES_PER_MINUTE")?;
    }
    if let Some(value) = std::env::var_os("CLIPSYNC_LIMITS_BYTES_PER_MINUTE") {
        cfg.limits.bytes_per_minute = parse_env(&value, "CLIPSYNC_LIMITS_BYTES_PER_MINUTE")?;
    }
    Ok(())
}

fn parse_env<T: std::str::FromStr>(value: &std::ffi::OsStr, name: &str) -> Result<T> {
    value
        .to_str()
        .ok_or_else(|| Error::Config(format!("{name} nao e UTF-8")))?
        .parse()
        .map_err(|_| Error::Config(format!("{name} tem valor invalido")))
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
    fn websocket_limit_covers_clipboard_envelopes() {
        let cfg = ClipboardConfig::default();

        assert_eq!(
            cfg.max_websocket_message_bytes(),
            6 * 16 * 1024 * 1024 + 8 * 1024
        );
        assert!(cfg.max_websocket_message_bytes() > 25 * 1024 * 1024 * 4 / 3);
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
    fn load_or_default_rejects_invalid_toml() {
        let dir =
            std::env::temp_dir().join(format!("clipsync-test-invalid-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "[clipboard\n").unwrap();

        let result = Config::load_or_default(Some(&path));

        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
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
