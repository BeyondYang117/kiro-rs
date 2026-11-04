//! Completion metadata emitted by the Kiro runtime.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
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
            || self
                .stop_reason
                .as_deref()
                .is_some_and(|reason| reason.eq_ignore_ascii_case("refusal"))
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
    fn refusal_is_recognized_with_or_without_details() {
        for value in [
            json!({"stopReason":"refusal"}),
            json!({"stopReason":"REFUSAL", "stopDetails":null}),
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
