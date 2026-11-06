//! Completion metadata emitted by the Kiro runtime.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    /// New runtimes report exact counts here. Keep observational data tolerant
    /// of unknown/invalid values so it cannot break an otherwise valid answer.
    #[serde(default)]
    pub token_usage: Option<serde_json::Value>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
}

/// Per-response snapshots, never summed: metadata can be repeated or partial.
#[derive(Debug, Clone, Default)]
pub struct ReportedUsage {
    pub input_tokens: Option<i32>,
    pub output_tokens: Option<i32>,
    pub cache_read_input_tokens: Option<i32>,
    pub cache_creation_input_tokens: Option<i32>,
}

impl ReportedUsage {
    pub fn observe(&mut self, metadata: &MetadataEvent) {
        let Some(usage) = &metadata.token_usage else {
            return;
        };
        for (target, name) in [
            (&mut self.input_tokens, "uncachedInputTokens"),
            (&mut self.output_tokens, "outputTokens"),
            (&mut self.cache_read_input_tokens, "cacheReadInputTokens"),
            (
                &mut self.cache_creation_input_tokens,
                "cacheWriteInputTokens",
            ),
        ] {
            if let Some(count) = usage
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .and_then(|count| i32::try_from(count).ok())
            {
                *target = Some(count);
            }
        }
    }

    /// Apply only reported fields; callers supply their existing fallback estimates.
    pub fn apply(&self, usage: &mut serde_json::Value) {
        for (name, count) in [
            ("input_tokens", self.input_tokens),
            ("output_tokens", self.output_tokens),
            ("cache_read_input_tokens", self.cache_read_input_tokens),
            (
                "cache_creation_input_tokens",
                self.cache_creation_input_tokens,
            ),
        ] {
            if let Some(count) = count {
                usage[name] = count.into();
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StopDetails {
    #[serde(default)]
    pub refusal: Option<RefusalDetails>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefusalDetails {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

impl MetadataEvent {
    pub fn is_refusal(&self) -> bool {
        self.stop_details
            .as_ref()
            .is_some_and(|details| details.refusal.is_some())
            || self.stop_reason.as_deref().is_some_and(|reason| {
                ["refusal", "content_filtered", "guardrail_intervened"]
                    .iter()
                    .any(|value| reason.eq_ignore_ascii_case(value))
            })
    }

    pub fn output_limited(&self) -> bool {
        self.stop_reason
            .as_deref()
            .is_some_and(|reason| reason.eq_ignore_ascii_case("max_tokens"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_usage_snapshots_preserve_zero_partial_updates_and_ignore_invalid_fields() {
        let mut reported = ReportedUsage::default();
        for value in [
            json!({"tokenUsage":{"uncachedInputTokens":17,"outputTokens":23,"cacheReadInputTokens":99}}),
            json!({"tokenUsage":{"outputTokens":0,"cacheWriteInputTokens":11}}),
            json!({"tokenUsage":{"uncachedInputTokens":-1,"outputTokens":3.5,"cacheReadInputTokens":"bad", "cacheWriteInputTokens":2147483648u64}}),
            json!({"tokenUsage":null}),
            json!({"tokenUsage":"unknown"}),
            json!({}),
        ] {
            reported.observe(&serde_json::from_value(value).unwrap());
        }
        let mut usage = json!({"input_tokens":123,"output_tokens":456});
        reported.apply(&mut usage);
        assert_eq!(
            usage,
            json!({"input_tokens":17,"output_tokens":0,
            "cache_read_input_tokens":99,"cache_creation_input_tokens":11})
        );
        let mut fallback = json!({"input_tokens":123,"output_tokens":456});
        ReportedUsage::default().apply(&mut fallback);
        assert_eq!(fallback, json!({"input_tokens":123,"output_tokens":456}));
    }

    #[test]
    fn refusal_is_recognized_with_or_without_details() {
        for value in [
            json!({"stopReason":"refusal"}),
            json!({"stopReason":"REFUSAL", "stopDetails":null}),
            json!({"stopReason":"CONTENT_FILTERED"}),
            json!({"stopReason":"content_filtered", "stopDetails":{"refusal":null}}),
            json!({"stopReason":"GUARDRAIL_INTERVENED"}),
            json!({"stopDetails":{"refusal":{}}}),
        ] {
            let metadata: MetadataEvent = serde_json::from_value(value).unwrap();
            assert!(metadata.is_refusal());
        }
    }

    #[test]
    fn normal_metadata_and_null_refusals_remain_valid() {
        for value in [
            json!({}),
            json!({"stopReason":"end_turn", "stopDetails":{"refusal":null}}),
            json!({"stopReason":"max_tokens", "stopDetails":{}}),
        ] {
            let metadata: MetadataEvent = serde_json::from_value(value.clone()).unwrap();
            assert!(!metadata.is_refusal());
            assert_eq!(
                metadata.output_limited(),
                value["stopReason"] == "max_tokens"
            );
        }
    }
}
