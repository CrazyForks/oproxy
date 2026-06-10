use std::sync::Arc;
use tracing::{debug, instrument};

use crate::middleware::{Middleware, MiddlewareAction, RequestContext, ResponseContext};

#[derive(Clone)]
pub struct MiddlewareChain {
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl MiddlewareChain {
    pub fn new() -> Self {
        Self {
            middlewares: Vec::new(),
        }
    }

    pub fn add_middleware(&mut self, middleware: Arc<dyn Middleware>) {
        self.middlewares.push(middleware);
    }

    /// Returns the names of all registered middlewares in execution order.
    pub fn list_plugins(&self) -> Vec<String> {
        self.middlewares
            .iter()
            .map(|m| m.name().to_string())
            .collect()
    }

    /// Decides the forwarding class for an exchange from the body hints declared
    /// by the active plugins (see [`Middleware::body_hint`]). Computed from the
    /// request head only, before any body byte is forwarded, so the engine never
    /// buffers a stream by accident. Any plugin needing the full body forces the
    /// buffered class; see [`crate::core::forward::select_class`] for the rules.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn forward_class(&self, head: &RequestContext) -> crate::core::forward::ForwardClass {
        crate::core::forward::select_class(self.middlewares.iter().map(|m| m.body_hint(head)))
    }

    /// Full protocol-aware execution plan. This supersedes `forward_class` for
    /// new code while keeping the old method available for tests/callers that
    /// only care about buffered vs streaming.
    pub fn capability_plan(
        &self,
        head: &RequestContext,
        protocol: &crate::core::forward::ProtocolContext,
    ) -> crate::core::forward::CapabilityPlan {
        crate::core::forward::plan_execution(
            protocol,
            self.middlewares.iter().map(|m| m.body_hint(head)),
        )
    }

    #[instrument(skip(self, ctx))]
    pub async fn execute_request(&self, ctx: &mut RequestContext) -> MiddlewareAction {
        for middleware in &self.middlewares {
            debug!("Executing middleware request step");
            let action = middleware.on_request(ctx).await;
            if action != MiddlewareAction::Continue {
                debug!("Middleware request step stopped chain");
                return action;
            }
        }
        MiddlewareAction::Continue
    }

    #[instrument(skip(self, ctx))]
    pub async fn execute_response(&self, ctx: &mut ResponseContext) -> MiddlewareAction {
        // Response middleware are typically executed in reverse order
        for middleware in self.middlewares.iter().rev() {
            debug!("Executing middleware response step");
            let action = middleware.on_response(ctx).await;
            if action != MiddlewareAction::Continue {
                debug!("Middleware response step stopped chain");
                return action;
            }
        }
        MiddlewareAction::Continue
    }

    /// Collect per-stream observers from all plugins for the streaming forward
    /// path. Called after the request middleware chain has run so `req` has the
    /// final session ID and side-channel state. The response head is delivered
    /// to each observer separately via [`BodyObserver::on_response_head`].
    pub fn stream_observers(
        &self,
        req: &crate::middleware::RequestContext,
    ) -> Vec<Box<dyn crate::middleware::BodyObserver>> {
        self.middlewares
            .iter()
            .filter_map(|m| m.stream_observer(req))
            .collect()
    }
}

impl Default for MiddlewareChain {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{Middleware, MiddlewareAction, RequestContext, ResponseContext};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Records which order middlewares fired by pushing a label into a shared vec.
    struct OrderMiddleware {
        label: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
        req_action: MiddlewareAction,
        res_action: MiddlewareAction,
    }

    #[async_trait]
    impl Middleware for OrderMiddleware {
        fn name(&self) -> &str {
            self.label
        }
        async fn on_request(&self, _ctx: &mut RequestContext) -> MiddlewareAction {
            self.order.lock().unwrap().push(self.label);
            self.req_action
        }
        async fn on_response(&self, _ctx: &mut ResponseContext) -> MiddlewareAction {
            self.order.lock().unwrap().push(self.label);
            self.res_action
        }
    }

    fn req() -> RequestContext {
        RequestContext {
            method: "GET".to_string(),
            uri: "/".to_string(),
            headers: crate::middleware::HeaderMap::new(),
            body: bytes::Bytes::new(),
            host: "localhost".to_string(),
            ..Default::default()
        }
    }

    fn res() -> ResponseContext {
        ResponseContext {
            status: 200,
            headers: crate::middleware::HeaderMap::new(),
            body: bytes::Bytes::new(),
            request_uri: "/".to_string(),
            session_id: None,
            ttfb_ms: 0,
            body_ms: 0,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn empty_chain_returns_continue_for_request() {
        let chain = MiddlewareChain::new();
        assert_eq!(
            chain.execute_request(&mut req()).await,
            MiddlewareAction::Continue
        );
    }

    #[tokio::test]
    async fn empty_chain_returns_continue_for_response() {
        let chain = MiddlewareChain::new();
        assert_eq!(
            chain.execute_response(&mut res()).await,
            MiddlewareAction::Continue
        );
    }

    #[tokio::test]
    async fn request_middlewares_run_in_insertion_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "A",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::Continue,
        }));
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "B",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::Continue,
        }));
        chain.execute_request(&mut req()).await;
        assert_eq!(*order.lock().unwrap(), vec!["A", "B"]);
    }

    #[tokio::test]
    async fn response_middlewares_run_in_reverse_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "A",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::Continue,
        }));
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "B",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::Continue,
        }));
        chain.execute_response(&mut res()).await;
        assert_eq!(*order.lock().unwrap(), vec!["B", "A"]);
    }

    #[tokio::test]
    async fn stop_and_return_short_circuits_request_chain() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "A",
            order: order.clone(),
            req_action: MiddlewareAction::StopAndReturn,
            res_action: MiddlewareAction::Continue,
        }));
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "B",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::Continue,
        }));
        let action = chain.execute_request(&mut req()).await;
        assert_eq!(action, MiddlewareAction::StopAndReturn);
        assert_eq!(
            *order.lock().unwrap(),
            vec!["A"],
            "B must not run after StopAndReturn"
        );
    }

    #[tokio::test]
    async fn stop_and_return_short_circuits_response_chain() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        // B is added second → runs FIRST on response (reverse order)
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "A",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::Continue,
        }));
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "B",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::StopAndReturn,
        }));
        let action = chain.execute_response(&mut res()).await;
        assert_eq!(action, MiddlewareAction::StopAndReturn);
        assert_eq!(
            *order.lock().unwrap(),
            vec!["B"],
            "A must not run after B returns StopAndReturn"
        );
    }

    /// A plugin that declares a streaming-inspect body hint.
    struct StreamingInspector;

    #[async_trait]
    impl Middleware for StreamingInspector {
        fn name(&self) -> &str {
            "streaming-inspector"
        }
        fn body_hint(&self, _head: &RequestContext) -> crate::core::forward::BodyHint {
            crate::core::forward::BodyHint::StreamingInspect {
                granularity: crate::core::forward::Granularity::Bytes,
            }
        }
    }

    /// A plugin using the default body hint (FullBody).
    struct FullBodyPlugin;

    #[async_trait]
    impl Middleware for FullBodyPlugin {
        fn name(&self) -> &str {
            "full-body"
        }
    }

    #[test]
    fn forward_class_defaults_to_buffered_for_empty_chain() {
        let chain = MiddlewareChain::new();
        assert_eq!(
            chain.forward_class(&req()),
            crate::core::forward::ForwardClass::Buffered
        );
    }

    #[test]
    fn forward_class_is_streaming_when_all_plugins_stream() {
        let mut chain = MiddlewareChain::new();
        chain.add_middleware(Arc::new(StreamingInspector));
        assert_eq!(
            chain.forward_class(&req()),
            crate::core::forward::ForwardClass::Streaming
        );
    }

    #[test]
    fn forward_class_is_buffered_when_any_plugin_needs_full_body() {
        let mut chain = MiddlewareChain::new();
        chain.add_middleware(Arc::new(StreamingInspector));
        chain.add_middleware(Arc::new(FullBodyPlugin));
        assert_eq!(
            chain.forward_class(&req()),
            crate::core::forward::ForwardClass::Buffered,
            "a single full-body plugin must force the buffered class"
        );
    }

    #[tokio::test]
    async fn pause_action_short_circuits_and_is_propagated() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut chain = MiddlewareChain::new();
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "A",
            order: order.clone(),
            req_action: MiddlewareAction::Pause,
            res_action: MiddlewareAction::Continue,
        }));
        chain.add_middleware(Arc::new(OrderMiddleware {
            label: "B",
            order: order.clone(),
            req_action: MiddlewareAction::Continue,
            res_action: MiddlewareAction::Continue,
        }));
        let action = chain.execute_request(&mut req()).await;
        assert_eq!(action, MiddlewareAction::Pause);
        assert_eq!(*order.lock().unwrap(), vec!["A"]);
    }

    /// A chain composed only of non-body plugins must yield `Streaming`.
    #[test]
    fn forward_class_is_streaming_for_non_body_real_plugins() {
        use crate::middleware::plugins::access_control::AccessControlMiddleware;
        use crate::middleware::plugins::capture_filter::CaptureFilterMiddleware;
        use crate::middleware::plugins::dns_override::DnsOverrideMiddleware;
        use crate::middleware::plugins::jwt_inspector::JwtInspectorMiddleware;
        use crate::middleware::plugins::map_remote::MapRemoteMiddleware;
        use crate::middleware::plugins::routing::ThrottlingMiddleware;
        use std::collections::HashMap;
        use tokio::sync::RwLock;

        let mut chain = MiddlewareChain::new();
        chain.add_middleware(Arc::new(AccessControlMiddleware::new(vec![])));
        chain.add_middleware(Arc::new(CaptureFilterMiddleware::new(Arc::new(
            RwLock::new(Default::default()),
        ))));
        chain.add_middleware(Arc::new(DnsOverrideMiddleware {
            overrides: Arc::new(RwLock::new(HashMap::new())),
        }));
        chain.add_middleware(Arc::new(JwtInspectorMiddleware));
        chain.add_middleware(Arc::new(MapRemoteMiddleware::new(vec![])));
        chain.add_middleware(Arc::new(ThrottlingMiddleware {
            config: Arc::new(RwLock::new(Default::default())),
        }));
        assert_eq!(
            chain.forward_class(&req()),
            crate::core::forward::ForwardClass::Streaming,
            "a chain of non-body plugins must yield Streaming"
        );
    }
}
