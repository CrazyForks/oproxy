//! Phase 2 forwarding contracts (protocol-support RFC §8.2).
//!
//! This module defines the interfaces the streaming-forwarder work is built on.
//! The `UpstreamTransport`/`UpstreamConn`/`CertResolver` family is public
//! library API used by the HTTP/3 transport (`transport/http3.rs`, behind the
//! `http3` feature) and will be wired into `ProxyEngine` when the buffered path
//! is switched to the streaming path. `dead_code` is suppressed module-wide
//! so the binary compilation doesn't flag these staged interfaces.
//!
//! Design properties (see RFC §8.2):
//!   * `UpstreamTransport`/`UpstreamConn` speak in **bidirectional streams of
//!     frames**, never request/response, so one shape covers h1/h2/h3 + gRPC.
//!   * Bodies are back-pressured channels (`BodyTx`/`BodyRx`); a bounded channel
//!     models flow control — `send` only resolves when the peer has capacity.
//!   * Protocol negotiation is exposed via `UpstreamConn::protocol()` /
//!     `max_concurrent_streams()`; callers never inspect the wire.
//!   * Cert resolution is pluggable (`CertResolver`) and shared across the TLS
//!     client and (later) the QUIC endpoint.
//!   * The body-class boundary is a **head-only** declaration (`BodyHint`) so the
//!     core can pick buffered-vs-streaming before any body byte moves.
#![allow(dead_code)]
//!

use async_trait::async_trait;
use bytes::Bytes;
use std::sync::Arc;

use crate::middleware::HeaderMap;

/// Negotiated application protocol of an upstream connection. The engine learns
/// capacity/semantics from this, not by reading wire bytes. For HTTP/3 the value
/// is implicit (QUIC always means h3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedProtocol {
    H1,
    H2,
    H3,
}

impl NegotiatedProtocol {
    /// Whether multiple request streams may be multiplexed concurrently on one
    /// connection. False for HTTP/1.1 (one in-flight exchange per connection).
    pub fn is_multiplexed(self) -> bool {
        matches!(self, NegotiatedProtocol::H2 | NegotiatedProtocol::H3)
    }

    pub fn label(self) -> &'static str {
        match self {
            NegotiatedProtocol::H1 => "HTTP/1.1",
            NegotiatedProtocol::H2 => "HTTP/2",
            NegotiatedProtocol::H3 => "HTTP/3",
        }
    }
}

/// The common currency for every protocol: a body is a sequence of data frames
/// optionally terminated by a trailers frame (h2/h3/gRPC trailers).
#[derive(Debug, Clone)]
pub enum Frame {
    Data(Bytes),
    Trailers(HeaderMap),
}

/// Granularity at which an inspecting plugin wants to observe a streamed body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    /// Per network chunk, as bytes arrive.
    Bytes,
    /// Per length-prefixed application message (e.g. the 5-byte gRPC frame).
    /// `needs_full_body` then applies *per message*, never to the whole stream.
    Messages,
}

/// Head-only declaration of how a plugin needs to access the body. Declared from
/// the request head **before the first body byte is forwarded**, so the engine
/// can choose the forwarding class without ever buffering a stream by accident.
///
/// `StreamingMutate` is intentionally **not** part of v1 — streaming is
/// inspect-only. It is reserved for a future RFC:
///
/// ```text
/// // StreamingMutate { granularity: Granularity },  // reserved, not in v1
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyHint {
    /// Buffer the whole body; mutation is allowed (mock, rewrite, Lua, breakpoint).
    FullBody,
    /// Observe the streamed body without mutating it (inspectors).
    StreamingInspect { granularity: Granularity },
}

impl Default for BodyHint {
    /// Default preserves today's behaviour: plugins are assumed to need the whole
    /// body unless they opt into streaming.
    fn default() -> Self {
        BodyHint::FullBody
    }
}

/// Which forwarding path handles an exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardClass {
    /// Today's reqwest path: body buffered (subject to `max_body_bytes`),
    /// middleware may mutate it, response assembled before returning.
    Buffered,
    /// Streaming pump: body relayed frame-by-frame with back-pressure,
    /// inspect-only in v1.
    Streaming,
}

/// Selects the forwarding class from the body hints of the active plugins on a
/// route. Rules (RFC §8.2):
///   * any `FullBody` ⇒ `Buffered` (a mutator/whole-body reader is present);
///   * otherwise, at least one `StreamingInspect` ⇒ `Streaming`;
///   * no hints at all ⇒ `Buffered` (conservative default == current behaviour).
pub fn select_class<I>(hints: I) -> ForwardClass
where
    I: IntoIterator<Item = BodyHint>,
{
    let mut saw_streaming = false;
    for hint in hints {
        match hint {
            BodyHint::FullBody => return ForwardClass::Buffered,
            BodyHint::StreamingInspect { .. } => saw_streaming = true,
        }
    }
    if saw_streaming {
        ForwardClass::Streaming
    } else {
        ForwardClass::Buffered
    }
}

/// Errors raised while establishing or driving an upstream transport.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("upstream connect failed: {0}")]
    Connect(String),
    #[error("upstream stream closed unexpectedly")]
    Closed,
    #[error("upstream transport error: {0}")]
    Other(String),
}

/// Where to forward an exchange, decomposed so impls never re-parse a URL.
#[derive(Debug, Clone)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl Origin {
    pub fn is_tls(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("https") || self.scheme.eq_ignore_ascii_case("wss")
    }
}

/// Request head handed to a transport. The body travels separately as a
/// back-pressured `BodyRx`, so the head is available for routing/negotiation
/// before any payload is read.
#[derive(Debug, Clone)]
pub struct RequestHead {
    pub method: String,
    pub uri: String,
    pub headers: HeaderMap,
}

/// Response head returned by a transport ahead of its streamed body.
#[derive(Debug, Clone)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: HeaderMap,
    pub protocol: NegotiatedProtocol,
}

/// A streamed upstream response: head first, then a back-pressured body.
pub struct ResponseStream {
    pub head: ResponseHead,
    pub body: BodyRx,
}

/// Producer half of a body stream. `send` awaits channel capacity, which models
/// the peer's flow-control window (h2 `WINDOW_UPDATE` / QUIC stream FC / h1
/// socket writability): a fast producer is paused until the slow consumer drains.
pub struct BodyTx {
    inner: tokio::sync::mpsc::Sender<Result<Frame, TransportError>>,
}

/// Consumer half of a body stream.
pub struct BodyRx {
    inner: tokio::sync::mpsc::Receiver<Result<Frame, TransportError>>,
}

/// Creates a back-pressured body channel. `window` is the number of in-flight
/// frames permitted before the producer is paused — the flow-control proxy.
pub fn body_channel(window: usize) -> (BodyTx, BodyRx) {
    let (tx, rx) = tokio::sync::mpsc::channel(window.max(1));
    (BodyTx { inner: tx }, BodyRx { inner: rx })
}

impl BodyTx {
    /// Pushes a frame, awaiting until the consumer has capacity (back-pressure).
    pub async fn send(&self, frame: Frame) -> Result<(), TransportError> {
        self.inner
            .send(Ok(frame))
            .await
            .map_err(|_| TransportError::Closed)
    }

    /// Propagates a stream error to the consumer.
    pub async fn send_error(&self, err: TransportError) -> Result<(), TransportError> {
        self.inner
            .send(Err(err))
            .await
            .map_err(|_| TransportError::Closed)
    }
}

impl BodyRx {
    /// Awaits the next frame. `None` marks a clean end of stream.
    pub async fn recv(&mut self) -> Option<Result<Frame, TransportError>> {
        self.inner.recv().await
    }
}

/// Pumps every frame from `src` to `dst`. Back-pressure is automatic: `dst.send`
/// awaits the downstream window, so a slow consumer throttles a fast producer
/// without unbounded buffering. This is the inspect-only relay; a future
/// mutator would sit between `src` and `dst`.
pub async fn relay(mut src: BodyRx, dst: &BodyTx) -> Result<(), TransportError> {
    while let Some(item) = src.recv().await {
        dst.send(item?).await?;
    }
    Ok(())
}

/// Resolves certificates for both the downstream MITM acceptor and upstream
/// clients (TLS today, QUIC in Phase 5) — one source of truth for issuance and
/// verification. The existing `CertificateAuthority` is the intended impl.
pub trait CertResolver: Send + Sync {
    /// Leaf cert + private key (DER) for an intercepted SNI, or `None` if it
    /// cannot be issued. Shape matches `CertificateAuthority::get_certificate_for_domain`.
    fn resolve_server(&self, sni: &str) -> Option<(Vec<u8>, Vec<u8>)>;

    /// Client config fed to both tokio-rustls and (Phase 5) quinn.
    fn client_config(&self) -> Arc<rustls::ClientConfig>;
}

/// A negotiated upstream connection. For h1 exactly one exchange is in flight;
/// for h2/h3 the handle may be cloned and `send_request` issued concurrently.
#[async_trait]
pub trait UpstreamConn: Send + Sync {
    fn protocol(&self) -> NegotiatedProtocol;

    /// Concurrency capacity — 1 for HTTP/1.1, the peer's SETTINGS value for h2/h3.
    fn max_concurrent_streams(&self) -> usize;

    /// Sends one request and returns its streamed response. `body` is the
    /// consumable request-body stream the transport reads from; the caller
    /// retains the paired [`BodyTx`] to feed it (note: the RFC sketch named this
    /// parameter `BodyTx` from the producer's view — in code the transport is
    /// handed the receiver).
    async fn send_request(
        &self,
        head: RequestHead,
        body: BodyRx,
    ) -> Result<ResponseStream, TransportError>;
}

/// Opens/borrows connections to an origin. One impl per wire family:
/// `BufferedReqwest` (h1/h2 buffered, wraps today's path), `StreamingHyper`
/// (h1/h2 streaming), and later `H3Quinn` (h3). The engine picks an impl from
/// the forwarding class plus the Alt-Svc cache.
#[async_trait]
pub trait UpstreamTransport: Send + Sync {
    async fn connect(
        &self,
        origin: &Origin,
        certs: &dyn CertResolver,
    ) -> Result<Box<dyn UpstreamConn>, TransportError>;
}

/// `UpstreamTransport` backed by reqwest — wraps today's buffered forwarding
/// path behind the streaming abstraction. The request body is drained to bytes
/// (the buffered class never streams the request), but the *response* is exposed
/// as a real streamed `BodyRx`, so callers already speak the streaming API.
///
/// reqwest manages its own TLS, so this impl ignores the [`CertResolver`]; cert
/// plumbing matters only for `StreamingHyper`/QUIC.
pub struct BufferedReqwest {
    client: reqwest::Client,
}

impl BufferedReqwest {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl UpstreamTransport for BufferedReqwest {
    async fn connect(
        &self,
        _origin: &Origin,
        _certs: &dyn CertResolver,
    ) -> Result<Box<dyn UpstreamConn>, TransportError> {
        Ok(Box::new(ReqwestConn {
            client: self.client.clone(),
        }))
    }
}

struct ReqwestConn {
    client: reqwest::Client,
}

#[async_trait]
impl UpstreamConn for ReqwestConn {
    fn protocol(&self) -> NegotiatedProtocol {
        // reqwest negotiates per-request; the precise version is reported on the
        // response (`ResponseHead.protocol`). At the connection level we model the
        // buffered path as single-exchange.
        NegotiatedProtocol::H1
    }

    fn max_concurrent_streams(&self) -> usize {
        1
    }

    async fn send_request(
        &self,
        head: RequestHead,
        mut body: BodyRx,
    ) -> Result<ResponseStream, TransportError> {
        // Buffered class: collect the request body frames before sending.
        let mut buf = Vec::new();
        while let Some(item) = body.recv().await {
            if let Frame::Data(d) = item? {
                buf.extend_from_slice(&d);
            }
        }

        let method = reqwest::Method::from_bytes(head.method.as_bytes())
            .map_err(|e| TransportError::Other(format!("invalid method: {e}")))?;
        let mut rb = self.client.request(method, &head.uri);
        for (name, value) in &head.headers {
            if name.eq_ignore_ascii_case("content-length") {
                continue; // reqwest recomputes this
            }
            if let (Ok(n), Ok(v)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
            ) {
                rb = rb.header(n, v);
            }
        }
        if !buf.is_empty() {
            rb = rb.body(buf);
        }

        let resp = rb
            .send()
            .await
            .map_err(|e| TransportError::Connect(e.to_string()))?;

        let status = resp.status().as_u16();
        let protocol = match resp.version() {
            axum::http::Version::HTTP_2 => NegotiatedProtocol::H2,
            axum::http::Version::HTTP_3 => NegotiatedProtocol::H3,
            _ => NegotiatedProtocol::H1,
        };
        let mut headers = HeaderMap::new();
        for (name, value) in resp.headers().iter() {
            headers.append(name.as_str(), value.to_str().unwrap_or("").to_string());
        }
        let resp_head = ResponseHead {
            status,
            headers,
            protocol,
        };

        // Stream the response body back through a back-pressured channel.
        let (tx, rx) = body_channel(16);
        tokio::spawn(async move {
            let mut resp = resp;
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if tx.send(Frame::Data(chunk)).await.is_err() {
                            break; // consumer dropped
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send_error(TransportError::Other(e.to_string())).await;
                        break;
                    }
                }
            }
        });

        Ok(ResponseStream {
            head: resp_head,
            body: rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hints_default_to_buffered() {
        assert_eq!(select_class(std::iter::empty()), ForwardClass::Buffered);
    }

    #[test]
    fn any_full_body_forces_buffered() {
        let hints = [
            BodyHint::StreamingInspect {
                granularity: Granularity::Messages,
            },
            BodyHint::FullBody,
        ];
        assert_eq!(select_class(hints), ForwardClass::Buffered);
    }

    #[test]
    fn all_streaming_inspect_selects_streaming() {
        let hints = [
            BodyHint::StreamingInspect {
                granularity: Granularity::Bytes,
            },
            BodyHint::StreamingInspect {
                granularity: Granularity::Messages,
            },
        ];
        assert_eq!(select_class(hints), ForwardClass::Streaming);
    }

    #[test]
    fn default_body_hint_is_full_body() {
        assert_eq!(BodyHint::default(), BodyHint::FullBody);
    }

    #[test]
    fn protocol_multiplexing_and_labels() {
        assert!(!NegotiatedProtocol::H1.is_multiplexed());
        assert!(NegotiatedProtocol::H2.is_multiplexed());
        assert!(NegotiatedProtocol::H3.is_multiplexed());
        assert_eq!(NegotiatedProtocol::H2.label(), "HTTP/2");
    }

    #[tokio::test]
    async fn body_channel_relays_frames_in_order() {
        let (tx, rx) = body_channel(4);
        let (out_tx, mut out_rx) = body_channel(4);

        let pump = tokio::spawn(async move { relay(rx, &out_tx).await });

        tx.send(Frame::Data(Bytes::from_static(b"a")))
            .await
            .unwrap();
        tx.send(Frame::Data(Bytes::from_static(b"b")))
            .await
            .unwrap();
        drop(tx); // clean end of stream

        let mut seen = Vec::new();
        while let Some(item) = out_rx.recv().await {
            if let Frame::Data(d) = item.unwrap() {
                seen.extend_from_slice(&d);
            }
        }
        pump.await.unwrap().unwrap();
        assert_eq!(seen, b"ab");
    }

    #[tokio::test]
    async fn body_channel_applies_back_pressure() {
        // window of 1: the second send must not complete until the first is read.
        let (tx, mut rx) = body_channel(1);
        tx.send(Frame::Data(Bytes::from_static(b"1")))
            .await
            .unwrap();

        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(30),
            tx.send(Frame::Data(Bytes::from_static(b"2"))),
        )
        .await;
        assert!(
            blocked.is_err(),
            "second send should block until consumer drains"
        );

        // Drain one frame, freeing a window slot.
        let _ = rx.recv().await;
        tx.send(Frame::Data(Bytes::from_static(b"3")))
            .await
            .unwrap();
    }

    /// Cert resolver stub for transports that manage their own TLS (reqwest).
    struct NoCerts;
    impl CertResolver for NoCerts {
        fn resolve_server(&self, _sni: &str) -> Option<(Vec<u8>, Vec<u8>)> {
            None
        }
        fn client_config(&self) -> Arc<rustls::ClientConfig> {
            Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore::empty())
                    .with_no_client_auth(),
            )
        }
    }

    #[tokio::test]
    async fn buffered_reqwest_returns_streamed_response_through_the_abstraction() {
        use axum::{Router, routing::get};

        let app = Router::new().route("/hi", get(|| async { "hello-streamed" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = BufferedReqwest::new(reqwest::Client::new());
        let origin = Origin {
            scheme: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port: addr.port(),
        };
        let conn = transport.connect(&origin, &NoCerts).await.unwrap();
        assert_eq!(conn.max_concurrent_streams(), 1);

        // Empty request body: drop the sender to signal end of stream.
        let (btx, brx) = body_channel(4);
        drop(btx);

        let head = RequestHead {
            method: "GET".to_string(),
            uri: format!("http://{addr}/hi"),
            headers: HeaderMap::new(),
        };
        let mut resp = conn.send_request(head, brx).await.unwrap();
        assert_eq!(resp.head.status, 200);

        let mut body = Vec::new();
        while let Some(item) = resp.body.recv().await {
            if let Frame::Data(d) = item.unwrap() {
                body.extend_from_slice(&d);
            }
        }
        assert_eq!(&body, b"hello-streamed");
    }
}
