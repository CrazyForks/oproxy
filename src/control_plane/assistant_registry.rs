use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::workspace::{WorkspaceActionDefinition, workspace_action_definitions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AssistantToolInfo {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct AssistantFeatureDoc {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) category: String,
    pub(super) purpose: String,
    #[serde(default)]
    pub(super) ui_steps: Vec<String>,
    pub(super) use_when: Vec<String>,
    pub(super) read_tools: Vec<String>,
    pub(super) action_endpoints: Vec<String>,
    pub(super) examples: Vec<String>,
    pub(super) notes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AssistantToolDefinition {
    name: String,
    description: String,
    category: String,
    openai_spec: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct AssistantCapabilityManifest {
    #[serde(default)]
    response_guidance: Vec<String>,
    features: Vec<AssistantFeatureDoc>,
    tools: Vec<AssistantToolDefinition>,
}

pub(super) fn openai_tool_specs() -> Vec<Value> {
    let mut specs: Vec<Value> = assistant_manifest()
        .tools
        .iter()
        .map(|tool| tool.openai_spec.clone())
        .collect();
    specs.extend(
        workspace_action_definitions()
            .into_iter()
            .filter_map(|action| {
                if action.openai_spec.is_null() {
                    None
                } else {
                    Some(action.openai_spec)
                }
            }),
    );
    specs
}

#[cfg(test)]
pub(super) fn openai_tool_names() -> Vec<String> {
    openai_tool_specs()
        .into_iter()
        .filter_map(|spec| {
            spec.pointer("/function/name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

pub(super) fn read_feature_catalog(args: Value) -> Result<Value, String> {
    let feature_id = args.get("feature_id").and_then(Value::as_str);
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(|q| q.trim().to_ascii_lowercase())
        .filter(|q| !q.is_empty());
    let mut features = assistant_feature_catalog();

    if let Some(feature_id) = feature_id {
        features.retain(|feature| feature.id == feature_id);
    }
    if let Some(query) = query {
        features.retain(|feature| feature_matches_query(feature, &query));
    }

    Ok(json!({
        "features": features,
        "rules": {
            "read_only": "Read tools may run automatically.",
            "confirmed_changes": "Every mutation, replay, deletion, import/export, and outbound network request must be proposed as a confirmation card.",
            "payload_source_of_truth": "Confirmed action payloads should match the UI/admin API shapes and are normalized/validated server-side before execution."
        }
    }))
}

pub(super) fn compact_feature_catalog() -> String {
    assistant_feature_catalog()
        .iter()
        .map(|feature| {
            format!(
                "{}: {} UI: {} Actions: {}",
                feature.id,
                feature.purpose,
                if feature.ui_steps.is_empty() {
                    "see matching UI surface".to_string()
                } else {
                    feature.ui_steps.join(" > ")
                },
                if feature.action_endpoints.is_empty() {
                    "read-only".to_string()
                } else {
                    feature.action_endpoints.join(", ")
                }
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

pub(super) fn assistant_response_guidance() -> String {
    let guidance = &assistant_manifest().response_guidance;
    if guidance.is_empty() {
        "For how-to questions, explain the UI path first, then mention assistant automation if available.".to_string()
    } else {
        guidance.join(" ")
    }
}

pub(super) fn assistant_feature_catalog() -> Vec<AssistantFeatureDoc> {
    assistant_manifest().features.clone()
}

pub(super) fn workspace_action_name_for_tool(tool_name: &str) -> Option<String> {
    workspace_action_definitions()
        .into_iter()
        .find(|action| workspace_tool_name(action).as_deref() == Some(tool_name))
        .map(|action| action.name)
}

pub(super) fn workspace_tool_name_for_action(action_type: &str) -> Option<String> {
    workspace_action_definitions()
        .into_iter()
        .find(|action| action.name == action_type)
        .and_then(|action| workspace_tool_name(&action))
}

pub(super) fn tool_info() -> Vec<AssistantToolInfo> {
    let mut tools: Vec<AssistantToolInfo> = assistant_manifest()
        .tools
        .iter()
        .map(|tool| AssistantToolInfo {
            name: tool.name.clone(),
            description: tool.description.clone(),
            category: tool.category.clone(),
        })
        .collect();
    tools.extend(
        workspace_action_definitions()
            .into_iter()
            .map(|action| AssistantToolInfo {
                name: workspace_tool_name(&action).unwrap_or(action.name),
                description: action.description,
                category: action.category,
            }),
    );
    tools
}

fn feature_matches_query(feature: &AssistantFeatureDoc, query: &str) -> bool {
    let haystack = format!(
        "{} {} {} {} {} {} {}",
        feature.id,
        feature.title,
        feature.category,
        feature.purpose,
        feature.use_when.join(" "),
        feature.action_endpoints.join(" "),
        feature.examples.join(" "),
    )
    .to_ascii_lowercase();
    haystack.contains(query)
}

fn workspace_tool_name(action: &WorkspaceActionDefinition) -> Option<String> {
    action
        .openai_spec
        .pointer("/function/name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn assistant_manifest() -> &'static AssistantCapabilityManifest {
    static MANIFEST: OnceLock<AssistantCapabilityManifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_yaml::from_str(include_str!("assistant_capabilities.yaml"))
            .expect("assistant capabilities manifest is valid YAML")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_catalog_teaches_map_remote_endpoint() {
        let catalog = assistant_feature_catalog();
        let feature = catalog
            .iter()
            .find(|feature| feature.id == "map_remote")
            .expect("map remote feature doc");

        assert!(
            feature
                .action_endpoints
                .iter()
                .any(|endpoint| endpoint == "/admin/map-remote-rules")
        );
        assert!(feature.read_tools.iter().any(|tool| tool == "get_rules"));
        assert!(
            feature
                .examples
                .iter()
                .any(|example| example.contains("map"))
        );
    }

    #[test]
    fn feature_catalog_query_finds_related_features() {
        let result = read_feature_catalog(json!({ "query": "websocket" })).expect("catalog");
        let features = result["features"].as_array().expect("features array");

        assert!(features.iter().any(|feature| feature["id"] == "inspectors"));
    }

    #[test]
    fn tool_registry_exposes_feature_catalog() {
        assert!(
            tool_info()
                .iter()
                .any(|tool| tool.name == "get_feature_catalog" && tool.category == "read")
        );
    }

    #[test]
    fn tool_registry_exposes_workspace_actions_from_manifest() {
        assert!(tool_info().iter().any(|tool| {
            tool.name == "workspace_sessions_apply_filter" && tool.category == "ui"
        }));
        assert!(openai_tool_specs().iter().any(|spec| {
            spec.pointer("/function/name").and_then(Value::as_str)
                == Some("workspace_sessions_apply_filter")
        }));
        assert_eq!(
            workspace_action_name_for_tool("workspace_sessions_apply_filter").as_deref(),
            Some("sessions.apply_filter")
        );
    }

    #[test]
    fn feature_catalog_covers_protocol_features() {
        let catalog = assistant_feature_catalog();
        for id in ["protocol_observability", "http3", "telemetry"] {
            assert!(
                catalog.iter().any(|feature| feature.id == id),
                "feature catalog must document '{id}'"
            );
        }
        let breakpoints = catalog
            .iter()
            .find(|feature| feature.id == "breakpoints")
            .expect("breakpoints feature doc");
        assert!(
            breakpoints.notes.iter().any(|note| note.contains("tier")),
            "breakpoints doc must explain tiers"
        );
        let compose = catalog
            .iter()
            .find(|feature| feature.id == "compose_forward")
            .expect("compose feature doc");
        assert!(
            compose
                .action_endpoints
                .iter()
                .any(|endpoint| endpoint == "/admin/forward/websocket"),
            "compose doc must list the WebSocket forward endpoint"
        );
    }

    #[test]
    fn tool_registry_exposes_protocol_read_tools() {
        for name in [
            "get_protocol_metrics",
            "get_connections",
            "get_breakpoint_diagnostics",
        ] {
            assert!(
                tool_info()
                    .iter()
                    .any(|tool| tool.name == name && tool.category == "read"),
                "tool registry must expose '{name}' as a read tool"
            );
        }
    }

    #[test]
    fn response_guidance_prioritizes_ui_steps() {
        let guidance = assistant_response_guidance();

        assert!(guidance.contains("UI steps first"));
    }

    #[test]
    fn https_setup_feature_has_ui_steps() {
        let catalog = assistant_feature_catalog();
        let feature = catalog
            .iter()
            .find(|feature| feature.id == "https_mitm_ca")
            .expect("https setup feature doc");

        assert!(feature.ui_steps.iter().any(|step| step.contains("Root CA")));
        assert!(feature.ui_steps.iter().any(|step| step.contains("trust")));
    }
}
