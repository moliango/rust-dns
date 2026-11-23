// SPDX-License-Identifier: GPL-3.0-only
//! TCP latency and HTTPS download probes.
//!
//! The download path deliberately avoids a full HTTP client: it opens the TCP
//! socket itself (so `SO_MARK`/`SO_BINDTODEVICE`/source binding apply to the
//! actual transfer socket), performs a TLS handshake with rustls, then issues a
//! minimal HTTP/1.1 GET and counts decrypted body bytes against a byte/time
//! budget.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;

use crate::ip_source::{simulated_latency_ms, simulated_loss_pct, simulated_speed_mbps};
use crate::types::{Family, summarize_latency};

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Socket identity applied to every probe connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeSocketOptions {
    /// `SO_MARK` (Linux only; needs `CAP_NET_ADMIN`).
    pub mark: Option<u32>,
    /// `SO_BINDTODEVICE` interface name.
    pub device: Option<String>,
    /// Local source address to bind.
    pub source_ip: Option<IpAddr>,
}

impl ProbeSocketOptions {
    pub fn is_empty(&self) -> bool {
        self.mark.is_none() && self.device.is_none() && self.source_ip.is_none()
    }
}

/// One latency measurement attempt per call; failures are `None` samples.
pub trait LatencyProbe: Send + Sync {
    fn probe(&self, ip: IpAddr) -> BoxFuture<'_, Vec<Option<Duration>>>;
}

/// One download measurement; the returned value is Mbps.
pub trait DownloadProbe: Send + Sync {
    fn measure(&self, ip: IpAddr) -> BoxFuture<'_, Result<f64>>;
}

/// Real TCP handshake probe.
pub struct TcpLatencyProbe {
    port: u16,
    samples: u32,
    timeout: Duration,
    options: ProbeSocketOptions,
}

impl TcpLatencyProbe {
    pub fn new(port: u16, samples: u32, timeout: Duration, options: ProbeSocketOptions) -> Self {
        Self {
            port,
            samples,
            timeout,
            options,
        }
    }
}

impl LatencyProbe for TcpLatencyProbe {
    fn probe(&self, ip: IpAddr) -> BoxFuture<'_, Vec<Option<Duration>>> {
        Box::pin(async move {
            let addr = SocketAddr::new(ip, self.port);
            let mut samples = Vec::with_capacity(self.samples as usize);
            for _ in 0..self.samples {
                let started = Instant::now();
                match connect_with_options(addr, &self.options, self.timeout).await {
                    Ok(_stream) => samples.push(Some(started.elapsed())),
                    Err(_) => samples.push(None),
                }
            }
            samples
        })
    }
}

/// Open a TCP connection with mark/device/source options and a deadline.
pub async fn connect_with_options(
    addr: SocketAddr,
    options: &ProbeSocketOptions,
    timeout: Duration,
) -> io::Result<TcpStream> {
    if let Some(source) = options.source_ip
        && source.is_ipv4() != addr.is_ipv4()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source address family does not match probe address",
        ));
    }

    let domain = socket2::Domain::for_address(addr);
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    apply_socket_options(&socket, addr, options)?;
    socket.set_nonblocking(true)?;

    match socket.connect(&addr.into()) {
        Ok(()) => {}
        Err(err)
            if err.kind() == io::ErrorKind::WouldBlock
                || err.raw_os_error() == Some(libc::EINPROGRESS) => {}
        Err(err) => return Err(err),
    }

    let std_stream = std::net::TcpStream::from(socket);
    // SAFETY: the descriptor stays owned by `afd` until `into_inner` below and
    // is never closed or replaced while registered.
    let afd = unsafe { tokio::io::unix::AsyncFd::register(std_stream)? };
    let wait_connected = async {
        let guard = afd.writable().await?;
        if let Some(err) = guard.get_inner().take_error()? {
            return Err(err);
        }
        Ok(())
    };
    let result: io::Result<()> = match tokio::time::timeout(timeout, wait_connected).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
    };
    result?;

    TcpStream::from_std(afd.into_inner())
}

fn apply_socket_options(
    socket: &socket2::Socket,
    addr: SocketAddr,
    options: &ProbeSocketOptions,
) -> io::Result<()> {
    let _ = addr;
    if let Some(source) = options.source_ip {
        socket.bind(&SocketAddr::new(source, 0).into())?;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Some(mark) = options.mark {
            socket.set_mark(mark)?;
        }
        if let Some(device) = &options.device {
            socket.bind_device(Some(device.as_bytes()))?;
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        if options.mark.is_some() || options.device.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("mark/device socket options are unsupported on this platform for {addr}"),
            ));
        }
    }
    Ok(())
}

/// Parsed download URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadUrl {
    pub scheme: DownloadScheme,
    pub host: String,
    pub port: u16,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadScheme {
    Http,
    Https,
}

impl DownloadUrl {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("download URL is empty");
        }
        if input.chars().any(|c| c.is_control() || c.is_whitespace()) {
            bail!("download URL contains whitespace or control characters");
        }
        let (scheme, rest) = input
            .split_once("://")
            .context("download URL must include a scheme")?;
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "https" => DownloadScheme::Https,
            "http" => DownloadScheme::Http,
            other => bail!("unsupported download URL scheme {other:?}"),
        };
        let rest = rest.split('#').next().unwrap_or(rest);
        let (authority, path) = match rest.split_once('/') {
            Some((authority, tail)) => (authority, format!("/{tail}")),
            None => (rest, "/".to_string()),
        };
        if authority.is_empty() {
            bail!("download URL has no host");
        }
        if authority.contains('@') {
            bail!("download URL must not contain userinfo");
        }
        let (host, explicit_port) = split_host_port(authority)?;
        validate_url_host(&host)?;
        let port = match explicit_port {
            Some(port) => port,
            None => match scheme {
                DownloadScheme::Https => 443,
                DownloadScheme::Http => 80,
            },
        };
        let path = if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        };
        Ok(Self {
            scheme,
            host,
            port,
            path,
        })
    }

    pub fn is_tls(&self) -> bool {
        self.scheme == DownloadScheme::Https
    }

    /// `Host` header, including brackets for IPv6 and a non-default port.
    pub fn host_header(&self) -> String {
        let default = if self.is_tls() { 443 } else { 80 };
        let host = if self.host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

fn split_host_port(authority: &str) -> Result<(String, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .context("unterminated IPv6 literal in URL")?;
        let port = match after.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if after.is_empty() => None,
            None => bail!("invalid characters after IPv6 literal"),
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            Ok((host.to_string(), Some(parse_port(port)?)))
        }
        _ => Ok((authority.to_string(), None)),
    }
}

fn parse_port(input: &str) -> Result<u16> {
    if input.is_empty() {
        bail!("download URL has an empty port");
    }
    input
        .parse::<u16>()
        .with_context(|| format!("invalid download URL port {input:?}"))
}

fn validate_url_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("download URL has an empty host");
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !crate::ip_source::is_public_ip(&ip) {
            bail!("download URL host {ip} is not a public address");
        }
        return Ok(());
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".local") {
        bail!("download URL host {host:?} is not routable");
    }
    if !host.is_ascii() {
        bail!("download URL host must be ASCII (IDNA punycode form)");
    }
    if host.len() > 253 {
        bail!("download URL host is too long");
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            bail!("download URL host has an invalid label");
        }
        if label.starts_with('-') || label.ends_with('-') {
            bail!("download URL host label starts or ends with '-'");
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("download URL host has an invalid character");
        }
    }
    Ok(())
}

/// Real HTTPS (or HTTP) download probe.
pub struct HttpDownloadProbe {
    url: DownloadUrl,
    options: ProbeSocketOptions,
    connect_timeout: Duration,
    time_limit: Duration,
    byte_limit: u64,
    tls: Arc<rustls::ClientConfig>,
}

impl HttpDownloadProbe {
    pub fn new(
        url: DownloadUrl,
        options: ProbeSocketOptions,
        connect_timeout: Duration,
        time_limit: Duration,
        byte_limit: u64,
    ) -> Result<Self> {
        if time_limit.is_zero() {
            bail!("download time limit must be greater than zero");
        }
        if byte_limit == 0 {
            bail!("download byte limit must be greater than zero");
        }
        let tls = build_tls_client()?;
        Ok(Self {
            url,
            options,
            connect_timeout,
            time_limit,
            byte_limit,
            tls,
        })
    }
}

/// Build a rustls client with the Mozilla root store (via `webpki-roots`).
///
/// The crypto provider is installed once per process; repeated calls are
/// harmless because the error from a second install is ignored.
pub fn build_tls_client() -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// Upper bound for a single helper GET (range lists and trace responses are
/// small; this prevents an untrusted peer from exhausting memory).
const MAX_HELPER_BODY: u64 = 1024 * 1024;

/// Minimal HTTPS/HTTP GET that returns the response body.
///
/// `addr` is the resolved peer, `server_name` is used for TLS SNI and
/// certificate verification, and `host_header` goes into the request. Socket
/// options (mark/device/source) are applied exactly as for probes, so the
/// bypass identity is validated end to end.
pub async fn https_get_body(
    addr: SocketAddr,
    tls_name: &str,
    host_header: &str,
    path: &str,
    options: &ProbeSocketOptions,
    timeout: Duration,
    tls: &Arc<rustls::ClientConfig>,
) -> Result<Vec<u8>> {
    if !path.starts_with('/') {
        bail!("HTTP request path must start with '/'");
    }
    let tcp = connect_with_options(addr, options, timeout)
        .await
        .with_context(|| format!("connect to {addr}"))?;
    let name = server_name(tls_name)?;
    let connector = TlsConnector::from(tls.clone());
    let mut stream = tokio::time::timeout(timeout, connector.connect(name, tcp))
        .await
        .map_err(|_| anyhow!("TLS handshake with {addr} timed out"))?
        .with_context(|| format!("TLS handshake with {addr}"))?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: rust-dns-cf-optimizer/0.1\r\n\
         Accept: text/plain, */*\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    tokio::time::timeout(
        timeout,
        (&mut stream).take(MAX_HELPER_BODY).read_to_end(&mut raw),
    )
    .await
    .map_err(|_| anyhow!("read from {addr} timed out"))??;
    split_http_response(&raw)
}

/// Strip and validate response headers, then return the (decoded) body.
fn split_http_response(raw: &[u8]) -> Result<Vec<u8>> {
    let header_end =
        find_header_end(raw).context("connection closed before HTTP response headers completed")?;
    let header = &raw[..header_end + 4];
    validate_status(header)?;
    let body = &raw[header_end + 4..];
    if header_is_chunked(header) {
        decode_chunked(body)
    } else {
        Ok(body.to_vec())
    }
}

fn header_is_chunked(header: &[u8]) -> bool {
    String::from_utf8_lossy(header).lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    })
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("truncated chunked body")?;
        let size_line =
            std::str::from_utf8(&data[..line_end]).context("chunk size is not valid ASCII")?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .with_context(|| format!("invalid chunk size {size_line:?}"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            bail!("truncated chunked body");
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if let Some(rest) = data.strip_prefix(b"\r\n".as_slice()) {
            data = rest;
        }
    }
    Ok(out)
}

impl DownloadProbe for HttpDownloadProbe {
    fn measure(&self, ip: IpAddr) -> BoxFuture<'_, Result<f64>> {
        Box::pin(async move {
            let addr = SocketAddr::new(ip, self.url.port);
            let tcp = connect_with_options(addr, &self.options, self.connect_timeout)
                .await
                .with_context(|| format!("connect to {addr} for download test"))?;
            let (body_bytes, elapsed) = if self.url.is_tls() {
                let server_name = server_name(&self.url.host)?;
                let connector = TlsConnector::from(self.tls.clone());
                let mut tls =
                    tokio::time::timeout(self.time_limit, connector.connect(server_name, tcp))
                        .await
                        .map_err(|_| anyhow!("TLS handshake to {ip} timed out"))?
                        .with_context(|| format!("TLS handshake to {ip}"))?;
                transfer(&mut tls, &self.url, self.time_limit, self.byte_limit).await?
            } else {
                let mut tcp = tcp;
                transfer(&mut tcp, &self.url, self.time_limit, self.byte_limit).await?
            };
            if body_bytes == 0 {
                bail!("download from {ip} returned no body bytes");
            }
            let seconds = elapsed.as_secs_f64().max(0.001);
            let mbps = body_bytes as f64 * 8.0 / seconds / 1_000_000.0;
            Ok((mbps * 100.0).round() / 100.0)
        })
    }
}

fn server_name(host: &str) -> Result<rustls::pki_types::ServerName<'static>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(rustls::pki_types::ServerName::IpAddress(ip.into()));
    }
    rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| anyhow!("invalid TLS server name {host:?}"))
}

async fn transfer<S>(
    stream: &mut S,
    url: &DownloadUrl,
    time_limit: Duration,
    byte_limit: u64,
) -> Result<(u64, Duration)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rust-dns-cf-optimizer/0.1\r\n\
         Accept: */*\r\nConnection: close\r\n\r\n",
        url.path,
        url.host_header()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let started = Instant::now();
    let deadline = started + time_limit;
    let mut buf = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    let mut header = Vec::new();
    let mut header_len: Option<u64> = None;

    loop {
        if total >= byte_limit {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let want = ((byte_limit - total) as usize).min(buf.len());
        match tokio::time::timeout(remaining, stream.read(&mut buf[..want])).await {
            Err(_) => break, // time budget reached; keep the partial measurement
            Ok(Ok(0)) => break,
            Ok(Ok(read)) => {
                if header_len.is_none() {
                    header.extend_from_slice(&buf[..read]);
                    if let Some(pos) = find_header_end(&header) {
                        validate_status(&header)?;
                        header_len = Some((pos + 4) as u64);
                    } else if header.len() > 16 * 1024 {
                        bail!("HTTP response headers are too large");
                    }
                }
                total += read as u64;
            }
            Ok(Err(err)) => {
                if header_len.is_none() {
                    return Err(err).context("read HTTP response");
                }
                break;
            }
        }
    }

    let header_len = header_len
        .ok_or_else(|| anyhow!("connection closed before HTTP response headers completed"))?;
    Ok((total.saturating_sub(header_len), started.elapsed()))
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn validate_status(header: &[u8]) -> Result<()> {
    let text = String::from_utf8_lossy(header);
    let first_line = text.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let status = parts.next().and_then(|s| s.parse::<u16>().ok());
    if !version.starts_with("HTTP/") {
        bail!("invalid HTTP status line {first_line:?}");
    }
    match status {
        Some(200 | 206) => Ok(()),
        Some(code) => bail!("download URL returned HTTP status {code}"),
        None => bail!("invalid HTTP status line {first_line:?}"),
    }
}

/// Deterministic latency probe used by `--dry-run`.
pub struct DryRunLatencyProbe {
    family: Family,
    samples: u32,
}

impl DryRunLatencyProbe {
    pub fn new(family: Family, samples: u32) -> Self {
        Self { family, samples }
    }
}

impl LatencyProbe for DryRunLatencyProbe {
    fn probe(&self, ip: IpAddr) -> BoxFuture<'_, Vec<Option<Duration>>> {
        let _ = self.family; // fixture shape is encoded in the IP itself
        let samples = self.samples;
        Box::pin(async move {
            let base = simulated_latency_ms(ip);
            let loss = simulated_loss_pct(ip);
            let ok = (((100.0 - loss) / 100.0) * f64::from(samples))
                .round()
                .clamp(0.0, f64::from(samples)) as u32;
            let mut out = Vec::with_capacity(samples as usize);
            for index in 0..samples {
                if index < ok {
                    // Deterministic jitter keeps min and avg distinct.
                    let jitter = f64::from(index % 3) * 0.4;
                    out.push(Some(Duration::from_secs_f64((base + jitter) / 1000.0)));
                } else {
                    out.push(None);
                }
            }
            out
        })
    }
}

/// Deterministic download probe used by `--dry-run`.
pub struct DryRunDownloadProbe;

impl DownloadProbe for DryRunDownloadProbe {
    fn measure(&self, ip: IpAddr) -> BoxFuture<'_, Result<f64>> {
        Box::pin(async move { Ok(simulated_speed_mbps(ip)) })
    }
}

/// Convenience used by callers that want stats without the pipeline.
pub fn dry_run_stats(ip: IpAddr, samples: u32) -> crate::types::LatencyStats {
    let base = simulated_latency_ms(ip);
    let loss = simulated_loss_pct(ip);
    let ok = (((100.0 - loss) / 100.0) * f64::from(samples))
        .round()
        .clamp(0.0, f64::from(samples)) as usize;
    let mut values = Vec::with_capacity(samples as usize);
    for index in 0..samples as usize {
        values.push(if index < ok {
            Some(Duration::from_secs_f64(
                (base + (index % 3) as f64 * 0.4) / 1000.0,
            ))
        } else {
            None
        });
    }
    summarize_latency(&values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_url_parses_https_and_default_port() {
        let url = DownloadUrl::parse("https://speed.cloudflare.com/__down?bytes=1000").unwrap();
        assert_eq!(url.scheme, DownloadScheme::Https);
        assert_eq!(url.host, "speed.cloudflare.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.path, "/__down?bytes=1000");
        assert_eq!(url.host_header(), "speed.cloudflare.com");
    }

    #[test]
    fn download_url_parses_explicit_port_and_ipv6() {
        let url = DownloadUrl::parse("http://[2606:4700:4700::1111]:8080/trace").unwrap();
        assert_eq!(url.host, "2606:4700:4700::1111");
        assert_eq!(url.port, 8080);
        assert_eq!(url.host_header(), "[2606:4700:4700::1111]:8080");
    }

    #[test]
    fn download_url_rejects_bad_inputs() {
        for bad in [
            "ftp://speed.cloudflare.com/x",
            "https://localhost/x",
            "https://127.0.0.1/x",
            "https://192.168.1.1/x",
            "https:///x",
            "https://speed.cloudflare.com:notaport/x",
            "https://user@speed.cloudflare.com/x",
            "https://speed.cloudflare.com/a b",
        ] {
            assert!(DownloadUrl::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn dry_run_probe_math_is_deterministic() {
        let ip: IpAddr = "104.16.0.1".parse().unwrap();
        let stats = dry_run_stats(ip, 4);
        assert!(stats.avg_ms.is_some());
        assert_eq!(stats.loss_pct, 0.0);

        let lossy: IpAddr = "104.16.0.8".parse().unwrap();
        let stats = dry_run_stats(lossy, 4);
        assert_eq!(stats.loss_pct, 25.0);
    }

    #[tokio::test]
    async fn dry_run_probe_returns_configured_sample_count() {
        let probe = DryRunLatencyProbe::new(Family::V4, 5);
        let ip: IpAddr = "104.16.0.9".parse().unwrap();
        let samples = probe.probe(ip).await;
        assert_eq!(samples.len(), 5);
        assert!(samples.iter().all(Option::is_none));
    }

    #[test]
    fn status_validation_accepts_only_success() {
        assert!(validate_status(b"HTTP/1.1 200 OK\r\n\r\n").is_ok());
        assert!(validate_status(b"HTTP/1.1 206 Partial Content\r\n\r\n").is_ok());
        assert!(validate_status(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
        assert!(validate_status(b"garbage\r\n\r\n").is_err());
    }

    #[test]
    fn header_end_detection() {
        assert_eq!(find_header_end(b"HTTP/1.1 200 OK\r\n\r\nbody"), Some(15));
        assert_eq!(find_header_end(b"incomplete"), None);
    }

    #[test]
    fn http_body_helper_decodes_plain_and_chunked_responses() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody";
        assert_eq!(split_http_response(raw).unwrap(), b"body");

        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(split_http_response(chunked).unwrap(), b"abcde");

        assert!(split_http_response(b"HTTP/1.1 500 Oops\r\n\r\n").is_err());
        assert!(split_http_response(b"not-http").is_err());
    }

    #[tokio::test]
    async fn transfer_counts_only_the_response_body() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let responder = tokio::spawn(async move {
            server
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                .await
                .unwrap();
        });
        let url = DownloadUrl::parse("https://speed.cloudflare.com/__down?bytes=5").unwrap();
        let (body, _elapsed) = transfer(&mut client, &url, Duration::from_secs(2), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(body, 5);
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn transfer_rejects_non_success_status() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let responder = tokio::spawn(async move {
            server
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let url = DownloadUrl::parse("https://speed.cloudflare.com/missing").unwrap();
        let err = transfer(&mut client, &url, Duration::from_secs(2), 1024 * 1024)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
        responder.await.unwrap();
    }
}
