use crate::config::ProxyHeaderRule;
use crate::error::{Error, Result};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use std::env;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 1024 * 1024;
const MITM_CA_COMMON_NAME: &str = "Thousand Birds Agent Sandbox MITM CA";

trait ReadWrite: Read + Write {}

impl<T: Read + Write> ReadWrite for T {}

pub struct NetworkProxy {
    addr: String,
    ca_certificate_pem: Option<String>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct ProxyState {
    rules: Vec<ProxyHeaderRule>,
    tls_mitm: Option<TlsMitm>,
    client_config: Arc<ClientConfig>,
}

struct TlsMitm {
    ca_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl NetworkProxy {
    pub fn start(rules: Vec<ProxyHeaderRule>, tls_mitm: bool) -> Result<Self> {
        Self::start_with_advertise_host(rules, tls_mitm, None)
    }

    pub fn start_with_advertise_host(
        rules: Vec<ProxyHeaderRule>,
        tls_mitm: bool,
        advertise_host: Option<IpAddr>,
    ) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rules = resolve_rules(rules)?;
        let tls_mitm = if tls_mitm {
            Some(TlsMitm::new()?)
        } else {
            None
        };
        let ca_certificate_pem = tls_mitm.as_ref().map(|mitm| mitm.ca_pem.clone());
        let listen_ip = advertise_host
            .map(|host| {
                if host.is_ipv4() {
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                } else {
                    IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
                }
            })
            .unwrap_or_else(host_reachable_ip);
        let listener = TcpListener::bind((listen_ip, 0)).map_err(Error::io)?;
        listener.set_nonblocking(true).map_err(Error::io)?;
        let local_addr = listener.local_addr().map_err(Error::io)?;
        let addr = match advertise_host {
            Some(host) => format!("{}:{}", host, local_addr.port()),
            None => local_addr.to_string(),
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let state = Arc::new(ProxyState {
            rules,
            tls_mitm,
            client_config: Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(RootCertStore::from_iter(
                        webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                    ))
                    .with_no_client_auth(),
            ),
        });
        let thread = thread::spawn(move || run_proxy(listener, thread_shutdown, state));
        Ok(Self {
            addr,
            ca_certificate_pem,
            shutdown,
            thread: Some(thread),
        })
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn ca_certificate_pem(&self) -> Option<&str> {
        self.ca_certificate_pem.as_deref()
    }
}

impl TlsMitm {
    fn new() -> Result<Self> {
        let mut params = CertificateParams::new(Vec::new()).map_err(tls_error)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, MITM_CA_COMMON_NAME);
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        params.key_usages.push(KeyUsagePurpose::KeyCertSign);
        params.key_usages.push(KeyUsagePurpose::CrlSign);

        let key_pair = KeyPair::generate().map_err(tls_error)?;
        let cert = params.self_signed(&key_pair).map_err(tls_error)?;
        Ok(Self {
            ca_pem: cert.pem(),
            issuer: Issuer::new(params, key_pair),
        })
    }

    fn server_config_for_host(&self, host: &str) -> std::io::Result<Arc<ServerConfig>> {
        let host = certificate_host(host);
        let mut params =
            CertificateParams::new(vec![host.clone()]).map_err(|err| io_other(err.to_string()))?;
        params.distinguished_name.push(DnType::CommonName, host);
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);
        params.use_authority_key_identifier_extension = true;

        let key_pair = KeyPair::generate().map_err(|err| io_other(err.to_string()))?;
        let cert: Certificate = params
            .signed_by(&key_pair, &self.issuer)
            .map_err(|err| io_other(err.to_string()))?;
        let cert_chain = vec![CertificateDer::from(cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(|err| io_other(err.to_string()))?;
        Ok(Arc::new(config))
    }
}

fn host_reachable_ip() -> IpAddr {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80))?;
            socket.local_addr()
        })
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn resolve_rules(rules: Vec<ProxyHeaderRule>) -> Result<Vec<ProxyHeaderRule>> {
    let mut resolved = Vec::new();
    for mut rule in rules {
        if rule.value_from_env {
            rule.value = env::var(&rule.value).map_err(|_| {
                Error::InvalidArgument(format!(
                    "proxy header rule references unset environment variable {}",
                    rule.value
                ))
            })?;
            rule.value_from_env = false;
        }
        resolved.push(rule);
    }
    Ok(resolved)
}

impl Drop for NetworkProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(&self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_proxy(listener: TcpListener, shutdown: Arc<AtomicBool>, state: Arc<ProxyState>) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let state = state.clone();
                thread::spawn(move || {
                    let _ = handle_client(stream, &state);
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break,
        }
    }
}

fn handle_client(mut client: TcpStream, state: &ProxyState) -> std::io::Result<()> {
    let request = read_headers(&mut client)?;
    let Some(header_end) = find_header_end(&request) else {
        return Ok(());
    };
    let header_bytes = &request[..header_end];
    let body_prefix = &request[header_end + 4..];
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(());
    };
    let parts: Vec<_> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        return Ok(());
    }

    if parts[0].eq_ignore_ascii_case("CONNECT") {
        if let Some(mitm) = &state.tls_mitm {
            let (host, _) = split_host_port(parts[1], 443);
            if has_matching_rules(&host, &state.rules) {
                proxy_connect_mitm(&mut client, parts[1], mitm, state)
            } else {
                proxy_connect_tunnel(&mut client, parts[1])
            }
        } else {
            proxy_connect_tunnel(&mut client, parts[1])
        }
    } else {
        let headers: Vec<String> = lines.map(ToString::to_string).collect();
        proxy_http_to_new_upstream(
            &mut client,
            parts[0],
            parts[1],
            parts[2],
            &headers,
            body_prefix,
            80,
            &state.rules,
        )
    }
}

fn proxy_connect_tunnel(client: &mut TcpStream, authority: &str) -> std::io::Result<()> {
    let mut upstream = connect_authority(authority, 443)?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    tunnel(client, &mut upstream)
}

fn proxy_connect_mitm(
    client: &mut TcpStream,
    authority: &str,
    mitm: &TlsMitm,
    state: &ProxyState,
) -> std::io::Result<()> {
    let (host, port) = split_host_port(authority, 443);
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;

    let server_config = mitm.server_config_for_host(&host)?;
    let server_conn =
        ServerConnection::new(server_config).map_err(|err| io_other(err.to_string()))?;
    let mut client_tls = rustls::StreamOwned::new(server_conn, client.try_clone()?);

    let upstream_tcp = connect_authority(authority, port)?;
    let server_name =
        ServerName::try_from(certificate_host(&host)).map_err(|err| io_other(err.to_string()))?;
    let client_conn = ClientConnection::new(state.client_config.clone(), server_name)
        .map_err(|err| io_other(err.to_string()))?;
    let mut upstream_tls = rustls::StreamOwned::new(client_conn, upstream_tcp);

    let request = read_headers(&mut client_tls)?;
    let Some(header_end) = find_header_end(&request) else {
        return Ok(());
    };
    let header_bytes = &request[..header_end];
    let body_prefix = &request[header_end + 4..];
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(());
    };
    let parts: Vec<_> = request_line.split_whitespace().collect();
    if parts.len() < 3 {
        return Ok(());
    }
    let headers: Vec<String> = lines.map(ToString::to_string).collect();
    proxy_http_with_upstream(
        &mut client_tls,
        &mut upstream_tls,
        &host,
        parts[0],
        parts[1],
        parts[2],
        &headers,
        body_prefix,
        &state.rules,
    )?;
    let _ = upstream_tls.sock.shutdown(Shutdown::Both);
    Ok(())
}

fn read_headers<R: Read + ?Sized>(stream: &mut R) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..count]);
        if find_header_end(&buf).is_some() || buf.len() > MAX_HEADER_BYTES {
            break;
        }
    }
    Ok(buf)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

fn proxy_http_to_new_upstream(
    client: &mut dyn ReadWrite,
    method: &str,
    target: &str,
    version: &str,
    headers: &[String],
    body_prefix: &[u8],
    default_port: u16,
    rules: &[ProxyHeaderRule],
) -> std::io::Result<()> {
    let Some((host, port, path)) = parse_http_target(target, headers, default_port) else {
        client.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    };
    let mut upstream = connect_authority(&format!("{host}:{port}"), port)?;
    proxy_http_with_upstream(
        client,
        &mut upstream,
        &host,
        method,
        &path,
        version,
        headers,
        body_prefix,
        rules,
    )
}

fn proxy_http_with_upstream(
    client: &mut dyn ReadWrite,
    upstream: &mut dyn ReadWrite,
    rule_host: &str,
    method: &str,
    path: &str,
    version: &str,
    headers: &[String],
    body_prefix: &[u8],
    rules: &[ProxyHeaderRule],
) -> std::io::Result<()> {
    let matching_rules = matching_rules(rule_host, rules);
    let mut request = format!("{method} {path} {version}\r\n");
    for header in headers {
        if header.is_empty() || should_drop_header(header, &matching_rules) {
            continue;
        }
        if is_hop_by_hop_header(header) {
            continue;
        }
        request.push_str(header);
        request.push_str("\r\n");
    }
    for rule in matching_rules {
        request.push_str(&rule.header);
        request.push_str(": ");
        request.push_str(&rule.value);
        request.push_str("\r\n");
    }
    request.push_str("Connection: close\r\n");
    request.push_str("\r\n");
    upstream.write_all(request.as_bytes())?;
    forward_request_body(client, upstream, headers, body_prefix)?;
    upstream.flush()?;
    std::io::copy(upstream, client)?;
    Ok(())
}

fn parse_http_target(
    target: &str,
    headers: &[String],
    default_port: u16,
) -> Option<(String, u16, String)> {
    if let Some(rest) = target.strip_prefix("http://") {
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = split_host_port(authority, 80);
        return Some((host, port, path.to_string()));
    }
    if let Some(rest) = target.strip_prefix("https://") {
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let (host, port) = split_host_port(authority, 443);
        return Some((host, port, path.to_string()));
    }

    let host = headers.iter().find_map(|header| {
        let (name, value) = header.split_once(':')?;
        name.eq_ignore_ascii_case("host")
            .then(|| value.trim().to_string())
    })?;
    let (host, port) = split_host_port(&host, default_port);
    Some((host, port, target.to_string()))
}

fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    if let Some(host) = authority.strip_prefix('[') {
        if let Some((host, rest)) = host.split_once(']') {
            let port = rest
                .strip_prefix(':')
                .and_then(|port| port.parse::<u16>().ok())
                .unwrap_or(default_port);
            return (host.to_string(), port);
        }
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (
            host.to_string(),
            port.parse::<u16>().unwrap_or(default_port),
        ),
        _ => (authority.to_string(), default_port),
    }
}

fn certificate_host(host: &str) -> String {
    host.trim_matches(&['[', ']'][..])
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn connect_authority(authority: &str, default_port: u16) -> std::io::Result<TcpStream> {
    let (host, port) = split_host_port(authority, default_port);
    (host.as_str(), port)
        .to_socket_addrs()?
        .find_map(|addr| TcpStream::connect(addr).ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected, authority))
}

fn matching_rules<'a>(host: &str, rules: &'a [ProxyHeaderRule]) -> Vec<&'a ProxyHeaderRule> {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    rules
        .iter()
        .filter(|rule| {
            let rule_host = rule.host.trim_end_matches('.').to_ascii_lowercase();
            rule_host == "*" || rule_host == normalized
        })
        .collect()
}

fn has_matching_rules(host: &str, rules: &[ProxyHeaderRule]) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    rules.iter().any(|rule| {
        let rule_host = rule.host.trim_end_matches('.').to_ascii_lowercase();
        rule_host == "*" || rule_host == normalized
    })
}

fn should_drop_header(header: &str, rules: &[&ProxyHeaderRule]) -> bool {
    let Some((name, _)) = header.split_once(':') else {
        return false;
    };
    rules
        .iter()
        .any(|rule| rule.header.eq_ignore_ascii_case(name.trim()))
}

fn is_hop_by_hop_header(header: &str) -> bool {
    let Some((name, _)) = header.split_once(':') else {
        return false;
    };
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "connection" | "proxy-connection" | "keep-alive" | "te" | "trailer" | "upgrade"
    )
}

fn forward_request_body(
    client: &mut dyn ReadWrite,
    upstream: &mut dyn Write,
    headers: &[String],
    body_prefix: &[u8],
) -> std::io::Result<()> {
    if let Some(length) = content_length(headers) {
        let prefix_len = body_prefix.len().min(length);
        upstream.write_all(&body_prefix[..prefix_len])?;
        let mut remaining = length - prefix_len;
        let mut buf = [0u8; 8192];
        while remaining > 0 {
            let read_len = buf.len().min(remaining);
            let count = client.read(&mut buf[..read_len])?;
            if count == 0 {
                break;
            }
            upstream.write_all(&buf[..count])?;
            remaining -= count;
        }
    } else if is_chunked(headers) {
        upstream.write_all(body_prefix)?;
        copy_chunked_body(client, upstream, body_prefix)?;
    }
    Ok(())
}

fn content_length(headers: &[String]) -> Option<usize> {
    headers.iter().find_map(|header| {
        let (name, value) = header.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })
}

fn is_chunked(headers: &[String]) -> bool {
    headers.iter().any(|header| {
        let Some((name, value)) = header.split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
    })
}

fn copy_chunked_body(
    client: &mut dyn Read,
    upstream: &mut dyn Write,
    already_forwarded: &[u8],
) -> std::io::Result<()> {
    if chunked_body_complete(already_forwarded) {
        return Ok(());
    }
    let mut seen = already_forwarded.to_vec();
    let mut buf = [0u8; 8192];
    while !chunked_body_complete(&seen) {
        let count = client.read(&mut buf)?;
        if count == 0 {
            break;
        }
        upstream.write_all(&buf[..count])?;
        seen.extend_from_slice(&buf[..count]);
        if seen.len() > MAX_HEADER_BYTES {
            let keep_from = seen.len() - MAX_HEADER_BYTES;
            seen.drain(..keep_from);
        }
    }
    Ok(())
}

fn chunked_body_complete(buf: &[u8]) -> bool {
    buf.windows(5).any(|window| window == b"\r\n0\r\n")
        || buf.windows(7).any(|window| window == b"\r\n0\r\n\r\n")
}

fn tunnel(a: &mut TcpStream, b: &mut TcpStream) -> std::io::Result<()> {
    let mut a_read = a.try_clone()?;
    let mut a_write = a.try_clone()?;
    let mut b_read = b.try_clone()?;
    let mut b_write = b.try_clone()?;

    let left = thread::spawn(move || {
        let _ = std::io::copy(&mut a_read, &mut b_write);
        let _ = b_write.shutdown(Shutdown::Write);
    });
    let right = thread::spawn(move || {
        let _ = std::io::copy(&mut b_read, &mut a_write);
        let _ = a_write.shutdown(Shutdown::Write);
    });
    let _ = left.join();
    let _ = right.join();
    Ok(())
}

fn tls_error(error: rcgen::Error) -> Error {
    Error::InvalidArgument(format!("failed to create TLS MITM certificate: {error}"))
}

fn io_other(message: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_targets_with_default_port() {
        let headers = vec!["Host: api.example.test".to_string()];
        assert_eq!(
            parse_http_target("/v1/messages", &headers, 443),
            Some((
                "api.example.test".to_string(),
                443,
                "/v1/messages".to_string()
            ))
        );
        assert_eq!(
            parse_http_target("https://api.example.test:8443/v1", &[], 80),
            Some(("api.example.test".to_string(), 8443, "/v1".to_string()))
        );
    }

    #[test]
    fn strips_hop_by_hop_headers() {
        assert!(is_hop_by_hop_header("Connection: keep-alive"));
        assert!(is_hop_by_hop_header("Proxy-Connection: keep-alive"));
        assert!(!is_hop_by_hop_header("Authorization: token"));
    }

    #[test]
    fn creates_mitm_ca() {
        let ca = TlsMitm::new().expect("mitm ca");
        assert!(ca.ca_pem.contains("BEGIN CERTIFICATE"));
    }
}
