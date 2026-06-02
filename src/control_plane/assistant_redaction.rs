use serde_json::Value;

pub(crate) fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut redacted = serde_json::Map::new();
            for (key, value) in map {
                if is_sensitive_key(key) {
                    redacted.insert(key.clone(), Value::String("[REDACTED]".to_string()));
                } else {
                    redacted.insert(key.clone(), redact_value(value));
                }
            }
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::String(text) => Value::String(redact_string(text)),
        _ => value.clone(),
    }
}

pub(crate) fn redact_string(input: &str) -> String {
    let mut out = redact_uri_query_secrets(input);
    let uri_was_redacted = out != input;
    for marker in [
        "sk-",
        "Bearer ",
        "authorization:",
        "cookie:",
        "api_key",
        "secret",
        "token",
    ] {
        if uri_was_redacted {
            break;
        }
        if out
            .to_ascii_lowercase()
            .contains(&marker.to_ascii_lowercase())
        {
            out = "[REDACTED]".to_string();
            break;
        }
    }
    if looks_like_jwt(&out) {
        out = "[REDACTED]".to_string();
    }
    if out.len() > 4_000 {
        out.truncate(4_000);
        out.push_str("...[truncated]");
    }
    out
}

pub(crate) fn redact_uri(input: &str) -> String {
    let redacted = redact_uri_query_secrets(input);
    if redacted == input {
        redact_string(input)
    } else {
        redacted
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("authorization")
        || key.contains("cookie")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("password")
}

fn redact_uri_query_secrets(input: &str) -> String {
    let Some((before_query, after_query)) = input.split_once('?') else {
        return input.to_string();
    };
    let (query, fragment) = after_query
        .split_once('#')
        .map(|(query, fragment)| (query, Some(fragment)))
        .unwrap_or((after_query, None));
    let mut changed = false;
    let redacted_query = query
        .split('&')
        .map(|part| {
            let Some((key, value)) = part.split_once('=') else {
                if is_sensitive_key(part) {
                    changed = true;
                    return format!("{part}=[REDACTED]");
                }
                return part.to_string();
            };
            if is_sensitive_key(key) || looks_like_jwt(value) {
                changed = true;
                format!("{key}=[REDACTED]")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");

    if !changed {
        return input.to_string();
    }

    match fragment {
        Some(fragment) => format!("{before_query}?{redacted_query}#{fragment}"),
        None => format!("{before_query}?{redacted_query}"),
    }
}

fn looks_like_jwt(input: &str) -> bool {
    let token = input.trim();
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            part.len() >= 8
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_sensitive_keys_recursively() {
        let value = json!({
            "headers": {
                "authorization": "Bearer sk-test",
                "x-ok": "visible"
            },
            "nested": [{ "api_key": "sk-test" }]
        });
        let redacted = redact_value(&value);
        assert_eq!(redacted["headers"]["authorization"], "[REDACTED]");
        assert_eq!(redacted["headers"]["x-ok"], "visible");
        assert_eq!(redacted["nested"][0]["api_key"], "[REDACTED]");
    }

    #[test]
    fn redacts_sensitive_query_values_without_dropping_path_context() {
        let value = redact_uri("/login?token=abc123&next=/home&api_key=secret#top");

        assert_eq!(
            value,
            "/login?token=[REDACTED]&next=/home&api_key=[REDACTED]#top"
        );
    }

    #[test]
    fn redacts_jwt_like_values() {
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";

        assert_eq!(redact_string(token), "[REDACTED]");
        assert_eq!(
            redact_uri(&format!("/callback?id_token={token}&state=ok")),
            "/callback?id_token=[REDACTED]&state=ok"
        );
    }
}
