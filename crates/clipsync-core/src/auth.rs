//! Primitivas de identidade, sessão e autorização do relay.
//!
//! Este módulo não cria um segundo servidor. Ele fornece o contrato que um
//! relay ou cliente pode usar antes de encaminhar uma mensagem. O token de
//! sessão é um bearer token e, portanto, só deve ser transportado por TLS.
//! E2E exige uma negociação de chaves no wire e não é implementado
//! implicitamente aqui.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::protocol::{DeviceId, Message};

macro_rules! identity_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            pub fn from_string(value: impl Into<String>) -> Option<Self> {
                let value = value.into();
                (!value.trim().is_empty()).then_some(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

identity_type!(ServerId);
identity_type!(UserId);
identity_type!(GroupId);
identity_type!(SessionId);

/// Identidade lógica de um participante, separada do nome exibido.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub user_id: UserId,
    pub device_id: DeviceId,
}

/// Credencial emitida pelo relay depois de um pareamento bem-sucedido.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCredential {
    pub session_id: SessionId,
    pub principal: Principal,
    pub token: String,
    pub expires_at: u64,
}

/// Estado mínimo de autenticação de sessões. O token nunca é derivado de
/// nome, endereço, server_id ou device_id.
#[derive(Debug)]
pub struct SessionAuthenticator {
    ttl: Duration,
    sessions: HashMap<SessionId, SessionCredential>,
}

impl SessionAuthenticator {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            sessions: HashMap::new(),
        }
    }

    pub fn issue(&mut self, principal: Principal) -> SessionCredential {
        let session_id = SessionId::new();
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let credential = SessionCredential {
            session_id: session_id.clone(),
            principal,
            token: hex::encode(bytes),
            expires_at: now().saturating_add(self.ttl.as_secs()),
        };
        self.sessions.insert(session_id, credential.clone());
        credential
    }

    pub fn authenticate(&mut self, session_id: &SessionId, token: &str) -> Option<Principal> {
        let credential = self.sessions.get(session_id)?;
        if credential.expires_at < now() || !constant_time_eq(&credential.token, token) {
            return None;
        }
        Some(credential.principal.clone())
    }

    pub fn revoke(&mut self, session_id: &SessionId) -> bool {
        self.sessions.remove(session_id).is_some()
    }
}

/// Política explícita de autorização. Uma mensagem unicast exige que o
/// destino esteja no mesmo grupo indicado; broadcast de grupo usa a mesma
/// regra para impedir cross-tenant forwarding.
#[derive(Debug, Default)]
pub struct GroupAuthorizer {
    groups: HashMap<GroupId, HashSet<DeviceId>>,
}

impl GroupAuthorizer {
    pub fn add_member(&mut self, group: GroupId, device: DeviceId) {
        self.groups.entry(group).or_default().insert(device);
    }

    pub fn remove_member(&mut self, group: &GroupId, device: &DeviceId) -> bool {
        self.groups
            .get_mut(group)
            .is_some_and(|members| members.remove(device))
    }

    pub fn authorize(
        &self,
        source: &DeviceId,
        destination: Option<&DeviceId>,
        group: &GroupId,
    ) -> Result<(), AuthorizationError> {
        let members = self
            .groups
            .get(group)
            .ok_or(AuthorizationError::UnknownGroup)?;
        if !members.contains(source) {
            return Err(AuthorizationError::SourceNotMember);
        }
        if destination.is_some_and(|id| !members.contains(id)) {
            return Err(AuthorizationError::DestinationNotMember);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationError {
    UnknownGroup,
    SourceNotMember,
    DestinationNotMember,
    Replay,
    SourceMismatch,
}

/// Proteção contra replay por sessão. Sequências devem ser estritamente
/// crescentes; retransmissão fora de ordem é rejeitada deliberadamente.
#[derive(Debug, Default)]
pub struct ReplayProtector {
    highest: HashMap<SessionId, u64>,
}

impl ReplayProtector {
    pub fn accept(&mut self, session: &SessionId, sequence: u64) -> bool {
        let last = self.highest.entry(session.clone()).or_insert(0);
        if sequence <= *last {
            return false;
        }
        *last = sequence;
        true
    }

    pub fn forget(&mut self, session: &SessionId) {
        self.highest.remove(session);
    }
}

/// Envelope autenticado e endereçado. O relay deve preencher `source` com a
/// identidade da sessão, nunca com dados fornecidos pelo payload recebido.
#[derive(Debug, Clone)]
pub struct RelayEnvelope {
    pub session_id: SessionId,
    pub source: DeviceId,
    pub destination: Option<DeviceId>,
    pub group: GroupId,
    pub sequence: u64,
    pub payload: Message,
}

impl RelayEnvelope {
    pub fn authorize(
        &mut self,
        authenticated_source: &DeviceId,
        groups: &GroupAuthorizer,
        replay: &mut ReplayProtector,
    ) -> Result<(), AuthorizationError> {
        if &self.source != authenticated_source {
            return Err(AuthorizationError::SourceMismatch);
        }
        if !replay.accept(&self.session_id, self.sequence) {
            return Err(AuthorizationError::Replay);
        }
        groups.authorize(&self.source, self.destination.as_ref(), &self.group)?;
        self.payload = self.payload.clone().with_origin(authenticated_source);
        Ok(())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let mut difference = left.len() ^ right.len();
    for (a, b) in left.bytes().zip(right.bytes()) {
        difference |= usize::from(a ^ b);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(device: &str) -> Principal {
        Principal {
            user_id: UserId::new(),
            device_id: DeviceId::from(device),
        }
    }

    #[test]
    fn session_token_authenticates_and_wrong_token_fails() {
        let mut auth = SessionAuthenticator::new(Duration::from_secs(60));
        let credential = auth.issue(principal("device-a"));
        assert!(auth
            .authenticate(&credential.session_id, &credential.token)
            .is_some());
        assert!(auth.authenticate(&credential.session_id, "wrong").is_none());
    }

    #[test]
    fn group_authorization_rejects_forged_destination() {
        let group = GroupId::new();
        let mut policy = GroupAuthorizer::default();
        policy.add_member(group.clone(), DeviceId::from("source"));
        assert_eq!(
            policy.authorize(
                &DeviceId::from("source"),
                Some(&DeviceId::from("other")),
                &group
            ),
            Err(AuthorizationError::DestinationNotMember)
        );
    }

    #[test]
    fn replay_and_forged_source_are_rejected() {
        let session = SessionId::new();
        let group = GroupId::new();
        let source = DeviceId::from("source");
        let mut policy = GroupAuthorizer::default();
        policy.add_member(group.clone(), source.clone());
        let payload = Message::ClipboardText {
            mime: "text/plain".into(),
            content: "secret".into(),
            origin: DeviceId::from("forged"),
            sha256: "hash".into(),
        };
        let mut envelope = RelayEnvelope {
            session_id: session.clone(),
            source: source.clone(),
            destination: None,
            group,
            sequence: 1,
            payload,
        };
        let mut replay = ReplayProtector::default();
        assert_eq!(envelope.authorize(&source, &policy, &mut replay), Ok(()));
        assert!(matches!(
            envelope.authorize(&source, &policy, &mut replay),
            Err(AuthorizationError::Replay)
        ));
        assert!(matches!(
            envelope.authorize(&DeviceId::from("attacker"), &policy, &mut replay),
            Err(AuthorizationError::SourceMismatch)
        ));
    }
}
