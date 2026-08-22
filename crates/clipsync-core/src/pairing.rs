//! Pareamento por PIN (estilo "digite o código na tela").
//!
//! O servidor gera um PIN de 6 dígitos com expiração e o exibe
//! localmente no daemon. O cliente submete o PIN digitado pelo usuário
//! junto com o `challenge_id` e o nonce recebidos no desafio. O PIN
//! nunca atravessa o fio: a resposta do desafio carrega apenas o
//! `challenge_id`, o nonce e a expiração. Se válido, o device é marcado
//! como pareado e passa a ser confiado nas conexões seguintes.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fs2::FileExt;
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
    /// ID da conexão que solicitou o pareamento.
    pub session_id: String,
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
/// Formato: TOML com um array `devices` de [`TrustedDevice`]. O processo que
/// o altera deve possuir um [`TrustedStoreLock`]; o daemon mantém esse lock
/// durante toda a execução e é o owner do estado em memória.
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

    /// Salva o store atomicamente em um path TOML.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| crate::Error::Config(format!("falha criando {parent:?}: {e}")))?;
        }
        let toml_str = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::Config(format!("falha serializando trusted: {e}")))?;
        crate::persistence::atomic_write(path, toml_str.as_bytes())
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

/// Lock interprocesso exclusivo associado a um trusted store.
#[derive(Debug)]
pub struct TrustedStoreLock {
    _file: File,
}

impl TrustedStoreLock {
    /// Tenta adquirir ownership exclusivo sem bloquear.
    pub fn try_acquire(store_path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = store_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| crate::Error::Config(format!("falha criando {parent:?}: {e}")))?;
        }
        let lock_path = store_path.with_extension("toml.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| crate::Error::Config(format!("falha abrindo {lock_path:?}: {e}")))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Self { _file: file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(crate::Error::StoreBusy(store_path.to_path_buf()))
            }
            Err(error) => Err(crate::Error::Config(format!(
                "falha adquirindo lock de {store_path:?}: {error}"
            ))),
        }
    }
}

/// Gerencia desafios de pareamento ativos e o store de confiados.
///
/// Existe no máximo um desafio global porque a interface local apresenta um
/// único PIN. Iniciar outro desafio invalida o anterior; a sessão continua
/// sendo parte da validação e o nome nunca é usado como identidade.
///
/// Quando construído com [`PairingManager::new_with_store`], as
/// alterações de confiança (submit, trust, untrust) são persistidas
/// automaticamente em disco via [`TrustedStore`].
#[derive(Default)]
pub struct PairingManager {
    challenge: Option<PairChallenge>,
    trusted: HashMap<DeviceId, TrustedDevice>,
    /// Se `Some`, alterações de confiança são persistidas neste path.
    store_path: Option<PathBuf>,
    /// Mantém ownership exclusivo do store durante toda a vida do manager.
    _store_lock: Option<TrustedStoreLock>,
}

impl std::fmt::Debug for PairingManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingManager")
            .field("challenge", &self.challenge)
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
        let store_lock = TrustedStoreLock::try_acquire(&path)?;
        let store = TrustedStore::load(&path)?;
        let mut trusted = HashMap::new();
        for dev in store.devices {
            trusted.insert(dev.id.clone(), dev);
        }
        Ok(Self {
            challenge: None,
            trusted,
            store_path: Some(path),
            _store_lock: Some(store_lock),
        })
    }

    /// Persiste e só então publica um novo conjunto de devices confiados.
    fn commit_trusted(&mut self, trusted: HashMap<DeviceId, TrustedDevice>) -> Result<()> {
        if let Some(path) = &self.store_path {
            let store = TrustedStore {
                devices: trusted.values().cloned().collect(),
            };
            store.save(path)?;
        }
        self.trusted = trusted;
        Ok(())
    }

    /// Cria um novo desafio associado exclusivamente a `session_id`.
    pub fn start_challenge(
        &mut self,
        session_id: &str,
        device_name: &str,
        ttl: Duration,
    ) -> PairChallenge {
        let code = generate_pin();
        let nonce = generate_nonce();
        let challenge_id = uuid::Uuid::new_v4().to_string();
        let challenge = PairChallenge {
            challenge_id: challenge_id.clone(),
            code: code.clone(),
            nonce: nonce.clone(),
            expires_at: Instant::now() + ttl,
            attempts_left: MAX_ATTEMPTS,
            session_id: session_id.to_owned(),
            device_name: device_name.to_owned(),
        };
        // O PIN só pode ser exibido localmente; nunca vai para logs coletáveis.
        debug!(device = %device_name, "novo desafio de pareamento");
        self.challenge = Some(challenge.clone());
        challenge
    }

    /// Valida a submissão de PIN. Consome uma tentativa.
    ///
    /// `kind` é a categoria do device (ex: "android", "linux", "ios"),
    /// informada pelo client no handshake `hello`.
    pub fn submit(
        &mut self,
        session_id: &str,
        challenge_id: &str,
        nonce: &str,
        code: &str,
        kind: &str,
    ) -> std::result::Result<DeviceId, PairingError> {
        let challenge = self
            .challenge
            .as_mut()
            .filter(|challenge| challenge.challenge_id == challenge_id)
            .ok_or(PairingError::Invalid(PairFailReason::Expired))?;

        if challenge.session_id != session_id {
            return Err(PairingError::Invalid(PairFailReason::InvalidCode));
        }
        let device_name = challenge.device_name.clone();

        if challenge.expires_at <= Instant::now() {
            self.challenge = None;
            return Err(PairingError::Invalid(PairFailReason::Expired));
        }

        if challenge.nonce != nonce {
            return Err(PairingError::Invalid(PairFailReason::InvalidCode));
        }

        if challenge.code != code {
            challenge.attempts_left = challenge.attempts_left.saturating_sub(1);
            if challenge.attempts_left == 0 {
                self.challenge = None;
                return Err(PairingError::Invalid(PairFailReason::TooManyAttempts));
            }
            return Err(PairingError::Invalid(PairFailReason::InvalidCode));
        }

        // O desafio só é consumido depois que a confiança foi persistida.
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
        let mut next_trusted = self.trusted.clone();
        next_trusted.insert(dev_id.clone(), trusted);
        self.commit_trusted(next_trusted)?;
        self.challenge = None;
        info!(device = %device_name, id = %dev_id, "device pareado");
        Ok(dev_id)
    }

    /// Cancela todos os desafios pertencentes a uma conexão encerrada.
    pub fn cancel_session(&mut self, session_id: &str) {
        if matches!(&self.challenge, Some(challenge) if challenge.session_id == session_id) {
            self.challenge = None;
        }
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
        let mut next_trusted = self.trusted.clone();
        next_trusted.insert(id.clone(), trusted);
        self.commit_trusted(next_trusted)?;
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
        let mut next_trusted = self.trusted.clone();
        if next_trusted.remove(id).is_some() {
            self.commit_trusted(next_trusted)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Retorna o PIN mais recente ainda ativo, se houver.
    ///
    /// Acessor mínimo exposto para o ícone de bandeja do `clipsyncd`.
    pub fn active_pin(&self) -> Option<String> {
        self.challenge
            .as_ref()
            .filter(|challenge| challenge.expires_at > Instant::now())
            .map(|challenge| challenge.code.clone())
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
        let ch = pm.start_challenge("session-1", "Pixel 8", DEFAULT_PIN_TTL);
        let device_id = pm
            .submit(
                "session-1",
                &ch.challenge_id,
                &ch.nonce,
                &ch.code,
                "android",
            )
            .unwrap();
        assert!(pm.is_trusted(&device_id));
    }

    #[test]
    fn wrong_code_consumes_attempt() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("session-1", "Pixel 8", DEFAULT_PIN_TTL);
        let r = pm.submit(
            "session-1",
            &ch.challenge_id,
            &ch.nonce,
            "000000",
            "android",
        );
        assert!(matches!(
            r,
            Err(PairingError::Invalid(PairFailReason::InvalidCode))
        ));
        assert_eq!(
            pm.challenge.as_ref().unwrap().attempts_left,
            MAX_ATTEMPTS - 1
        );
    }

    #[test]
    fn wrong_nonce_rejected() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("session-1", "Pixel 8", DEFAULT_PIN_TTL);
        let r = pm.submit(
            "session-1",
            &ch.challenge_id,
            "deadbeef",
            "000000",
            "android",
        );
        assert!(matches!(
            r,
            Err(PairingError::Invalid(PairFailReason::InvalidCode))
        ));
    }

    #[test]
    fn wrong_challenge_id_rejected() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("session-1", "Pixel 8", DEFAULT_PIN_TTL);
        let r = pm.submit(
            "session-1",
            "ch-inexistente",
            &ch.nonce,
            &ch.code,
            "android",
        );
        assert!(matches!(
            r,
            Err(PairingError::Invalid(PairFailReason::Expired))
        ));
        assert_eq!(pm.challenge.as_ref().unwrap().attempts_left, MAX_ATTEMPTS);
    }

    #[test]
    fn trust_bootstrap() {
        let mut pm = PairingManager::new();
        let id = pm.trust("my-phone", "android").unwrap();
        assert!(pm.is_trusted(&id));
        assert_eq!(pm.trusted_devices().len(), 1);
    }

    #[test]
    fn newer_challenge_invalidates_previous_one() {
        let mut pm = PairingManager::new();
        let first = pm.start_challenge("session-1", "Pixel 8", DEFAULT_PIN_TTL);
        let second = pm.start_challenge("session-2", "Galaxy S23", DEFAULT_PIN_TTL);

        assert_eq!(pm.active_pin().as_deref(), Some(second.code.as_str()));
        assert!(matches!(
            pm.submit(
                "session-1",
                &first.challenge_id,
                &first.nonce,
                &first.code,
                "android",
            ),
            Err(PairingError::Invalid(PairFailReason::Expired))
        ));

        let dev_id = pm
            .submit(
                "session-2",
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
    fn challenge_cannot_be_submitted_by_another_session_with_same_name() {
        let mut pm = PairingManager::new();
        let challenge = pm.start_challenge("session-a", "same-name", DEFAULT_PIN_TTL);
        let result = pm.submit(
            "session-b",
            &challenge.challenge_id,
            &challenge.nonce,
            &challenge.code,
            "linux",
        );
        assert!(matches!(
            result,
            Err(PairingError::Invalid(PairFailReason::InvalidCode))
        ));
        assert_eq!(
            pm.challenge
                .as_ref()
                .map(|active| active.challenge_id.as_str()),
            Some(challenge.challenge_id.as_str())
        );
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
    fn trust_does_not_publish_failed_commit() {
        let path = std::env::temp_dir().join(format!("trusted-write-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let mut pm = PairingManager::new_with_store(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(pm.trust("my-phone", "android").is_err());
        assert!(pm.trusted_devices().is_empty());
        let _ = std::fs::remove_dir(&path);
    }

    #[test]
    fn submit_does_not_publish_or_consume_on_failed_commit() {
        let path =
            std::env::temp_dir().join(format!("trusted-submit-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let mut pm = PairingManager::new_with_store(&path).unwrap();
        let challenge = pm.start_challenge("session-1", "my-phone", DEFAULT_PIN_TTL);
        std::fs::create_dir(&path).unwrap();

        assert!(pm
            .submit(
                "session-1",
                &challenge.challenge_id,
                &challenge.nonce,
                &challenge.code,
                "android",
            )
            .is_err());
        assert!(pm.trusted_devices().is_empty());
        assert_eq!(pm.active_pin().as_deref(), Some(challenge.code.as_str()));
        let _ = std::fs::remove_dir(&path);
    }

    #[test]
    fn untrust_does_not_publish_failed_commit() {
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
    fn trusted_store_has_single_process_owner() {
        let dir = std::env::temp_dir().join(format!(
            "clipsync-store-lock-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let path = dir.join("trusted.toml");
        let owner = TrustedStoreLock::try_acquire(&path).unwrap();

        assert!(matches!(
            TrustedStoreLock::try_acquire(&path),
            Err(crate::Error::StoreBusy(busy_path)) if busy_path == path
        ));

        drop(owner);
        assert!(TrustedStoreLock::try_acquire(&path).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persistence_survives_manager_reload() {
        let dir = std::env::temp_dir().join(format!("clipsync-persist-{}", std::process::id()));
        let path = dir.join("trusted.toml");
        let _ = std::fs::remove_file(&path);

        // 1) Pareia um device com persistência.
        let mut pm = PairingManager::new_with_store(&path).unwrap();
        let ch = pm.start_challenge("session-1", "Pixel 8", DEFAULT_PIN_TTL);
        let device_id = pm
            .submit(
                "session-1",
                &ch.challenge_id,
                &ch.nonce,
                &ch.code,
                "android",
            )
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
        let ch = pm.start_challenge("session-1", "iPhone 15", DEFAULT_PIN_TTL);
        let device_id = pm
            .submit("session-1", &ch.challenge_id, &ch.nonce, &ch.code, "ios")
            .unwrap();
        let devices = pm.trusted_devices();
        let device = devices.iter().find(|d| d.id == device_id).unwrap();
        assert_eq!(device.kind, "ios", "kind deve ser propagado do submit");
    }
}
