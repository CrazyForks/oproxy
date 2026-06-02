use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::api::{SessionListFilter, SessionListOptions};

use super::assistant_redaction::{redact_uri, redact_value};
use super::workspace::{SessionsViewState, SortDirection};

#[derive(Debug, Clone, Serialize)]
pub(super) struct AssistantContext {
    pub(super) workspace: AssistantWorkspaceContext,
    pub(super) visible_sessions: AssistantVisibleSessionsContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) selected_session: Option<AssistantSessionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) client_hints: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct AssistantWorkspaceContext {
    pub(super) active_surface: String,
    pub(super) sessions_view: Value,
    pub(super) feature_views: Value,
    pub(super) assistant_context: Value,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct AssistantVisibleSessionsContext {
    pub(super) total: usize,
    pub(super) filtered_total: usize,
    pub(super) limit: Option<usize>,
    pub(super) facets: Value,
    pub(super) selected_session_id: Option<String>,
    pub(super) selected_session_in_visible_results: bool,
    pub(super) sessions: Vec<AssistantSessionSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct AssistantSessionSummary {
    pub(super) id: String,
    pub(super) timestamp: chrono::DateTime<chrono::Utc>,
    pub(super) method: String,
    pub(super) uri: String,
    pub(super) host: String,
    pub(super) status: Option<u16>,
    pub(super) source: crate::session::SessionSource,
    pub(super) tags: Vec<String>,
    pub(super) note: Option<String>,
}

pub(super) async fn build_assistant_context(
    state: &Arc<AppState>,
    client_context: Option<&Value>,
) -> AssistantContext {
    let workspace = state.workspace.read().await.clone();
    let visible = state
        .api_handler
        .list_sessions(SessionListOptions {
            limit: Some(20),
            filter: session_filter_from_workspace_context(&workspace.sessions_view),
            ..SessionListOptions::default()
        })
        .await;
    let selected_session_id = workspace.sessions_view.selected_session_id.clone();
    let selected_session_in_visible_results = selected_session_id
        .as_deref()
        .is_some_and(|id| visible.sessions.iter().any(|session| session.id == id));
    let selected_session = match selected_session_id.as_deref() {
        Some(id) => state
            .api_handler
            .get_session_details(id)
            .await
            .map(|detail| assistant_session_summary(detail.exchange)),
        None => None,
    };
    let sessions = visible
        .sessions
        .into_iter()
        .map(assistant_session_summary)
        .collect();

    AssistantContext {
        workspace: AssistantWorkspaceContext {
            active_surface: serde_json::to_value(&workspace.active_surface)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "sessions".to_string()),
            sessions_view: redact_value(&json!(workspace.sessions_view)),
            feature_views: redact_value(&json!(workspace.feature_views)),
            assistant_context: redact_value(&json!(workspace.assistant_context)),
        },
        visible_sessions: AssistantVisibleSessionsContext {
            total: visible.total,
            filtered_total: visible.filtered_total,
            limit: visible.limit,
            facets: redact_value(&json!(visible.facets)),
            selected_session_id,
            selected_session_in_visible_results,
            sessions,
        },
        selected_session,
        client_hints: client_context.map(redact_value),
    }
}

fn assistant_session_summary(exchange: crate::session::Exchange) -> AssistantSessionSummary {
    AssistantSessionSummary {
        id: exchange.id,
        timestamp: exchange.timestamp,
        method: exchange.request.method,
        uri: redact_uri(&exchange.request.uri),
        host: exchange.request.host,
        status: exchange.response.as_ref().map(|response| response.status),
        source: exchange.source,
        tags: exchange.tags,
        note: exchange.note,
    }
}

fn session_filter_from_workspace_context(view: &SessionsViewState) -> SessionListFilter {
    SessionListFilter {
        query: view.query.clone(),
        regex: view.regex,
        methods: Some(view.methods.clone()),
        status_buckets: Some(view.status_buckets.clone()),
        host_focus: view.host_focus.clone(),
        host_filter: view.host_filter.clone(),
        sort: crate::api::SessionSort {
            key: view.sort.key.clone(),
            dir: match view.sort.dir {
                SortDirection::Asc => crate::api::SessionSortDirection::Asc,
                SortDirection::Desc => crate::api::SessionSortDirection::Desc,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::workspace::{SessionsViewMode, WorkspaceSort};

    #[test]
    fn workspace_context_filter_preserves_backend_view_semantics() {
        let view = SessionsViewState {
            query: "host:api.test.com".to_string(),
            regex: true,
            methods: vec!["POST".to_string()],
            status_buckets: vec!["5".to_string()],
            host_focus: vec!["api.test.com".to_string()],
            host_filter: Some("api.test.com".to_string()),
            sort: WorkspaceSort {
                key: "status".to_string(),
                dir: SortDirection::Desc,
            },
            view_mode: SessionsViewMode::Sequence,
            selected_session_id: Some("s1".to_string()),
        };

        let filter = session_filter_from_workspace_context(&view);

        assert_eq!(filter.query, "host:api.test.com");
        assert!(filter.regex);
        assert_eq!(filter.methods, Some(vec!["POST".to_string()]));
        assert_eq!(filter.status_buckets, Some(vec!["5".to_string()]));
        assert_eq!(filter.host_focus, vec!["api.test.com".to_string()]);
        assert_eq!(filter.host_filter.as_deref(), Some("api.test.com"));
        assert_eq!(filter.sort.key, "status");
        assert!(matches!(
            filter.sort.dir,
            crate::api::SessionSortDirection::Desc
        ));
    }
}
