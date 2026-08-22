//! E2E authenticated payloads for relay transport.
//!
//! Keys are provisioned to peers out of band (the relay never receives them).
//! AES-GCM is used from the audited `aes-gcm` crate; nonce uniqueness comes from
//! a fresh random nonce per message and replay protection from the relay
//! sequence number bound into the AEAD associated data.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::protocol::Message;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedRelayPayload {
    pub key_id: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayHeader<'a> {
    pub session_id: &'a str,
    pub source: &'a str,
    pub destination: Option<&'a str>,
    pub group: &'a str,
    pub sequence: u64,
    pub key_id: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum RelayCryptoError {
    #[error("invalid relay crypto field")]
    InvalidField,
    #[error("unknown relay key")]
    UnknownKey,
    #[error("AEAD authentication failed")]
    Authentication,
    #[error("invalid message: {0}")]
    Message(#[from] serde_json::Error),
}

/// Key ring containing the current and retained previous keys during rotation.
#[derive(Debug, Clone)]
pub struct RelayKeyRing {
    current: String,
    keys: HashMap<String, [u8; 32]>,
}

impl RelayKeyRing {
    pub fn new(key_id: impl Into<String>, key: [u8; 32]) -> Result<Self, RelayCryptoError> {
        let key_id = key_id.into();
        validate_key_id(&key_id)?;
        let mut keys = HashMap::new();
        keys.insert(key_id.clone(), key);
        Ok(Self {
            current: key_id,
            keys,
        })
    }

    pub fn rotate(
        &mut self,
        key_id: impl Into<String>,
        key: [u8; 32],
    ) -> Result<(), RelayCryptoError> {
        let key_id = key_id.into();
        validate_key_id(&key_id)?;
        self.keys.insert(key_id.clone(), key);
        self.current = key_id;
        Ok(())
    }

    pub fn encrypt_message(
        &self,
        header: &RelayHeader<'_>,
        message: &Message,
    ) -> Result<EncryptedRelayPayload, RelayCryptoError> {
        let plaintext = serde_json::to_vec(message)?;
        self.encrypt(header, &plaintext)
    }

    pub fn decrypt_message(
        &self,
        header: &RelayHeader<'_>,
        payload: &EncryptedRelayPayload,
    ) -> Result<Message, RelayCryptoError> {
        serde_json::from_slice(&self.decrypt(header, payload)?).map_err(Into::into)
    }

    pub fn encrypt(
        &self,
        header: &RelayHeader<'_>,
        plaintext: &[u8],
    ) -> Result<EncryptedRelayPayload, RelayCryptoError> {
        let key = self
            .keys
            .get(self.current.as_str())
            .ok_or(RelayCryptoError::UnknownKey)?;
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| RelayCryptoError::InvalidField)?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: aad(header).as_bytes(),
                },
            )
            .map_err(|_| RelayCryptoError::Authentication)?;
        Ok(EncryptedRelayPayload {
            key_id: self.current.clone(),
            nonce: hex::encode(nonce),
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ciphertext),
        })
    }

    pub fn decrypt(
        &self,
        header: &RelayHeader<'_>,
        payload: &EncryptedRelayPayload,
    ) -> Result<Vec<u8>, RelayCryptoError> {
        let key = self
            .keys
            .get(&payload.key_id)
            .ok_or(RelayCryptoError::UnknownKey)?;
        let nonce_bytes =
            hex::decode(&payload.nonce).map_err(|_| RelayCryptoError::InvalidField)?;
        if nonce_bytes.len() != 12 {
            return Err(RelayCryptoError::InvalidField);
        }
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&payload.ciphertext)
            .map_err(|_| RelayCryptoError::InvalidField)?;
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| RelayCryptoError::InvalidField)?;
        cipher
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &ciphertext,
                    aad: aad(header).as_bytes(),
                },
            )
            .map_err(|_| RelayCryptoError::Authentication)
    }
}

fn validate_key_id(key_id: &str) -> Result<(), RelayCryptoError> {
    if key_id.is_empty()
        || key_id.len() > 64
        || !key_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Err(RelayCryptoError::InvalidField)
    } else {
        Ok(())
    }
}

fn aad(header: &RelayHeader<'_>) -> String {
    format!(
        "clipsync-relay-v1\0{}\0{}\0{}\0{}\0{}\0{}",
        header.session_id,
        header.source,
        header.destination.unwrap_or(""),
        header.group,
        header.sequence,
        header.key_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'a>(key_id: &'a str, sequence: u64) -> RelayHeader<'a> {
        RelayHeader {
            session_id: "session",
            source: "source",
            destination: Some("target"),
            group: "group",
            sequence,
            key_id,
        }
    }

    fn message() -> Message {
        Message::ClipboardText {
            mime: "text/plain".into(),
            content: "secret".into(),
            origin: "source".into(),
            sha256: "hash".into(),
        }
    }

    #[test]
    fn round_trip_and_wrong_key() {
        let ring = RelayKeyRing::new("v1", [7; 32]).unwrap();
        let encrypted = ring.encrypt_message(&header("v1", 1), &message()).unwrap();
        assert_eq!(
            ring.decrypt_message(&header("v1", 1), &encrypted)
                .unwrap()
                .type_name(),
            "clipboard_text"
        );
        let wrong = RelayKeyRing::new("v1", [8; 32]).unwrap();
        assert!(matches!(
            wrong.decrypt_message(&header("v1", 1), &encrypted),
            Err(RelayCryptoError::Authentication)
        ));
    }

    #[test]
    fn tamper_and_replay_bound_header_fail() {
        let ring = RelayKeyRing::new("v1", [7; 32]).unwrap();
        let mut encrypted = ring.encrypt_message(&header("v1", 1), &message()).unwrap();
        encrypted.ciphertext.replace_range(0..1, "A");
        assert!(ring.decrypt_message(&header("v1", 1), &encrypted).is_err());
        let encrypted = ring.encrypt_message(&header("v1", 1), &message()).unwrap();
        assert!(ring.decrypt_message(&header("v1", 2), &encrypted).is_err());
    }

    #[test]
    fn rotation_retains_previous_key_and_uses_new_key() {
        let mut ring = RelayKeyRing::new("v1", [7; 32]).unwrap();
        let old = ring.encrypt_message(&header("v1", 1), &message()).unwrap();
        ring.rotate("v2", [8; 32]).unwrap();
        assert!(ring.decrypt_message(&header("v1", 1), &old).is_ok());
        let new = ring.encrypt_message(&header("v2", 2), &message()).unwrap();
        assert_eq!(new.key_id, "v2");
    }
}
