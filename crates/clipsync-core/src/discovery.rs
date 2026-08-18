//! Descoberta automática de devices via mDNS.
//!
//! O daemon anuncia um serviço DNS-SD em `_clipsync._tcp.local.`
//! com o nome amigável do PC (ex: `luis-arch`). Apps Android
//! fazem browse desse tipo de serviço para encontrar o PC sem
//! digitar IP manualmente.
//!
//! Referência do formato DNS-SD:
//! <https://datatracker.ietf.org/doc/html/rfc6763>

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, info};

use crate::error::{Error, Result};
use crate::SERVICE_TYPE;

/// Instância única de descoberta mDNS por daemon.
pub struct Discovery {
    daemon: ServiceDaemon,
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery").finish_non_exhaustive()
    }
}

/// Resultado de um browse mDNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredService {
    pub fullname: String,
    pub instance: String,
    pub port: u16,
    pub addrs: Vec<IpAddr>,
    pub properties: HashMap<String, String>,
}

impl DiscoveredService {
    pub fn socket_addrs(&self) -> Vec<SocketAddr> {
        self.addrs
            .iter()
            .map(|a| SocketAddr::new(*a, self.port))
            .collect()
    }
}

impl Discovery {
    /// Cria a instância daemon mDNS.
    pub fn new() -> Result<Self> {
        let daemon = ServiceDaemon::new().map_err(Error::Mdns)?;
        Ok(Self { daemon })
    }

    /// Anuncia o serviço deste host com o nome dado.
    ///
    /// Retorna erro se não for possível determinar o IP local (por
    /// exemplo, sem conectividade de rede), evitando anunciar
    /// `127.0.0.1` que seria inacessível para outros hosts.
    pub fn announce(&self, name: &str, port: u16) -> Result<()> {
        let instance = sanitize_instance(name);
        let host = format!("{instance}.local");
        let type_full = SERVICE_TYPE;
        let ip = local_ip_v4()
            .ok_or_else(|| Error::Config("não foi possível determinar o IP local IPv4".into()))?;

        let properties: HashMap<String, String> = [
            ("version", env!("CARGO_PKG_VERSION").to_owned()),
            ("platform", "linux".to_owned()),
            ("cap_text", "1".to_owned()),
            ("cap_image", "1".to_owned()),
            ("cap_html", "0".to_owned()),
            ("cap_files", "0".to_owned()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();

        let service = ServiceInfo::new(
            type_full,
            &instance,
            &host,
            ip.to_string(),
            port,
            properties,
        )
        .map_err(Error::Mdns)?
        .enable_addr_auto();

        self.daemon.register(service).map_err(Error::Mdns)?;
        info!(instance, port, ip = %ip, "mDNS: serviço anunciado");
        Ok(())
    }

    /// Para de anunciar o serviço.
    pub fn shutdown(&mut self) {
        let _ = self.daemon.shutdown();
    }

    /// Faz browse do serviço de outra instância. Útil para o futuro
    /// modo cliente (`clipsyncd discover`), e testado em CI.
    pub async fn browse(
        &self,
        timeout: Duration,
        settle_ms: u64,
    ) -> Result<Vec<DiscoveredService>> {
        let receiver = self.daemon.browse(SERVICE_TYPE).map_err(Error::Mdns)?;

        let mut seen: HashMap<String, DiscoveredService> = HashMap::new();
        let mut last_change = tokio::time::Instant::now();
        let deadline = last_change + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            let ev_res = tokio::time::timeout(remaining, receiver.recv_async()).await;
            let ev = match ev_res {
                Ok(Ok(e)) => e,
                Ok(Err(_)) => break,
                Err(_) => break,
            };

            if let ServiceEvent::ServiceResolved(info) = ev {
                let key = info.get_fullname().to_owned();
                let addrs: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
                let instance = info.get_hostname().trim_end_matches(".local.").to_owned();
                let properties: HashMap<String, String> = info
                    .get_properties()
                    .iter()
                    .map(|p| {
                        let key = p.key().to_string();
                        let val = p.val_str().to_string();
                        (key, val)
                    })
                    .collect();

                seen.insert(
                    key.clone(),
                    DiscoveredService {
                        fullname: key,
                        instance,
                        port: info.get_port(),
                        addrs,
                        properties,
                    },
                );
                last_change = tokio::time::Instant::now();
                debug!(services = seen.len(), "mDNS: serviço resolvido");
            } else if let ServiceEvent::ServiceRemoved(_, ref fullname) = ev {
                seen.remove(fullname);
                last_change = tokio::time::Instant::now();
            }

            if last_change.elapsed() >= Duration::from_millis(settle_ms) {
                break;
            }
        }

        let mut result: Vec<_> = seen.into_values().collect();
        result.sort_by(|a, b| a.instance.cmp(&b.instance));
        Ok(result)
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

/// Sanitiza o nome para ser um instance name DNS-SD válido.
fn sanitize_instance(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut trimmed = cleaned.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        trimmed = "clipsync-host".to_owned();
    }
    if trimmed.len() > 63 {
        trimmed.truncate(63);
    }
    trimmed
}

/// Descobre o IP local preferido da máquina via UDP probe.
fn local_ip_v4() -> Option<Ipv4Addr> {
    use std::net::UdpSocket;
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let local = sock.local_addr().ok()?;
    match local.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_handles_unicode_and_spaces() {
        assert_eq!(sanitize_instance("luis arch"), "luis-arch");
        assert_eq!(sanitize_instance("---"), "clipsync-host");
        assert_eq!(sanitize_instance("a"), "a");
    }

    #[test]
    fn local_ip_v4_never_returns_loopback() {
        // Se a rede está disponível, deve retornar um IP não-loopback.
        // Se não está, deve retornar None — mas nunca 127.0.0.1.
        if let Some(ip) = local_ip_v4() {
            assert!(!ip.is_loopback(), "local_ip_v4() retornou loopback: {ip}");
        }
        // None é aceitável em ambientes sem rede (CI headless).
    }

    #[test]
    fn discovered_service_socket_addrs() {
        let svc = DiscoveredService {
            fullname: "test._clipsync._tcp.local.".into(),
            instance: "test".into(),
            port: 8765,
            addrs: vec!["192.168.1.10".parse().unwrap()],
            properties: Default::default(),
        };
        let addrs = svc.socket_addrs();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 8765);
    }
}
