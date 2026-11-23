// SPDX-License-Identifier: GPL-3.0-only
//! Upstream transports: UDP, TCP and DoT.
//!
//! Only IP literals are accepted (`udp://1.2.3.4[:53]`, `tcp://...`,
//! `tls://1.2.3.4[:853]`). Host names are rejected with
//! [`CoreError::Unsupported`]; bootstrap resolution is a later task.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hickory_proto::ProtoError;
use hickory_proto::op::Message;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

use crate::Upstream;
use crate::error::{CoreError, Result};

/// Default plain DNS port for `udp://` and `tcp://`.
pub const DEFAULT_DNS_PORT: u16 = 53;
/// Default DNS-over-TLS port for `tls://`.
pub const DEFAULT_TLS_PORT: u16 = 853;
/// Default per-query exchange timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum DNS message size (2-byte TCP framing limit).
const MAX_MESSAGE_SIZE: usize = u16::MAX as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Udp,
    Tcp,
    Tls,
}

/// A parsed upstream address: transport scheme plus the target socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamAddr {
    pub scheme: Scheme,
    pub addr: SocketAddr,
}

/// Parse `scheme://ip[:port]`.
///
/// Host names are intentionally rejected. IPv6 literals may be bracketed
/// (`tls://[2001:db8::1]:853`) or bare (`tls://2001:db8::1`).
pub fn parse_upstream_addr(raw: &str) -> Result<UpstreamAddr> {
    let (scheme, rest) = raw.split_once("://").ok_or_else(|| {
        CoreError::Unsupported(format!(
            "upstream {raw:?} must use udp://, tcp:// or tls://"
        ))
    })?;

    let scheme = match scheme {
        "udp" => Scheme::Udp,
        "tcp" => Scheme::Tcp,
        "tls" => Scheme::Tls,
        other => {
            return Err(CoreError::Unsupported(format!(
                "upstream scheme {other:?} is not supported (use udp/tcp/tls)"
            )));
        }
    };

    let default_port = match scheme {
        Scheme::Udp | Scheme::Tcp => DEFAULT_DNS_PORT,
        Scheme::Tls => DEFAULT_TLS_PORT,
    };

    let addr = parse_socket_addr(rest, default_port)?;
    Ok(UpstreamAddr { scheme, addr })
}

fn parse_socket_addr(host: &str, default_port: u16) -> Result<SocketAddr> {
    if host.is_empty() {
        return Err(CoreError::Unsupported("upstream address is empty".into()));
    }

    // `1.2.3.4:5353` and `[2001:db8::1]:853` parse directly.
    if let Ok(addr) = SocketAddr::from_str(host) {
        return Ok(addr);
    }

    // `1.2.3.4` and `[2001:db8::1]` (no port) fall back to the default port.
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = IpAddr::from_str(bare) {
        return Ok(SocketAddr::new(ip, default_port));
    }

    Err(CoreError::Unsupported(format!(
        "upstream address {host:?} is not an IP literal (bootstrap is not implemented yet)"
    )))
}

/// Build a transport for `addr`.
///
/// `verify` only affects `tls://`; `false` installs a certificate verifier
/// that accepts any certificate and is intended for tests/dev only.
pub fn build_upstream(addr: &str, verify: bool, timeout: Duration) -> Result<Arc<dyn Upstream>> {
    let parsed = parse_upstream_addr(addr)?;
    let timeout = if timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        timeout
    };

    match parsed.scheme {
        Scheme::Udp => Ok(Arc::new(UdpUpstream {
            addr: parsed.addr,
            timeout,
        })),
        Scheme::Tcp => Ok(Arc::new(TcpUpstream {
            addr: parsed.addr,
            timeout,
        })),
        Scheme::Tls => {
            let config = tls_client_config(verify)?;
            let server_name =
                ServerName::IpAddress(rustls::pki_types::IpAddr::from(parsed.addr.ip()));
            Ok(Arc::new(DotUpstream {
                addr: parsed.addr,
                server_name,
                connector: TlsConnector::from(config),
                timeout,
            }))
        }
    }
}

/// Build the rustls client configuration used by DoT.
fn tls_client_config(verify: bool) -> Result<Arc<ClientConfig>> {
    let config = if verify {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    } else {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification::new()))
            .with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// Accept-anything verifier for `verify=false`. Test/dev only.
struct NoCertificateVerification {
    provider: CryptoProvider,
}

impl NoCertificateVerification {
    fn new() -> Self {
        Self {
            provider: rustls::crypto::ring::default_provider(),
        }
    }
}

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl fmt::Debug for NoCertificateVerification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NoCertificateVerification")
    }
}

/// Plain UDP: one fresh socket (and random source port) per query.
pub struct UdpUpstream {
    addr: SocketAddr,
    timeout: Duration,
}

#[async_trait]
impl Upstream for UdpUpstream {
    async fn exchange(&self, query: &Message) -> Result<Message> {
        let bytes = query.to_vec()?;
        let addr = self.addr;
        let exchange = async move {
            let bind = match addr {
                SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
                SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
            };
            let socket = UdpSocket::bind(bind).await?;
            socket.connect(addr).await?;
            let sent = socket.send(&bytes).await?;
            if sent != bytes.len() {
                return Err(CoreError::Upstream(format!(
                    "udp {addr}: short write ({sent}/{} bytes)",
                    bytes.len()
                )));
            }
            let mut buf = vec![0u8; MAX_MESSAGE_SIZE];
            let len = socket.recv(&mut buf).await?;
            decode_message(&buf[..len])
        };

        match timeout(self.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(CoreError::Upstream(format!(
                "udp {} timed out after {:?}",
                self.addr, self.timeout
            ))),
        }
    }
}

/// Plain TCP with 2-byte length framing.
pub struct TcpUpstream {
    addr: SocketAddr,
    timeout: Duration,
}

#[async_trait]
impl Upstream for TcpUpstream {
    async fn exchange(&self, query: &Message) -> Result<Message> {
        let bytes = query.to_vec()?;
        let addr = self.addr;
        let exchange = async move {
            let mut stream = TcpStream::connect(addr).await?;
            let _ = stream.set_nodelay(true);
            exchange_framed(&mut stream, &bytes).await
        };

        match timeout(self.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(CoreError::Upstream(format!(
                "tcp {} timed out after {:?}",
                self.addr, self.timeout
            ))),
        }
    }
}

/// DNS-over-TLS with 2-byte length framing.
pub struct DotUpstream {
    addr: SocketAddr,
    server_name: ServerName<'static>,
    connector: TlsConnector,
    timeout: Duration,
}

#[async_trait]
impl Upstream for DotUpstream {
    async fn exchange(&self, query: &Message) -> Result<Message> {
        let bytes = query.to_vec()?;
        let addr = self.addr;
        let server_name = self.server_name.clone();
        let connector = self.connector.clone();
        let exchange = async move {
            let stream = TcpStream::connect(addr).await?;
            let _ = stream.set_nodelay(true);
            let mut tls = connector
                .connect(server_name, stream)
                .await
                .map_err(|err| {
                    CoreError::Upstream(format!("tls handshake with {addr} failed: {err}"))
                })?;
            exchange_framed(&mut tls, &bytes).await
        };

        match timeout(self.timeout, exchange).await {
            Ok(result) => result,
            Err(_) => Err(CoreError::Upstream(format!(
                "tls {} timed out after {:?}",
                self.addr, self.timeout
            ))),
        }
    }
}

/// Write a 2-byte length-prefixed query and read the framed reply.
async fn exchange_framed<S>(stream: &mut S, bytes: &[u8]) -> Result<Message>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let len = u16::try_from(bytes.len()).map_err(|_| {
        CoreError::Upstream(format!("query too large to frame: {} bytes", bytes.len()))
    })?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;

    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(CoreError::Upstream("empty TCP/TLS response frame".into()));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    decode_message(&buf)
}

fn decode_message(bytes: &[u8]) -> Result<Message> {
    Message::from_vec(bytes).map_err(|err| CoreError::Dns(ProtoError::from(err)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ip_literals_and_default_ports() {
        let parsed = parse_upstream_addr("udp://1.2.3.4").unwrap();
        assert_eq!(parsed.scheme, Scheme::Udp);
        assert_eq!(parsed.addr, SocketAddr::from(([1, 2, 3, 4], 53)));

        let parsed = parse_upstream_addr("tcp://1.2.3.4:5353").unwrap();
        assert_eq!(parsed.scheme, Scheme::Tcp);
        assert_eq!(parsed.addr.port(), 5353);

        let parsed = parse_upstream_addr("tls://8.8.8.8").unwrap();
        assert_eq!(parsed.scheme, Scheme::Tls);
        assert_eq!(parsed.addr.port(), 853);

        let parsed = parse_upstream_addr("tls://[2001:db8::1]").unwrap();
        assert_eq!(parsed.addr.ip(), IpAddr::from_str("2001:db8::1").unwrap());
        assert_eq!(parsed.addr.port(), 853);
    }

    #[test]
    fn rejects_hostnames_and_unknown_schemes() {
        for raw in [
            "udp://dns.google",
            "tls://dns.google:853",
            "https://1.2.3.4",
            "1.2.3.4",
            "udp://",
        ] {
            let err = parse_upstream_addr(raw).unwrap_err();
            assert!(
                matches!(err, CoreError::Unsupported(_)),
                "{raw}: unexpected error {err}"
            );
        }
    }

    #[test]
    fn builds_all_transports_including_dangerous_tls() {
        let timeout = Duration::from_millis(500);
        assert!(build_upstream("udp://127.0.0.1", true, timeout).is_ok());
        assert!(build_upstream("tcp://127.0.0.1", true, timeout).is_ok());
        assert!(build_upstream("tls://127.0.0.1", true, timeout).is_ok());
        assert!(build_upstream("tls://127.0.0.1", false, timeout).is_ok());
        assert!(build_upstream("tls://dns.google", true, timeout).is_err());
    }
}
