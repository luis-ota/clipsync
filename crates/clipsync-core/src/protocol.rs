//! Protocolo de aplicação trocado entre o daemon e os clients.
//!
//! O transporte é WebSocket (RFC 6455). Frames de texto carregam
//! mensagens JSON; frames binários são reservados para transferências
//! de arquivos (v0.3). Toda mensagem inclui um discriminador `type`
//! e a versão do protocolo `v`.
//!
//! A versão atual está em [`crate::PROTOCOL_VERSION`]. Mudanças
//! incompatíveis incrementam a versão major e exigem um novo
//! handshake.
//!
//! Veja [`docs/PROTOCOL.md`] para a especificação completa.

use std::fmt;

use serde::{Deserialize, Serialize};

pub use crate::PROTOCOL_VERSION;

/// Identificador estável de um device. UUID v4.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for DeviceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for DeviceId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for DeviceId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Categoria do device, usada para UI e telemetria.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Android,
    Linux,
    Macos,
    Windows,
    Ios,
    Other,
}

impl DeviceKind {
    pub fn is_mobile(&self) -> bool {
        matches!(self, Self::Android | Self::Ios)
    }
}

impl fmt::Display for DeviceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Android => "android",
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Ios => "ios",
            Self::Other => "other",
        };
        f.write_str(s)
    }
}

/// Metadados do device anunciados no handshake `hello`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// ID atribuído pelo servidor após o pareamento.
    /// `None` na primeira conexão de um device novo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<DeviceId>,

    /// Nome amigável exibido ao usuário ("Pixel 8", "luis-arch").
    pub name: String,

    pub kind: DeviceKind,

    /// Versão do SO ("Android 14", "Arch Linux 6.10.4").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,

    /// Versão do app cliente (ex: "clipsync-android 0.1.0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,

    /// Capabilities oferecidas pelo device. Permite que o servidor
    /// decida se aceita (ex: recusar transferência de arquivo se o
    /// device reporta `files = false`).
    #[serde(default)]
    pub capabilities: Capabilities,
}

impl DeviceInfo {
    pub fn new(name: impl Into<String>, kind: DeviceKind) -> Self {
        Self {
            id: None,
            name: name.into(),
            kind,
            os_version: None,
            app_version: None,
            capabilities: Capabilities::default(),
        }
    }

    pub fn with_os_version(mut self, v: impl Into<String>) -> Self {
        self.os_version = Some(v.into());
        self
    }

    pub fn with_app_version(mut self, v: impl Into<String>) -> Self {
        self.app_version = Some(v.into());
        self
    }

    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.capabilities = caps;
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub text: bool,
    #[serde(default)]
    pub html: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub files: bool,
}

/// Mensagens do protocolo de aplicação.
///
/// O `#[serde(tag = "type")]` produz payloads como
/// `{"type": "hello", "v": 1, "device": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// Primeira mensagem enviada pelo cliente ao se conectar.
    Hello { v: u16, device: DeviceInfo },

    /// Servidor responde com um PIN de 6 dígitos para pareamento.
    PairChallenge {
        /// PIN numérico, string de 6 dígitos.
        code: String,
        /// Unix timestamp (segundos) de expiração.
        expires_at: i64,
        /// Nonce aleatório que o cliente deve ecoar em `pair_submit`.
        nonce: String,
    },

    /// Cliente submete o PIN digitado.
    PairSubmit { code: String, nonce: String },

    /// Servidor confirma pareamento e atribui um device_id estável.
    PairOk {
        /// ID permanente do device. Persistir no client.
        device_id: DeviceId,
        /// ID único desta sessão de conexão (não persistente).
        session_id: String,
        /// Nome do servidor (PC), para exibir no client.
        server_name: String,
        /// Capabilities habilitadas neste pareamento.
        capabilities: Capabilities,
    },

    /// Servidor rejeita o pareamento.
    PairFail {
        reason: PairFailReason,
        /// Mensagem legível para humanos.
        message: String,
    },

    /// Sincronização de texto.
    ClipboardText {
        mime: String,
        content: String,
        /// Device que originou o conteúdo. Usado para anti-eco.
        origin: DeviceId,
        /// SHA-256 hex do conteúdo (dedup + anti-eco).
        sha256: String,
    },

    /// Sincronização de imagem. Bytes inline em base64 (v0.1).
    ClipboardImage {
        /// MIME type: `image/png`, `image/jpeg`, `image/gif`.
        mime: String,
        /// Bytes da imagem codificados em base64 standard.
        data_b64: String,
        width: Option<u32>,
        height: Option<u32>,
        sha256: String,
        origin: DeviceId,
    },

    /// Sincronização de rich text (HTML + plain text fallback).
    ClipboardHtml {
        html: String,
        alt: Option<String>,
        sha256: String,
        origin: DeviceId,
    },

    /// Keepalive. Cliente e servidor devem responder com `pong`.
    Ping { ts: i64 },

    /// Resposta ao ping.
    Pong { ts: i64 },

    /// Erro genérico (ex: payload muito grande, capability não habilitada).
    Error { code: String, message: String },
}

impl Message {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::PairChallenge { .. } => "pair_challenge",
            Self::PairSubmit { .. } => "pair_submit",
            Self::PairOk { .. } => "pair_ok",
            Self::PairFail { .. } => "pair_fail",
            Self::ClipboardText { .. } => "clipboard_text",
            Self::ClipboardImage { .. } => "clipboard_image",
            Self::ClipboardHtml { .. } => "clipboard_html",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::Error { .. } => "error",
        }
    }

    /// Wrapper que injeta a versão do protocolo para clientes que não
    /// conhecem o campo `v`.
    pub fn wrap(self) -> serde_json::Value {
        let mut v = serde_json::to_value(&self).expect("Message sempre serializa");
        if let Some(obj) = v.as_object_mut() {
            obj.insert("v".into(), serde_json::Value::from(PROTOCOL_VERSION));
        }
        v
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairFailReason {
    InvalidCode,
    Expired,
    TooManyAttempts,
    Banned,
    Internal,
}

impl fmt::Display for PairFailReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::InvalidCode => "invalid_code",
            Self::Expired => "expired",
            Self::TooManyAttempts => "too_many_attempts",
            Self::Banned => "banned",
            Self::Internal => "internal",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_serializes_with_type_tag() {
        let m = Message::Hello {
            v: PROTOCOL_VERSION,
            device: DeviceInfo::new("test", DeviceKind::Android),
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["type"], "hello");
        assert_eq!(json["v"], PROTOCOL_VERSION);
    }

    #[test]
    fn device_id_roundtrips() {
        let id = DeviceId::new();
        let s = serde_json::to_string(&id).unwrap();
        let back: DeviceId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }
}
