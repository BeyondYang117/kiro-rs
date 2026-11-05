//! Bounded previews of the response delivered to the client, including SSE deltas.
use super::{Value, redact};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const MAX_PREVIEW_BYTES: usize = 16 * 1024;
const MAX_BLOCKS: u64 = 64;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResponsePreview {
    pub content: Vec<Value>,
    pub stop_reason: Option<String>,
    pub truncated: bool,
}

/// Keep a UTF-8 prefix, and redact only after reassembling all stream fragments.
pub(crate) fn append_preview(target: &mut String, text: &str, limit: usize) -> bool {
    let mut end = text.len().min(limit.saturating_sub(target.len()));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
    end < text.len()
}

struct Budget {
    bytes: usize,
    nodes: usize,
    truncated: bool,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            bytes: MAX_PREVIEW_BYTES,
            nodes: 512,
            truncated: false,
        }
    }
}

impl Budget {
    fn text(&mut self, text: &str) -> String {
        let mut result = String::new();
        self.truncated |= append_preview(&mut result, text, self.bytes);
        self.bytes -= result.len();
        result
    }

    fn copy(&mut self, value: &Value, depth: usize) -> Value {
        if self.nodes == 0 || self.bytes == 0 || depth > 16 {
            self.truncated = true;
            return Value::Null;
        }
        self.nodes -= 1;
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(values) => {
                self.truncated |= values.len() > MAX_BLOCKS as usize;
                Value::Array(
                    values
                        .iter()
                        .take(MAX_BLOCKS as usize)
                        .map(|v| self.copy(v, depth + 1))
                        .collect(),
                )
            }
            Value::Object(fields) => {
                let mut result = serde_json::Map::new();
                for (key, value) in fields {
                    if self.nodes == 0 || self.bytes < key.len() {
                        self.truncated = true;
                        break;
                    }
                    self.bytes -= key.len();
                    result.insert(key.clone(), self.copy(value, depth + 1));
                }
                Value::Object(result)
            }
            _ => value.clone(),
        }
    }
}

fn redact_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = redact(text),
        Value::Array(values) => values.iter_mut().for_each(redact_value),
        Value::Object(fields) => {
            for (key, value) in fields {
                let probe = format!("{key}=preview-secret");
                if redact(&probe) != probe {
                    *value = Value::String("[REDACTED]".into());
                } else {
                    redact_value(value);
                }
            }
        }
        _ => {}
    }
}

impl ResponsePreview {
    pub fn from_message(message: &Value) -> Option<Self> {
        message["content"].as_array()?;
        let mut budget = Budget::default();
        let mut content = budget.copy(&message["content"], 0);
        redact_value(&mut content);
        Some(Self {
            content: content.as_array().cloned().unwrap_or_default(),
            stop_reason: message["stop_reason"]
                .as_str()
                .map(|s| redact(&s.chars().take(256).collect::<String>())),
            truncated: budget.truncated,
        })
    }
}

#[derive(Default)]
pub(super) struct StreamResponsePreview {
    blocks: BTreeMap<u64, Value>,
    tool_inputs: BTreeMap<u64, String>,
    budget: Budget,
    stop_reason: Option<String>,
    started: bool,
}

impl StreamResponsePreview {
    pub fn mark_truncated(&mut self) {
        self.started = true;
        self.budget.truncated = true;
    }

    pub fn observe(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("message_start") => {
                self.started = true;
                if let Some(content) = event["message"]["content"].as_array() {
                    for (index, block) in content.iter().enumerate().take(MAX_BLOCKS as usize) {
                        self.blocks.insert(index as u64, self.budget.copy(block, 0));
                    }
                    self.budget.truncated |= content.len() > MAX_BLOCKS as usize;
                }
            }
            Some("content_block_start" | "content_block_delta" | "content_block_stop") => {
                self.started = true;
                let Some(index) = event["index"].as_u64() else {
                    return;
                };
                if index >= MAX_BLOCKS {
                    self.budget.truncated = true;
                    return;
                }
                match event["type"].as_str() {
                    Some("content_block_start") => {
                        self.blocks
                            .insert(index, self.budget.copy(&event["content_block"], 0));
                    }
                    Some("content_block_delta") => {
                        let delta = &event["delta"];
                        let field = match delta["type"].as_str() {
                            Some("text_delta") => "text",
                            Some("thinking_delta") => "thinking",
                            Some("signature_delta") => "signature",
                            Some("compaction_delta") => "content",
                            Some("input_json_delta") => {
                                if let Some(text) = delta["partial_json"].as_str() {
                                    self.tool_inputs
                                        .entry(index)
                                        .or_default()
                                        .push_str(&self.budget.text(text));
                                }
                                return;
                            }
                            _ => return,
                        };
                        if let Some(text) = delta[field].as_str()
                            && let Some(block) = self.blocks.get_mut(&index)
                            && let Some(fields) = block.as_object_mut()
                        {
                            let target = fields
                                .entry(field)
                                .or_insert_with(|| Value::String(String::new()));
                            if let Value::String(target) = target {
                                target.push_str(&self.budget.text(text));
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                self.stop_reason = event["delta"]["stop_reason"]
                    .as_str()
                    .map(|s| redact(&s.chars().take(256).collect::<String>()));
            }
            _ => {}
        }
    }

    pub fn finish(mut self) -> Option<ResponsePreview> {
        if !self.started {
            return None;
        }
        for (index, input) in self.tool_inputs {
            if let Some(block) = self.blocks.get_mut(&index)
                && let Some(fields) = block.as_object_mut()
            {
                fields.insert(
                    "input".into(),
                    serde_json::from_str(&input).unwrap_or(Value::String(input)),
                );
            }
        }
        // Parsing tool JSON can introduce many nodes; apply the structural budget
        // again to the final content, then redact across reassembled fragments.
        let mut budget = Budget::default();
        let mut content = budget.copy(&Value::Array(self.blocks.into_values().collect()), 0);
        redact_value(&mut content);
        Some(ResponsePreview {
            content: content.as_array().cloned().unwrap_or_default(),
            stop_reason: self.stop_reason,
            truncated: self.budget.truncated || budget.truncated,
        })
    }
}
