use async_trait::async_trait;
use bytes::Bytes;

use crate::middleware::{
    BodyObserver, Middleware, MiddlewareAction, RequestContext, ResponseContext,
};
use crate::session::{GrpcField, GrpcInfo, GrpcMessage, InspectorData, SharedSessionManager};

pub struct GrpcInspectorMiddleware {
    pub session_manager: SharedSessionManager,
}

impl GrpcInspectorMiddleware {
    pub fn is_grpc(ctx: &RequestContext) -> bool {
        ctx.headers
            .get("content-type")
            .map(|ct| ct.starts_with("application/grpc"))
            .unwrap_or(false)
    }

    /// Parse service and method from URI pattern `/package.ServiceName/MethodName`.
    pub fn parse_uri(uri: &str) -> (Option<String>, Option<String>) {
        let path = uri
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let path = if let Some(slash) = path.find('/') {
            &path[slash..]
        } else {
            path
        };
        let parts: Vec<&str> = path.trim_start_matches('/').splitn(2, '/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            (Some(parts[0].to_string()), Some(parts[1].to_string()))
        } else if parts.len() == 1 && !parts[0].is_empty() {
            (Some(parts[0].to_string()), None)
        } else {
            (None, None)
        }
    }

    /// Decode a gRPC framed message:
    /// [1 byte: compressed flag][4 bytes: big-endian message length][N bytes: protobuf]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn decode_grpc_frame(data: &[u8]) -> Option<(bool, Vec<u8>)> {
        if data.len() < 5 {
            return None;
        }
        let compressed = data[0] != 0;
        let msg_len = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
        if data.len() < 5 + msg_len {
            return None;
        }
        Some((compressed, data[5..5 + msg_len].to_vec()))
    }

    /// Wire-format decode without schema — extracts field_number, wire_type, value.
    pub fn decode_wire_format(data: &[u8]) -> Vec<GrpcField> {
        let mut fields = Vec::new();
        let mut pos = 0;

        while pos < data.len() {
            // Read varint tag
            let (tag, n) = match read_varint(data, pos) {
                Some(v) => v,
                None => break,
            };
            pos += n;
            let field_number = (tag >> 3) as u32;
            let wire_type = (tag & 0x7) as u8;

            let value = match wire_type {
                0 => {
                    // Varint
                    match read_varint(data, pos) {
                        Some((v, n)) => {
                            pos += n;
                            serde_json::Value::Number(v.into())
                        }
                        None => break,
                    }
                }
                1 => {
                    // 64-bit
                    if pos + 8 > data.len() {
                        break;
                    }
                    let bytes = &data[pos..pos + 8];
                    pos += 8;
                    let hex = bytes
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>();
                    serde_json::Value::String(hex)
                }
                2 => {
                    // Length-delimited
                    match read_varint(data, pos) {
                        Some((len, n)) => {
                            pos += n;
                            let len = len as usize;
                            if pos + len > data.len() {
                                break;
                            }
                            let bytes = &data[pos..pos + len];
                            pos += len;
                            // Try to decode as UTF-8 string, else hex
                            match std::str::from_utf8(bytes) {
                                Ok(s) => serde_json::Value::String(s.to_string()),
                                Err(_) => {
                                    let hex = bytes
                                        .iter()
                                        .map(|b| format!("{:02x}", b))
                                        .collect::<String>();
                                    serde_json::Value::String(hex)
                                }
                            }
                        }
                        None => break,
                    }
                }
                5 => {
                    // 32-bit
                    if pos + 4 > data.len() {
                        break;
                    }
                    let bytes = &data[pos..pos + 4];
                    pos += 4;
                    let hex = bytes
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>();
                    serde_json::Value::String(hex)
                }
                _ => {
                    // Unknown wire type — stop parsing
                    break;
                }
            };

            fields.push(GrpcField {
                field_number,
                wire_type,
                value,
            });
        }
        fields
    }
}

fn read_varint(data: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    let start = pos;
    loop {
        if pos >= data.len() || shift >= 64 {
            return None;
        }
        let byte = data[pos];
        pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
    }
    Some((result, pos - start))
}

/// Incremental gRPC frame splitter for the streaming forwarder (Phase 4).
///
/// gRPC length-prefixed framing is `[1B compressed flag][4B big-endian length]
/// [N bytes message]`. On an HTTP/2 (or h3) stream these frames arrive split
/// across, or coalesced within, arbitrary network chunks. `push` buffers bytes
/// and returns every *complete* message it can, leaving any partial trailing
/// frame buffered for the next chunk. This is what makes `needs_full_body` apply
/// **per message, not per stream** (RFC §8.2, `Granularity::Messages`): each
/// message is delivered whole to the inspector while the stream is never buffered.
#[derive(Debug, Default)]
pub struct GrpcFrameSplitter {
    buf: Vec<u8>,
}

impl GrpcFrameSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns every complete `(compressed, message_bytes)`
    /// frame now available. A partial trailing frame stays buffered.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<(bool, Vec<u8>)> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 5 {
                break;
            }
            let msg_len =
                u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
            let frame_len = 5 + msg_len;
            if self.buf.len() < frame_len {
                break;
            }
            let compressed = self.buf[0] != 0;
            let payload = self.buf[5..frame_len].to_vec();
            out.push((compressed, payload));
            self.buf.drain(0..frame_len);
        }
        out
    }

    /// Number of buffered bytes belonging to an as-yet-incomplete frame.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

impl GrpcInspectorMiddleware {
    /// Builds a recorded [`GrpcMessage`] from a single decoded frame.
    pub fn message_from_frame(
        direction: &str,
        compressed: bool,
        proto_bytes: &[u8],
    ) -> GrpcMessage {
        GrpcMessage {
            direction: direction.to_string(),
            compressed,
            length: proto_bytes.len() as u32,
            fields: Self::decode_wire_format(proto_bytes),
        }
    }
}

#[async_trait]
impl Middleware for GrpcInspectorMiddleware {
    fn name(&self) -> &str {
        "GrpcInspectorMiddleware"
    }

    fn body_hint(&self, head: &RequestContext) -> crate::core::forward::BodyHint {
        if Self::is_grpc(head) {
            crate::core::forward::BodyHint::StreamingInspect {
                granularity: crate::core::forward::Granularity::Messages,
            }
        } else {
            crate::core::forward::BodyHint::FullBody
        }
    }

    fn stream_observer(&self, req: &RequestContext) -> Option<Box<dyn BodyObserver>> {
        if !Self::is_grpc(req) {
            return None;
        }
        let session_id = req.session_id.clone()?;
        let (service, method) = Self::parse_uri(&req.uri);
        Some(Box::new(GrpcStreamObserver {
            session_manager: self.session_manager.clone(),
            session_id,
            service,
            method,
            req_splitter: GrpcFrameSplitter::new(),
            res_splitter: GrpcFrameSplitter::new(),
            messages: Vec::new(),
            grpc_status: None,
            grpc_status_message: None,
        }))
    }

    async fn on_request(&self, ctx: &mut RequestContext) -> MiddlewareAction {
        if !Self::is_grpc(ctx) {
            return MiddlewareAction::Continue;
        }
        // On the streaming path the body is not buffered; set service/method from URI
        // only. The GrpcStreamObserver will decode the actual frames.
        let (service, method) = Self::parse_uri(&ctx.uri);
        ctx.inspector.grpc = Some(GrpcInfo {
            service,
            method,
            messages: vec![],
            grpc_status: None,
            grpc_status_message: None,
        });
        MiddlewareAction::Continue
    }
}

// ── GrpcStreamObserver ───────────────────────────────────────────────────────

/// Per-stream observer for gRPC exchanges. Splits the bidirectional byte stream
/// into length-prefixed messages and records them as [`GrpcMessage`] entries.
/// Created by [`GrpcInspectorMiddleware::stream_observer`].
struct GrpcStreamObserver {
    session_manager: SharedSessionManager,
    session_id: String,
    service: Option<String>,
    method: Option<String>,
    req_splitter: GrpcFrameSplitter,
    res_splitter: GrpcFrameSplitter,
    messages: Vec<GrpcMessage>,
    /// `grpc-status` value from response headers (best-effort; trailers not accessible
    /// via the reqwest streaming API in v1).
    grpc_status: Option<String>,
    grpc_status_message: Option<String>,
}

#[async_trait]
impl BodyObserver for GrpcStreamObserver {
    fn on_request_chunk(&mut self, chunk: &Bytes) {
        for (compressed, proto) in self.req_splitter.push(chunk) {
            self.messages
                .push(GrpcInspectorMiddleware::message_from_frame(
                    "request", compressed, &proto,
                ));
        }
    }

    fn on_response_head(&mut self, res: &ResponseContext, _start: std::time::Instant) {
        self.grpc_status = res.headers.get("grpc-status").cloned();
        self.grpc_status_message = res.headers.get("grpc-message").cloned();
    }

    fn on_chunk(&mut self, chunk: &Bytes) {
        for (compressed, proto) in self.res_splitter.push(chunk) {
            self.messages
                .push(GrpcInspectorMiddleware::message_from_frame(
                    "response", compressed, &proto,
                ));
        }
    }

    async fn finish(self: Box<Self>) {
        self.session_manager.update_inspector_data(
            &self.session_id,
            InspectorData {
                grpc: Some(GrpcInfo {
                    service: self.service,
                    method: self.method,
                    messages: self.messages,
                    grpc_status: self.grpc_status,
                    grpc_status_message: self.grpc_status_message,
                }),
                ..Default::default()
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make_ctx(content_type: &str, uri: &str) -> RequestContext {
        let mut headers = crate::middleware::HeaderMap::new();
        headers.insert("content-type", content_type.to_string());
        RequestContext {
            method: "POST".to_string(),
            uri: uri.to_string(),
            headers,
            body: bytes::Bytes::new(),
            host: "api.example.com".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn grpc_content_type_detected() {
        let ctx = make_ctx("application/grpc", "/pkg.Service/Method");
        assert!(GrpcInspectorMiddleware::is_grpc(&ctx));
    }

    #[test]
    fn grpc_proto_content_type_detected() {
        let ctx = make_ctx("application/grpc+proto", "/pkg.Service/Method");
        assert!(GrpcInspectorMiddleware::is_grpc(&ctx));
    }

    #[test]
    fn non_grpc_not_detected() {
        let ctx = make_ctx("application/json", "/api");
        assert!(!GrpcInspectorMiddleware::is_grpc(&ctx));
    }

    #[test]
    fn service_and_method_extracted_from_uri() {
        let (svc, method) = GrpcInspectorMiddleware::parse_uri("/pkg.UserService/GetUser");
        assert_eq!(svc.as_deref(), Some("pkg.UserService"));
        assert_eq!(method.as_deref(), Some("GetUser"));
    }

    #[test]
    fn uri_with_host_prefix_parsed() {
        let (svc, method) =
            GrpcInspectorMiddleware::parse_uri("http://api.example.com/pkg.Service/Method");
        assert_eq!(svc.as_deref(), Some("pkg.Service"));
        assert_eq!(method.as_deref(), Some("Method"));
    }

    #[test]
    fn empty_uri_returns_none() {
        let (svc, method) = GrpcInspectorMiddleware::parse_uri("/");
        assert!(svc.is_none());
        assert!(method.is_none());
    }

    #[test]
    fn grpc_frame_parsed_correctly() {
        // Build a valid gRPC frame: [0x00][0x00 0x00 0x00 0x05][proto bytes]
        let proto = b"\x0a\x03foo"; // field 1, wire type 2, "foo"
        let mut frame = vec![0u8, 0, 0, 0, proto.len() as u8];
        frame.extend_from_slice(proto);
        let (compressed, data) = GrpcInspectorMiddleware::decode_grpc_frame(&frame).unwrap();
        assert!(!compressed);
        assert_eq!(data, proto);
    }

    #[test]
    fn compressed_frame_flag_set() {
        let proto = b"\x0a\x03foo";
        let mut frame = vec![0x01u8, 0, 0, 0, proto.len() as u8]; // compressed flag = 1
        frame.extend_from_slice(proto);
        let (compressed, _) = GrpcInspectorMiddleware::decode_grpc_frame(&frame).unwrap();
        assert!(compressed);
    }

    #[test]
    fn short_frame_returns_none() {
        assert!(GrpcInspectorMiddleware::decode_grpc_frame(&[0x00, 0x00]).is_none());
    }

    #[test]
    fn wire_format_varint_field_extracted() {
        // field 1, wire type 0 (varint), value 42
        // tag = (1 << 3) | 0 = 0x08, value = 42
        let data = vec![0x08u8, 0x2a];
        let fields = GrpcInspectorMiddleware::decode_wire_format(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, 1);
        assert_eq!(fields[0].wire_type, 0);
        assert_eq!(fields[0].value, serde_json::json!(42));
    }

    #[test]
    fn wire_format_string_field_extracted() {
        // field 1, wire type 2 (length-delimited), value "hi"
        // tag = 0x0a, length = 2, "hi"
        let data = vec![0x0au8, 0x02, b'h', b'i'];
        let fields = GrpcInspectorMiddleware::decode_wire_format(&data);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].value, serde_json::json!("hi"));
    }

    #[test]
    fn empty_proto_gives_no_fields() {
        let fields = GrpcInspectorMiddleware::decode_wire_format(&[]);
        assert!(fields.is_empty());
    }

    fn grpc_frame(compressed: bool, proto: &[u8]) -> Vec<u8> {
        let mut f = vec![if compressed { 1 } else { 0 }];
        f.extend_from_slice(&(proto.len() as u32).to_be_bytes());
        f.extend_from_slice(proto);
        f
    }

    #[test]
    fn splitter_emits_single_complete_frame() {
        let mut s = GrpcFrameSplitter::new();
        let out = s.push(&grpc_frame(false, b"\x0a\x03foo"));
        assert_eq!(out.len(), 1);
        assert!(!out[0].0);
        assert_eq!(out[0].1, b"\x0a\x03foo");
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn splitter_emits_multiple_frames_in_one_chunk() {
        let mut s = GrpcFrameSplitter::new();
        let mut chunk = grpc_frame(false, b"aa");
        chunk.extend_from_slice(&grpc_frame(true, b"bbbb"));
        let out = s.push(&chunk);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].1, b"aa");
        assert!(out[1].0, "second frame's compressed flag must be set");
        assert_eq!(out[1].1, b"bbbb");
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn splitter_reassembles_frame_split_across_chunks() {
        let mut s = GrpcFrameSplitter::new();
        let frame = grpc_frame(false, b"hello-message");
        let (a, b) = frame.split_at(4); // split mid-header
        assert!(s.push(a).is_empty(), "partial header yields nothing yet");
        assert!(s.pending() > 0);
        let out = s.push(b);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, b"hello-message");
        assert_eq!(s.pending(), 0);
    }

    #[test]
    fn splitter_keeps_trailing_partial_frame_buffered() {
        let mut s = GrpcFrameSplitter::new();
        let mut chunk = grpc_frame(false, b"complete");
        chunk.extend_from_slice(&[0, 0, 0, 0, 10, b'p']); // header claims 10, only 1 byte
        let out = s.push(&chunk);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, b"complete");
        assert_eq!(
            s.pending(),
            6,
            "the incomplete trailing frame stays buffered"
        );
    }

    #[test]
    fn message_from_frame_decodes_fields_and_direction() {
        // field 1, wire type 2, "hi"
        let msg = GrpcInspectorMiddleware::message_from_frame(
            "response",
            true,
            &[0x0a, 0x02, b'h', b'i'],
        );
        assert_eq!(msg.direction, "response");
        assert!(msg.compressed);
        assert_eq!(msg.length, 4);
        assert_eq!(msg.fields.len(), 1);
        assert_eq!(msg.fields[0].value, serde_json::json!("hi"));
    }

    // ── GrpcStreamObserver tests ──────────────────────────────────────────────

    fn make_sm() -> crate::session::SharedSessionManager {
        std::sync::Arc::new(crate::session::SessionManager::new(10_000))
    }

    fn grpc_ctx(uri: &str) -> RequestContext {
        let mut headers = crate::middleware::HeaderMap::new();
        headers.insert("content-type", "application/grpc+proto".to_string());
        RequestContext {
            method: "POST".to_string(),
            uri: uri.to_string(),
            headers,
            host: "grpc.example.com".to_string(),
            session_id: Some("sess-1".to_string()),
            ..Default::default()
        }
    }

    fn empty_res() -> crate::middleware::ResponseContext {
        crate::middleware::ResponseContext {
            status: 200,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn observer_decodes_request_and_response_messages_in_order() {
        let sm = make_sm();
        sm.record_request("sess-1".to_string(), grpc_ctx("/pkg.Svc/Method"));
        sm.flush().await;
        let mw = GrpcInspectorMiddleware {
            session_manager: sm.clone(),
        };
        let req = grpc_ctx("/pkg.Svc/Method");
        let mut obs = mw.stream_observer(&req).unwrap();

        let req_frame = grpc_frame(false, &[0x0a, 0x03, b'r', b'e', b'q']); // "req"
        obs.on_request_chunk(&Bytes::from(req_frame));

        obs.on_response_head(&empty_res(), std::time::Instant::now());

        let res_frame = grpc_frame(false, &[0x0a, 0x03, b'r', b'e', b's']); // "res"
        obs.on_chunk(&Bytes::from(res_frame));

        obs.finish().await;
        sm.flush().await;

        // update_inspector_data is queued; peek via get_session
        let ex = sm.get_session("sess-1").unwrap();
        let grpc = ex.inspector_data.as_ref().unwrap().grpc.as_ref().unwrap();
        assert_eq!(grpc.service.as_deref(), Some("pkg.Svc"));
        assert_eq!(grpc.method.as_deref(), Some("Method"));
        assert_eq!(grpc.messages.len(), 2);
        assert_eq!(grpc.messages[0].direction, "request");
        assert_eq!(grpc.messages[1].direction, "response");
    }

    #[tokio::test]
    async fn observer_reassembles_frame_split_across_chunks() {
        let sm = make_sm();
        sm.record_request("sess-1".to_string(), grpc_ctx("/pkg.Svc/Bidi"));
        sm.flush().await;
        let mw = GrpcInspectorMiddleware {
            session_manager: sm.clone(),
        };
        let req = grpc_ctx("/pkg.Svc/Bidi");
        let mut obs = mw.stream_observer(&req).unwrap();
        obs.on_response_head(&empty_res(), std::time::Instant::now());

        let frame = grpc_frame(false, b"split-message");
        let (a, b) = frame.split_at(4);
        obs.on_chunk(&Bytes::copy_from_slice(a));
        obs.on_chunk(&Bytes::copy_from_slice(b));
        obs.finish().await;
        sm.flush().await;

        let ex = sm.get_session("sess-1").unwrap();
        let msgs = &ex
            .inspector_data
            .as_ref()
            .unwrap()
            .grpc
            .as_ref()
            .unwrap()
            .messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].direction, "response");
        assert_eq!(msgs[0].length as usize, b"split-message".len());
    }

    #[tokio::test]
    async fn observer_captures_grpc_status_from_response_headers() {
        let sm = make_sm();
        sm.record_request("sess-1".to_string(), grpc_ctx("/pkg.Svc/Unary"));
        sm.flush().await;
        let mw = GrpcInspectorMiddleware {
            session_manager: sm.clone(),
        };
        let req = grpc_ctx("/pkg.Svc/Unary");
        let mut obs = mw.stream_observer(&req).unwrap();

        let mut res = empty_res();
        res.headers
            .insert("grpc-status".to_string(), "0".to_string());
        res.headers
            .insert("grpc-message".to_string(), "OK".to_string());
        obs.on_response_head(&res, std::time::Instant::now());
        obs.finish().await;
        sm.flush().await;

        let ex = sm.get_session("sess-1").unwrap();
        let grpc = ex.inspector_data.as_ref().unwrap().grpc.as_ref().unwrap();
        assert_eq!(grpc.grpc_status.as_deref(), Some("0"));
        assert_eq!(grpc.grpc_status_message.as_deref(), Some("OK"));
    }

    #[tokio::test]
    async fn body_hint_is_streaming_for_grpc_non_grpc_stays_full() {
        use crate::core::forward::{BodyHint, Granularity};
        let sm = make_sm();
        let mw = GrpcInspectorMiddleware {
            session_manager: sm.clone(),
        };
        let grpc = grpc_ctx("/pkg.Svc/M");
        assert_eq!(
            mw.body_hint(&grpc),
            BodyHint::StreamingInspect {
                granularity: Granularity::Messages
            }
        );
        let non_grpc = make_ctx("application/json", "/api");
        assert_eq!(mw.body_hint(&non_grpc), BodyHint::FullBody);
    }
}
