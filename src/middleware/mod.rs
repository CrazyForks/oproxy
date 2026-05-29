use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A response a middleware wants the engine to return immediately instead of
/// forwarding upstream (mock, map-local, Lua `abort()`, breakpoint timeout, …).
///
/// This is the typed replacement for the old `x-oproxy-mock-response` header
/// protocol: the body is carried as raw [`Bytes`] so binary payloads survive
/// without a base64 round-trip, and nothing leaks into the forwarded headers.
#[derive(Debug, Clone)]
pub struct InterceptedResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: Bytes,
    /// Session tags to attach when this response is recorded (e.g. "mock").
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RequestContext {
    pub method: String,
    pub uri: String,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub host: String,
    /// Raw bytes of the body as received from the client. Populated by the engine
    /// before the middleware chain runs. Middlewares that modify `body` (text) should
    /// clear this to `None` so the engine knows to forward the modified string rather
    /// than the original bytes. Not serialised — only live in memory.
    #[serde(skip)]
    pub body_bytes: Option<Bytes>,
    // ── Internal middleware ↔ engine side-channel ───────────────────────────────
    // The fields below replace the former `x-oproxy-*` pseudo-header protocol.
    // They are in-memory only (`#[serde(skip)]`) so they never serialise into
    // recordings/exports and can never leak to the upstream server.
    /// Upstream target override (Routing / DNS override / MITM). When set the
    /// engine forwards here instead of the request's original host.
    #[serde(skip)]
    pub destination: Option<String>,
    /// Session id assigned by InspectionMiddleware, used to correlate the
    /// response back to the exact request even under concurrent same-URI traffic.
    #[serde(skip)]
    pub session_id: Option<String>,
    /// Set by CaptureFilterMiddleware to suppress session recording for this host.
    #[serde(skip)]
    pub skip_recording: bool,
    /// Short-circuit response set by Mock / map-local / Lua / breakpoint timeout.
    /// When present the engine returns it instead of forwarding upstream.
    #[serde(skip)]
    pub mock_response: Option<InterceptedResponse>,
    /// Parsed inspector data (JWT / GraphQL / gRPC) populated by the inspector
    /// middlewares and consumed by InspectionMiddleware.
    #[serde(skip)]
    pub inspector: crate::session::InspectorData,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResponseContext {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub request_uri: String,
    // Injected by InspectionMiddleware during on_request; used in on_response for exact
    // session lookup so concurrent requests to the same URI don't overwrite each other.
    #[serde(default)]
    pub session_id: Option<String>,
    // Time from request send to response headers received (DNS+TCP+TLS+TTFB).
    #[serde(default)]
    pub ttfb_ms: u64,
    // Time to read response body after headers received.
    #[serde(default)]
    pub body_ms: u64,
    /// Canonical bytes of the response body (decoded from gzip/deflate/br if needed).
    /// Engine uses these when no middleware modified `body`. Not serialised.
    #[serde(skip)]
    pub body_bytes: Option<Bytes>,
    /// Session tags to attach when this exchange is recorded. Typed replacement
    /// for the former `x-oproxy-tags` response header. Not serialised.
    #[serde(skip)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareAction {
    Continue,      // Proceed to next middleware
    StopAndReturn, // Stop chain and return current response (e.g., Map Local)
    #[allow(dead_code)]
    Pause, // Halt execution (e.g., Breakpoint)
}

#[async_trait]
pub trait Middleware: Send + Sync {
    fn name(&self) -> &str;

    /// Process the request before it is sent to the target server.
    async fn on_request(&self, ctx: &mut RequestContext) -> MiddlewareAction;

    /// Process the response before it is sent back to the client.
    async fn on_response(&self, ctx: &mut ResponseContext) -> MiddlewareAction;
}

pub mod chain;
pub mod plugins;
