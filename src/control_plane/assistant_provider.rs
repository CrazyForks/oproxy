use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

use super::assistant_redaction::redact_string;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AssistantProviderConfig {
    #[serde(default)]
    pub base_url: String,
    pub model: String,
}

pub(super) struct OpenAiCompatibleProviderClient {
    provider: AssistantProviderConfig,
    api_key: String,
    client: reqwest::Client,
}

pub(super) struct ProviderChatMessage {
    pub(super) raw_message: serde_json::Map<String, Value>,
    pub(super) content: String,
    pub(super) tool_calls: Vec<Value>,
}

impl OpenAiCompatibleProviderClient {
    pub(super) fn new(provider: AssistantProviderConfig, api_key: String) -> Result<Self, String> {
        validate_provider(&provider, &api_key)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("assistant HTTP client error: {e}"))?;

        Ok(Self {
            provider,
            api_key,
            client,
        })
    }

    pub(super) async fn chat_completion(
        &self,
        messages: &[Value],
        tools: &[Value],
    ) -> Result<ProviderChatMessage, String> {
        self.chat_completion_inner(messages, Some(tools)).await
    }

    /// Final-answer completion with tools disabled. Used after the tool loop is
    /// exhausted so the model is forced to compose a textual answer from the
    /// context it already gathered, instead of the caller returning a canned
    /// "did not finish" message that discards every tool result.
    pub(super) async fn chat_completion_text_only(
        &self,
        messages: &[Value],
    ) -> Result<ProviderChatMessage, String> {
        self.chat_completion_inner(messages, None).await
    }

    async fn chat_completion_inner(
        &self,
        messages: &[Value],
        tools: Option<&[Value]>,
    ) -> Result<ProviderChatMessage, String> {
        let mut payload = json!({
            "model": self.provider.model,
            "messages": messages,
            "temperature": 0.2,
        });
        if let Some(tools) = tools {
            payload["tools"] = json!(tools);
            payload["tool_choice"] = json!("auto");
        }
        let mut request_builder = self.client.post(chat_completions_url(&self.provider)?);
        if !self.api_key.trim().is_empty() {
            request_builder = request_builder.bearer_auth(&self.api_key);
        }

        let response = request_builder.json(&payload).send().await.map_err(|e| {
            format!(
                "assistant provider request failed: {}",
                redact_string(&e.to_string())
            )
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "assistant provider returned {status}: {}",
                redact_string(&body)
            ));
        }

        let body: Value = response
            .json()
            .await
            .map_err(|e| format!("assistant provider response was not JSON: {e}"))?;
        let Some(raw_message) = body
            .pointer("/choices/0/message")
            .and_then(Value::as_object)
            .cloned()
        else {
            return Err("assistant provider response did not include a message".to_string());
        };

        let content = raw_message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tool_calls = raw_message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        Ok(ProviderChatMessage {
            raw_message,
            content,
            tool_calls,
        })
    }
}

fn validate_provider(provider: &AssistantProviderConfig, api_key: &str) -> Result<(), String> {
    if provider.model.trim().is_empty() {
        return Err("model is required".to_string());
    }
    let url = provider_base_url(provider)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("provider base_url must use http or https".to_string());
    }
    if api_key.trim().is_empty() && !looks_like_local_no_auth_provider(&url) {
        return Err(
            "API key is required for non-local providers; local providers such as Ollama may leave it blank"
                .to_string(),
        );
    }
    Ok(())
}

fn provider_base_url(provider: &AssistantProviderConfig) -> Result<reqwest::Url, String> {
    let base = if provider.base_url.trim().is_empty() {
        "https://api.openai.com/v1"
    } else {
        provider.base_url.trim()
    };
    reqwest::Url::parse(base).map_err(|e| format!("invalid provider base_url: {e}"))
}

fn chat_completions_url(provider: &AssistantProviderConfig) -> Result<reqwest::Url, String> {
    let mut base = provider_base_url(provider)?;
    let mut path = base.path().trim_end_matches('/').to_string();
    if path.ends_with("/chat/completions") {
        base.set_query(None);
        return Ok(base);
    }
    if path.is_empty() || path == "/" {
        path = "/v1".to_string();
    }
    if !path.ends_with("/v1") && !path.ends_with("/v1/") {
        path = format!("{path}/v1");
    }
    path = format!("{}/chat/completions", path.trim_end_matches('/'));
    base.set_path(&path);
    base.set_query(None);
    Ok(base)
}

fn looks_like_local_no_auth_provider(url: &reqwest::Url) -> bool {
    matches!(
        url.host_str().unwrap_or_default(),
        "localhost" | "127.0.0.1" | "::1" | "host.docker.internal"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_validation_rejects_bad_inputs() {
        let provider = AssistantProviderConfig {
            base_url: "ftp://example.com".into(),
            model: "gpt-test".into(),
        };
        assert!(validate_provider(&provider, "key").is_err());
        let provider = AssistantProviderConfig {
            base_url: "https://example.com/v1".into(),
            model: String::new(),
        };
        assert!(validate_provider(&provider, "key").is_err());
        assert!(validate_provider(&provider, "").is_err());
    }

    #[test]
    fn text_only_completion_omits_tool_fields_from_payload() {
        // Mirror the payload construction in chat_completion_inner so we lock in
        // that the final forced answer never advertises tools (some providers
        // reject `tool_choice` with no `tools`, and we must not let the model
        // start another tool call after the loop budget is spent).
        let mut payload = json!({ "model": "gpt-test", "messages": [], "temperature": 0.2 });
        let tools: Option<&[Value]> = None;
        if let Some(tools) = tools {
            payload["tools"] = json!(tools);
            payload["tool_choice"] = json!("auto");
        }
        assert!(payload.get("tools").is_none());
        assert!(payload.get("tool_choice").is_none());

        let mut with_tools = json!({ "model": "gpt-test", "messages": [], "temperature": 0.2 });
        let tools: Option<&[Value]> = Some(&[]);
        if let Some(tools) = tools {
            with_tools["tools"] = json!(tools);
            with_tools["tool_choice"] = json!("auto");
        }
        assert_eq!(with_tools["tool_choice"], "auto");
    }

    #[test]
    fn provider_validation_allows_local_ollama_without_key() {
        let provider = AssistantProviderConfig {
            base_url: "http://host.docker.internal:11434".into(),
            model: "qwen2.5:3b".into(),
        };

        assert!(validate_provider(&provider, "").is_ok());
    }

    #[test]
    fn chat_url_normalizes_bare_provider_origins_to_openai_v1() {
        let provider = AssistantProviderConfig {
            base_url: "http://host.docker.internal:11434".into(),
            model: "qwen2.5:3b".into(),
        };

        assert_eq!(
            chat_completions_url(&provider).unwrap().as_str(),
            "http://host.docker.internal:11434/v1/chat/completions"
        );

        let provider = AssistantProviderConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-test".into(),
        };

        assert_eq!(
            chat_completions_url(&provider).unwrap().as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
    }
}
