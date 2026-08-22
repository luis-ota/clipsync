//! Identidade TLS do daemon. O certificado é autoassinado, persistente e
//! autenticado pelos clients através do fingerprint SHA-256 do DER.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls;

use crate::config::SecurityConfig;
use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Identity {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub fingerprint: String,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl Identity {
    pub fn load_or_generate(security: &SecurityConfig) -> Result<Self> {
        let cert_path = security
            .tls_cert_path
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(default_cert_path);
        let key_path = security
            .tls_key_path
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(default_key_path);
        if !cert_path.exists() || !key_path.exists() {
            if security.tls_server_names.is_empty() {
                return Err(Error::Config(
                    "identidade TLS exige pelo menos um nome SAN".into(),
                ));
            }
            let cert = generate_simple_self_signed(security.tls_server_names.clone())
                .map_err(|e| Error::Config(format!("falha gerando certificado TLS: {e}")))?;
            write_private(&cert_path, cert.cert.pem().as_bytes())?;
            write_private(&key_path, cert.key_pair.serialize_pem().as_bytes())?;
        }
        require_private_permissions(&cert_path)?;
        require_private_permissions(&key_path)?;
        let cert_pem = std::fs::read(&cert_path).map_err(Error::Io)?;
        let key_pem = std::fs::read(&key_path).map_err(Error::Io)?;
        let mut cert_reader = std::io::BufReader::new(cert_pem.as_slice());
        let cert_der = rustls_pemfile::certs(&mut cert_reader)
            .next()
            .transpose()
            .map_err(|e| Error::Config(format!("certificado TLS PEM invalido: {e}")))?
            .ok_or_else(|| Error::Config("certificado TLS PEM vazio".into()))?
            .to_vec();
        let mut key_reader = std::io::BufReader::new(key_pem.as_slice());
        let key_der = rustls_pemfile::pkcs8_private_keys(&mut key_reader)
            .next()
            .transpose()
            .map_err(|e| Error::Config(format!("chave TLS PEM invalida: {e}")))?
            .ok_or_else(|| Error::Config("chave TLS PKCS#8 PEM vazia".into()))?
            .secret_pkcs8_der()
            .to_vec();
        if cert_der.is_empty() || key_der.is_empty() {
            return Err(Error::Config("identidade TLS vazia".into()));
        }
        let fingerprint = fingerprint(&cert_der);
        if let Some(expected) = &security.tls_fingerprint {
            if normalize_fingerprint(expected) != fingerprint {
                return Err(Error::Config(
                    "fingerprint TLS local não corresponde à configuração".into(),
                ));
            }
        }
        Ok(Self {
            cert_path,
            key_path,
            fingerprint,
            cert_der,
            key_der,
        })
    }

    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let cert = CertificateDer::from(self.cert_der.clone());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()));
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .map_err(|e| Error::Config(format!("certificado TLS inválido: {e}")))?;
        Ok(Arc::new(config))
    }
}

pub fn fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn normalize_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(Error::Io)?;
    #[cfg(unix)]
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(Error::Io)?;
    std::fs::rename(tmp, path).map_err(Error::Io)
}

fn require_private_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(Error::Io)?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o600 {
            return Err(Error::Config(format!(
                "arquivo de identidade {path:?} deve ter modo 0600 (atual {mode:o})"
            )));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn default_cert_path() -> PathBuf {
    crate::config::config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("tls-cert.der")
}
fn default_key_path() -> PathBuf {
    crate::config::config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("tls-key.der")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trip_and_pin_failure() {
        let dir = std::env::temp_dir().join(format!("clipsync-tls-{}", std::process::id()));
        let mut cfg = SecurityConfig {
            tls_cert_path: Some(dir.join("cert.pem").display().to_string()),
            tls_key_path: Some(dir.join("key.pem").display().to_string()),
            ..Default::default()
        };
        let first = Identity::load_or_generate(&cfg).unwrap();
        let second = Identity::load_or_generate(&cfg).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        cfg.tls_fingerprint = Some("00".repeat(32));
        assert!(Identity::load_or_generate(&cfg).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
