use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use hyper::body::Incoming;
use hyper::{Request, Response};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::transport::TransportContext;
use crate::transport::lifecycle::wait_for_shutdown;
use crate::transport::tls::mitm_intercept;

/// First byte of a TLS record carrying a handshake (ClientHello). Used to tell
/// a real TLS connection from a cleartext CONNECT tunnel (e.g. `ws://`).
const TLS_HANDSHAKE_RECORD: u8 = 0x16;

/// Wraps a stream so that a previously-read prefix is replayed before the inner
/// bytes. Lets us peek the first byte to detect TLS and then hand the *whole*
/// stream (prefix included) to either the MITM interceptor or a plain tunnel.
struct PrefixedIo<IO> {
    prefix: Vec<u8>,
    pos: usize,
    inner: IO,
}

impl<IO> PrefixedIo<IO> {
    fn new(prefix: Vec<u8>, inner: IO) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<IO: AsyncRead + Unpin> AsyncRead for PrefixedIo<IO> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<IO: AsyncWrite + Unpin> AsyncWrite for PrefixedIo<IO> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, data)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

pub async fn handle_connect(
    req: Request<Incoming>,
    context: TransportContext,
    peer: Option<std::net::SocketAddr>,
    mut shutdown: watch::Receiver<bool>,
) -> Response<Body> {
    let sm = context.session_manager.clone();
    let engine = context.engine.clone();
    let dns_overrides = context.dns_overrides.clone();
    let connections = context.connections.clone();
    let connect_timeout = context.connect_timeout;
    let handshake_timeout = context.handshake_timeout;

    let host = req
        .uri()
        .authority()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let addr = {
        let raw = if host.contains(':') {
            host.clone()
        } else {
            format!("{}:443", host)
        };
        let ovr = dns_overrides.read().await;
        if !ovr.is_empty() {
            let (hostname, port_part) = raw.split_once(':').unwrap_or((&raw, "443"));
            if let Some(entry) = ovr.get(hostname).filter(|e| e.enabled) {
                format!("{}:{}", entry.ip, port_part)
            } else {
                raw
            }
        } else {
            raw
        }
    };
    let hostname = host.split(':').next().unwrap_or(&host).to_string();

    let is_mitm = engine.mitm_enabled && engine.ca.is_some();
    let session_id = uuid::Uuid::new_v4().to_string();
    if !is_mitm {
        sm.record_request(
            session_id.clone(),
            crate::middleware::RequestContext {
                method: "CONNECT".to_string(),
                uri: format!("https://{}", host),
                headers: crate::middleware::HeaderMap::new(),
                body: bytes::Bytes::new(),
                host: host.clone(),
                ..Default::default()
            },
        );
    }

    let on_upgrade = hyper::upgrade::on(req);
    let start = std::time::Instant::now();

    connections.spawn_tracked("connect-tunnel", peer, async move {
        let tunnel = async {
            match on_upgrade.await {
                Ok(upgraded) => {
                    // When MITM is enabled, peek the first byte: a TLS ClientHello
                    // (0x16) is intercepted; anything else is a cleartext CONNECT
                    // tunnel (e.g. a ws:// upgrade) that must be tunnelled as-is,
                    // not fed to the TLS acceptor (which would consume the first
                    // bytes and break the connection). The peeked byte is replayed
                    // via PrefixedIo so no data is lost.
                    let stream = {
                        let mut io = hyper_util::rt::TokioIo::new(upgraded);
                        if engine.mitm_enabled
                            && engine.ca.is_some()
                        {
                            // Read the first available chunk (not a single byte) so
                            // hyper's Upgraded read-ahead is captured whole, then
                            // replay it via PrefixedIo. Only the first byte is used
                            // for detection: 0x16 = TLS handshake record.
                            let mut peek = vec![0u8; 8192];
                            let n = match io.read(&mut peek).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => n,
                            };
                            peek.truncate(n);
                            let is_tls = peek[0] == TLS_HANDSHAKE_RECORD;
                            let prefixed = PrefixedIo::new(peek, io);
                            if is_tls && let Some(ca) = engine.ca.clone() {
                                mitm_intercept(
                                    prefixed,
                                    hostname.clone(),
                                    host.clone(),
                                    engine.clone(),
                                    ca,
                                    handshake_timeout,
                                )
                                .await;
                                return;
                            }
                            // MITM mode: TLS detection skipped the initial record_request.
                            // Record the cleartext tunnel as a CONNECT session now so it
                            // appears in the dashboard (e.g. h2c gRPC, ws://). Labelled
                            // http:// — this tunnel is explicitly NOT TLS.
                            sm.record_request(
                                session_id.clone(),
                                crate::middleware::RequestContext {
                                    method: "CONNECT".to_string(),
                                    uri: format!("http://{}", host),
                                    headers: crate::middleware::HeaderMap::new(),
                                    body: bytes::Bytes::new(),
                                    host: host.clone(),
                                    ..Default::default()
                                },
                            );
                            tracing::debug!(host=%hostname, "CONNECT tunnel is not TLS; tunnelling without MITM");
                            prefixed
                        } else {
                            PrefixedIo::new(Vec::new(), io)
                        }
                    };
                    match timeout(connect_timeout, tokio::net::TcpStream::connect(&addr)).await {
                        Ok(Ok(mut upstream)) => {
                            let mut io = stream;
                            let result =
                                tokio::io::copy_bidirectional(&mut io, &mut upstream).await;
                            let (to_server, to_client) = result.unwrap_or((0, 0));
                            sm.record_response_with_metrics(
                                session_id.clone(),
                                crate::middleware::ResponseContext {
                                    status: 200,
                                    headers: crate::middleware::HeaderMap::new(),
                                    body: bytes::Bytes::from(format!(
                                        "↑{} ↓{}",
                                        fmt_bytes(to_server),
                                        fmt_bytes(to_client)
                                    )),
                                    request_uri: format!("https://{}", host),
                                    session_id: Some(session_id),
                                    ..Default::default()
                                },
                                crate::session::InspectionMetrics {
                                    latency_ms: start.elapsed().as_millis() as u64,
                                    request_size_bytes: to_server as usize,
                                    response_size_bytes: to_client as usize,
                                    status_code: 200,
                                    ttfb_ms: 0,
                                    body_ms: 0,
                                    ..Default::default()
                                },
                            );
                        }
                        Ok(Err(e)) => {
                            tracing::error!(error=%e, addr=%addr, "CONNECT upstream unreachable");
                            sm.record_response_with_metrics(
                                session_id.clone(),
                                crate::middleware::ResponseContext {
                                    status: 502,
                                    headers: crate::middleware::HeaderMap::new(),
                                    body: bytes::Bytes::from(format!("upstream unreachable: {}", e)),
                                    request_uri: format!("https://{}", host),
                                    session_id: Some(session_id),
                                    ..Default::default()
                                },
                                crate::session::InspectionMetrics {
                                    latency_ms: start.elapsed().as_millis() as u64,
                                    request_size_bytes: 0,
                                    response_size_bytes: 0,
                                    status_code: 502,
                                    ttfb_ms: 0,
                                    body_ms: 0,
                                    ..Default::default()
                                },
                            );
                        }
                        Err(_) => {
                            tracing::error!(addr=%addr, timeout_secs=connect_timeout.as_secs(), "CONNECT upstream timed out");
                            sm.record_response_with_metrics(
                                session_id.clone(),
                                crate::middleware::ResponseContext {
                                    status: 504,
                                    headers: crate::middleware::HeaderMap::new(),
                                    body: bytes::Bytes::from_static(b"upstream connect timed out"),
                                    request_uri: format!("https://{}", host),
                                    session_id: Some(session_id),
                                    ..Default::default()
                                },
                                crate::session::InspectionMetrics {
                                    latency_ms: start.elapsed().as_millis() as u64,
                                    request_size_bytes: 0,
                                    response_size_bytes: 0,
                                    status_code: 504,
                                    ttfb_ms: 0,
                                    body_ms: 0,
                                    ..Default::default()
                                },
                            );
                        }
                    }
                }
                Err(e) => tracing::error!(error=%e, "CONNECT upgrade failed"),
            }
        };
        tokio::pin!(tunnel);
        tokio::select! {
            _ = &mut tunnel => {}
            _ = wait_for_shutdown(&mut shutdown) => {
                tracing::debug!(host=%host, "CONNECT tunnel stopped by shutdown");
            }
        }
    });

    Response::builder().status(200).body(Body::empty()).unwrap()
}

fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1_048_576 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.1}MB", n as f64 / 1_048_576.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prefixed_io_replays_prefix_then_inner() {
        // The peeked first byte(s) must be replayed ahead of the rest of the
        // stream so the downstream tunnel/MITM sees the complete handshake.
        let inner: &[u8] = b"GET / HTTP/1.1\r\n";
        let mut s = PrefixedIo::new(vec![0x16, 0x03], inner);
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        assert_eq!(&out[..2], &[0x16, 0x03], "prefix replayed first");
        assert_eq!(&out[2..], inner, "inner bytes follow intact");
    }

    #[tokio::test]
    async fn prefixed_io_empty_prefix_is_passthrough() {
        let inner: &[u8] = b"hello";
        let mut s = PrefixedIo::new(Vec::new(), inner);
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        assert_eq!(&out, inner);
    }

    // Reproduces the MITM CONNECT path: peek the first byte of a TLS ClientHello,
    // then complete a real rustls handshake through PrefixedIo. Guards against the
    // wrapper corrupting the TLS stream.
    #[tokio::test]
    async fn prefixed_io_completes_a_real_tls_handshake() {
        use tokio::io::AsyncWriteExt;
        use tokio_rustls::{TlsAcceptor, TlsConnector};

        crate::transport::tls::install_default_crypto_provider();

        let params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("params");
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("cert");
        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der: rustls::pki_types::PrivateKeyDer<'static> =
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into();

        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .expect("server cfg");
        let acceptor = TlsAcceptor::from(std::sync::Arc::new(server_config));

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(std::sync::Arc::new(client_config));

        let (client_io, mut server_io) = tokio::io::duplex(64 * 1024);

        let server = tokio::spawn(async move {
            let mut first = [0u8; 1];
            server_io.read_exact(&mut first).await.expect("peek byte");
            assert_eq!(
                first[0], 0x16,
                "ClientHello begins with a TLS handshake record"
            );
            let prefixed = PrefixedIo::new(first.to_vec(), server_io);
            let mut tls = acceptor.accept(prefixed).await.expect("server handshake");
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.expect("read app data");
            tls.write_all(&buf).await.expect("echo");
            tls.flush().await.expect("flush");
        });

        let domain = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut tls = connector
            .connect(domain, client_io)
            .await
            .expect("client handshake");
        tls.write_all(b"ping").await.unwrap();
        tls.flush().await.unwrap();
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping", "app data survives the prefixed handshake");
        server.await.unwrap();
    }
}
