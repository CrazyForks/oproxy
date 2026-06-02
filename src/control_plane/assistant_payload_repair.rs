use serde_json::{Value, json};

pub(super) fn repair_assistant_payload(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_array_field(key)
                    && let Some(array) = singleton_as_json_array(value)
                {
                    *value = array;
                    repair_assistant_payload(value);
                    continue;
                }
                if is_numeric_field(key)
                    && let Some(number) = numeric_string_as_json_number(value)
                {
                    *value = number;
                    continue;
                }
                if is_boolean_field(key)
                    && let Some(boolean) = boolean_string_as_json_bool(value)
                {
                    *value = boolean;
                    continue;
                }
                repair_assistant_payload(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                repair_assistant_payload(item);
            }
        }
        _ => {}
    }
}

fn is_numeric_field(key: &str) -> bool {
    matches!(
        key,
        "bandwidth_limit_kbps"
            | "call_count"
            | "code"
            | "delay_ms"
            | "latency_ms"
            | "limit"
            | "offset"
            | "port"
            | "status"
    )
}

fn numeric_string_as_json_number(value: &Value) -> Option<Value> {
    let raw = value.as_str()?.trim();
    if raw.is_empty() || !raw.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>().ok().map(|number| json!(number))
}

fn is_boolean_field(key: &str) -> bool {
    matches!(
        key,
        "enabled" | "include_bodies" | "merge" | "raw" | "regex"
    )
}

fn boolean_string_as_json_bool(value: &Value) -> Option<Value> {
    match value.as_str()?.trim().to_ascii_lowercase().as_str() {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        _ => None,
    }
}

fn is_array_field(key: &str) -> bool {
    matches!(
        key,
        "actions" | "events" | "host_focus" | "hosts" | "methods" | "responses" | "status_buckets"
    )
}

fn singleton_as_json_array(value: &Value) -> Option<Value> {
    match value {
        Value::Array(_) | Value::Null => None,
        Value::Object(_) => Some(Value::Array(vec![value.clone()])),
        Value::String(raw) => {
            let items: Vec<Value> = raw
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(item.to_string()))
                .collect();
            if items.is_empty() {
                None
            } else {
                Some(Value::Array(items))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repairs_known_numeric_and_boolean_strings_recursively() {
        let mut payload = json!({
            "enabled": "false",
            "location": { "port": "443", "mode": "glob" },
            "actions": [{ "type": "set_status", "code": "500" }],
            "regex": "true"
        });

        repair_assistant_payload(&mut payload);

        assert_eq!(payload["enabled"], false);
        assert_eq!(payload["location"]["port"], 443);
        assert_eq!(payload["actions"][0]["code"], 500);
        assert_eq!(payload["regex"], true);
    }

    #[test]
    fn repairs_known_singleton_array_fields() {
        let mut payload = json!({
            "actions": { "type": "set_header", "name": "x-request-id", "value": "1233" },
            "location": { "methods": "GET, POST" },
            "responses": { "status": "200", "body": "ok", "headers": {}, "delay_ms": "0" }
        });

        repair_assistant_payload(&mut payload);

        assert!(payload["actions"].is_array());
        assert_eq!(payload["actions"][0]["type"], "set_header");
        assert_eq!(payload["location"]["methods"], json!(["GET", "POST"]));
        assert!(payload["responses"].is_array());
        assert_eq!(payload["responses"][0]["status"], 200);
        assert_eq!(payload["responses"][0]["delay_ms"], 0);
    }

    #[test]
    fn does_not_repair_unknown_or_unsafe_string_fields() {
        let mut payload = json!({
            "name": "500",
            "destination": "true",
            "enabled": "yes",
            "code": "5xx",
            "latency_ms": "-1"
        });

        repair_assistant_payload(&mut payload);

        assert_eq!(payload["name"], "500");
        assert_eq!(payload["destination"], "true");
        assert_eq!(payload["enabled"], "yes");
        assert_eq!(payload["code"], "5xx");
        assert_eq!(payload["latency_ms"], "-1");
    }
}
