use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::core::engine::ProxyEngine;
use crate::middleware::plugins::dns_override::DnsOverrides;
use crate::transport::lifecycle::ConnectionSupervisor;

#[derive(Clone)]
pub struct TransportContext {
    pub session_manager: crate::session::SharedSessionManager,
    pub breakpoint_manager: Arc<crate::middleware::plugins::breakpoints::BreakpointManager>,
    pub mock_rules: crate::middleware::plugins::mock::SharedMockRules,
    pub engine: Arc<ProxyEngine>,
    pub dns_overrides: Arc<RwLock<DnsOverrides>>,
    pub connections: ConnectionSupervisor,
    pub inspect_ws_frames: bool,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
}
