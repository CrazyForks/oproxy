use axum::{extract::State, response::IntoResponse};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{CloseFrame, Message},
    },
};

use crate::AppState;
use crate::core::engine::is_binary_content_type;
use crate::core::forward::{
    ApplicationProtocol, BodyMode, ProtocolContext, WireProtocol, encode_grpc_frame,
};
use crate::middleware::{RequestContext, ResponseContext};
use crate::security::{AdminEgressPolicy, enforce_admin_egress_policy};
use crate::session::{
    GrpcMessage, InspectionMetrics, SessionEvent, SessionSource, WsDirection, WsFrame,
};

use super::admin_egress_policy_response;

/// How long the Compose WebSocket forwarder waits for the next server frame
/// before considering the exchange complete.
const WS_FORWARD_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// Cap on server frames collected by the Compose WebSocket forwarder; keeps a
/// chatty upstream from holding the admin request open indefinitely.
const WS_FORWARD_MAX_SERVER_FRAMES: usize = 10;

#[derive(serde::Deserialize)]
pub(super) struct ForwardReq {
    #[serde(default)]
    kind: ForwardKind,
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ForwardKind {
    #[default]
    Http,
    Http2,
    Http3,
    Grpc,
}

#[derive(serde::Serialize)]
struct ForwardResp {
    status: u16,
    status_text: String,
    headers: HashMap<String, String>,
    body: String,
    is_binary: bool,
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<ForwardProtocolResp>,
}

#[derive(serde::Serialize)]
struct ForwardProtocolResp {
    downstream: String,
    upstream: Option<String>,
    application: String,
    body_mode: String,
}

#[derive(serde::Deserialize)]
pub(super) struct ForwardWsReq {
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    frames: Vec<ForwardWsFrameReq>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct ForwardWsFrameReq {
    #[serde(default = "default_ws_opcode")]
    opcode: String,
    #[serde(default)]
    payload: String,
}

fn default_ws_opcode() -> String {
    "text".to_string()
}

#[derive(serde::Serialize)]
pub(super) struct ForwardWsResp {
    status: u16,
    status_text: String,
    session_id: String,
    frames: Vec<ForwardWsFrameResp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<ForwardProtocolResp>,
}

/// Why a WebSocket forward failed, so each caller (axum handler, assistant
/// executor) can map it to its own response shape.
#[derive(Debug)]
pub(super) enum ForwardWsFailure {
    /// Malformed request (bad URL/scheme). No session was recorded.
    BadRequest(String),
    /// Blocked by the admin egress policy. No session was recorded.
    EgressBlocked(String),
    /// Connection/upstream failure; a 502 response has already been recorded
    /// onto the session.
    Upstream { session_id: String, error: String },
}

#[derive(serde::Serialize)]
struct ForwardWsFrameResp {
    direction: String,
    opcode: String,
    payload: Option<String>,
    payload_len: usize,
}

pub(super) async fn forward_request(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<ForwardReq>,
) -> impl IntoResponse {
    let method = match reqwest::Method::from_bytes(req.method.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": format!("Invalid HTTP method: {}", req.method)
                })),
            )
                .into_response();
        }
    };
    let url_parsed = match reqwest::Url::parse(&req.url) {
        Ok(u) => u,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": format!("Invalid URL: {e}") })),
            )
                .into_response();
        }
    };
    if !matches!(url_parsed.scheme(), "http" | "https") {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": format!("Unsupported URL scheme: {}", url_parsed.scheme())
            })),
        )
            .into_response();
    }
    if let Err(e) =
        enforce_admin_egress_policy(&url_parsed, AdminEgressPolicy::from_config(&state.config))
            .await
    {
        return admin_egress_policy_response(e);
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let is_grpc = req.kind == ForwardKind::Grpc;
    let request_body = req.body.clone().unwrap_or_default();
    let body_bytes = if is_grpc {
        encode_grpc_frame(false, request_body.as_bytes())
    } else {
        request_body.into_bytes()
    };
    let host = match (url_parsed.host_str(), url_parsed.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => String::new(),
    };
    let display_uri = req.url.clone();

    // Record request in session manager
    let protocol_context = protocol_context_for_kind(req.kind, url_parsed.scheme(), None);
    let req_ctx = RequestContext {
        method: if is_grpc {
            "POST".to_string()
        } else {
            req.method.clone()
        },
        uri: display_uri.clone(),
        host: host.clone(),
        headers: req.headers.clone().into(),
        body: bytes::Bytes::from(body_bytes.clone()),
        downstream_protocol: Some(protocol_context.downstream.label().to_string()),
        protocol_context: Some(protocol_context),
        ..Default::default()
    };
    let request_size_bytes = req_ctx.body.len();
    state
        .api_handler
        .session_manager
        .record_request_with_source(session_id.clone(), req_ctx, SessionSource::AdminForward);
    if req.note.is_some() || req.tags.is_some() {
        state
            .api_handler
            .session_manager
            .annotate(&session_id, req.note.clone(), req.tags.clone())
            .await;
    }
    if is_grpc {
        state.api_handler.session_manager.append_event(
            &session_id,
            SessionEvent::GrpcMessage {
                direction: "request".to_string(),
                message: grpc_message("request", false, body_bytes.len().saturating_sub(5) as u32),
            },
        );
    }

    // Build and send request using the proxy engine's http client
    let method = if is_grpc {
        reqwest::Method::POST
    } else {
        method
    };
    let mut builder = state
        .proxy_engine
        .http_client()
        .await
        .request(method, &req.url);
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    if is_grpc
        && !req
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("content-type"))
    {
        builder = builder.header("content-type", "application/grpc+proto");
    }
    if is_grpc || !body_bytes.is_empty() {
        builder = builder.body(body_bytes.clone());
    }

    let t0 = std::time::Instant::now();
    match builder.send().await {
        Ok(res) => {
            let ttfb_ms = t0.elapsed().as_millis() as u64;
            let status = res.status().as_u16();
            let upstream_protocol = crate::core::engine::protocol_label(res.version()).to_string();
            let mut res_headers: HashMap<String, String> = HashMap::new();
            for (k, v) in res.headers() {
                res_headers.insert(k.to_string(), v.to_str().unwrap_or("").to_string());
            }
            let content_type = res_headers.get("content-type").cloned().unwrap_or_default();
            let bytes = res.bytes().await.unwrap_or_default();
            if is_grpc {
                for (compressed, payload_len) in decode_grpc_message_lengths(&bytes) {
                    state.api_handler.session_manager.append_event(
                        &session_id,
                        SessionEvent::GrpcMessage {
                            direction: "response".to_string(),
                            message: grpc_message("response", compressed, payload_len),
                        },
                    );
                }
            }
            let body_ms = t0.elapsed().as_millis() as u64 - ttfb_ms;
            let (body, is_binary) = if is_binary_content_type(&content_type) {
                (
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                    true,
                )
            } else {
                (String::from_utf8_lossy(&bytes).to_string(), false)
            };

            // Record response (one protocol context, shared by the recording and
            // the API response below).
            let final_protocol = protocol_context_for_kind(
                req.kind,
                url_parsed.scheme(),
                Some(protocol_from_label(&upstream_protocol)),
            );
            let res_ctx = ResponseContext {
                status,
                headers: res_headers.clone().into(),
                body: bytes.clone(),
                request_uri: display_uri,
                session_id: Some(session_id.clone()),
                ttfb_ms,
                body_ms,
                protocol: Some(upstream_protocol.clone()),
                protocol_context: Some(final_protocol.clone()),
                ..Default::default()
            };
            let metrics = crate::session::InspectionMetrics {
                latency_ms: t0.elapsed().as_millis() as u64,
                request_size_bytes,
                response_size_bytes: bytes.len(),
                status_code: status,
                ttfb_ms,
                body_ms,
                protocol: Some(upstream_protocol.clone()),
                ..Default::default()
            };
            state
                .api_handler
                .session_manager
                .record_response_with_metrics(session_id.clone(), res_ctx, metrics);

            let status_text = reqwest::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("")
                .to_string();
            axum::Json(ForwardResp {
                status,
                status_text,
                headers: res_headers,
                body,
                is_binary,
                session_id,
                protocol: Some(protocol_response(final_protocol)),
            })
            .into_response()
        }
        Err(e) => {
            let res_ctx = ResponseContext {
                status: 502,
                body: bytes::Bytes::from(e.to_string()),
                request_uri: display_uri,
                session_id: Some(session_id.clone()),
                protocol_context: Some(protocol_context_for_kind(
                    req.kind,
                    url_parsed.scheme(),
                    None,
                )),
                ..Default::default()
            };
            let metrics = crate::session::InspectionMetrics {
                latency_ms: t0.elapsed().as_millis() as u64,
                request_size_bytes,
                response_size_bytes: e.to_string().len(),
                status_code: 502,
                ttfb_ms: t0.elapsed().as_millis() as u64,
                body_ms: 0,
                ..Default::default()
            };
            state
                .api_handler
                .session_manager
                .record_response_with_metrics(session_id.clone(), res_ctx, metrics);
            (axum::http::StatusCode::BAD_GATEWAY, e.to_string()).into_response()
        }
    }
}

pub(super) async fn forward_websocket(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<ForwardWsReq>,
) -> impl IntoResponse {
    match forward_websocket_exchange(&state, req).await {
        Ok(resp) => axum::Json(resp).into_response(),
        Err(ForwardWsFailure::BadRequest(error)) => (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
        Err(ForwardWsFailure::EgressBlocked(error)) => admin_egress_policy_response(error),
        Err(ForwardWsFailure::Upstream { session_id, error }) => (
            axum::http::StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({ "error": error, "session_id": session_id })),
        )
            .into_response(),
    }
}

/// Connects to an upstream WebSocket, sends the scripted frames, collects up to
/// [`WS_FORWARD_MAX_SERVER_FRAMES`] replies, and records everything as an
/// admin-forward session. Shared by the `/admin/forward/websocket` handler and
/// the assistant action executor so both paths behave identically.
pub(super) async fn forward_websocket_exchange(
    state: &Arc<AppState>,
    req: ForwardWsReq,
) -> Result<ForwardWsResp, ForwardWsFailure> {
    let url = reqwest::Url::parse(&req.url)
        .map_err(|e| ForwardWsFailure::BadRequest(format!("Invalid URL: {e}")))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(ForwardWsFailure::BadRequest(format!(
            "Unsupported WebSocket URL scheme: {}",
            url.scheme()
        )));
    }
    let policy_url = ws_url_to_http(&url).map_err(ForwardWsFailure::BadRequest)?;
    enforce_admin_egress_policy(&policy_url, AdminEgressPolicy::from_config(&state.config))
        .await
        .map_err(ForwardWsFailure::EgressBlocked)?;

    let session_id = uuid::Uuid::new_v4().to_string();
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => String::new(),
    };
    let mut headers = req.headers.clone();
    headers.insert("upgrade".to_string(), "websocket".to_string());
    let proto = ProtocolContext {
        upstream: Some(WireProtocol::WebSocket),
        ..ProtocolContext::websocket(url.scheme()).with_identity(Some(session_id.clone()), Some(1))
    };
    let req_ctx = RequestContext {
        method: "WS".to_string(),
        uri: req.url.clone(),
        host,
        headers: headers.clone().into(),
        body: bytes::Bytes::new(),
        connection_id: Some(session_id.clone()),
        stream_id: Some(1),
        downstream_protocol: Some("WebSocket".to_string()),
        protocol_context: Some(proto.clone()),
        ..Default::default()
    };
    state
        .api_handler
        .session_manager
        .record_request_with_source(session_id.clone(), req_ctx, SessionSource::AdminForward);
    if req.note.is_some() || req.tags.is_some() {
        state
            .api_handler
            .session_manager
            .annotate(&session_id, req.note.clone(), req.tags.clone())
            .await;
    }

    let started = std::time::Instant::now();
    let mut request = match req.url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            return Err(record_ws_failure(
                state,
                session_id,
                &req.url,
                started,
                e.to_string(),
            ));
        }
    };
    for (name, value) in req.headers {
        if is_ws_forward_header(&name)
            && let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(&value),
            )
        {
            request.headers_mut().insert(name, value);
        }
    }

    let mut ws = match connect_async(request).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            return Err(record_ws_failure(
                state,
                session_id,
                &req.url,
                started,
                e.to_string(),
            ));
        }
    };

    let mut frames = Vec::new();
    for frame in req.frames {
        let message = match frame.opcode.as_str() {
            "binary" => Message::Binary(
                base64::engine::general_purpose::STANDARD
                    .decode(frame.payload.as_bytes())
                    .unwrap_or_else(|_| frame.payload.clone().into_bytes()),
            ),
            "ping" => Message::Ping(frame.payload.into_bytes()),
            "pong" => Message::Pong(frame.payload.into_bytes()),
            "close" => Message::Close(Some(CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: frame.payload.into(),
            })),
            _ => Message::Text(frame.payload),
        };
        record_ws_message(
            state,
            &session_id,
            WsDirection::ClientToServer,
            &message,
            &mut frames,
        );
        if let Err(e) = ws.send(message).await {
            return Err(record_ws_failure(
                state,
                session_id,
                &req.url,
                started,
                e.to_string(),
            ));
        }
    }

    while let Ok(Some(next)) = tokio::time::timeout(WS_FORWARD_REPLY_TIMEOUT, ws.next()).await {
        match next {
            Ok(message) => {
                record_ws_message(
                    state,
                    &session_id,
                    WsDirection::ServerToClient,
                    &message,
                    &mut frames,
                );
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
            Err(e) => {
                return Err(record_ws_failure(
                    state,
                    session_id,
                    &req.url,
                    started,
                    e.to_string(),
                ));
            }
        }
        if frames.iter().filter(|f| f.direction == "server").count() >= WS_FORWARD_MAX_SERVER_FRAMES
        {
            break;
        }
    }
    let _ = ws.close(None).await;

    let response = ResponseContext {
        status: 101,
        headers: HashMap::from([("upgrade".to_string(), "websocket".to_string())]).into(),
        request_uri: req.url.clone(),
        session_id: Some(session_id.clone()),
        protocol: Some("WebSocket".to_string()),
        protocol_context: Some(proto.clone()),
        ..Default::default()
    };
    state
        .api_handler
        .session_manager
        .record_response_with_metrics(
            session_id.clone(),
            response,
            InspectionMetrics {
                latency_ms: started.elapsed().as_millis() as u64,
                status_code: 101,
                protocol: Some("WebSocket".to_string()),
                ..Default::default()
            },
        );

    Ok(ForwardWsResp {
        status: 101,
        status_text: "Switching Protocols".to_string(),
        session_id,
        frames,
        protocol: Some(protocol_response(proto)),
    })
}

fn record_ws_failure(
    state: &Arc<AppState>,
    session_id: String,
    request_uri: &str,
    started: std::time::Instant,
    error: String,
) -> ForwardWsFailure {
    let response = ResponseContext {
        status: 502,
        body: bytes::Bytes::from(error.clone()),
        request_uri: request_uri.to_string(),
        session_id: Some(session_id.clone()),
        ..Default::default()
    };
    state
        .api_handler
        .session_manager
        .record_response_with_metrics(
            session_id.clone(),
            response,
            InspectionMetrics {
                latency_ms: started.elapsed().as_millis() as u64,
                status_code: 502,
                response_size_bytes: error.len(),
                ..Default::default()
            },
        );
    ForwardWsFailure::Upstream { session_id, error }
}

fn protocol_context_for_kind(
    kind: ForwardKind,
    scheme: &str,
    upstream: Option<WireProtocol>,
) -> ProtocolContext {
    match kind {
        ForwardKind::Http => ProtocolContext {
            downstream: WireProtocol::Http1,
            upstream,
            application: ApplicationProtocol::Http,
            body_mode: BodyMode::Full,
            scheme: scheme.to_string(),
            connection_id: None,
            stream_id: None,
        },
        ForwardKind::Http2 => ProtocolContext {
            downstream: WireProtocol::Http2,
            upstream,
            application: ApplicationProtocol::Http,
            body_mode: BodyMode::Full,
            scheme: scheme.to_string(),
            connection_id: None,
            stream_id: None,
        },
        ForwardKind::Http3 => ProtocolContext {
            downstream: WireProtocol::Http3,
            upstream,
            application: ApplicationProtocol::Http,
            body_mode: BodyMode::Full,
            scheme: scheme.to_string(),
            connection_id: None,
            stream_id: None,
        },
        ForwardKind::Grpc => ProtocolContext {
            downstream: WireProtocol::Http2,
            upstream,
            application: ApplicationProtocol::Grpc,
            body_mode: BodyMode::StreamMessages,
            scheme: scheme.to_string(),
            connection_id: None,
            stream_id: None,
        },
    }
}

fn protocol_from_label(label: &str) -> WireProtocol {
    match label {
        "HTTP/2" => WireProtocol::Http2,
        "HTTP/3" => WireProtocol::Http3,
        _ => WireProtocol::Http1,
    }
}

fn protocol_response(ctx: ProtocolContext) -> ForwardProtocolResp {
    ForwardProtocolResp {
        downstream: ctx.downstream.label().to_string(),
        upstream: ctx.upstream.map(|p| p.label().to_string()),
        application: ctx.application.match_value().to_string(),
        body_mode: ctx.body_mode.match_value().to_string(),
    }
}

fn decode_grpc_message_lengths(bytes: &[u8]) -> Vec<(bool, u32)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        let compressed = bytes[i] != 0;
        let len = u32::from_be_bytes([bytes[i + 1], bytes[i + 2], bytes[i + 3], bytes[i + 4]]);
        i += 5;
        let end = i.saturating_add(len as usize);
        if end > bytes.len() {
            break;
        }
        out.push((compressed, len));
        i = end;
    }
    out
}

fn grpc_message(direction: &str, compressed: bool, length: u32) -> GrpcMessage {
    GrpcMessage {
        direction: direction.to_string(),
        compressed,
        length,
        fields: Vec::new(),
    }
}

fn ws_url_to_http(url: &reqwest::Url) -> Result<reqwest::Url, String> {
    let mut out = url.clone();
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => return Err(format!("Unsupported WebSocket URL scheme: {other}")),
    };
    out.set_scheme(scheme)
        .map_err(|_| format!("Unable to translate {} URL for egress policy", url.scheme()))?;
    Ok(out)
}

fn is_ws_forward_header(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-accept"
            | "content-length"
    )
}

fn record_ws_message(
    state: &Arc<AppState>,
    session_id: &str,
    direction: WsDirection,
    message: &Message,
    frames: &mut Vec<ForwardWsFrameResp>,
) {
    let (opcode, payload_len, payload_text, payload_hex, opcode_label) = match message {
        Message::Text(text) => (
            0x1,
            text.len(),
            Some(text.chars().take(512).collect::<String>()),
            None,
            "text",
        ),
        Message::Binary(data) => {
            let hex = data
                .iter()
                .take(64)
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            (
                0x2,
                data.len(),
                None,
                if hex.is_empty() { None } else { Some(hex) },
                "binary",
            )
        }
        Message::Ping(data) => (0x9, data.len(), None, None, "ping"),
        Message::Pong(data) => (0xA, data.len(), None, None, "pong"),
        Message::Close(_) => (0x8, 0, None, None, "close"),
        Message::Frame(_) => return,
    };
    let frame = WsFrame {
        timestamp: chrono::Utc::now(),
        direction: direction.clone(),
        opcode,
        payload_len,
        payload_text: payload_text.clone(),
        payload_hex: payload_hex.clone(),
    };
    state
        .api_handler
        .session_manager
        .append_ws_frame(session_id, frame);
    frames.push(ForwardWsFrameResp {
        direction: match direction {
            WsDirection::ClientToServer => "client".to_string(),
            WsDirection::ServerToClient => "server".to_string(),
        },
        opcode: opcode_label.to_string(),
        payload: payload_text.or(payload_hex),
        payload_len,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_frame_round_trips_message_lengths() {
        let mut body = encode_grpc_frame(false, b"hello");
        body.extend_from_slice(&encode_grpc_frame(true, b"world!"));

        assert_eq!(
            decode_grpc_message_lengths(&body),
            vec![(false, 5), (true, 6)]
        );
    }

    #[test]
    fn grpc_frame_decoder_ignores_truncated_tail() {
        let mut body = encode_grpc_frame(false, b"ok");
        body.extend_from_slice(&[0, 0, 0, 0, 8, b'p']);

        assert_eq!(decode_grpc_message_lengths(&body), vec![(false, 2)]);
    }

    #[test]
    fn grpc_message_carries_direction_in_event_payload() {
        let message = grpc_message("response", true, 42);

        assert_eq!(message.direction, "response");
        assert!(message.compressed);
        assert_eq!(message.length, 42);
    }

    #[test]
    fn websocket_policy_url_translates_ws_schemes() {
        let ws = reqwest::Url::parse("ws://example.test/socket").unwrap();
        let wss = reqwest::Url::parse("wss://example.test/socket").unwrap();

        assert_eq!(
            ws_url_to_http(&ws).unwrap().as_str(),
            "http://example.test/socket"
        );
        assert_eq!(
            ws_url_to_http(&wss).unwrap().as_str(),
            "https://example.test/socket"
        );
    }

    #[test]
    fn websocket_forward_header_filter_blocks_handshake_headers() {
        assert!(!is_ws_forward_header("Connection"));
        assert!(!is_ws_forward_header("sec-websocket-key"));
        assert!(!is_ws_forward_header("content-length"));
        assert!(is_ws_forward_header("authorization"));
        assert!(is_ws_forward_header("x-trace-id"));
    }

    #[test]
    fn grpc_protocol_response_uses_stream_message_body_mode() {
        let response = protocol_response(protocol_context_for_kind(
            ForwardKind::Grpc,
            "https",
            Some(WireProtocol::Http2),
        ));

        assert_eq!(response.downstream, "HTTP/2");
        assert_eq!(response.upstream.as_deref(), Some("HTTP/2"));
        assert_eq!(response.application, "grpc");
        assert_eq!(response.body_mode, "stream_messages");
    }
}
