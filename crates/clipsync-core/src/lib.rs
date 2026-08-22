//! `clipsync-core` — núcleo reutilizável do `clipsync`.
//!
//! Esta crate contém toda a lógica "headless": protocolo, transporte
//! WebSocket, descoberta mDNS, abstração de clipboard, pareamento e
//! estado de peers. Ela é desenhada para ser usada tanto pelo daemon
//! (`clipsyncd`) quanto, eventualmente, por um cliente desktop ou
//! um binding para o app Android.
//!
//! Não há nenhuma dependência de GUI ou platform-specific além do
//! backend de clipboard (Wayland / X11) — toda a lógica de negócio
//! é agnóstica de plataforma.

#![deny(rust_2018_idioms)]
#![warn(missing_debug_implementations)]
#![warn(unreachable_pub)]

pub mod auth;
pub mod clipboard;
pub mod config;
pub mod discovery;
pub mod dispatch;
pub mod error;
pub mod outbound;
pub mod pairing;
pub mod peer;
mod persistence;
pub mod protocol;
pub mod relay_crypto;
pub mod server;
pub mod state;
pub mod tls;
pub mod transfer;
pub mod transport;

pub use auth::{GroupId, Principal, ServerId, SessionCredential, SessionId, UserId};
pub use error::{Error, Result};
pub use protocol::{DeviceId, DeviceInfo, DeviceKind, Message};

/// Versão do protocolo de aplicação. Incrementado em mudanças
/// incompatíveis.
pub const PROTOCOL_VERSION: u16 = 1;

/// Tipo de serviço mDNS anunciado pelo daemon. Dispositivos clientes
/// (apps Android) fazem browse nesse tipo para descobrir o PC.
pub const SERVICE_TYPE: &str = "_clipsync._tcp.local.";
