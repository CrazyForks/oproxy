use axum::body::Body;
use futures_util::{SinkExt, StreamExt};
use hyper::body::Incoming;
use hyper::{Request, Response};
use tokio::sync::watch;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        handshake::client::generate_key,
        protocol::{Message, Role},
    },
};

use crate::transport::TransportContext;
use crate::transport::lifecycle::wait_for_shutdown;

pub fn is_websocket_upgrade<B>(req: &Request<B>) -> bool {
    req.headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

/// Returns `true` when the request is an RFC 8441 extended-CONNECT (h2
/// WebSocket upgrade via `:protocol = websocket`).
// RFC 8441 extended-CONNECT detection — not yet wired to a handler, stub kept
// for the upcoming WebSocket-over-h2 path. Suppress the binary dead_code lint.
#[allow(dead_code)]
pub fn is_h2_websocket_upgrade<B>(req: &Request<B>) -> bool {
    req.method() == hyper::Method::from_bytes(b"CONNECT").unwrap_or(hyper::Method::GET)
        && req
            .headers()
            .get("protocol")
            .or_else(|| req.headers().get(":protocol"))
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("websocket"))
            .unwrap_or(false)
}

fn map_tungstenite_message_to_record(
    msg: &Message,
) -> Option<(u8, usize, Option<String>, Option<String>)> {
    match msg {
        Message::Text(text) => {
            let preview = text.chars().take(512).collect::<String>();
            Some((0x1, text.len(), Some(preview), None))
        }
        Message::Binary(data) => {
            let chunk = &data[..data.len().min(64)];
            let hex: String = chunk.iter().map(|b| format!("{:02x}", b)).collect();
            Some((
                0x2,
                data.len(),
                None,
                if hex.is_empty() { None } else { Some(hex) },
            ))
        }
        Message::Ping(_) => Some((0x9, 0, None, None)),
        Message::Pong(_) => Some((0xA, 0, None, None)),
        Message::Close(_) => Some((0x8, 0, None, None)),
        Message::Frame(_) => None,
    }
}

pub async fn handle_websocket(
    req: Request<Incoming>,
    context: TransportContext,
    session_id: String,
    peer: Option<std::net::SocketAddr>,
    mut shutdown: watch::Receiver<bool>,
) -> Response<Body> {
    let sm = context.session_manager.clone();
    let connections = context.connections.clone();
    let inspect_frames = context.inspect_ws_frames;
    let connect_timeout = context.connect_timeout;
    let handshake_timeout = context.handshake_timeout;

    let uri = req.uri().clone();
    let headers = req.headers().clone();
    // Downstream connection identity (Phase 7) — the upgrade request carries the
    // DownstreamConn extension inserted by the accept loop.
    let (ws_connection_id, ws_stream_id) = {
        let conn = req
            .extensions()
            .get::<crate::transport::http::DownstreamConn>();
        (conn.map(|c| c.id.clone()), conn.map(|c| c.next_stream()))
    };
    let ws_downstream_protocol =
        Some(crate::core::engine::protocol_label(req.version()).to_string());

    let host_header = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let target_host = uri
        .host()
        .map(|s| s.to_string())
        .unwrap_or_else(|| host_header.split(':').next().unwrap_or("").to_string());

    // Determine scheme: if the proxy is in MITM mode and the original scheme was
    // https/wss, use wss; otherwise fall back to port heuristic.
    let scheme = uri.scheme_str().unwrap_or("ws");
    let default_port: u16 = if scheme == "wss" || scheme == "https" {
        443
    } else {
        80
    };
    let port: u16 = uri.port_u16().unwrap_or(default_port);
    let use_tls = scheme == "wss" || scheme == "https" || port == 443;

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let upstream_scheme = if use_tls { "wss" } else { "ws" };
    let upstream_url = format!(
        "{}://{}:{}{}",
        upstream_scheme, target_host, port, path_and_query
    );
    let record_uri = upstream_url.clone();

    // Build the upstream WebSocket request, forwarding the client's headers.
    let mut ws_req = tokio_tungstenite::tungstenite::http::Request::builder()
        .uri(&upstream_url)
        .header("host", format!("{}:{}", target_host, port))
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key());

    for (name, value) in &headers {
        let name_str = name.as_str().to_lowercase();
        // Forward application headers; skip WS protocol-negotiation headers we set above.
        if matches!(
            name_str.as_str(),
            "host"
                | "upgrade"
                | "connection"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-accept"
        ) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            ws_req = ws_req.header(name.as_str(), v);
        }
    }

    let ws_req = match ws_req.body(()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error=%e, "WS request build failed");
            return Response::builder()
                .status(502)
                .body(Body::from("WebSocket request build failed"))
                .unwrap();
        }
    };

    // Connect to the upstream WebSocket server (with TLS if required).
    let upstream_ws = match tokio::time::timeout(
        connect_timeout + handshake_timeout,
        connect_async_tls_with_config(ws_req, None, false, None),
    )
    .await
    {
        Ok(Ok((ws, _response))) => ws,
        Ok(Err(e)) => {
            tracing::warn!(error=%e, url=%upstream_url, "WS upstream connect failed");
            return Response::builder()
                .status(502)
                .body(Body::from("WebSocket upstream connect failed"))
                .unwrap();
        }
        Err(_) => {
            tracing::warn!(url=%upstream_url, "WS upstream connect timed out");
            return Response::builder()
                .status(504)
                .body(Body::from("WebSocket upstream connect timed out"))
                .unwrap();
        }
    };

    // Record the session request head.
    let mut req_headers_map = crate::middleware::HeaderMap::new();
    for (k, v) in &headers {
        if let Ok(v) = v.to_str() {
            req_headers_map.append(k.to_string(), v.to_string());
        }
    }
    sm.record_request(
        session_id.clone(),
        crate::middleware::RequestContext {
            method: "WS".to_string(),
            uri: record_uri,
            headers: req_headers_map,
            body: bytes::Bytes::new(),
            host: target_host.clone(),
            connection_id: ws_connection_id,
            stream_id: ws_stream_id,
            downstream_protocol: ws_downstream_protocol,
            ..Default::default()
        },
    );

    // Complete the h1 upgrade handshake with the downstream client.
    let mut builder = Response::builder().status(101);
    builder = builder.header("upgrade", "websocket");
    builder = builder.header("connection", "Upgrade");
    // Echo Sec-WebSocket-Accept so the client's browser completes the handshake.
    if let Some(key) = headers
        .get("sec-websocket-key")
        .and_then(|v| v.to_str().ok())
    {
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        builder = builder.header("sec-websocket-accept", accept);
    }

    let on_upgrade = hyper::upgrade::on(req);
    connections.spawn_tracked("websocket-tunnel", peer, async move {
        let upgraded = tokio::select! {
            upgraded = on_upgrade => upgraded,
            _ = wait_for_shutdown(&mut shutdown) => {
                tracing::debug!("WS client upgrade stopped by shutdown");
                return;
            }
        };
        let upgraded = match upgraded {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(error=%e, "WS client upgrade failed");
                return;
            }
        };

        let client_io = hyper_util::rt::TokioIo::new(upgraded);
        let client_ws = WebSocketStream::from_raw_socket(client_io, Role::Server, None).await;

        relay_ws(
            client_ws,
            upstream_ws,
            sm,
            session_id,
            inspect_frames,
            &mut shutdown,
        )
        .await;
    });

    builder
        .body(Body::empty())
        .unwrap_or_else(|_| Response::builder().status(500).body(Body::empty()).unwrap())
}

async fn relay_ws(
    mut client: WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>,
    mut upstream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    sm: crate::session::SharedSessionManager,
    session_id: String,
    inspect_frames: bool,
    shutdown: &mut watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            msg = client.next() => {
                match msg {
                    Some(Ok(m)) => {
                        if inspect_frames {
                            record_frame(&sm, &session_id, &m, crate::session::WsDirection::ClientToServer);
                        }
                        let is_close = m.is_close();
                        if upstream.send(m).await.is_err() || is_close {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            msg = upstream.next() => {
                match msg {
                    Some(Ok(m)) => {
                        if inspect_frames {
                            record_frame(&sm, &session_id, &m, crate::session::WsDirection::ServerToClient);
                        }
                        let is_close = m.is_close();
                        if client.send(m).await.is_err() || is_close {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            _ = wait_for_shutdown(shutdown) => {
                tracing::debug!("WS relay stopped by shutdown");
                break;
            }
        }
    }
    let _ = client.close(None).await;
    let _ = upstream.close(None).await;
}

fn record_frame(
    sm: &crate::session::SharedSessionManager,
    session_id: &str,
    msg: &Message,
    direction: crate::session::WsDirection,
) {
    if let Some((opcode, payload_len, payload_text, payload_hex)) =
        map_tungstenite_message_to_record(msg)
    {
        sm.append_ws_frame(
            session_id,
            crate::session::WsFrame {
                timestamp: chrono::Utc::now(),
                direction,
                opcode,
                payload_len,
                payload_text,
                payload_hex,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_upgrade_header_is_case_insensitive() {
        let req = Request::builder()
            .header("upgrade", "WebSocket")
            .body(())
            .unwrap();

        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn non_websocket_upgrade_not_detected() {
        let req = Request::builder()
            .header("upgrade", "h2c")
            .body(())
            .unwrap();
        assert!(!is_websocket_upgrade(&req));
    }

    #[test]
    fn text_message_maps_to_opcode_1() {
        let msg = Message::Text("hello".to_string());
        let (opcode, len, text, hex) = map_tungstenite_message_to_record(&msg).unwrap();
        assert_eq!(opcode, 0x1);
        assert_eq!(len, 5);
        assert_eq!(text.as_deref(), Some("hello"));
        assert!(hex.is_none());
    }

    #[test]
    fn binary_message_maps_to_opcode_2_with_hex() {
        let msg = Message::Binary(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let (opcode, len, text, hex) = map_tungstenite_message_to_record(&msg).unwrap();
        assert_eq!(opcode, 0x2);
        assert_eq!(len, 4);
        assert!(text.is_none());
        assert_eq!(hex.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn close_message_maps_to_opcode_8() {
        let msg = Message::Close(None);
        let (opcode, _, _, _) = map_tungstenite_message_to_record(&msg).unwrap();
        assert_eq!(opcode, 0x8);
    }

    #[test]
    fn large_text_preview_truncated_to_512_chars() {
        let big = "x".repeat(1000);
        let msg = Message::Text(big);
        let (_, len, text, _) = map_tungstenite_message_to_record(&msg).unwrap();
        assert_eq!(len, 1000);
        assert_eq!(text.unwrap().len(), 512);
    }
}
