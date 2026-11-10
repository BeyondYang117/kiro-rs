//! Debug JSON stays inside authenticated log details and shares their retention.
use super::{CURRENT, redact};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_JSON_BYTES: usize = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 4 * MAX_JSON_BYTES;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DebugJson {
    original_bytes: usize,
    truncated: bool,
    json: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted_reason: Option<String>,
}

fn sensitive_key(key: &str) -> bool {
    let key: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        key.as_str(),
        "authorization"
            | "cookie"
            | "setcookie"
            | "signature"
            | "redactedcontent"
            | "redactedthinking"
            | "profilearn"
            | "userid"
            | "deviceid"
            | "machineid"
            | "accountuuid"
            | "privatekey"
            | "credentials"
            | "secret"
            | "xapikey"
            | "proxyauthorization"
            | "email"
            | "clientid"
            | "proxyusername"
    ) || key.ends_with("token")
        || key.ends_with("apikey")
        || key.ends_with("apikeys")
        || key.ends_with("password")
        || key.ends_with("secret")
}

fn safe_text(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_owned();
    // A shorter configured key may prefix another credential. Replace complete
    // values first so the longer credential cannot leave an exposed suffix.
    let mut secrets: Vec<_> = secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .copied()
        .collect();
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    for secret in secrets {
        text = text.replace(secret, "[REDACTED]");
    }
    let text = redact(&text);
    static PRIVATE_KEY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PRIVATE_KEY.get_or_init(|| regex::Regex::new(
        r"(?s)-----BEGIN (?:[A-Z0-9]+ )?PRIVATE KEY-----.*?(?:-----END (?:[A-Z0-9]+ )?PRIVATE KEY-----|$)"
    ).expect("private key regex")).replace_all(&text, "[REDACTED]").into_owned()
}

fn sanitize(
    value: &Value,
    secrets: &[&str],
    depth: usize,
    budget: &mut usize,
    nodes: &mut usize,
    truncated: &mut bool,
) -> Value {
    if depth > 64 || *nodes >= 100_000 || *budget == 0 {
        *truncated = true;
        return Value::String("[OMITTED: debug limit]".into());
    }
    *nodes += 1;
    match value {
        Value::Object(fields) => {
            let binary = fields
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "base64" | "redacted_thinking"));
            let mut result = serde_json::Map::new();
            for (key, child) in fields {
                if *budget == 0 || *nodes >= 100_000 {
                    *truncated = true;
                    break;
                }
                *budget = budget.saturating_sub(key.len());
                let safe = if sensitive_key(key) || key == "bytes" || (binary && key == "data") {
                    Value::String("[REDACTED]".into())
                } else {
                    sanitize(child, secrets, depth + 1, budget, nodes, truncated)
                };
                result.insert(safe_text(key, secrets), safe);
            }
            Value::Object(result)
        }
        Value::Array(items) => {
            let mut result = Vec::new();
            for child in items {
                if *budget == 0 || *nodes >= 100_000 {
                    *truncated = true;
                    break;
                }
                result.push(sanitize(
                    child,
                    secrets,
                    depth + 1,
                    budget,
                    nodes,
                    truncated,
                ));
            }
            Value::Array(result)
        }
        Value::String(text) => {
            // Redact before truncating so a cut cannot expose part of a secret.
            let mut safe = safe_text(text, secrets);
            if safe.len() > *budget {
                let mut end = *budget;
                while !safe.is_char_boundary(end) {
                    end -= 1;
                }
                safe.truncate(end);
                *truncated = true;
            }
            *budget = budget.saturating_sub(safe.len());
            Value::String(safe)
        }
        other => other.clone(),
    }
}

fn capture(body: &[u8], secrets: &[&str], used: &mut usize) -> DebugJson {
    let mut result = DebugJson {
        original_bytes: body.len(),
        truncated: false,
        json: None,
        omitted_reason: None,
    };
    if *used >= MAX_REQUEST_BYTES {
        result.truncated = true;
        result.omitted_reason = Some("request_debug_budget_exceeded".into());
        return result;
    }
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let mut budget = MAX_JSON_BYTES.min(MAX_REQUEST_BYTES - *used);
        let value = sanitize(
            &value,
            secrets,
            0,
            &mut budget,
            &mut 0,
            &mut result.truncated,
        );
        let size = serde_json::to_vec(&value).expect("JSON value").len();
        if size <= MAX_JSON_BYTES.min(MAX_REQUEST_BYTES - *used) {
            *used += size;
            result.json = Some(value);
        } else {
            result.truncated = true;
            result.omitted_reason = Some("json_debug_size_exceeded".into());
        }
    } else {
        // Never fall back to unstructured body text if JSON parsing fails.
        result.omitted_reason = Some("invalid_json".into());
    }
    result
}

pub(crate) fn capture_client_request(body: &[u8], secrets: &[&str]) {
    let _ = CURRENT.try_with(|context| {
        let mut record = context.lock();
        record.debug_secrets = secrets
            .iter()
            .filter(|secret| !secret.is_empty())
            .map(|secret| (*secret).to_owned())
            .collect();
        record.request_debug = Some(capture(body, secrets, &mut record.debug_bytes));
    });
}

pub(crate) fn capture_upstream_request(body: &str, secrets: &[&str]) {
    let _ = CURRENT.try_with(|context| {
        let mut record = context.lock();
        if record.upstream_attempts.last().is_some() {
            let client_secrets = record.debug_secrets.clone();
            let mut secrets = secrets.to_vec();
            secrets.extend(client_secrets.iter().map(String::as_str));
            // The selected upstream credential is only known after conversion.
            // Remove its opaque values from the already redacted client JSON too.
            if let Some(debug) = &mut record.request_debug
                && let Some(value) = &mut debug.json
            {
                let mut budget = MAX_JSON_BYTES;
                *value = sanitize(
                    value,
                    &secrets,
                    0,
                    &mut budget,
                    &mut 0,
                    &mut debug.truncated,
                );
            }
            let captured = capture(body.as_bytes(), &secrets, &mut record.debug_bytes);
            record.upstream_attempts.last_mut().unwrap().request_debug = Some(captured);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_redaction_preserves_json_and_hides_nested_secrets_and_binary() {
        assert_eq!(
            safe_text("opaque-key-longer", &["opaque-key", "opaque-key-longer"]),
            "[REDACTED]"
        );
        assert_eq!(
            safe_text(
                "-----BEGIN RSA PRIVATE KEY-----\nprivate material\n-----END RSA PRIVATE KEY-----",
                &[]
            ),
            "[REDACTED]"
        );
        let input = json!({
            "system":"Debug Rust. password='private phrase' Bearer private-bearer",
            "metadata":{"user_id":"private-identity"},
            "messages":[{"content":[
                {"type":"text","text":"fn main() {}\nopaque-runtime-secret\n中文"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"private-image"}},
                {"type":"redacted_thinking","data":"private-reasoning"},
                {"type":"thinking","thinking":"inspect src/main.rs","signature":"private-signature"}
            ]}],
            "context":{"api_key":"private-key","refreshToken":"private-refresh","nested":{"password":["private-array"]}},
            "images":[{"source":{"bytes":"private-image"}}],
            "profileArn":"private-profile",
            "input":{"command":"curl https://private-user:private-pass@host.test; apiKey=private-inline"}
        });
        let captured = capture(
            input.to_string().as_bytes(),
            &["opaque-runtime-secret"],
            &mut 0,
        );
        assert!(!captured.truncated);
        let safe = serde_json::to_string(&captured).unwrap();
        assert!(!safe.contains("private"), "{safe}");
        assert!(!safe.contains("opaque-runtime-secret"));
        let value = captured.json.unwrap();
        assert_eq!(
            value["messages"][0]["content"][0]["text"],
            "fn main() {}\n[REDACTED]\n中文"
        );
        assert_eq!(
            value["messages"][0]["content"][3]["thinking"],
            "inspect src/main.rs"
        );
        assert_eq!(
            value["messages"][0]["content"][1]["source"]["media_type"],
            "image/png"
        );
    }

    #[test]
    fn debug_limits_fail_closed_and_preserve_valid_utf8_json() {
        let input = json!({"text": "中".repeat(MAX_JSON_BYTES)}).to_string();
        let mut used = 0;
        let result = capture(input.as_bytes(), &[], &mut used);
        assert!(result.truncated);
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_JSON_BYTES + 256);
        let invalid = capture(b"not JSON password=private-sentinel", &[], &mut used);
        assert!(invalid.json.is_none());
        assert_eq!(invalid.omitted_reason.as_deref(), Some("invalid_json"));
        let mut used = MAX_REQUEST_BYTES;
        let over_budget = capture(b"{}", &[], &mut used);
        assert!(over_budget.truncated);
        assert!(over_budget.json.is_none());
    }
}
