use crate::middleware::{Middleware, MiddlewareAction, RequestContext};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A DNS override entry. Supports backward-compat deserialization from the old plain-string format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DnsValue {
    /// Legacy storage format: just an IP string.
    Legacy(String),
    /// Current format with an enabled toggle.
    Entry(DnsEntry),
}

impl DnsValue {
    pub fn into_entry(self) -> DnsEntry {
        match self {
            DnsValue::Legacy(ip) => DnsEntry { ip, enabled: true },
            DnsValue::Entry(e) => e,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DnsEntry {
    pub ip: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

pub type DnsOverrides = HashMap<String, DnsEntry>;

pub struct DnsOverrideMiddleware {
    pub overrides: Arc<RwLock<DnsOverrides>>,
}

#[async_trait]
impl Middleware for DnsOverrideMiddleware {
    fn name(&self) -> &str {
        "dns_override"
    }

    fn body_hint(
        &self,
        _head: &crate::middleware::RequestContext,
    ) -> crate::core::forward::BodyHint {
        crate::core::forward::BodyHint::StreamingInspect {
            granularity: crate::core::forward::Granularity::Bytes,
        }
    }

    async fn on_request(&self, ctx: &mut RequestContext) -> MiddlewareAction {
        let ovr = self.overrides.read().await;
        if ovr.is_empty() {
            return MiddlewareAction::Continue;
        }
        // Strip port from host to get the bare hostname for lookup.
        let (hostname, port) = if let Some(colon) = ctx.host.rfind(':') {
            (&ctx.host[..colon], &ctx.host[colon + 1..])
        } else {
            (ctx.host.as_str(), "")
        };
        if let Some(entry) = ovr.get(hostname).filter(|e| e.enabled) {
            let new_host = if port.is_empty() {
                entry.ip.clone()
            } else {
                format!("{}:{}", entry.ip, port)
            };
            let scheme_port = if port == "443" { "https" } else { "http" };
            let dest = format!("{}://{}", scheme_port, new_host);
            ctx.host = new_host;
            ctx.destination = Some(dest);
        }
        MiddlewareAction::Continue
    }
}
