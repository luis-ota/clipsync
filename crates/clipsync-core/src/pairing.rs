//! Pareamento por PIN (estilo "digite o código na tela").
//!
//! O servidor gera um PIN de 6 dígitos com expiração. O cliente
//! submete o PIN junto com um nonce recebido no desafio. Se válido,
//! o device é marcado como pareado e passa a ser confiado nas
//! conexões seguintes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::Rng;
use tracing::{debug, info};

use crate::error::Result;
use crate::protocol::{DeviceId, PairFailReason};

/// Duração padrão de um PIN.
pub const DEFAULT_PIN_TTL: Duration = Duration::from_secs(120);
/// Máximo de tentativas por desafio.
pub const MAX_ATTEMPTS: u8 = 5;

/// Um desafio de pareamento em andamento.
#[derive(Debug, Clone)]
pub struct PairChallenge {
    /// PIN numérico de 6 dígitos (string).
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Gerencia desafios de pareamento ativos e o store de confiados.
///
/// Invariante: existe no máximo UM desafio de pareamento ativo por vez.
/// `start_challenge` invalida qualquer desafio anterior (mesmo não
/// expirado), garantindo que `active_pin()` seja determinístico e que o
/// PIN exibido no tray corresponda ao device que está pareando agora.
#[derive(Debug, Default)]
pub struct PairingManager {
    challenges: HashMap<String, PairChallenge>,
    trusted: HashMap<DeviceId, TrustedDevice>,
}

impl PairingManager {
    pub fn new() -> Self {
        Self::default()
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
        let challenge = PairChallenge {
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
    pub fn submit(
        &mut self,
        device_name: &str,
        nonce: &str,
        code: &str,
    ) -> Result<DeviceId, PairFailReason> {
        let challenge = self
            .challenges
            .get_mut(device_name)
            .ok_or(PairFailReason::Expired)?;

        if challenge.expires_at <= Instant::now() {
            self.challenges.remove(device_name);
            return Err(PairFailReason::Expired);
        }

        if challenge.nonce != nonce {
            return Err(PairFailReason::InvalidCode);
        }

        if challenge.code != code {
            challenge.attempts_left = challenge.attempts_left.saturating_sub(1);
            if challenge.attempts_left == 0 {
                self.challenges.remove(device_name);
                return Err(PairFailReason::TooManyAttempts);
            }
            return Err(PairFailReason::InvalidCode);
        }

        // Sucesso: consome o desafio.
        let dev_id = DeviceId::new();
        let now = chrono::Utc::now().timestamp();
        let trusted = TrustedDevice {
            id: dev_id.clone(),
            name: device_name.to_owned(),
            kind: "android".to_owned(),
            last_seen: now,
            paired_at: now,
            trusted: true,
        };
        self.trusted.insert(dev_id.clone(), trusted);
        self.challenges.remove(device_name);
        info!(device = %device_name, id = %dev_id, "device pareado");
        Ok(dev_id)
    }

    /// Marca um device como confiado diretamente (para bootstrap
    /// via CLI ou config).
    pub fn trust(&mut self, name: &str, kind: &str) -> DeviceId {
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
        info!(device = %name, id = %id, "device confiado via bootstrap");
        id
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
    pub fn untrust(&mut self, id: &DeviceId) -> bool {
        self.trusted.remove(id).is_some()
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
        let device_id = pm.submit("Pixel 8", &ch.nonce, &ch.code).unwrap();
        assert!(pm.is_trusted(&device_id));
    }

    #[test]
    fn wrong_code_consumes_attempt() {
        let mut pm = PairingManager::new();
        let ch = pm.start_challenge("Pixel 8");
        let r = pm.submit("Pixel 8", &ch.nonce, "000000");
        assert_eq!(r, Err(PairFailReason::InvalidCode));
        assert_eq!(pm.challenges["Pixel 8"].attempts_left, MAX_ATTEMPTS - 1);
    }

    #[test]
    fn wrong_nonce_rejected() {
        let mut pm = PairingManager::new();
        pm.start_challenge("Pixel 8");
        let r = pm.submit("Pixel 8", "deadbeef", "000000");
        assert_eq!(r, Err(PairFailReason::InvalidCode));
    }

    #[test]
    fn trust_bootstrap() {
        let mut pm = PairingManager::new();
        let id = pm.trust("my-phone", "android");
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
        let r = pm.submit("Pixel 8", &first.nonce, &first.code);
        assert_eq!(r, Err(PairFailReason::Expired));

        // O desafio atual permanece válido e pareia normalmente.
        let dev_id = pm
            .submit("Galaxy S23", &second.nonce, &second.code)
            .unwrap();
        assert!(pm.is_trusted(&dev_id));
        assert_eq!(pm.active_pin(), None);
    }
}
