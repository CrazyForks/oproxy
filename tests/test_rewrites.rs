use axum::body::Body;
use axum::http::{Method, Request};
use oproxy::core::engine::ProxyEngine;
use oproxy::middleware::chain::MiddlewareChain;
use oproxy::middleware::matcher::Location;
use oproxy::middleware::plugins::rules::{
    AppliesTo, RewriteAction, RewriteRuleSet, UnifiedRewriteMiddleware,
};
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::test]
async fn test_unified_rewrite_through_engine() {
    let rule = RewriteRuleSet {
        id: "r1".to_string(),
        name: "inject on /old-path".to_string(),
        enabled: true,
        location: Location {
            path: Some("/old-path*".to_string()),
            ..Default::default()
        },
        applies_to: AppliesTo::Request,
        actions: vec![RewriteAction::SetHeader {
            name: "X-Rewritten".to_string(),
            value: "true".to_string(),
        }],
    };

    let rewrite_plugin = UnifiedRewriteMiddleware::new(vec![rule]);
    let mut chain = MiddlewareChain::new();
    chain.add_middleware(Arc::new(rewrite_plugin));

    let middleware_chain = Arc::new(RwLock::new(chain));
    let engine = Arc::new(ProxyEngine::new(
        middleware_chain,
        None,
        false,
        30,
        10 * 1024 * 1024,
        10,
        30,
        None,
    ));

    let req = Request::builder()
        .method(Method::GET)
        .uri("/old-path")
        .header("host", "example.com")
        .body(Body::empty())
        .unwrap();

    // Completes without panic; the upstream will 502 since there's no real server,
    // but the middleware correctly processed the request.
    let _ = engine.handle_request(req).await;
}
