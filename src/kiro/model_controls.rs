//! Only explicit, complete catalog constraints are enforced. This is not a JSON Schema engine.
use serde_json::Value;

use super::models::AvailableModel;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ModelControls {
    max_tokens: Option<u64>,
    effort: Option<String>,
    thinking_type: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModelParameterError(pub &'static str);

impl std::fmt::Display for ModelParameterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is incompatible with the fresh model capabilities reported by available accounts",
            self.0
        )
    }
}

impl std::error::Error for ModelParameterError {}

pub(crate) fn native_thinking_type<'a>(model: &str, kind: &'a str) -> &'a str {
    match kind {
        "disabled" if model == "claude-sonnet-5.5" => "between_tools",
        "enabled" if model.starts_with("claude-") && !model.ends_with("4.5") => "adaptive",
        _ => kind,
    }
}

impl ModelControls {
    #[cfg(test)]
    pub(crate) fn from_request(body: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(body).ok()?;
        Self::from_value(&value)
    }

    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let fields = value.get("additionalModelRequestFields")?.as_object()?;
        Some(Self {
            max_tokens: fields.get("max_tokens").and_then(Value::as_u64),
            effort: fields
                .get("output_config")
                .and_then(|v| v.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            thinking_type: fields
                .get("thinking")
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    pub(crate) fn validate(&self, model: &AvailableModel) -> Result<(), ModelParameterError> {
        if let (Some(requested), Some(limit)) = (self.max_tokens, model.max_output_tokens)
            && requested > limit
        {
            return Err(ModelParameterError("max_tokens"));
        }
        if let Some(effort) = &self.effort
            && model.effort_schema_path.as_deref() == Some("output_config.effort")
            && !model.effort_levels.is_empty()
            && !model.effort_levels.contains(effort)
        {
            return Err(ModelParameterError("output_config.effort"));
        }
        if let Some(kind) = &self.thinking_type
            && !model.thinking_types.is_empty()
            && !model
                .thinking_types
                .iter()
                .any(|allowed| allowed == native_thinking_type(&model.model_id, kind))
        {
            return Err(ModelParameterError("thinking.type"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> AvailableModel {
        AvailableModel {
            model_id: id.into(),
            model_name: id.into(),
            provider: None,
            rate_multiplier: None,
            rate_unit: None,
            max_input_tokens: None,
            max_output_tokens: None,
            effort_levels: vec![],
            effort_schema_path: None,
            default_effort: None,
            thinking_types: vec![],
            default_thinking: None,
            proxy_supported: true,
        }
    }

    #[test]
    fn thinking_compatibility_matches_native_transport() {
        for (id, kind, allowed) in [
            ("claude-sonnet-5.5", "disabled", "between_tools"),
            ("claude-sonnet-4.6", "enabled", "adaptive"),
            ("claude-opus-4.5", "enabled", "enabled"),
            ("claude-haiku-4.5", "disabled", "disabled"),
        ] {
            let mut model = model(id);
            model.thinking_types = vec![allowed.into()];
            let controls = ModelControls::from_request(
                &serde_json::json!({"additionalModelRequestFields":{
                    "thinking":{"type":kind, "budget_tokens":2000}
                }})
                .to_string(),
            )
            .unwrap();
            assert!(controls.validate(&model).is_ok(), "{id}: {kind}");
            model.thinking_types = vec!["different-type".into()];
            assert_eq!(controls.validate(&model).unwrap_err().0, "thinking.type");
        }
    }

    #[test]
    fn effort_schema_paths_and_missing_controls_are_not_guessed() {
        let mut model = model("claude-sonnet-4.6");
        model.effort_levels = vec!["low".into()];
        model.effort_schema_path = Some("reasoning.effort".into());
        let controls = ModelControls::from_request(
            r#"{"additionalModelRequestFields":{"output_config":{"effort":"high"}}}"#,
        )
        .unwrap();
        assert!(controls.validate(&model).is_ok());
        model.effort_schema_path = Some("output_config.effort".into());
        assert_eq!(
            controls.validate(&model).unwrap_err().0,
            "output_config.effort"
        );
        let missing =
            ModelControls::from_request(r#"{"additionalModelRequestFields":{}}"#).unwrap();
        assert!(missing.validate(&model).is_ok());
        for body in ["not-json", "{}", r#"{"additionalModelRequestFields":null}"#] {
            assert!(ModelControls::from_request(body).is_none());
        }
    }
}
