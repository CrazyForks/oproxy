use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::sync::OnceLock;

use axum::{extract::State, response::IntoResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::AppState;

use super::assistant_payload_repair::repair_assistant_payload;

pub(crate) type SharedWorkspaceState = Arc<RwLock<WorkspaceState>>;

const WORKSPACE_ID: &str = "default";
const MAX_QUERY_LEN: usize = 4096;
const MAX_HOST_FOCUS: usize = 32;
const MAX_HOST_LEN: usize = 255;
const MAX_ID_LEN: usize = 256;
const MAX_FEATURE_VIEW_KEY_LEN: usize = 64;
const MAX_FEATURE_VIEW_COUNT: usize = 32;

const ALLOWED_METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "CONNECT", "OPTIONS", "HEAD",
];
const ALLOWED_STATUS_BUCKETS: &[&str] = &["1", "2", "3", "4", "5", "-"];
const ALLOWED_SORT_KEYS: &[&str] = &[
    "idx", "method", "status", "host", "path", "type", "reqSize", "total", "ts", "protocol",
];
const WORKSPACE_ACTION_PREFIX: &str = "wa_";

pub(crate) fn new_workspace_state() -> SharedWorkspaceState {
    Arc::new(RwLock::new(WorkspaceState::default()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceActionRisk {
    UiSafe,
    UiSensitive,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkspaceActionManifest {
    #[serde(default)]
    workspace_actions: Vec<WorkspaceActionDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceActionDefinition {
    pub name: String,
    pub description: String,
    pub category: String,
    pub risk: WorkspaceActionRisk,
    #[serde(default)]
    pub refreshed_resources: Vec<String>,
    #[serde(default)]
    pub openai_spec: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WorkspaceState {
    pub id: String,
    pub version: u64,
    pub active_surface: WorkspaceSurface,
    pub sessions_view: SessionsViewState,
    pub feature_views: BTreeMap<String, FeatureViewState>,
    pub assistant_context: WorkspaceAssistantContext,
    pub updated_at: DateTime<Utc>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            id: WORKSPACE_ID.to_string(),
            version: 1,
            active_surface: WorkspaceSurface::Sessions,
            sessions_view: SessionsViewState::default(),
            feature_views: default_feature_views(),
            assistant_context: WorkspaceAssistantContext::default(),
            updated_at: Utc::now(),
        }
    }
}

impl WorkspaceState {
    fn apply_patch(&mut self, patch: WorkspacePatch) -> Result<(), String> {
        if let Some(active_surface) = patch.active_surface {
            self.active_surface = active_surface;
        }
        if let Some(sessions_view) = patch.sessions_view {
            self.sessions_view.apply_patch(sessions_view)?;
        }
        if let Some(feature_views) = patch.feature_views {
            if feature_views.len() > MAX_FEATURE_VIEW_COUNT {
                return Err(format!(
                    "feature_views may include at most {MAX_FEATURE_VIEW_COUNT} entries"
                ));
            }
            for (key, patch) in feature_views {
                validate_feature_view_key(&key)?;
                let entry = self.feature_views.entry(key).or_default();
                entry.apply_patch(patch)?;
            }
        }
        self.validate()?;
        self.version = self.version.saturating_add(1);
        self.updated_at = Utc::now();
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        self.sessions_view.validate()?;
        if self.feature_views.len() > MAX_FEATURE_VIEW_COUNT {
            return Err(format!(
                "feature_views may include at most {MAX_FEATURE_VIEW_COUNT} entries"
            ));
        }
        for (key, view) in &self.feature_views {
            validate_feature_view_key(key)?;
            view.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceSurface {
    Sessions,
    Dashboard,
    Connections,
    Compose,
    Rules,
    Breakpoints,
    Mock,
    Lua,
    Inspector,
    Dns,
    Capture,
    Webhooks,
    Ca,
    Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionsViewState {
    pub query: String,
    pub regex: bool,
    pub methods: Vec<String>,
    pub status_buckets: Vec<String>,
    pub host_focus: Vec<String>,
    pub host_filter: Option<String>,
    pub sort: WorkspaceSort,
    pub view_mode: SessionsViewMode,
    pub selected_session_id: Option<String>,
    /// Client-side wire-protocol facet filter (e.g. ["h2", "h3"]).
    /// Not used for server-side filtering; persisted so the UI can restore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wire_filter: Vec<String>,
    /// Client-side application-protocol facet filter (e.g. ["grpc", "ws"]).
    /// Not used for server-side filtering; persisted so the UI can restore it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub app_filter: Vec<String>,
}

impl Default for SessionsViewState {
    fn default() -> Self {
        Self {
            query: String::new(),
            regex: false,
            methods: ALLOWED_METHODS.iter().map(|m| (*m).to_string()).collect(),
            status_buckets: ALLOWED_STATUS_BUCKETS
                .iter()
                .map(|b| (*b).to_string())
                .collect(),
            host_focus: Vec::new(),
            host_filter: None,
            sort: WorkspaceSort::default(),
            view_mode: SessionsViewMode::Sequence,
            selected_session_id: None,
            wire_filter: Vec::new(),
            app_filter: Vec::new(),
        }
    }
}

impl SessionsViewState {
    fn apply_patch(&mut self, patch: SessionsViewPatch) -> Result<(), String> {
        if let Some(query) = patch.query {
            self.query = query;
        }
        if let Some(regex) = patch.regex {
            self.regex = regex;
        }
        if let Some(methods) = patch.methods {
            self.methods = normalize_methods(methods)?;
        }
        if let Some(status_buckets) = patch.status_buckets {
            self.status_buckets = normalize_status_buckets(status_buckets)?;
        }
        if let Some(host_focus) = patch.host_focus {
            self.host_focus = normalize_host_list(host_focus)?;
        }
        match patch.host_filter {
            PatchField::Unspecified => {}
            PatchField::Clear => self.host_filter = None,
            PatchField::Set(host_filter) => {
                self.host_filter = normalize_optional_host(Some(host_filter))?;
            }
        }
        if let Some(sort) = patch.sort {
            sort.validate()?;
            self.sort = sort;
        }
        if let Some(view_mode) = patch.view_mode {
            self.view_mode = view_mode;
        }
        match patch.selected_session_id {
            PatchField::Unspecified => {}
            PatchField::Clear => self.selected_session_id = None,
            PatchField::Set(selected_session_id) => {
                self.selected_session_id =
                    normalize_optional_id(Some(selected_session_id), "selected_session_id")?;
            }
        }
        if let Some(wire_filter) = patch.wire_filter {
            self.wire_filter = wire_filter;
        }
        if let Some(app_filter) = patch.app_filter {
            self.app_filter = app_filter;
        }
        self.validate()
    }

    fn validate(&self) -> Result<(), String> {
        if self.query.len() > MAX_QUERY_LEN {
            return Err(format!(
                "sessions_view.query may be at most {MAX_QUERY_LEN} bytes"
            ));
        }
        normalize_methods(self.methods.clone())?;
        normalize_status_buckets(self.status_buckets.clone())?;
        normalize_host_list(self.host_focus.clone())?;
        normalize_optional_host(self.host_filter.clone())?;
        self.sort.validate()?;
        normalize_optional_id(self.selected_session_id.clone(), "selected_session_id")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WorkspaceSort {
    pub key: String,
    pub dir: SortDirection,
}

impl Default for WorkspaceSort {
    fn default() -> Self {
        Self {
            key: "idx".to_string(),
            dir: SortDirection::Asc,
        }
    }
}

impl WorkspaceSort {
    fn validate(&self) -> Result<(), String> {
        if !ALLOWED_SORT_KEYS.contains(&self.key.as_str()) {
            return Err(format!(
                "unsupported sessions_view.sort.key '{}'; allowed: {}",
                self.key,
                ALLOWED_SORT_KEYS.join(", ")
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionsViewMode {
    Sequence,
    Structure,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FeatureViewState {
    pub tab: Option<String>,
    pub selected_id: Option<String>,
}

impl FeatureViewState {
    fn apply_patch(&mut self, patch: FeatureViewPatch) -> Result<(), String> {
        match patch.tab {
            PatchField::Unspecified => {}
            PatchField::Clear => self.tab = None,
            PatchField::Set(tab) => {
                self.tab = normalize_optional_id(Some(tab), "feature_views.tab")?;
            }
        }
        match patch.selected_id {
            PatchField::Unspecified => {}
            PatchField::Clear => self.selected_id = None,
            PatchField::Set(selected_id) => {
                self.selected_id =
                    normalize_optional_id(Some(selected_id), "feature_views.selected_id")?;
            }
        }
        self.validate()
    }

    fn validate(&self) -> Result<(), String> {
        normalize_optional_id(self.tab.clone(), "feature_views.tab")?;
        normalize_optional_id(self.selected_id.clone(), "feature_views.selected_id")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WorkspaceAssistantContext {
    pub last_intent: Option<String>,
    pub last_workspace_action_id: Option<String>,
    pub pending_action_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkspacePatchRequest {
    pub base_version: Option<u64>,
    pub patch: WorkspacePatch,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkspaceActionRequest {
    #[serde(rename = "type")]
    pub action_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct WorkspacePatch {
    pub active_surface: Option<WorkspaceSurface>,
    pub sessions_view: Option<SessionsViewPatch>,
    pub feature_views: Option<BTreeMap<String, FeatureViewPatch>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct SessionsViewPatch {
    pub query: Option<String>,
    pub regex: Option<bool>,
    pub methods: Option<Vec<String>>,
    pub status_buckets: Option<Vec<String>>,
    pub host_focus: Option<Vec<String>>,
    #[serde(default)]
    pub host_filter: PatchField<String>,
    pub sort: Option<WorkspaceSort>,
    pub view_mode: Option<SessionsViewMode>,
    #[serde(default)]
    pub selected_session_id: PatchField<String>,
    pub wire_filter: Option<Vec<String>>,
    pub app_filter: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct FeatureViewPatch {
    #[serde(default)]
    pub tab: PatchField<String>,
    #[serde(default)]
    pub selected_id: PatchField<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) enum PatchField<T> {
    #[default]
    Unspecified,
    Clear,
    Set(T),
}

impl<'de, T> Deserialize<'de> for PatchField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PatchFieldVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T> serde::de::Visitor<'de> for PatchFieldVisitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = PatchField<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a value or null")
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PatchField::Clear)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(PatchField::Clear)
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                T::deserialize(deserializer).map(PatchField::Set)
            }
        }

        deserializer.deserialize_option(PatchFieldVisitor(std::marker::PhantomData))
    }
}

#[derive(Debug, Serialize)]
struct WorkspaceEnvelope {
    workspace: WorkspaceState,
}

#[derive(Debug, Serialize)]
struct WorkspacePatchResponse {
    ok: bool,
    workspace: WorkspaceState,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceActionResult {
    pub ok: bool,
    pub action_id: String,
    #[serde(rename = "type")]
    pub action_type: String,
    pub message: String,
    pub workspace: WorkspaceState,
    pub refreshed_resources: Vec<String>,
}

pub(super) async fn get_workspace(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(WorkspaceEnvelope {
        workspace: state.workspace.read().await.clone(),
    })
}

pub(super) async fn patch_workspace(
    State(state): State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<WorkspacePatchRequest>,
) -> impl IntoResponse {
    let mut workspace = state.workspace.write().await;
    if let Some(base_version) = req.base_version
        && base_version != workspace.version
    {
        return workspace_error(
            axum::http::StatusCode::CONFLICT,
            format!(
                "workspace version conflict: current version is {}, request base_version was {}",
                workspace.version, base_version
            ),
        );
    }

    let mut next = workspace.clone();
    if let Err(error) = next.apply_patch(req.patch) {
        return workspace_error(axum::http::StatusCode::BAD_REQUEST, error);
    }
    *workspace = next.clone();
    axum::Json(WorkspacePatchResponse {
        ok: true,
        workspace: next,
    })
    .into_response()
}

pub(super) async fn execute_workspace_action(
    State(state): State<Arc<AppState>>,
    axum::extract::Json(req): axum::extract::Json<WorkspaceActionRequest>,
) -> impl IntoResponse {
    match apply_workspace_action(&state, req).await {
        Ok(result) => axum::Json(result).into_response(),
        Err(error) => workspace_error(axum::http::StatusCode::BAD_REQUEST, error),
    }
}

pub(crate) async fn apply_workspace_action(
    state: &Arc<AppState>,
    mut req: WorkspaceActionRequest,
) -> Result<WorkspaceActionResult, String> {
    repair_assistant_payload(&mut req.payload);
    let action = workspace_action_definition(&req.action_type)
        .ok_or_else(|| format!("workspace action '{}' is not allowlisted", req.action_type))?;
    let mut workspace = state.workspace.write().await;
    let mut next = workspace.clone();
    let message = apply_workspace_action_to_state(&mut next, &action, req.payload)?;
    next.assistant_context.last_intent = Some(action.name.clone());
    next.assistant_context.last_workspace_action_id = Some(workspace_action_id());
    next.version = next.version.saturating_add(1);
    next.updated_at = Utc::now();
    next.validate()?;

    let action_id = next
        .assistant_context
        .last_workspace_action_id
        .clone()
        .unwrap_or_else(workspace_action_id);
    *workspace = next.clone();

    Ok(WorkspaceActionResult {
        ok: true,
        action_id,
        action_type: action.name,
        message,
        workspace: next,
        refreshed_resources: action.refreshed_resources,
    })
}

pub(crate) fn workspace_action_definitions() -> Vec<WorkspaceActionDefinition> {
    workspace_manifest().workspace_actions.clone()
}

fn apply_workspace_action_to_state(
    workspace: &mut WorkspaceState,
    action: &WorkspaceActionDefinition,
    payload: Value,
) -> Result<String, String> {
    match action.name.as_str() {
        "navigation.open_surface" => action_open_surface(workspace, payload),
        "sessions.apply_filter" => action_apply_sessions_filter(workspace, payload),
        "sessions.clear_filter" => action_clear_sessions_filter(workspace, payload),
        "sessions.select" => action_select_session(workspace, payload),
        "sessions.set_view_mode" => action_set_sessions_view_mode(workspace, payload),
        "rules.open_tab" => action_open_rules_tab(workspace, payload),
        _ => Err(format!(
            "workspace action '{}' is declared but has no executor",
            action.name
        )),
    }
}

fn default_feature_views() -> BTreeMap<String, FeatureViewState> {
    [
        (
            "rules".to_string(),
            FeatureViewState {
                tab: Some("rules".to_string()),
                selected_id: None,
            },
        ),
        (
            "mock".to_string(),
            FeatureViewState {
                tab: Some("rules".to_string()),
                selected_id: None,
            },
        ),
        (
            "dns".to_string(),
            FeatureViewState {
                tab: None,
                selected_id: None,
            },
        ),
    ]
    .into_iter()
    .collect()
}

fn action_open_surface(workspace: &mut WorkspaceState, payload: Value) -> Result<String, String> {
    let surface = payload
        .get("surface")
        .cloned()
        .ok_or_else(|| "navigation.open_surface requires payload.surface".to_string())?;
    let surface: WorkspaceSurface =
        serde_json::from_value(surface).map_err(|_| "invalid workspace surface".to_string())?;
    workspace.active_surface = surface.clone();
    Ok(format!("Opened {}", workspace_surface_label(&surface)))
}

fn action_apply_sessions_filter(
    workspace: &mut WorkspaceState,
    payload: Value,
) -> Result<String, String> {
    let patch = SessionsViewPatch {
        query: payload
            .get("query")
            .and_then(Value::as_str)
            .map(str::to_string),
        regex: payload.get("regex").and_then(Value::as_bool),
        methods: payload.get("methods").and_then(parse_string_array),
        status_buckets: payload.get("status_buckets").and_then(parse_string_array),
        host_focus: payload.get("host_focus").and_then(parse_string_array),
        host_filter: parse_patch_string_field(payload.get("host_filter")),
        sort: payload
            .get("sort")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| format!("invalid sessions.apply_filter sort: {e}"))?,
        view_mode: payload
            .get("view_mode")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| format!("invalid sessions.apply_filter view_mode: {e}"))?,
        selected_session_id: parse_patch_string_field(payload.get("selected_session_id")),
        wire_filter: payload.get("wire_filter").and_then(parse_string_array),
        app_filter: payload.get("app_filter").and_then(parse_string_array),
    };

    workspace.active_surface = WorkspaceSurface::Sessions;
    workspace.sessions_view.apply_patch(patch)?;
    Ok("Applied Sessions filter".to_string())
}

fn action_clear_sessions_filter(
    workspace: &mut WorkspaceState,
    _payload: Value,
) -> Result<String, String> {
    let selected_session_id = workspace.sessions_view.selected_session_id.clone();
    workspace.active_surface = WorkspaceSurface::Sessions;
    workspace.sessions_view = SessionsViewState {
        selected_session_id,
        ..SessionsViewState::default()
    };
    Ok("Cleared Sessions filters".to_string())
}

fn action_select_session(workspace: &mut WorkspaceState, payload: Value) -> Result<String, String> {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "sessions.select requires payload.id".to_string())?;
    workspace.active_surface = WorkspaceSurface::Sessions;
    workspace.sessions_view.selected_session_id =
        normalize_optional_id(Some(id.to_string()), "selected_session_id")?;
    Ok("Selected session".to_string())
}

fn action_set_sessions_view_mode(
    workspace: &mut WorkspaceState,
    payload: Value,
) -> Result<String, String> {
    let mode = payload
        .get("view_mode")
        .or_else(|| payload.get("mode"))
        .cloned()
        .ok_or_else(|| "sessions.set_view_mode requires payload.view_mode".to_string())?;
    workspace.active_surface = WorkspaceSurface::Sessions;
    workspace.sessions_view.view_mode = serde_json::from_value(mode)
        .map_err(|_| "invalid sessions view_mode; expected sequence or structure".to_string())?;
    Ok(format!(
        "Switched Sessions to {} view",
        match workspace.sessions_view.view_mode {
            SessionsViewMode::Sequence => "sequence",
            SessionsViewMode::Structure => "structure",
        }
    ))
}

fn action_open_rules_tab(workspace: &mut WorkspaceState, payload: Value) -> Result<String, String> {
    let tab = payload
        .get("tab")
        .and_then(Value::as_str)
        .ok_or_else(|| "rules.open_tab requires payload.tab".to_string())?;
    let tab = normalize_optional_id(Some(tab.to_string()), "feature_views.rules.tab")?
        .ok_or_else(|| "rules.open_tab requires a non-empty tab".to_string())?;
    workspace.active_surface = WorkspaceSurface::Rules;
    workspace
        .feature_views
        .entry("rules".to_string())
        .or_default()
        .tab = Some(tab.clone());
    Ok(format!("Opened Rules {tab} tab"))
}

fn parse_string_array(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

fn parse_patch_string_field(value: Option<&Value>) -> PatchField<String> {
    match value {
        None => PatchField::Unspecified,
        Some(Value::Null) => PatchField::Clear,
        Some(Value::String(value)) => PatchField::Set(value.clone()),
        Some(other) => PatchField::Set(other.to_string()),
    }
}

fn workspace_action_definition(name: &str) -> Option<WorkspaceActionDefinition> {
    workspace_manifest()
        .workspace_actions
        .iter()
        .find(|action| action.name == name)
        .cloned()
}

fn workspace_manifest() -> &'static WorkspaceActionManifest {
    static MANIFEST: OnceLock<WorkspaceActionManifest> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_yaml::from_str(include_str!("assistant_capabilities.yaml"))
            .expect("assistant capabilities manifest includes valid workspace actions")
    })
}

fn workspace_action_id() -> String {
    format!(
        "{WORKSPACE_ACTION_PREFIX}{}",
        chrono::Utc::now().timestamp_millis()
    )
}

fn workspace_surface_label(surface: &WorkspaceSurface) -> &'static str {
    match surface {
        WorkspaceSurface::Sessions => "Sessions",
        WorkspaceSurface::Dashboard => "Dashboard",
        WorkspaceSurface::Connections => "Connections",
        WorkspaceSurface::Compose => "Compose",
        WorkspaceSurface::Rules => "Rules",
        WorkspaceSurface::Breakpoints => "Breakpoints",
        WorkspaceSurface::Mock => "Mock Server",
        WorkspaceSurface::Lua => "Lua Scripts",
        WorkspaceSurface::Inspector => "Inspectors",
        WorkspaceSurface::Dns => "DNS Override",
        WorkspaceSurface::Capture => "Capture Filter",
        WorkspaceSurface::Webhooks => "Webhooks",
        WorkspaceSurface::Ca => "Root CA",
        WorkspaceSurface::Settings => "Settings",
    }
}

fn normalize_methods(methods: Vec<String>) -> Result<Vec<String>, String> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for method in methods {
        let method = method.trim().to_ascii_uppercase();
        if !ALLOWED_METHODS.contains(&method.as_str()) {
            return Err(format!(
                "unsupported sessions_view.methods entry '{method}'; allowed: {}",
                ALLOWED_METHODS.join(", ")
            ));
        }
        if seen.insert(method.clone()) {
            normalized.push(method);
        }
    }
    Ok(normalized)
}

fn normalize_status_buckets(buckets: Vec<String>) -> Result<Vec<String>, String> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for bucket in buckets {
        let bucket = bucket.trim().to_string();
        if !ALLOWED_STATUS_BUCKETS.contains(&bucket.as_str()) {
            return Err(format!(
                "unsupported sessions_view.status_buckets entry '{bucket}'; allowed: {}",
                ALLOWED_STATUS_BUCKETS.join(", ")
            ));
        }
        if seen.insert(bucket.clone()) {
            normalized.push(bucket);
        }
    }
    Ok(normalized)
}

fn normalize_host_list(hosts: Vec<String>) -> Result<Vec<String>, String> {
    if hosts.len() > MAX_HOST_FOCUS {
        return Err(format!(
            "sessions_view.host_focus may include at most {MAX_HOST_FOCUS} hosts"
        ));
    }
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for host in hosts {
        if let Some(host) = normalize_host(host)?
            && seen.insert(host.clone())
        {
            normalized.push(host);
        }
    }
    Ok(normalized)
}

fn normalize_optional_host(host: Option<String>) -> Result<Option<String>, String> {
    match host {
        Some(host) => normalize_host(host),
        None => Ok(None),
    }
}

fn normalize_host(host: String) -> Result<Option<String>, String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Ok(None);
    }
    if host.len() > MAX_HOST_LEN {
        return Err(format!("host values may be at most {MAX_HOST_LEN} bytes"));
    }
    if host.contains('/') || host.contains('\\') || host.contains(char::is_whitespace) {
        return Err(
            "host values must be hostnames, not URLs or whitespace-separated text".to_string(),
        );
    }
    Ok(Some(host))
}

fn normalize_optional_id(id: Option<String>, field: &str) -> Result<Option<String>, String> {
    match id {
        Some(id) => {
            let id = id.trim().to_string();
            if id.is_empty() {
                return Ok(None);
            }
            if id.len() > MAX_ID_LEN {
                return Err(format!("{field} may be at most {MAX_ID_LEN} bytes"));
            }
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

fn validate_feature_view_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_FEATURE_VIEW_KEY_LEN {
        return Err(format!(
            "feature view keys must be 1-{MAX_FEATURE_VIEW_KEY_LEN} bytes"
        ));
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    {
        return Err(
            "feature view keys may contain only lowercase letters, digits, '_' or '-'".to_string(),
        );
    }
    Ok(())
}

fn workspace_error(status: axum::http::StatusCode, error: String) -> axum::response::Response {
    (
        status,
        axum::Json(serde_json::json!({
            "ok": false,
            "error": error,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_contract_matches_ui_defaults() {
        let workspace = WorkspaceState::default();
        assert_eq!(workspace.id, "default");
        assert_eq!(workspace.version, 1);
        assert_eq!(workspace.active_surface, WorkspaceSurface::Sessions);
        assert_eq!(
            workspace.sessions_view.methods,
            vec![
                "GET", "POST", "PUT", "PATCH", "DELETE", "CONNECT", "OPTIONS", "HEAD"
            ]
        );
        assert_eq!(
            workspace.sessions_view.status_buckets,
            vec!["1", "2", "3", "4", "5", "-"]
        );
        assert_eq!(workspace.sessions_view.sort.key, "idx");
        assert_eq!(workspace.sessions_view.sort.dir, SortDirection::Asc);
        assert_eq!(
            workspace.sessions_view.view_mode,
            SessionsViewMode::Sequence
        );
    }

    #[test]
    fn workspace_serializes_with_stable_api_strings() {
        let workspace = WorkspaceState::default();
        let value = serde_json::to_value(&workspace).expect("workspace serializes");

        assert_eq!(value["active_surface"], "sessions");
        assert_eq!(value["sessions_view"]["view_mode"], "sequence");
        assert_eq!(value["sessions_view"]["sort"]["key"], "idx");
        assert_eq!(value["sessions_view"]["sort"]["dir"], "asc");
        assert_eq!(
            value["sessions_view"]["methods"],
            serde_json::json!([
                "GET", "POST", "PUT", "PATCH", "DELETE", "CONNECT", "OPTIONS", "HEAD"
            ])
        );
    }

    #[test]
    fn patch_contract_accepts_json_null_to_clear_nullable_fields() {
        let req: WorkspacePatchRequest = serde_json::from_value(serde_json::json!({
            "base_version": 3,
            "patch": {
                "sessions_view": {
                    "host_filter": null,
                    "selected_session_id": null
                },
                "feature_views": {
                    "rules": {
                        "tab": null,
                        "selected_id": null
                    }
                }
            }
        }))
        .expect("patch request deserializes");

        assert_eq!(req.base_version, Some(3));
        let sessions = req.patch.sessions_view.expect("sessions patch");
        assert_eq!(sessions.host_filter, PatchField::Clear);
        assert_eq!(sessions.selected_session_id, PatchField::Clear);
        let mut feature_views = req.patch.feature_views.expect("feature views");
        let rules = feature_views.remove("rules").expect("rules patch");
        assert_eq!(rules.tab, PatchField::Clear);
        assert_eq!(rules.selected_id, PatchField::Clear);
    }

    #[test]
    fn patch_merges_sessions_view_and_increments_version() {
        let mut workspace = WorkspaceState::default();
        workspace
            .apply_patch(WorkspacePatch {
                active_surface: Some(WorkspaceSurface::Sessions),
                sessions_view: Some(SessionsViewPatch {
                    query: Some("host:api.test.com status:500".to_string()),
                    methods: Some(vec![
                        "get".to_string(),
                        "POST".to_string(),
                        "GET".to_string(),
                    ]),
                    status_buckets: Some(vec!["5".to_string(), "4".to_string(), "5".to_string()]),
                    host_focus: Some(vec!["API.TEST.COM.".to_string()]),
                    sort: Some(WorkspaceSort {
                        key: "total".to_string(),
                        dir: SortDirection::Desc,
                    }),
                    view_mode: Some(SessionsViewMode::Structure),
                    ..SessionsViewPatch::default()
                }),
                ..WorkspacePatch::default()
            })
            .expect("patch should apply");

        assert_eq!(workspace.version, 2);
        assert_eq!(
            workspace.sessions_view.query,
            "host:api.test.com status:500"
        );
        assert_eq!(workspace.sessions_view.methods, vec!["GET", "POST"]);
        assert_eq!(workspace.sessions_view.status_buckets, vec!["5", "4"]);
        assert_eq!(workspace.sessions_view.host_focus, vec!["api.test.com"]);
        assert_eq!(workspace.sessions_view.sort.key, "total");
        assert_eq!(workspace.sessions_view.sort.dir, SortDirection::Desc);
        assert_eq!(
            workspace.sessions_view.view_mode,
            SessionsViewMode::Structure
        );
    }

    #[test]
    fn patch_can_clear_nullable_fields() {
        let mut workspace = WorkspaceState::default();
        workspace.sessions_view.host_filter = Some("api.test.com".to_string());
        workspace.sessions_view.selected_session_id = Some("s1".to_string());

        workspace
            .apply_patch(WorkspacePatch {
                sessions_view: Some(SessionsViewPatch {
                    host_filter: PatchField::Clear,
                    selected_session_id: PatchField::Clear,
                    ..SessionsViewPatch::default()
                }),
                ..WorkspacePatch::default()
            })
            .expect("patch should clear nullable fields");

        assert_eq!(workspace.sessions_view.host_filter, None);
        assert_eq!(workspace.sessions_view.selected_session_id, None);
    }

    #[test]
    fn rejects_invalid_method_status_and_sort_key() {
        let mut workspace = WorkspaceState::default();
        let method_err = workspace
            .apply_patch(WorkspacePatch {
                sessions_view: Some(SessionsViewPatch {
                    methods: Some(vec!["BREW".to_string()]),
                    ..SessionsViewPatch::default()
                }),
                ..WorkspacePatch::default()
            })
            .expect_err("invalid method should fail");
        assert!(method_err.contains("unsupported sessions_view.methods"));

        let status_err = workspace
            .apply_patch(WorkspacePatch {
                sessions_view: Some(SessionsViewPatch {
                    status_buckets: Some(vec!["7".to_string()]),
                    ..SessionsViewPatch::default()
                }),
                ..WorkspacePatch::default()
            })
            .expect_err("invalid status bucket should fail");
        assert!(status_err.contains("unsupported sessions_view.status_buckets"));

        let sort_err = workspace
            .apply_patch(WorkspacePatch {
                sessions_view: Some(SessionsViewPatch {
                    sort: Some(WorkspaceSort {
                        key: "secretColumn".to_string(),
                        dir: SortDirection::Asc,
                    }),
                    ..SessionsViewPatch::default()
                }),
                ..WorkspacePatch::default()
            })
            .expect_err("invalid sort key should fail");
        assert!(sort_err.contains("unsupported sessions_view.sort.key"));
    }

    #[test]
    fn rejects_url_like_host_focus_values() {
        let mut workspace = WorkspaceState::default();
        let err = workspace
            .apply_patch(WorkspacePatch {
                sessions_view: Some(SessionsViewPatch {
                    host_focus: Some(vec!["https://api.test.com".to_string()]),
                    ..SessionsViewPatch::default()
                }),
                ..WorkspacePatch::default()
            })
            .expect_err("url-like host should fail");
        assert!(err.contains("host values must be hostnames"));
    }

    #[test]
    fn feature_view_patch_validates_keys() {
        let mut feature_views = BTreeMap::new();
        feature_views.insert(
            "rules".to_string(),
            FeatureViewPatch {
                tab: PatchField::Set("map_remote".to_string()),
                selected_id: PatchField::Set("rule_1".to_string()),
            },
        );

        let mut workspace = WorkspaceState::default();
        workspace
            .apply_patch(WorkspacePatch {
                feature_views: Some(feature_views),
                ..WorkspacePatch::default()
            })
            .expect("feature view should patch");

        let rules = workspace.feature_views.get("rules").expect("rules view");
        assert_eq!(rules.tab.as_deref(), Some("map_remote"));
        assert_eq!(rules.selected_id.as_deref(), Some("rule_1"));
    }

    #[test]
    fn workspace_action_manifest_declares_safe_actions() {
        let actions = workspace_action_definitions();
        assert!(actions.iter().any(|action| {
            action.name == "sessions.apply_filter"
                && action.category == "ui"
                && action.risk == WorkspaceActionRisk::UiSafe
                && action.openai_spec.pointer("/function/name")
                    == Some(&serde_json::json!("workspace_sessions_apply_filter"))
        }));
        assert!(
            actions
                .iter()
                .any(|action| action.name == "navigation.open_surface")
        );
    }

    /// Capability honesty: the enum values advertised to the model in the
    /// `workspace_sessions_apply_filter` tool schema must exactly equal the
    /// allowlists the executor validates against (`ALLOWED_METHODS`,
    /// `ALLOWED_STATUS_BUCKETS`, `ALLOWED_SORT_KEYS`) and the SessionsViewMode
    /// variants. Otherwise the model is told it may use a value the executor
    /// rejects, or is denied a value the executor accepts.
    #[test]
    fn apply_filter_spec_enums_match_executor_allowlists() {
        let action = workspace_action_definition("sessions.apply_filter").expect("action");
        let props = action
            .openai_spec
            .pointer("/function/parameters/properties")
            .expect("apply_filter properties");

        let enum_at = |ptr: &str| -> Vec<String> {
            props
                .pointer(ptr)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("missing enum at {ptr}"))
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        };
        let expected = |tokens: &[&str]| -> Vec<String> {
            tokens.iter().map(|token| token.to_string()).collect()
        };

        assert_eq!(
            enum_at("/methods/items/enum"),
            expected(ALLOWED_METHODS),
            "apply_filter methods enum drifted from ALLOWED_METHODS"
        );
        assert_eq!(
            enum_at("/status_buckets/items/enum"),
            expected(ALLOWED_STATUS_BUCKETS),
            "apply_filter status_buckets enum drifted from ALLOWED_STATUS_BUCKETS"
        );
        assert_eq!(
            enum_at("/sort/properties/key/enum"),
            expected(ALLOWED_SORT_KEYS),
            "apply_filter sort.key enum drifted from ALLOWED_SORT_KEYS"
        );
        // SessionsViewMode is snake_case: sequence, structure.
        assert_eq!(
            enum_at("/view_mode/enum"),
            vec!["sequence".to_string(), "structure".to_string()],
            "apply_filter view_mode enum drifted from SessionsViewMode"
        );
    }

    #[test]
    fn workspace_action_apply_filter_updates_state() {
        let action = workspace_action_definition("sessions.apply_filter").expect("action");
        let mut workspace = WorkspaceState::default();

        let message = apply_workspace_action_to_state(
            &mut workspace,
            &action,
            serde_json::json!({
                "query": "host:api.test.com",
                "methods": ["get", "POST"],
                "status_buckets": ["4", "5"],
                "host_focus": ["API.TEST.COM."],
                "view_mode": "structure",
                "selected_session_id": null
            }),
        )
        .expect("action applies");

        assert_eq!(message, "Applied Sessions filter");
        assert_eq!(workspace.active_surface, WorkspaceSurface::Sessions);
        assert_eq!(workspace.sessions_view.query, "host:api.test.com");
        assert_eq!(workspace.sessions_view.methods, vec!["GET", "POST"]);
        assert_eq!(workspace.sessions_view.status_buckets, vec!["4", "5"]);
        assert_eq!(workspace.sessions_view.host_focus, vec!["api.test.com"]);
        assert_eq!(
            workspace.sessions_view.view_mode,
            SessionsViewMode::Structure
        );
        assert_eq!(workspace.sessions_view.selected_session_id, None);
    }

    #[test]
    fn workspace_action_apply_filter_accepts_repaired_boolean_strings() {
        let action = workspace_action_definition("sessions.apply_filter").expect("action");
        let mut workspace = WorkspaceState::default();
        let mut payload = serde_json::json!({
            "query": "api",
            "regex": "true"
        });
        repair_assistant_payload(&mut payload);

        apply_workspace_action_to_state(&mut workspace, &action, payload).expect("action applies");

        assert!(workspace.sessions_view.regex);
    }

    #[test]
    fn workspace_action_open_rules_tab_updates_feature_view() {
        let action = workspace_action_definition("rules.open_tab").expect("action");
        let mut workspace = WorkspaceState::default();

        apply_workspace_action_to_state(
            &mut workspace,
            &action,
            serde_json::json!({ "tab": "map_remote" }),
        )
        .expect("action applies");

        assert_eq!(workspace.active_surface, WorkspaceSurface::Rules);
        assert_eq!(
            workspace
                .feature_views
                .get("rules")
                .and_then(|view| view.tab.as_deref()),
            Some("map_remote")
        );
    }

    #[test]
    fn open_surface_accepts_dashboard_and_connections() {
        let action = workspace_action_definition("navigation.open_surface").expect("action");
        for (surface, expected) in [
            ("dashboard", WorkspaceSurface::Dashboard),
            ("connections", WorkspaceSurface::Connections),
        ] {
            let mut workspace = WorkspaceState::default();
            apply_workspace_action_to_state(
                &mut workspace,
                &action,
                serde_json::json!({ "surface": surface }),
            )
            .expect("surface navigates");
            assert_eq!(workspace.active_surface, expected);
        }
    }
}
