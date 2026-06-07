/// HTTP/3 (QUIC) listener and upstream client.
///
/// Compiled only with the `http3` Cargo feature. The listener accepts QUIC
/// connections, demultiplexes h3 request streams, normalises each request into
/// the same proxy-engine pipeline as the TCP listeners, and streams the
/// response back. Alt-Svc is injected at the engine level.
#[cfg(feature = "http3")]
mod inner {
    use std::{net::SocketAddr, sync::Arc};

    use axum::body::Body;
    use axum::http::Request;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use h3::server::RequestStream;
    use tokio::sync::watch;

    pub use h3_quinn::quinn;

    use crate::{core::engine::ProxyEngine, transport::lifecycle::wait_for_shutdown};

    // ── TLS / QUIC cert resolver ────────────────────────────────────────────

    /// Sync bridge between the async `CertificateAuthority` and the synchronous
    /// `rustls::server::ResolvesServerCert` trait required by quinn.
    struct QuicCertResolver {
        ca: Arc<crate::certs::CertificateAuthority>,
    }

    impl std::fmt::Debug for QuicCertResolver {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("QuicCertResolver").finish()
        }
    }

    impl rustls::server::ResolvesServerCert for QuicCertResolver {
        fn resolve(
            &self,
            client_hello: rustls::server::ClientHello<'_>,
        ) -> Option<Arc<rustls::sign::CertifiedKey>> {
            let sni = client_hello
                .server_name()
                .unwrap_or("localhost")
                .to_string();
            // Bridge async cert issuance to the synchronous rustls trait using
            // block_in_place (safe on multi-thread tokio workers).
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(self.ca.get_certificate_for_domain(&sni))
            });
            let (cert_der, key_der) = result.ok()?;
            let cert = rustls::pki_types::CertificateDer::from(cert_der);
            let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(key_der);
            let key = rustls::crypto::ring::sign::any_supported_type(
                &rustls::pki_types::PrivateKeyDer::Pkcs8(key_der),
            )
            .ok()?;
            Some(Arc::new(rustls::sign::CertifiedKey::new(vec![cert], key)))
        }
    }

    /// Build a `quinn::ServerConfig` for MITM HTTP/3.
    pub fn build_quic_server_config(
        ca: Arc<crate::certs::CertificateAuthority>,
    ) -> Result<quinn::ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
        let resolver = Arc::new(QuicCertResolver { ca });
        let mut tls_cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        tls_cfg.alpn_protocols = vec![b"h3".to_vec()];
        let quic_cfg = quinn::crypto::rustls::QuicServerConfig::try_from(tls_cfg)?;
        Ok(quinn::ServerConfig::with_crypto(Arc::new(quic_cfg)))
    }

    // ── Listener ────────────────────────────────────────────────────────────

    /// Bind a UDP socket for the HTTP/3 QUIC listener.
    pub async fn bind_h3_listener(
        bind_host: &str,
        port: u16,
        ca: Arc<crate::certs::CertificateAuthority>,
    ) -> Result<quinn::Endpoint, Box<dyn std::error::Error + Send + Sync>> {
        let server_cfg = build_quic_server_config(ca)?;
        let addr: SocketAddr = format!("{}:{}", bind_host, port).parse()?;
        let endpoint = quinn::Endpoint::server(server_cfg, addr)?;
        tracing::info!("oproxy HTTP/3 listener on udp://{}", addr);
        Ok(endpoint)
    }

    /// Accept QUIC connections and serve each request stream through the proxy engine.
    pub async fn run_h3_listener(
        endpoint: quinn::Endpoint,
        engine: Arc<ProxyEngine>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            let incoming = tokio::select! {
                conn = endpoint.accept() => match conn {
                    Some(c) => c,
                    None => {
                        tracing::info!("HTTP/3 endpoint closed");
                        break;
                    }
                },
                _ = wait_for_shutdown(&mut shutdown) => {
                    tracing::info!("HTTP/3 listener stopped");
                    endpoint.close(0u32.into(), b"server shutdown");
                    break;
                }
            };

            let engine = engine.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                handle_quic_connection(incoming, engine, shutdown).await;
            });
        }
    }

    async fn handle_quic_connection(
        incoming: quinn::Incoming,
        engine: Arc<ProxyEngine>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error=%e, "QUIC connection setup failed");
                return;
            }
        };

        let h3_conn = h3_quinn::Connection::new(conn);
        let mut h3_server: h3::server::Connection<_, Bytes> =
            match h3::server::builder().build(h3_conn).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(error=%e, "h3 server connection setup failed");
                    return;
                }
            };

        // One downstream identity for this QUIC connection; cloned into every
        // request stream so multiplexed h3 streams share a connection_id (Phase 7).
        let conn_identity = crate::transport::http::DownstreamConn::new();

        loop {
            let resolver = tokio::select! {
                res = h3_server.accept() => res,
                _ = wait_for_shutdown(&mut shutdown) => break,
            };
            let resolver = match resolver {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error=%e, "h3 accept error");
                    break;
                }
            };

            let engine = engine.clone();
            let conn_identity = conn_identity.clone();
            tokio::spawn(async move {
                let (req, stream) = match resolver.resolve_request().await {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::debug!(error=%e, "h3 resolve_request failed");
                        return;
                    }
                };
                handle_h3_request(req, stream, engine, conn_identity).await;
            });
        }
    }

    async fn handle_h3_request(
        req: Request<()>,
        mut stream: RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
        engine: Arc<ProxyEngine>,
        conn_identity: crate::transport::http::DownstreamConn,
    ) {
        let (parts, _) = req.into_parts();

        // Read body DATA frames from the h3 stream.
        let mut body_chunks: Vec<Bytes> = Vec::new();
        while let Ok(Some(chunk)) = stream.recv_data().await {
            let mut buf = chunk;
            use bytes::Buf;
            body_chunks.push(buf.copy_to_bytes(buf.remaining()));
        }
        let body_bytes: Bytes = body_chunks.into_iter().fold(Bytes::new(), |acc, chunk| {
            // Concat without allocation when possible.
            if acc.is_empty() {
                chunk
            } else {
                let mut v = Vec::with_capacity(acc.len() + chunk.len());
                v.extend_from_slice(&acc);
                v.extend_from_slice(&chunk);
                Bytes::from(v)
            }
        });

        let mut axum_req = match build_axum_request(parts, body_bytes) {
            Some(r) => r,
            None => {
                let resp = axum::http::Response::builder()
                    .status(400)
                    .body(())
                    .unwrap();
                let _ = stream.send_response(resp).await;
                let _ = stream.finish().await;
                return;
            }
        };
        // Attach the per-connection identity so the engine records connection_id /
        // stream_id for this h3 stream (Phase 7).
        axum_req.extensions_mut().insert(conn_identity);

        let response = engine.handle_request_with_destination(axum_req, None).await;

        let (resp_parts, resp_body) = response.into_parts();
        let mut h3_resp_builder = axum::http::Response::builder().status(resp_parts.status);
        for (k, v) in &resp_parts.headers {
            // Skip hop-by-hop headers that aren't valid in h3.
            let name = k.as_str().to_lowercase();
            if matches!(
                name.as_str(),
                "transfer-encoding" | "connection" | "keep-alive" | "upgrade"
            ) {
                continue;
            }
            h3_resp_builder = h3_resp_builder.header(k, v);
        }
        let h3_response = h3_resp_builder.body(()).unwrap_or_else(|_| {
            axum::http::Response::builder()
                .status(500)
                .body(())
                .unwrap()
        });

        if stream.send_response(h3_response).await.is_err() {
            return;
        }

        // Pipe the axum response body to h3 DATA frames.
        let mut body_stream = resp_body.into_data_stream();
        while let Some(chunk) = body_stream.next().await {
            match chunk {
                Ok(data) if !data.is_empty() => {
                    if stream.send_data(data).await.is_err() {
                        return;
                    }
                }
                _ => break,
            }
        }
        let _ = stream.finish().await;
    }

    fn build_axum_request(
        parts: axum::http::request::Parts,
        body: Bytes,
    ) -> Option<axum::http::Request<Body>> {
        let mut builder = axum::http::Request::builder()
            .method(parts.method.as_str())
            .version(axum::http::Version::HTTP_3)
            .uri(parts.uri.to_string());

        for (k, v) in &parts.headers {
            if k.as_str().starts_with(':') {
                continue;
            }
            builder = builder.header(k, v);
        }

        builder.body(Body::from(body)).ok()
    }

    // ── H3Quinn upstream transport ──────────────────────────────────────────

    /// `UpstreamTransport` that sends requests via HTTP/3.
    pub struct H3Quinn {
        endpoint: quinn::Endpoint,
    }

    impl H3Quinn {
        /// Build a client endpoint that trusts standard WebPKI roots.
        pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
            let mut root_store = rustls::RootCertStore::empty();
            for cert in webpki_roots::TLS_SERVER_ROOTS.iter().cloned() {
                root_store.roots.push(cert);
            }
            let crypto = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            let client_cfg = quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
            ));
            let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?;
            endpoint.set_default_client_config(client_cfg);
            Ok(Self { endpoint })
        }
    }

    #[async_trait::async_trait]
    impl crate::core::forward::UpstreamTransport for H3Quinn {
        async fn connect(
            &self,
            origin: &crate::core::forward::Origin,
            _certs: &dyn crate::core::forward::CertResolver,
        ) -> Result<Box<dyn crate::core::forward::UpstreamConn>, crate::core::forward::TransportError>
        {
            let addr_str = format!("{}:{}", origin.host, origin.port);
            let addr: SocketAddr = addr_str
                .parse()
                .map_err(|_| crate::core::forward::TransportError::Connect(addr_str.clone()))?;

            let conn = self
                .endpoint
                .connect(addr, &origin.host)
                .map_err(|e| crate::core::forward::TransportError::Connect(e.to_string()))?
                .await
                .map_err(|e| crate::core::forward::TransportError::Connect(e.to_string()))?;

            let h3_conn = h3_quinn::Connection::new(conn);
            let (mut driver, send_request) = h3::client::builder()
                .build::<_, _, Bytes>(h3_conn)
                .await
                .map_err(|e| crate::core::forward::TransportError::Connect(e.to_string()))?;

            tokio::spawn(async move {
                let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
            });

            Ok(Box::new(H3Conn { send_request }))
        }
    }

    struct H3Conn {
        send_request: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    }

    #[async_trait::async_trait]
    impl crate::core::forward::UpstreamConn for H3Conn {
        fn protocol(&self) -> crate::core::forward::NegotiatedProtocol {
            crate::core::forward::NegotiatedProtocol::H3
        }

        fn max_concurrent_streams(&self) -> usize {
            100
        }

        async fn send_request(
            &self,
            head: crate::core::forward::RequestHead,
            mut body: crate::core::forward::BodyRx,
        ) -> Result<crate::core::forward::ResponseStream, crate::core::forward::TransportError>
        {
            let mut sr = self.send_request.clone();
            let http_req = axum::http::Request::builder()
                .method(head.method.as_str())
                .uri(head.uri.as_str())
                .body(())
                .map_err(|e| crate::core::forward::TransportError::Connect(e.to_string()))?;

            let mut stream = sr
                .send_request(http_req)
                .await
                .map_err(|e| crate::core::forward::TransportError::Connect(e.to_string()))?;

            // Send body frames.
            use crate::core::forward::Frame;
            while let Some(frame) = body.recv().await {
                match frame {
                    Ok(Frame::Data(data)) => {
                        stream.send_data(data).await.map_err(|e| {
                            crate::core::forward::TransportError::Connect(e.to_string())
                        })?;
                    }
                    Ok(Frame::Trailers(_)) | Err(_) => break,
                }
            }
            stream
                .finish()
                .await
                .map_err(|e| crate::core::forward::TransportError::Connect(e.to_string()))?;

            let resp = stream
                .recv_response()
                .await
                .map_err(|e| crate::core::forward::TransportError::Connect(e.to_string()))?;

            let (parts, _) = resp.into_parts();
            let status = parts.status.as_u16();
            let mut headers = crate::middleware::HeaderMap::new();
            for (k, v) in &parts.headers {
                if let Ok(s) = v.to_str() {
                    headers.insert(k.to_string(), s.to_string());
                }
            }

            // Collect h3 DATA frames into a body channel.
            let (tx, rx) = crate::core::forward::body_channel(8);
            let tx_clone = tx;
            tokio::spawn(async move {
                while let Ok(Some(mut data)) = stream.recv_data().await {
                    use bytes::Buf;
                    let chunk = data.copy_to_bytes(data.remaining());
                    if tx_clone.send(Frame::Data(chunk)).await.is_err() {
                        break;
                    }
                }
            });

            Ok(crate::core::forward::ResponseStream {
                head: crate::core::forward::ResponseHead {
                    status,
                    headers,
                    protocol: crate::core::forward::NegotiatedProtocol::H3,
                },
                body: rx,
            })
        }
    }
}

#[cfg(feature = "http3")]
pub use inner::quinn;
#[cfg(feature = "http3")]
pub use inner::{H3Quinn, bind_h3_listener, build_quic_server_config, run_h3_listener};
