//! First-run example rules.
//!
//! A fresh install starts with every rules surface empty, which makes it hard
//! to discover what each feature can do. This module seeds one or more
//! **disabled** example entries for every traffic-modification feature the
//! first time oproxy runs.
//!
//! Seeding is per-file and non-destructive: an example set is written only when
//! its storage file does not exist yet. The moment a user (or the API) saves a
//! file — even an empty list — that file is never touched again, so deleting
//! the examples is permanent and upgrades never resurrect them.
//!
//! All examples share one consistent vocabulary so they read as a family:
//! - host `api.test.com` (traffic to play with), `blocked.test.com` (traffic to deny)
//! - path prefix `/api/v1`
//! - header `X-Example-Header` with value `example-value`
//! - remap destination `https://github.com`

use std::path::Path;

use crate::middleware::matcher::Location;
use crate::middleware::plugins::access_control::{AccessAction, AccessRule};
use crate::middleware::plugins::breakpoints::{BreakpointRule, BreakpointTier, BreakpointType};
use crate::middleware::plugins::capture_filter::{CaptureFilterConfig, FilterMode};
use crate::middleware::plugins::dns_override::{DnsEntry, DnsOverrides};
use crate::middleware::plugins::lua_engine::LuaScript;
use crate::middleware::plugins::map_local::MapLocalRule;
use crate::middleware::plugins::map_remote::MapRemoteRule;
use crate::middleware::plugins::mock::{
    MockBehavior, MockResponse, MockRule, TunnelDecision, WsFrameAction,
};
use crate::middleware::plugins::routing::ThrottlingConfig;
use crate::middleware::plugins::rules::{AppliesTo, RewriteAction, RewriteRuleSet};
use crate::storage;

/// Shared example vocabulary — keep every seeded rule on the same names/values.
const EXAMPLE_HOST: &str = "api.test.com";
const BLOCKED_HOST: &str = "blocked.test.com";
const EXAMPLE_PATH_GLOB: &str = "/api/v1/*";
const EXAMPLE_HEADER: &str = "X-Example-Header";
const EXAMPLE_VALUE: &str = "example-value";
const REWRITTEN_VALUE: &str = "rewritten-value";
const EXAMPLE_DESTINATION: &str = "https://github.com";
const MAP_LOCAL_FIXTURE: &str = "example-welcome.json";

fn host(h: &str) -> Location {
    Location {
        host: Some(h.to_string()),
        ..Default::default()
    }
}

fn host_path(h: &str, p: &str) -> Location {
    Location {
        host: Some(h.to_string()),
        path: Some(p.to_string()),
        ..Default::default()
    }
}

/// Seed disabled example entries for every feature whose storage file is
/// missing. Called once at startup, before the storage files are loaded.
/// Failures are logged and skipped — examples are a convenience, never a
/// startup requirement.
pub async fn seed_first_run_examples(storage_path: &Path) {
    seed(storage_path, "rule_sets.json", || async move {
        storage::save_rule_sets(storage_path, &example_rule_sets()).await
    })
    .await;
    seed(storage_path, "map_remote_rules.json", || async move {
        storage::save_map_remote_rules(storage_path, &example_map_remote_rules()).await
    })
    .await;
    seed_map_local(storage_path).await;
    seed(storage_path, "mock_rules.json", || async move {
        storage::save_mock_rules(storage_path, &example_mock_rules()).await
    })
    .await;
    seed(storage_path, "access_rules.json", || async move {
        storage::save_access_rules(storage_path, &example_access_rules()).await
    })
    .await;
    seed(storage_path, "dns_overrides.json", || async move {
        storage::save_dns_overrides(storage_path, &example_dns_overrides()).await
    })
    .await;
    seed(storage_path, "breakpoints.json", || async move {
        storage::save_breakpoints(storage_path, &example_breakpoints()).await
    })
    .await;
    seed(storage_path, "throttle.json", || async move {
        storage::save_throttle(storage_path, &example_throttle()).await
    })
    .await;
    seed(storage_path, "capture_filter.json", || async move {
        storage::save_capture_filter(storage_path, &example_capture_filter()).await
    })
    .await;
    seed(storage_path, "lua_scripts.json", || async move {
        storage::save_lua_scripts(storage_path, &example_lua_scripts()).await
    })
    .await;
}

async fn seed<F, Fut>(storage_path: &Path, file_name: &str, write: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<()>>,
{
    if storage_path.join(file_name).exists() {
        return;
    }
    match write().await {
        Ok(()) => tracing::info!(file = file_name, "Seeded first-run examples (disabled)"),
        Err(e) => tracing::warn!(file = file_name, error = %e, "Could not seed examples"),
    }
}

/// Map Local needs both the rule and the fixture file it points at. The fixture
/// lives in the managed `storage/map-local/` directory so the rule's relative
/// `file_path` resolves there.
async fn seed_map_local(storage_path: &Path) {
    let fixture = storage_path.join("map-local").join(MAP_LOCAL_FIXTURE);
    let body = format!(
        "{{\n  \"message\": \"Hello from oproxy Map Local\",\n  \"header\": \
         \"{EXAMPLE_HEADER}\",\n  \"value\": \"{EXAMPLE_VALUE}\"\n}}\n",
    );
    if !fixture.exists()
        && let Err(e) = tokio::fs::write(&fixture, body).await
    {
        tracing::warn!(error = %e, "Could not write Map Local example fixture");
    }
    seed(storage_path, "map_local_rules.json", || async move {
        storage::save_map_local_rules(storage_path, &example_map_local_rules()).await
    })
    .await;
}

// ── Per-feature examples (all disabled) ──────────────────────────────────────

fn example_rule_sets() -> Vec<RewriteRuleSet> {
    vec![
        RewriteRuleSet {
            id: "example-rewrite-add-header".into(),
            name: format!(
                "Example: Add {EXAMPLE_HEADER} to {EXAMPLE_HOST}{EXAMPLE_PATH_GLOB} requests"
            ),
            enabled: false,
            location: host_path(EXAMPLE_HOST, EXAMPLE_PATH_GLOB),
            applies_to: AppliesTo::Request,
            actions: vec![RewriteAction::SetHeader {
                name: EXAMPLE_HEADER.into(),
                value: EXAMPLE_VALUE.into(),
            }],
        },
        RewriteRuleSet {
            id: "example-rewrite-response-body".into(),
            name: format!("Example: Replace '{EXAMPLE_VALUE}' in {EXAMPLE_HOST} response bodies"),
            enabled: false,
            location: host(EXAMPLE_HOST),
            applies_to: AppliesTo::Response,
            actions: vec![RewriteAction::ReplaceBody {
                pattern: EXAMPLE_VALUE.into(),
                replacement: REWRITTEN_VALUE.into(),
            }],
        },
        RewriteRuleSet {
            id: "example-rewrite-redirect".into(),
            name: format!("Example: Redirect {EXAMPLE_HOST} to github.com"),
            enabled: false,
            location: host(EXAMPLE_HOST),
            applies_to: AppliesTo::Request,
            actions: vec![RewriteAction::Redirect {
                status: 302,
                location: EXAMPLE_DESTINATION.into(),
            }],
        },
    ]
}

fn example_map_remote_rules() -> Vec<MapRemoteRule> {
    vec![MapRemoteRule {
        id: "example-map-remote-github".into(),
        name: format!("Example: Map {EXAMPLE_HOST} to github.com"),
        enabled: false,
        location: host(EXAMPLE_HOST),
        destination: EXAMPLE_DESTINATION.into(),
    }]
}

fn example_map_local_rules() -> Vec<MapLocalRule> {
    vec![MapLocalRule {
        id: "example-map-local-welcome".into(),
        name: format!("Example: Serve {MAP_LOCAL_FIXTURE} for {EXAMPLE_HOST}/api/v1/welcome"),
        enabled: false,
        location: host_path(EXAMPLE_HOST, "/api/v1/welcome"),
        file_path: MAP_LOCAL_FIXTURE.into(),
        inline_body: None,
    }]
}

fn example_mock_rules() -> Vec<MockRule> {
    let mut grpc_trailers = crate::middleware::HeaderMap::new();
    grpc_trailers.insert("grpc-status", "0");
    grpc_trailers.insert("grpc-message", EXAMPLE_VALUE);

    vec![
        MockRule {
            id: "example-mock-http-status".into(),
            name: format!("Example: Mock JSON for {EXAMPLE_HOST}/api/v1/status"),
            enabled: false,
            location: host_path(EXAMPLE_HOST, "/api/v1/status"),
            behavior: Some(MockBehavior::HttpResponse {
                responses: vec![MockResponse {
                    status: 200,
                    headers: [
                        ("content-type".to_string(), "application/json".to_string()),
                        (EXAMPLE_HEADER.to_string(), EXAMPLE_VALUE.to_string()),
                    ]
                    .into_iter()
                    .collect(),
                    body: format!(
                        "{{\"status\":\"ok\",\"message\":\"Example mock response from oproxy\",\"header\":\"{EXAMPLE_HEADER}\",\"value\":\"{EXAMPLE_VALUE}\"}}"
                    ),
                    delay_ms: 0,
                }],
            }),
            responses: vec![],
            call_count: 0,
        },
        MockRule {
            id: "example-mock-websocket".into(),
            name: format!("Example: Scripted WebSocket reply on {EXAMPLE_HOST}/ws"),
            enabled: false,
            location: Location {
                wire_protocol: Some("websocket".into()),
                ..host_path(EXAMPLE_HOST, "/ws")
            },
            behavior: Some(MockBehavior::WebSocketScript {
                frames: vec![WsFrameAction {
                    opcode: 1, // text frame
                    payload: EXAMPLE_VALUE.into(),
                    delay_ms: 250,
                }],
            }),
            responses: vec![],
            call_count: 0,
        },
        MockRule {
            id: "example-mock-grpc".into(),
            name: format!("Example: gRPC trailers-only OK for {EXAMPLE_HOST}"),
            enabled: false,
            location: Location {
                application_protocol: Some("grpc".into()),
                ..host(EXAMPLE_HOST)
            },
            behavior: Some(MockBehavior::GrpcScript {
                messages: vec![],
                trailers: grpc_trailers,
            }),
            responses: vec![],
            call_count: 0,
        },
        MockRule {
            id: "example-mock-tunnel-refuse".into(),
            name: format!("Example: Refuse CONNECT tunnels to {BLOCKED_HOST}"),
            enabled: false,
            location: host(BLOCKED_HOST),
            behavior: Some(MockBehavior::TunnelDecision {
                decision: TunnelDecision {
                    allow: false,
                    delay_ms: 0,
                },
            }),
            responses: vec![],
            call_count: 0,
        },
    ]
}

fn example_access_rules() -> Vec<AccessRule> {
    vec![AccessRule {
        id: "example-access-block".into(),
        name: format!("Example: Block all requests to {BLOCKED_HOST}"),
        enabled: false,
        location: host(BLOCKED_HOST),
        action: AccessAction::Block,
    }]
}

fn example_dns_overrides() -> DnsOverrides {
    [(
        EXAMPLE_HOST.to_string(),
        DnsEntry {
            ip: "127.0.0.1".into(),
            enabled: false,
        },
    )]
    .into_iter()
    .collect()
}

fn example_breakpoints() -> Vec<BreakpointRule> {
    vec![BreakpointRule {
        id: "example-breakpoint-request".into(),
        location: host_path(EXAMPLE_HOST, EXAMPLE_PATH_GLOB),
        bp_type: BreakpointType::Request,
        tier: BreakpointTier::Body,
        enabled: false,
    }]
}

/// Slow-mobile-style preset: 2 s extra latency, 256 kbps. Disabled.
fn example_throttle() -> ThrottlingConfig {
    ThrottlingConfig {
        latency_ms: 2000,
        bandwidth_limit_kbps: 256,
        enabled: false,
    }
}

/// Pre-filled host list showing how a denylist would look; mode stays
/// `Disabled` so all traffic is still recorded.
fn example_capture_filter() -> CaptureFilterConfig {
    CaptureFilterConfig {
        mode: FilterMode::Disabled,
        hosts: vec![BLOCKED_HOST.to_string()],
    }
}

fn example_lua_scripts() -> Vec<LuaScript> {
    vec![LuaScript {
        id: "example-lua-tag-requests".into(),
        name: format!("Example: Tag {EXAMPLE_HOST} requests with {EXAMPLE_HEADER}"),
        code: format!(
            r#"-- Example script: adds {EXAMPLE_HEADER} to every request to {EXAMPLE_HOST}.
-- Enable it, send a request through the proxy, and look for the header
-- in the Sessions view. `request` and `response` are table globals;
-- call abort(status, body) to short-circuit a request.
if request and string.find(request.uri, "{EXAMPLE_HOST}", 1, true) then
  request.headers["{EXAMPLE_HEADER}"] = "{EXAMPLE_VALUE}"
end"#
        ),
        enabled: false,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oproxy-examples-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("map-local")).unwrap();
        dir
    }

    #[tokio::test]
    async fn seeds_every_feature_disabled_on_first_run() {
        let dir = temp_storage();
        seed_first_run_examples(&dir).await;

        let rule_sets = storage::load_rule_sets(&dir);
        assert_eq!(rule_sets.len(), 3);
        assert!(rule_sets.iter().all(|r| !r.enabled));

        let map_remote = storage::load_map_remote_rules(&dir);
        assert_eq!(map_remote.len(), 1);
        assert!(!map_remote[0].enabled);
        assert_eq!(map_remote[0].destination, EXAMPLE_DESTINATION);

        let map_local = storage::load_map_local_rules(&dir);
        assert_eq!(map_local.len(), 1);
        assert!(!map_local[0].enabled);
        assert!(dir.join("map-local").join(MAP_LOCAL_FIXTURE).is_file());

        let mocks = storage::load_mock_rules(&dir);
        assert_eq!(mocks.len(), 4);
        assert!(mocks.iter().all(|r| !r.enabled));

        let access = storage::load_access_rules(&dir);
        assert_eq!(access.len(), 1);
        assert!(!access[0].enabled);

        let dns = storage::load_dns_overrides(&dir);
        assert!(!dns.get(EXAMPLE_HOST).unwrap().enabled);

        let bps = storage::load_breakpoints(&dir);
        assert_eq!(bps.len(), 1);
        assert!(!bps[0].enabled);

        let throttle = storage::load_throttle(&dir);
        assert!(!throttle.enabled);
        assert_eq!(throttle.latency_ms, 2000);

        let cf = storage::load_capture_filter(&dir);
        assert_eq!(cf.mode, FilterMode::Disabled);

        let lua = storage::load_lua_scripts(&dir);
        assert_eq!(lua.len(), 1);
        assert!(!lua[0].enabled);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn never_overwrites_existing_state() {
        let dir = temp_storage();
        // User saved an empty rule list (deleted the examples) and one custom mock.
        storage::save_rule_sets(&dir, &[]).await.unwrap();
        let custom = MockRule {
            id: "user-rule".into(),
            name: "My mock".into(),
            enabled: true,
            location: Location::default(),
            behavior: None,
            responses: vec![],
            call_count: 0,
        };
        storage::save_mock_rules(&dir, std::slice::from_ref(&custom))
            .await
            .unwrap();

        seed_first_run_examples(&dir).await;

        assert!(storage::load_rule_sets(&dir).is_empty());
        let mocks = storage::load_mock_rules(&dir);
        assert_eq!(mocks.len(), 1);
        assert_eq!(mocks[0].id, "user-rule");
        // Untouched features were still seeded.
        assert_eq!(storage::load_access_rules(&dir).len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn example_lua_script_compiles() {
        let lua = mlua::Lua::new();
        for script in example_lua_scripts() {
            lua.load(&script.code).into_function().unwrap();
        }
    }
}
