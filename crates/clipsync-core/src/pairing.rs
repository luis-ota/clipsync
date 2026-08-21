//! Pareamento por PIN (estilo "digite o código na tela").
//!
//! O servidor gera um PIN de 6 dígitos com expiração e o exibe
//! localmente no daemon. O cliente submete o PIN digitado pelo usuário
//! junto com o `challenge_id` e o nonce recebidos no desafio. O PIN
//! nunca atravessa o fio: a resposta do desafio carrega apenas o
//! `challenge_id`, o nonce e a expiração. Se válido, o device é marcado
//! como pareado e passa a ser confiado nas conexões seguintes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::error::{Error, Result};
use crate::protocol::{DeviceId, PairFailReason};

/// Duração padrão de um PIN.
pub const DEFAULT_PIN_TTL: Duration = Duration::from_secs(120);
/// Máximo de tentativas por desafio.
pub const MAX_ATTEMPTS: u8 = 5;

/// Erros do fluxo de pareamento, incluindo falhas ao persistir confiança.
#[derive(Debug)]
pub enum PairingError {
    Invalid(PairFailReason),
    Store(Error),
}

impl From<Error> for PairingError {
    fn from(error: Error) -> Self {
        Self::Store(error)
    }
}

/// Um desafio de pareamento em andamento.
#[derive(Debug, Clone)]
pub struct PairChallenge {
    /// Identificador do desafio, ecoado pelo cliente em `pair_submit`.
    pub challenge_id: String,
    /// PIN numérico de 6 dígitos (string). Nunca vai para o wire;
    /// é exibido localmente no daemon.
    pub code: String,
    /// Nonce aleatório de 16 bytes hex, ecoado pelo cliente.
    pub nonce: String,
    /// Quando o desafio expira.
    pub expires_at: Instant,
    /// Tentativas restantes.
    pub attempts_left: u8,
    /// Metadata do device que solicitou o pareamento.
    pub device_name: String,
}

/// Registro de um device pareado e confiado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub id: DeviceId,
    pub name: String,
    pub kind: String,
    /// Unix timestamp (s) da última conexão.
    pub last_seen: i64,
    /// Unix timestamp (s) do pareamento.
    pub paired_at: i64,
    /// Se true, conexões deste device são aceitas sem PIN.
    pub trusted: bool,
}

/// Store persistido de devices confiados (`trusted.toml`).
///
/// Formato: TOML com um array `devices` de [`TrustedDevice`]. O daemon
/// ainda mantém os confiados em memória; este store é o formato de
/// intercâmbio usado pelas ferramentas offline do CLI (`list-peers`,
/// `untrust`) e pode ser usado pelo daemon para persistir ao encerrar.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustedStore {
    pub devices: Vec<TrustedDevice>,
}

impl TrustedStore {
    /// Carrega o store de um path TOML. Arquivo ausente => store vazio.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::Config(format!("falha lendo {path:?}: {e}")))?;
        toml::from_str(&contents)
            .map_err(|e| crate::Error::Config(format!("TOML inválido em {path:?}: {e}")))
    }

    /// Carrega do path padrão de trusted devices.
    pub fn load_default() -> Result<Self> {
        Self::load(&crate::config::trusted_devices_path()?)
    }

    /// Salva o store em um path TOML (cria os diretórios pais).
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| crate::Error::Config(format!("falha criando {parent:?}: {e}")))?;
        }
        let toml_str = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::Config(format!("falha serializando trusted: {e}")))?;
        std::fs::write(path, toml_str)
            .map_err(|e| crate::Error::Config(format!("falha salvando {path:?}: {e}")))?;
        Ok(())
    }

    /// Lista os devices confiados ordenados por `last_seen`
    /// (mais recente primeiro), espelhando [`PairingManager::trusted_devices`].
    pub fn trusted_devices(&self) -> Vec<&TrustedDevice> {
        let mut v: Vec<_> = self.devices.iter().collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.last_seen));
        v
    }

    /// Remove o device com o id dado. Retorna true se algum foi removido.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| d.id.as_str() != id);
        self.devices.len() != before
    }
}

/// Gerencia desafios de pareamento ativos e o store de confiados.
///
/// Invariante: existe no máximo UM desafio de pareamento ativo por vez.
/// `start_challenge` invalida qualquer desafio anterior (mesmo não
/// expirado), garantindo que `active_pin()` seja determinístico e que o
/// PIN exibido no tray corresponda ao device que está pareando agora.
///
/// Quando construído com [`PairingManager::new_with_store`], as
/// alterações de confiança (submit, trust, untrust) são persistidas
/// automaticamente em disco via [`TrustedStore`].
#[derive(Default)]
pub struct PairingManager {
    challenges: HashMap<String, PairChallenge>,
    trusted: HashMap<DeviceId, TrustedDevice>,
    /// Se `Some`, alterações de confiança são persistidas neste path.
    store_path: Option<PathBuf>,
}

impl std::fmt::Debug for PairingManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingManager")
            .field("challenges", &self.challenges)
            .field("trusted", &self.trusted)
            .field("store_path", &self.store_path)
            .finish()
    }
}

impl PairingManager {
    /// Cria um PairingManager sem persistência (útil para testes).
    pub fn new() -> Self {
        Self::default()
    }

    /// Cria um PairingManager com persistência em disco.
    ///
    /// Carrega o [`TrustedStore`] existente em `path` (arquivo ausente
    /// => store vazio) e popula `self.trusted` a partir dele. Todas as
    /// mutações de confiança (submit, trust, untrust) são automaticamente
    /// salvas em `path`.
    pub fn new_with_store(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let store = TrustedStore::load(&path)?;
        let mut trusted = HashMap::new();
        for dev in store.devices {
            trusted.insert(dev.id.clone(), dev);
        }
        Ok(Self {
            challenges: HashMap::new(),
            trusted,
            store_path: Some(path),
        })
    }

    /// Persiste o estado atual de `self.trusted` em disco.
    /// Chamado automaticamente após submit, trust e untrust quando
    /// `store_path` está configurado.
    fn persist(&self) -> Result<()> {
        if let Some(path) = &self.store_path {
            let store = TrustedStore {
                devices: self.trusted.values().cloned().collect(),
            };
            store.save(path)?;
        }
        Ok(())
    }

    /// Cria um novo desafio para um device não-confiado.
    ///
    /// O produto pareia um único device por vez, então este método
    /// invalida (remove) qualquer desafio anterior — mesmo não expirado —
    /// antes de registrar o novo. Assim, nunca há dois PINs candidatos
    /// simultaneamente e `active_pin()` permanece determinístico.
    pub fn start_challenge(&mut self, device_name: &str) -> PairChallenge {
        self.challenges.clear();

        let code = generate_pin();
        let nonce = generate_nonce();
        let challenge_id = uuid::Uuid::new_v4().to_string();
        let challenge = PairChallenge {
            challenge_id: challenge_id.clone(),
            code: code.clone(),
            nonce: nonce.clone(),
            expires_at: Instant::now() + DEFAULT_PIN_TTL,
            attempts_left: MAX_ATTEMPTS,
            device_name: device_name.to_owned(),
        };
        debug!(device = %device_name, %code, "novo desafio de pareamento");
        self.challenges
            .insert(device_name.to_owned(), challenge.clone());
        challenge
    }

    /// Valida a submissão de PIN. Consome uma tentativa.
    ///
    /// `kind` é a categoria do device (ex: "android", "linux", "ios"),
    /// informada pelo client no handshake `hello`.
    pub fn submit(
        &mut self,
        device_name: &str,
        challenge_id: &str,
        nonce: &str,
        code: &str,
        kind: &str,
    ) -> std::result::Result<DeviceId, PairingError> {
        let challenge = self
            .challenges
            .get_mut(device_name)
            .ok_or(PairingError::Invalid(PairFailReason::Expired))?;

        if challenge.expires_at <= Instant::now() {
            self.challenges.remove(device_name);
            return Err(PairingError::Invalid(PairFailReason::Expired));
        }

        if challenge.challenge_id != challenge_id {
            return Err(PairingError::Invalid(PairFailReason::InvalidCode));
        }

        if challenge.nonce != nonce {
            return Err(PairingError::Invalid(PairFailReason::InvalidCode));
        }

        if challenge.code != code {
            challenge.attempts_left = challenge.attempts_left.saturating_sub(1);
            if challenge.attempts_left == 0 {
                self.challenges.remove(device_name);
                return Err(PairingError::Invalid(PairFailReason::TooManyAttempts));
            }
            return Err(PairingError::Invalid(PairFailReason::InvalidCode));
        }

        // Sucesso: consome o desafio.
        let dev_id = DeviceId::new();
        let now = chrono::Utc::now().timestamp();
        let trusted = TrustedDevice {
            id: dev_id.clone(),
            name: device_name.to_owned(),
            kind: kind.to_owned(),
            last_seen: now,
            paired_at: now,
            trusted: true,
        };
        self.trusted.insert(dev_id.clone(), trusted);
        self.challenges.remove(device_name);
        if let Err(error) = self.persist() {
            self.trusted.remove(&dev_id);
            return Err(error.into());
        }
        info!(device = %device_name, id = %dev_id, "device pareado");
        Ok(dev_id)
    }

    /// Marca um device como confiado diretamente (para bootstrap
    /// via CLI ou config).
    pub fn trust(&mut self, name: &str, kind: &str) -> Result<DeviceId> {
        let id = DeviceId::new();
        let now = chrono::Utc::now().timestamp();
        let trusted = TrustedDevice {
            id: id.clone(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            last_seen: now,
            paired_at: now,
            trusted: true,
        };
        self.trusted.insert(id.clone(), trusted);
        if let Err(error) = self.persist() {
            self.trusted.remove(&id);
            return Err(error);
        }
        info!(device = %name, id = %id, "device confiado via bootstrap");
        Ok(id)
    }

    /// Verifica se um device já é confiado.
    pub fn is_trusted(&self, id: &DeviceId) -> bool {
        self.trusted.get(id).is_some_and(|t| t.trusted)
    }

    /// Retorna o nome registrado de um device confiado.
    pub fn device_name(&self, id: &DeviceId) -> Option<&str> {
        self.trusted.get(id).map(|t| t.name.as_str())
    }

    /// Atualiza `last_seen` de um device confiado.
    pub fn mark_seen(&mut self, id: &DeviceId) {
        if let Some(t) = self.trusted.get_mut(id) {
            t.last_seen = chrono::Utc::now().timestamp();
        }
    }

    /// Lista os devices confiados (para `clipsyncd list-peers`).
    pub fn trusted_devices(&self) -> Vec<&TrustedDevice> {
        let mut v: Vec<_> = self.trusted.values().collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.last_seen));
        v
    }

    /// Remove um device da lista de confiados.
    pub fn untrust(&mut self, id: &DeviceId) -> Result<bool> {
        let removed = self.trusted.remove(id);
        if let Some(device) = &removed {
            if let Err(error) = self.persist() {
                self.trusted.insert(id.clone(), device.clone());
                return Err(error);
            }
        }
        Ok(removed.is_some())
    }

    /// Retorna o PIN do desafio ativo (não expirado), se houver.
    ///
    /// Acessor mínimo exposto para o ícone de bandeja do `clipsyncd`
    /// exibir o PIN de pareamento atual sem acessar internos. Como o
    /// `PairingManager` mantém no máximo um desafio ativo por vez
    /// (`start_challenge` invalida os anteriores), o PIN retornado é
    /// determinístico — nunca há múltiplos candidatos.
    pub fn active_pin(&self) -> Option<String> {
        self.challenges
            .values()
            .filter(|c| c.expires_at > Instant::now())
            .map(|c| c.code.clone())
            .next()
    }
}

/// Gera um PIN de 6 dígitos com primeiro dígito != 0.
fn generate_pin() -> String {
    let mut rng = rand::thread_rng();
    let n: u32 = rng.gen_range(100_000..=999_999);
    n.to_string()
}

/// Gera um nonce de 16 bytes hex.
fn generate_nonce() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.gen();
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_is_6_digits() {
        for _ in 0..100 {
            let pin = generate_pin();
            assert_eq!(pin.len(), 6);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
            assert!(!pin.starts_with('0'));
        }
    }

    #[test]
    fn valid_pairing_flow() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("Pixel 8");
        let device_id = pm
            .submit("Pixel 8", &ch.challenge_id, &ch.nonce, &ch.code, "android")
            .unwrap();
        assert!(pm.is_trusted(&device_id));
    }

    #[test]
    fn wrong_code_consumes_attempt() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("Pixel 8");
        let r = pm.submit("Pixel 8", &ch.challenge_id, &ch.nonce, "000000", "android");
        assert!(matches!(
            r,
            Err(PairingError::Invalid(PairFailReason::InvalidCode))
        ));
        assert_eq!(pm.challenges["Pixel 8"].attempts_left, MAX_ATTEMPTS - 1);
    }

    #[test]
    fn wrong_nonce_rejected() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("Pixel 8");
        let r = pm.submit("Pixel 8", &ch.challenge_id, "deadbeef", "000000", "android");
        assert!(matches!(
            r,
            Err(PairingError::Invalid(PairFailReason::InvalidCode))
        ));
    }

    #[test]
    fn wrong_challenge_id_rejected() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("Pixel 8");
        let r = pm.submit("Pixel 8", "ch-inexistente", &ch.nonce, &ch.code, "android");
        assert!(matches!(
            r,
            Err(PairingError::Invalid(PairFailReason::InvalidCode))
        ));
        assert_eq!(pm.challenges["Pixel 8"].attempts_left, MAX_ATTEMPTS);
    }

    #[test]
    fn trust_bootstrap() {
        let mut pm = PairingManager::new();
        let id = pm.trust("my-phone", "android").unwrap();
        assert!(pm.is_trusted(&id));
        assert_eq!(pm.trusted_devices().len(), 1);
    }

    #[test]
    fn two_simultaneous_challenges_keeps_only_the_latest() {
        let mut pm = PairingManager::new();
        let first = pm.start_challenge("Pixel 8");
        let second = pm.start_challenge("Galaxy S23");

        // Invariante: apenas um desafio ativo por vez.
        assert_eq!(pm.challenges.len(), 1);
        assert_eq!(pm.active_pin().as_deref(), Some(second.code.as_str()));

        // O desafio anterior foi invalidado: submissão do PIN antigo falha.
        let r = pm.submit(
            "Pixel 8",
            &first.challenge_id,
            &first.nonce,
            &first.code,
            "android",
        );
        assert!(matches!(
            r,
            Err(PairingError::Invalid(PairFailReason::Expired))
        ));

        // O desafio atual permanece válido e pareia normalmente.
        let dev_id = pm
            .submit(
                "Galaxy S23",
                &second.challenge_id,
                &second.nonce,
                &second.code,
                "android",
            )
            .unwrap();
        assert!(pm.is_trusted(&dev_id));
        assert_eq!(pm.active_pin(), None);
    }

    #[test]
    fn trusted_store_roundtrips_toml() {
        let dir = std::env::temp_dir().join(format!("clipsync-pairing-{}", std::process::id()));
        let path = dir.join("trusted.toml");
        let _ = std::fs::remove_file(&path);
        let store = TrustedStore {
            devices: vec![TrustedDevice {
                id: DeviceId::from("abc-123"),
                name: "Pixel 8".into(),
                kind: "android".into(),
                last_seen: 1_700_000_000,
                paired_at: 1_690_000_000,
                trusted: true,
            }],
        };
        store.save(&path).unwrap();
        let loaded = TrustedStore::load(&path).unwrap();
        assert_eq!(loaded, store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn trusted_store_load_missing_is_empty() {
        let path = std::env::temp_dir().join(format!("trusted-missing-{}", std::process::id()));
        let store = TrustedStore::load(&path).unwrap();
        assert!(store.devices.is_empty());
    }

    #[test]
    fn trusted_store_load_propagates_read_error() {
        let path = std::env::temp_dir().join(format!("trusted-read-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).unwrap();

        assert!(TrustedStore::load(&path).is_err());
        let _ = std::fs::remove_dir(&path);
    }

    #[test]
    fn trust_propagates_write_error_and_rolls_back() {
        let path = std::env::temp_dir().join(format!("trusted-write-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let mut pm = PairingManager::new_with_store(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(pm.trust("my-phone", "android").is_err());
        assert!(pm.trusted_devices().is_empty());
        let _ = std::fs::remove_dir(&path);
    }

    #[test]
    fn submit_propagates_write_error_and_rolls_back() {
        let path =
            std::env::temp_dir().join(format!("trusted-submit-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let mut pm = PairingManager::new_with_store(&path).unwrap();
        let challenge = pm.start_challenge("my-phone");
        std::fs::create_dir(&path).unwrap();

        assert!(pm
            .submit(
                "my-phone",
                &challenge.challenge_id,
                &challenge.nonce,
                &challenge.code,
                "android",
            )
            .is_err());
        assert!(pm.trusted_devices().is_empty());
        let _ = std::fs::remove_dir(&path);
    }

    #[test]
    fn untrust_propagates_write_error_and_rolls_back() {
        let path =
            std::env::temp_dir().join(format!("trusted-untrust-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let mut pm = PairingManager::new_with_store(&path).unwrap();
        let id = pm.trust("my-phone", "android").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(pm.untrust(&id).is_err());
        assert!(pm.is_trusted(&id));
        let _ = std::fs::remove_dir(&path);
    }

    #[test]
    fn trusted_store_remove() {
        let mut store = TrustedStore {
            devices: vec![TrustedDevice {
                id: DeviceId::from("abc"),
                name: "Pixel 8".into(),
                kind: "android".into(),
                last_seen: 1,
                paired_at: 1,
                trusted: true,
            }],
        };
        assert!(store.remove("abc"));
        assert!(store.devices.is_empty());
        assert!(!store.remove("abc"));
    }

    #[test]
    fn trusted_store_sorted_by_last_seen_desc() {
        let store = TrustedStore {
            devices: vec![
                TrustedDevice {
                    id: DeviceId::from("old"),
                    name: "old".into(),
                    kind: "android".into(),
                    last_seen: 1,
                    paired_at: 1,
                    trusted: true,
                },
                TrustedDevice {
                    id: DeviceId::from("new"),
                    name: "new".into(),
                    kind: "android".into(),
                    last_seen: 3,
                    paired_at: 1,
                    trusted: true,
                },
            ],
        };
        let ids: Vec<_> = store
            .trusted_devices()
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(ids, vec!["new", "old"]);
    }

    #[test]
    fn persistence_survives_manager_reload() {
        let dir = std::env::temp_dir().join(format!("clipsync-persist-{}", std::process::id()));
        let path = dir.join("trusted.toml");
        let _ = std::fs::remove_file(&path);

        // 1) Pareia um device com persistência.
        let mut pm = PairingManager::new_with_store(&path).unwrap();
        let ch = pm.start_challenge("Pixel 8");
        let device_id = pm
            .submit("Pixel 8", &ch.challenge_id, &ch.nonce, &ch.code, "android")
            .unwrap();
        assert!(pm.is_trusted(&device_id));
        let device_name = pm.device_name(&device_id).unwrap().to_owned();
        drop(pm);

        // 2) Cria um novo PairingManager a partir do mesmo path.
        let pm2 = PairingManager::new_with_store(&path).unwrap();
        assert!(
            pm2.is_trusted(&device_id),
            "device deve permanecer confiado após reload"
        );
        assert_eq!(pm2.device_name(&device_id).unwrap(), device_name);
        assert_eq!(pm2.trusted_devices().len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn trust_and_untrust_persist() {
        let dir = std::env::temp_dir().join(format!("clipsync-trust-{}", std::process::id()));
        let path = dir.join("trusted.toml");
        let _ = std::fs::remove_file(&path);

        let mut pm = PairingManager::new_with_store(&path).unwrap();
        let id = pm.trust("my-phone", "android").unwrap();
        drop(pm);

        // Reload: trust persistido.
        let pm2 = PairingManager::new_with_store(&path).unwrap();
        assert!(pm2.is_trusted(&id));

        // Untrust + reload.
        let mut pm2 = pm2;
        pm2.untrust(&id).unwrap();
        drop(pm2);

        let pm3 = PairingManager::new_with_store(&path).unwrap();
        assert!(!pm3.is_trusted(&id), "device removido deve sumir no reload");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn submit_propagates_kind() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("iPhone 15");
        let device_id = pm
            .submit("iPhone 15", &ch.challenge_id, &ch.nonce, &ch.code, "ios")
            .unwrap();
        let devices = pm.trusted_devices();
        let device = devices.iter().find(|d| d.id == device_id).unwrap();
        assert_eq!(device.kind, "ios", "kind deve ser propagado do submit");
    }
}
