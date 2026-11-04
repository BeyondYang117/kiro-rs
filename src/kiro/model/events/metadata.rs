//! Completion metadata emitted by the Kiro runtime.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEvent {
    #[serde(default)]
    pub stop_reason: Option<String>,
}

impl MetadataEvent {
    pub fn output_limited(&self) -> bool {
        self.stop_reason
            .as_deref()
            .is_some_and(|reason| reason.eq_ignore_ascii_case("max_tokens"))
    }
}
