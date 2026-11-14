//! Anthropic → Kiro 协议转换器
//!
//! 负责将 Anthropic API 请求格式转换为 Kiro API 请求格式

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::kiro::model::requests::conversation::{
    AssistantMessage, ConversationState, CurrentMessage, HistoryAssistantMessage,
    HistoryUserMessage, KiroImage, Message, UserInputMessage, UserInputMessageContext, UserMessage,
};
use crate::kiro::model::requests::tool::{
    InputSchema, Tool, ToolResult, ToolSpecification, ToolUseEntry,
};

use super::types::{ContentBlock, MessagesRequest};

/// 规范化 JSON Schema，修复 MCP 工具定义中常见的类型问题
///
/// Claude Code / MCP 工具定义偶尔会出现 `required: null`、`properties: null` 等，
/// 导致上游返回 400 "Improperly formed request"。
fn normalize_json_schema(schema: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(mut obj) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": true
        });
    };

    // type（必须是字符串）
    if !obj
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("object".to_string()),
        );
    }

    // properties（必须是 object）
    match obj.get("properties") {
        Some(serde_json::Value::Object(_)) => {}
        _ => {
            obj.insert(
                "properties".to_string(),
                serde_json::Value::Object(serde_json::Map::new()),
            );
        }
    }

    // required（必须是 string 数组）
    let required = match obj.remove("required") {
        Some(serde_json::Value::Array(arr)) => serde_json::Value::Array(
            arr.into_iter()
                .filter_map(|v| v.as_str().map(|s| serde_json::Value::String(s.to_string())))
                .collect(),
        ),
        _ => serde_json::Value::Array(Vec::new()),
    };
    obj.insert("required".to_string(), required);

    // additionalProperties（允许 bool 或 object，其他按 true 处理）
    match obj.get("additionalProperties") {
        Some(serde_json::Value::Bool(_)) | Some(serde_json::Value::Object(_)) => {}
        _ => {
            obj.insert(
                "additionalProperties".to_string(),
                serde_json::Value::Bool(true),
            );
        }
    }

    serde_json::Value::Object(obj)
}

/// Kiro rejects `additionalProperties` in tool schemas. Boolean values can be
/// removed from the wire schema; `false` is enforced against returned tool
/// inputs locally. Schema-valued restrictions still cannot be represented.
fn validate_kiro_tool_schema(
    schema: &serde_json::Value,
    path: &str,
) -> Result<(), ConversionError> {
    use serde_json::Value;
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if object
        .get("additionalProperties")
        .is_some_and(Value::is_object)
    {
        return Err(ConversionError::InvalidToolFormat(format!(
            "{path}.additionalProperties cannot be represented by Kiro without weakening the schema"
        )));
    }

    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get(key).and_then(Value::as_object) {
            for (name, child) in children {
                validate_kiro_tool_schema(child, &format!("{path}.{key}.{name}"))?;
            }
        }
    }
    if let Some(dependencies) = object.get("dependencies").and_then(Value::as_object) {
        for (name, child) in dependencies {
            if child.is_object() {
                validate_kiro_tool_schema(child, &format!("{path}.dependencies.{name}"))?;
            }
        }
    }
    for key in [
        "items",
        "additionalItems",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        match object.get(key) {
            Some(Value::Array(children)) if key == "items" => {
                for (index, child) in children.iter().enumerate() {
                    validate_kiro_tool_schema(child, &format!("{path}.{key}[{index}]"))?;
                }
            }
            Some(child) => validate_kiro_tool_schema(child, &format!("{path}.{key}"))?,
            None => {}
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get(key).and_then(Value::as_array) {
            for (index, child) in children.iter().enumerate() {
                validate_kiro_tool_schema(child, &format!("{path}.{key}[{index}]"))?;
            }
        }
    }
    Ok(())
}

/// Remove only no-op schema keywords. `enum`, defaults, examples and other
/// object-valued data are deliberately outside the schema traversal.
fn normalize_kiro_tool_schema(schema: &mut serde_json::Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if object.get("additionalProperties") == Some(&serde_json::Value::Bool(true)) {
        object.remove("additionalProperties");
    }
    if object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        object.remove("required");
    }

    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_object_mut)
        {
            for child in children.values_mut() {
                normalize_kiro_tool_schema(child);
            }
        }
    }
    if let Some(dependencies) = object
        .get_mut("dependencies")
        .and_then(serde_json::Value::as_object_mut)
    {
        for child in dependencies.values_mut().filter(|value| value.is_object()) {
            normalize_kiro_tool_schema(child);
        }
    }
    for key in [
        "items",
        "additionalItems",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if let Some(child) = object.get_mut(key) {
            if let Some(children) = child.as_array_mut().filter(|_| key == "items") {
                for child in children {
                    normalize_kiro_tool_schema(child);
                }
            } else {
                normalize_kiro_tool_schema(child);
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_array_mut)
        {
            for child in children {
                normalize_kiro_tool_schema(child);
            }
        }
    }
}

pub(crate) fn strip_kiro_boolean_additional_properties(schema: &mut serde_json::Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if object
        .get("additionalProperties")
        .is_some_and(serde_json::Value::is_boolean)
    {
        object.remove("additionalProperties");
    }
    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_object_mut)
        {
            for child in children.values_mut() {
                strip_kiro_boolean_additional_properties(child);
            }
        }
    }
    if let Some(dependencies) = object
        .get_mut("dependencies")
        .and_then(serde_json::Value::as_object_mut)
    {
        for child in dependencies.values_mut().filter(|child| child.is_object()) {
            strip_kiro_boolean_additional_properties(child);
        }
    }
    for key in [
        "items",
        "additionalItems",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if let Some(child) = object.get_mut(key) {
            if let Some(children) = child.as_array_mut().filter(|_| key == "items") {
                for child in children {
                    strip_kiro_boolean_additional_properties(child);
                }
            } else {
                strip_kiro_boolean_additional_properties(child);
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_array_mut)
        {
            for child in children {
                strip_kiro_boolean_additional_properties(child);
            }
        }
    }
}

/// 追加到 Write 工具 description 末尾的内容
const WRITE_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: Large tool arguments can be truncated upstream. For large files, create a small skeleton first, then append or edit in chunks using the available tools. Keep each call below 50 lines and 12000 characters of generated content, whichever limit is reached first. Include all required arguments and finish the JSON object. After a failure, inspect the file and use smaller chunks; never repeat the same oversized write unchanged.";

/// 追加到 Edit 工具 description 末尾的内容
const EDIT_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: Split large replacements or patches into separate calls below 50 lines and 12000 characters of generated content each. Use precise anchors and include all required arguments. Inspect the result after a failure before retrying with smaller chunks. Remove any temporary placeholders when finished.";

const SHELL_TOOL_DESCRIPTION_SUFFIX: &str = "- IMPORTANT: Do not put an entire large file in a shell command, heredoc, script, or encoded payload. Split generated content into small complete calls below 50 lines and 12000 characters each. Prefer the available file editing tools for large files. After a truncation failure, inspect the current state and reduce the chunk size.";

/// 追加到系统提示词的分块写入策略
const SYSTEM_CHUNKED_POLICY: &str = "\
Tool output safety: upstream generation can truncate large tool arguments even when a high max_tokens was requested. \
Keep file writes, edits, patches, and generated shell scripts in small complete calls, below 50 lines and 12000 characters of generated content per call. \
For large files, create a skeleton then fill it in incrementally using the available tools; do not move an oversized payload into a shell heredoc or encoded command. \
Always include required arguments and close the JSON object. If a tool fails or output is truncated, inspect the current state, reduce the chunk size, and do not repeat the same failed call unchanged. \
Finish all chunks and verify the final file.";

fn chunked_tool_suffix(name: &str) -> &'static str {
    // MCP names use server prefixes; do not rely on one client's capitalization.
    match name
        .rsplit("__")
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
        .as_str()
    {
        "write" | "write_file" | "create_file" => WRITE_TOOL_DESCRIPTION_SUFFIX,
        "edit" | "edit_file" | "multiedit" | "apply_patch" => EDIT_TOOL_DESCRIPTION_SUFFIX,
        "bash" | "shell" | "exec" | "exec_command" | "execute_command" | "run_command" => {
            SHELL_TOOL_DESCRIPTION_SUFFIX
        }
        _ => "",
    }
}

/// 模型映射：将 Anthropic 模型名映射到 Kiro 模型 ID
/// 严格对照版本号
pub fn map_model(model: &str) -> Option<String> {
    let model_lower = model.to_lowercase();

    // Provider IDs are exact: never silently route a future tier/version to an
    // older model, or reinterpret a Claude thinking alias as a GPT control.
    if matches!(
        model_lower.as_str(),
        "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" | "kimi-k3"
    ) {
        return Some(model_lower);
    }

    if model_lower.contains("fable") {
        // Kiro 侧 ID 为点号形式 claude-fable-5.1（实测 claude-fable-5-1 / claude-fable-5 均被上游拒绝）
        // 目前 Kiro 仅上线 5.1 一个 Fable 版本，故与 haiku 分支一致采用无条件映射
        Some("claude-fable-5.1".to_string())
    } else if model_lower.contains("sonnet") {
        if model_lower.contains("sonnet-5-5") || model_lower.contains("sonnet-5.5") {
            Some("claude-sonnet-5.5".to_string())
        } else if model_lower.contains("sonnet-5") {
            Some("claude-sonnet-5".to_string())
        } else if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-sonnet-4.6".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-sonnet-4.5".to_string())
        } else {
            None
        }
    } else if model_lower.contains("opus") {
        // 5.5 必须先于 opus-5 判断：`contains("opus-5")` 会同时匹配 opus-5.5
        if model_lower.contains("opus-5-5") || model_lower.contains("opus-5.5") {
            Some("claude-opus-5.5".to_string())
        } else if model_lower.contains("opus-5") {
            Some("claude-opus-5".to_string())
        } else if model_lower.contains("4-5") || model_lower.contains("4.5") {
            Some("claude-opus-4.5".to_string())
        } else if model_lower.contains("4-6") || model_lower.contains("4.6") {
            Some("claude-opus-4.6".to_string())
        } else if model_lower.contains("4-7") || model_lower.contains("4.7") {
            Some("claude-opus-4.7".to_string())
        } else if model_lower.contains("4-8") || model_lower.contains("4.8") {
            Some("claude-opus-4.8".to_string())
        } else {
            None
        }
    } else if model_lower.contains("haiku") {
        if model_lower.contains("haiku-5-5") || model_lower.contains("haiku-5.5") {
            Some("claude-haiku-5.5".to_string())
        } else {
            Some("claude-haiku-4.5".to_string())
        }
    } else {
        None
    }
}

pub fn is_gpt(model: &str) -> bool {
    matches!(
        map_model(model).as_deref(),
        Some("gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna")
    )
}

pub fn is_non_claude(model: &str) -> bool {
    is_gpt(model) || model.eq_ignore_ascii_case("kimi-k3")
}

/// Proxy defaults follow Claude Code's model-specific effort choices.
pub fn default_effort(model: &str) -> &'static str {
    match map_model(model).as_deref() {
        Some("claude-opus-5.5" | "claude-sonnet-5.5" | "claude-haiku-5.5") => "medium",
        Some("claude-opus-4.7") => "xhigh",
        _ => "high",
    }
}

/// 根据模型名称返回对应的上下文窗口大小
///
/// 复用 `map_model` 的映射逻辑，确保窗口大小判断与模型映射一致。
/// Kiro 于 2026-03-24 将 Opus 4.6 和 Sonnet 4.6 升级至 1M 上下文。
/// Sonnet 5 / Opus 4.7 / 4.8 / Opus 5 / 5.5 / Fable 5.1 同 1M
pub fn get_context_window_size(model: &str) -> i32 {
    match map_model(model) {
        Some(mapped)
            if is_non_claude(&mapped)
                || mapped == "claude-fable-5.1"
                || mapped == "claude-sonnet-5"
                || mapped == "claude-sonnet-5.5"
                || mapped == "claude-haiku-5.5"
                || mapped == "claude-opus-5.5"
                || mapped == "claude-opus-5"
                || mapped == "claude-sonnet-4.6"
                || mapped == "claude-opus-4.6"
                || mapped == "claude-opus-4.7"
                || mapped == "claude-opus-4.8" =>
        {
            1_000_000
        }
        _ => 200_000,
    }
}

/// Builds the native Kiro controls documented by its model request schema.
pub fn additional_model_request_fields(req: &MessagesRequest) -> Option<serde_json::Value> {
    let mut fields = serde_json::Map::new();
    fields.insert("max_tokens".into(), req.max_tokens.into());
    if is_non_claude(&req.model) {
        if is_gpt(&req.model) {
            let effort = req
                .extra_controls
                .get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    req.output_config
                        .as_ref()
                        .map(|config| config.effort.as_str())
                        .filter(|effort| !effort.is_empty())
                });
            if let Some(effort) = effort {
                fields.insert("reasoning".into(), serde_json::json!({"effort":effort}));
            }
        }
        return Some(fields.into());
    }
    if let Some(thinking) = &req.thinking {
        // Keep client intent here; endpoint-specific normalization belongs to
        // native transport so legacy enabled budgets retain their semantics.
        let kind = thinking.thinking_type.as_str();
        let mut config = serde_json::json!({"type":kind});
        if kind == "enabled" {
            config["budget_tokens"] = thinking.budget_tokens.into();
        }
        if let Some(display) = &thinking.display {
            config["display"] = display.clone().into();
        }
        fields.insert("thinking".into(), config);
    }
    if let Some(output_config) = &req.output_config
        && !output_config.effort.is_empty()
    {
        fields.insert(
            "output_config".into(),
            serde_json::json!({ "effort": output_config.effort }),
        );
    }
    (!fields.is_empty()).then_some(serde_json::Value::Object(fields))
}

pub fn supports_adaptive_thinking(model: &str) -> bool {
    matches!(
        map_model(model).as_deref(),
        Some(
            "claude-fable-5.1"
                | "claude-sonnet-5"
                | "claude-sonnet-5.5"
                | "claude-haiku-5.5"
                | "claude-opus-5"
                | "claude-opus-5.5"
                | "claude-opus-4.7"
                | "claude-opus-4.8"
                | "claude-opus-4.6"
                | "claude-sonnet-4.6"
        )
    )
}

/// Output limits from Kiro's model schemas, separate from context windows.
pub fn get_max_output_tokens(model: &str) -> i32 {
    match map_model(model).as_deref() {
        Some(
            "claude-opus-4.5" | "claude-sonnet-4.5" | "claude-haiku-4.5" | "claude-opus-4.6"
            | "claude-opus-4.7" | "claude-sonnet-4.6" | "claude-sonnet-5.5",
        ) => 64_000,
        _ => 128_000,
    }
}

/// 转换结果
#[derive(Debug)]
pub struct ConversionResult {
    /// 转换后的 Kiro 请求
    pub conversation_state: ConversationState,
    /// 工具名称映射（短名称 → 原始名称），仅当存在超长工具名时非空
    pub tool_name_map: HashMap<String, String>,
}

/// 转换错误
#[derive(Debug)]
pub enum ConversionError {
    UnsupportedModel(String),
    EmptyMessages,
    InvalidToolFormat(String),
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversionError::UnsupportedModel(model) => write!(f, "模型不支持: {}", model),
            ConversionError::EmptyMessages => write!(f, "消息列表为空"),
            ConversionError::InvalidToolFormat(message) => write!(f, "工具格式错误: {message}"),
        }
    }
}

impl std::error::Error for ConversionError {}

/// 从 metadata.user_id 中提取 session UUID
///
/// 支持两种格式:
/// 1. 字符串格式: user_xxx_account__session_0b4445e1-f5be-49e1-87ce-62bbc28ad705
/// 2. JSON 格式: {"device_id":"...","account_uuid":"...","session_id":"UUID"}
///
/// 提取 session UUID 作为 conversationId
fn extract_session_id(user_id: &str) -> Option<String> {
    // 先尝试 JSON 解析
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(user_id)
        && let Some(session_id) = json.get("session_id").and_then(|v| v.as_str())
        && is_valid_uuid(session_id)
    {
        return Some(session_id.to_string());
    }

    // 回退到字符串格式: 查找 "session_" 后面的内容
    if let Some(pos) = user_id.find("session_") {
        let session_part = &user_id[pos + 8..]; // "session_" 长度为 8
        if let Some(uuid_str) = session_part.get(..36)
            && is_valid_uuid(uuid_str)
        {
            return Some(uuid_str.to_string());
        }
    }
    None
}

/// 仅接受标准的 36 字节 UUID 格式，并验证十六进制字符与分隔符位置。
fn is_valid_uuid(s: &str) -> bool {
    s.len() == 36 && Uuid::parse_str(s).is_ok()
}

/// 收集历史消息中使用的所有工具名称
fn collect_history_tool_names(history: &[Message]) -> Vec<String> {
    let mut tool_names = Vec::new();

    for msg in history {
        if let Message::Assistant(assistant_msg) = msg
            && let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses
        {
            for tool_use in tool_uses {
                if !tool_names.contains(&tool_use.name) {
                    tool_names.push(tool_use.name.clone());
                }
            }
        }
    }

    tool_names
}

/// 为历史中使用但不在 tools 列表中的工具创建占位符定义
/// Kiro API 要求：历史消息中引用的工具必须在 currentMessage.tools 中有定义
fn create_placeholder_tool(name: &str) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: name.to_string(),
            description: "Tool used in conversation history".to_string(),
            input_schema: InputSchema::from_json(serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {}
            })),
        },
    }
}

/// Validate raw blocks before permissive ContentBlock decoding can silently discard them.
/// Missing/null input retains the existing no-argument compatibility behavior.
fn validate_tool_blocks(messages: &[super::types::Message]) -> Result<(), ConversionError> {
    for (message_index, message) in messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let kind = block.get("type").and_then(|value| value.as_str());
            if !matches!(kind, Some("tool_use" | "tool_result")) {
                continue;
            }
            let path = format!("messages[{message_index}].content[{block_index}]");
            let invalid =
                |reason: &str| ConversionError::InvalidToolFormat(format!("{path}: {reason}"));
            if serde_json::from_value::<ContentBlock>(block.clone()).is_err() {
                return Err(invalid("invalid tool block field types"));
            }
            if kind == Some("tool_use") {
                if message.role != "assistant" {
                    return Err(invalid("tool_use requires assistant role"));
                }
                block
                    .get("id")
                    .and_then(|value| value.as_str())
                    .filter(|id| !id.trim().is_empty())
                    .ok_or_else(|| invalid("id must be a nonempty string"))?;
                if !block
                    .get("name")
                    .and_then(|value| value.as_str())
                    .is_some_and(|name| !name.trim().is_empty())
                {
                    return Err(invalid("name must be a nonempty string"));
                }
                if block
                    .get("input")
                    .is_some_and(|input| !input.is_null() && !input.is_object())
                {
                    return Err(invalid("input must be a JSON object"));
                }
            } else {
                if message.role != "user" {
                    return Err(invalid("tool_result requires user role"));
                }
                if !block
                    .get("tool_use_id")
                    .and_then(|value| value.as_str())
                    .is_some_and(|id| !id.trim().is_empty())
                {
                    return Err(invalid("tool_use_id must be a nonempty string"));
                }
            }
        }
    }
    Ok(())
}

// Reject contradictions rather than guessing which constraint the caller meant.
// Traverse schema positions only; enum/default/example objects are data.
fn validate_schema_constraints(
    schema: &serde_json::Value,
    path: &str,
) -> Result<(), ConversionError> {
    use serde_json::Value;
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    let matches_type = |value: &Value| {
        let matches = |kind: &str| match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            "number" => value.is_number(),
            "integer" => {
                value.as_i64().is_some()
                    || value.as_u64().is_some()
                    || value.as_f64().is_some_and(|n| n.fract() == 0.0)
            }
            _ => true,
        };
        match object.get("type") {
            Some(Value::String(kind)) => matches(kind),
            Some(Value::Array(kinds)) => kinds.iter().any(|k| k.as_str().is_some_and(matches)),
            _ => true,
        }
    };
    if object
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| !values.iter().any(matches_type))
        || object
            .get("const")
            .is_some_and(|value| !matches_type(value))
    {
        return Err(ConversionError::InvalidToolFormat(format!(
            "{path}: type contradicts enum/const"
        )));
    }
    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get(key).and_then(Value::as_object) {
            for (name, child) in children {
                validate_schema_constraints(child, &format!("{path}.{key}.{name}"))?;
            }
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if let Some(child) = object.get(key) {
            validate_schema_constraints(child, &format!("{path}.{key}"))?;
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get(key).and_then(Value::as_array) {
            for (index, child) in children.iter().enumerate() {
                validate_schema_constraints(child, &format!("{path}.{key}[{index}]"))?;
            }
        }
    }
    Ok(())
}

/// 将 Anthropic 请求转换为 Kiro 请求
pub fn convert_request(req: &MessagesRequest) -> Result<ConversionResult, ConversionError> {
    // 1. 映射模型
    let model_id = map_model(&req.model)
        .ok_or_else(|| ConversionError::UnsupportedModel(req.model.clone()))?;

    // 2. 检查消息列表
    if req.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    // 2.5. 预处理 prefill：如果末尾是 assistant，静默丢弃并截断到最后一条 user
    // Claude 4.x 已弃用 assistant prefill，Kiro API 也不支持
    let messages: &[_] = if req.messages.last().is_some_and(|m| m.role != "user") {
        tracing::info!("检测到末尾 assistant 消息（prefill），静默丢弃");
        let last_user_idx = req
            .messages
            .iter()
            .rposition(|m| m.role == "user")
            .ok_or(ConversionError::EmptyMessages)?;
        &req.messages[..=last_user_idx]
    } else {
        &req.messages
    };

    validate_tool_blocks(messages)?;
    if let Some(tools) = &req.tools {
        for (index, tool) in tools.iter().enumerate() {
            if let Some(kind) = tool.tool_type.as_deref().filter(|kind| *kind != "custom") {
                let hint = "client tools must omit type (or use custom) and supply input_schema; native server tools must use the server execution route";
                return Err(ConversionError::InvalidToolFormat(format!(
                    "tools[{index}].type {kind:?} (name={:?}) is not supported as a client tool; {hint}",
                    tool.name
                )));
            }
        }
        let mut names = HashMap::new();
        for (index, tool) in tools.iter().enumerate() {
            validate_schema_constraints(
                &serde_json::json!(tool.input_schema),
                &format!("tools[{index}].input_schema"),
            )?;
            validate_kiro_tool_schema(
                &serde_json::json!(tool.input_schema),
                &format!("tools[{index}].input_schema"),
            )?;
            let mut definition = serde_json::to_value(tool).expect("tool definition is JSON");
            definition["name"] = serde_json::json!(tool.name.to_lowercase());
            let conflict = names
                .insert(tool.name.to_lowercase(), definition.clone())
                .is_some_and(|previous| previous != definition);
            if tool.name.trim().is_empty() || conflict {
                return Err(ConversionError::InvalidToolFormat(format!(
                    "tools[{index}].name is empty or has conflicting definitions (case-insensitive)"
                )));
            }
        }
    }

    // 3. 生成会话 ID 和代理 ID
    // 优先从 metadata.user_id 中提取 session UUID 作为 conversationId
    let conversation_id = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .and_then(|user_id| extract_session_id(user_id))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let agent_continuation_id = Uuid::new_v4().to_string();

    // 4. 确定触发类型
    let chat_trigger_type = determine_chat_trigger_type(req);

    // 5. 将末尾连续的 user 合并为当前轮次，避免拆开同一批工具结果。
    let current_start = messages
        .iter()
        .rposition(|message| message.role != "user")
        .map_or(0, |index| index + 1);
    let current_messages: Vec<_> = messages[current_start..].iter().collect();
    let merged_current = merge_user_messages(&current_messages, &model_id)?.user_input_message;
    let text_content = merged_current.content;
    let images = merged_current.images;
    let mut tool_results = merged_current.user_input_message_context.tool_results;

    // 6. 转换工具定义（超长名称自动缩短并记录映射）
    let mut tool_name_map = HashMap::new();
    // Reserve every existing legal name before allocating aliases, including
    // history-only tools, so a shortened/sanitized alias cannot steal one.
    let mut reserve = |name: &str| {
        if valid_tool_name(name)
            && !tool_name_map
                .keys()
                .any(|key: &String| key.eq_ignore_ascii_case(name))
        {
            tool_name_map.insert(name.to_owned(), name.to_owned());
        }
    };
    for tool in req.tools.iter().flatten() {
        reserve(&tool.name);
    }
    for message in messages {
        if let Some(blocks) = message.content.as_array() {
            for block in blocks {
                if block["type"] == "tool_use" {
                    if let Some(name) = block["name"].as_str() {
                        reserve(name);
                    }
                }
            }
        }
    }
    let mut tools = convert_tools(&req.tools, &mut tool_name_map);

    // 7. 构建历史消息（需要先构建，以便收集历史中使用的工具）
    let mut history = build_history(
        req,
        &messages[..=current_start],
        &model_id,
        &mut tool_name_map,
    )?;
    normalize_tool_use_ids(&mut history, &mut tool_results)?;
    sanitize_history_tool_results(&mut history);

    // 8. 验证并过滤 tool_use/tool_result 配对
    // 移除孤立的 tool_result（没有对应的 tool_use）
    // 同时返回孤立的 tool_use_id 集合，用于后续清理
    let (validated_tool_results, orphaned_tool_use_ids) =
        validate_tool_pairing(&history, &tool_results);

    // 9. 从历史中移除孤立的 tool_use（Kiro API 要求 tool_use 必须有对应的 tool_result）
    remove_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);

    // 10. 收集历史中使用的工具名称，为缺失的工具生成占位符定义
    // Kiro API 要求：历史消息中引用的工具必须在 tools 列表中有定义
    // 注意：Kiro 匹配工具名称时忽略大小写，所以这里也需要忽略大小写比较
    let history_tool_names = collect_history_tool_names(&history);
    let mut existing_tool_names: std::collections::HashSet<_> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_lowercase())
        .collect();

    for tool_name in history_tool_names {
        if existing_tool_names.insert(tool_name.to_lowercase()) {
            tools.push(create_placeholder_tool(&tool_name));
        }
    }

    // 11. 构建 UserInputMessageContext
    let mut context = UserInputMessageContext::new();
    if !tools.is_empty() {
        context = context.with_tools(tools);
    }
    if !validated_tool_results.is_empty() {
        context = context.with_tool_results(validated_tool_results);
    }

    // 12. 构建当前消息
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let content = text_content;

    let mut user_input = UserInputMessage::new(content, &model_id)
        .with_context(context)
        .with_origin("AI_EDITOR");

    if !images.is_empty() {
        user_input = user_input.with_images(images);
    }

    let current_message = CurrentMessage::new(user_input);

    // 13. 构建 ConversationState
    let conversation_state = ConversationState::new(conversation_id)
        .with_agent_continuation_id(agent_continuation_id)
        .with_agent_task_type("vibe")
        .with_chat_trigger_type(chat_trigger_type)
        .with_current_message(current_message)
        .with_history(history);

    tool_name_map.retain(|alias, original| alias != original);
    if !tool_name_map.is_empty() {
        tracing::info!("工具名称映射: {} 个超长名称已缩短", tool_name_map.len());
    }

    Ok(ConversionResult {
        conversation_state,
        tool_name_map,
    })
}

/// 确定聊天触发类型
/// "AUTO" 模式可能会导致 400 Bad Request 错误
fn determine_chat_trigger_type(_req: &MessagesRequest) -> String {
    "MANUAL".to_string()
}

/// 处理消息内容，提取文本、图片和工具结果
fn process_message_content(
    content: &serde_json::Value,
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    match content {
        serde_json::Value::String(s) => {
            text_parts.push(s.clone());
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "text" => {
                            if let Some(text) = block.text {
                                text_parts.push(text);
                            }
                        }
                        "image" => {
                            if let Some(source) = block.source
                                && let Some(format) = get_image_format(&source.media_type)
                            {
                                images.push(KiroImage::from_base64(format, source.data));
                            }
                        }
                        "tool_result" => {
                            if let Some(tool_use_id) = block.tool_use_id {
                                // tool_result 内容中可能夹带 image 块（例如读取图片文件的工具），
                                // 需一并提取，否则图片会被静默丢弃
                                let (result_content, result_images) =
                                    extract_tool_result_content(&block.content);
                                images.extend(result_images);
                                let is_error = block.is_error.unwrap_or(false);

                                let mut result = if is_error {
                                    ToolResult::error(&tool_use_id, result_content)
                                } else {
                                    ToolResult::success(&tool_use_id, result_content)
                                };
                                result.status =
                                    Some(if is_error { "error" } else { "success" }.to_string());

                                tool_results.push(result);
                            }
                        }
                        "tool_use" => {
                            // tool_use 在 assistant 消息中处理，这里忽略
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    Ok((text_parts.join("\n"), images, tool_results))
}

/// 从 media_type 获取图片格式
fn get_image_format(media_type: &str) -> Option<String> {
    match media_type {
        "image/jpeg" => Some("jpeg".to_string()),
        "image/png" => Some("png".to_string()),
        "image/gif" => Some("gif".to_string()),
        "image/webp" => Some("webp".to_string()),
        _ => None,
    }
}

/// 提取工具结果内容
///
/// 返回 (文本内容, 图片列表)。
///
/// tool_result 的 content 数组里除 text 块外，还可能包含 image 块——
/// 例如读取图片文件、渲染 PDF 页、浏览器截图这类工具的返回。Kiro 的
/// ToolResult 结构只承载文本，故此处把 image 块单独提出来，由调用方
/// 合并进 UserInputMessage.images 一起上送，避免图片被静默丢弃。
fn extract_tool_result_content(content: &Option<serde_json::Value>) -> (String, Vec<KiroImage>) {
    let mut images = Vec::new();
    let text = match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                    continue;
                }
                // 形如 {"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}
                if item.get("type").and_then(|v| v.as_str()) == Some("image")
                    && let Some(source) = item.get("source")
                {
                    let media_type = source
                        .get("media_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let data = source.get("data").and_then(|v| v.as_str());
                    if let (Some(format), Some(data)) = (get_image_format(media_type), data) {
                        images.push(KiroImage::from_base64(format, data));
                    }
                }
            }
            parts.join("\n")
        }
        Some(v) => v.to_string(),
        None => String::new(),
    };
    (text, images)
}

/// Keep IDs unique on the wire without rejecting clients that reuse IDs across turns.
/// Identical calls in a merged assistant turn are replays; conflicting calls are ambiguous.
fn normalize_tool_use_ids(
    history: &mut [Message],
    current_results: &mut [ToolResult],
) -> Result<(), ConversionError> {
    let mut reserved = std::collections::HashSet::new();
    for message in history.iter() {
        match message {
            Message::Assistant(message) => {
                if let Some(calls) = &message.assistant_response_message.tool_uses {
                    reserved.extend(calls.iter().map(|call| call.tool_use_id.clone()));
                }
            }
            Message::User(message) => reserved.extend(
                message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                    .iter()
                    .map(|result| result.tool_use_id.clone()),
            ),
        }
    }
    reserved.extend(
        current_results
            .iter()
            .map(|result| result.tool_use_id.clone()),
    );
    let mut seen = std::collections::HashSet::new();
    let mut remap = HashMap::new();
    for (index, message) in history.iter_mut().enumerate() {
        match message {
            Message::Assistant(message) => {
                remap.clear();
                let Some(calls) = &mut message.assistant_response_message.tool_uses else {
                    continue;
                };
                let mut unique: Vec<ToolUseEntry> = Vec::new();
                for call in std::mem::take(calls) {
                    if let Some(previous) = unique
                        .iter()
                        .find(|previous| previous.tool_use_id == call.tool_use_id)
                    {
                        if previous.name != call.name || previous.input != call.input {
                            return Err(ConversionError::InvalidToolFormat(format!(
                                "history[{index}]: conflicting tool_use calls share an id in the same assistant turn"
                            )));
                        }
                        continue;
                    }
                    unique.push(call);
                }
                for call in &mut unique {
                    if !seen.insert(call.tool_use_id.clone()) {
                        // Stable across retries, and disjoint from every client-provided ID.
                        let mut salt = 0usize;
                        let replacement = loop {
                            let digest = Sha256::digest(
                                format!("{}:{index}:{salt}", call.tool_use_id).as_bytes(),
                            );
                            let candidate = format!("toolu_{}", &hex::encode(digest)[..32]);
                            if reserved.insert(candidate.clone()) {
                                break candidate;
                            }
                            salt += 1;
                        };
                        remap.insert(call.tool_use_id.clone(), replacement.clone());
                        call.tool_use_id = replacement;
                    }
                }
                *calls = unique;
            }
            Message::User(message) => {
                for result in &mut message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    if let Some(id) = remap.get(&result.tool_use_id) {
                        result.tool_use_id = id.clone();
                    }
                }
                remap.clear();
            }
        }
    }
    for result in current_results {
        if let Some(id) = remap.get(&result.tool_use_id) {
            result.tool_use_id = id.clone();
        }
    }
    Ok(())
}

/// 历史结果只能消费紧邻的 assistant 调用一次，不能与其他轮次或未来的调用配对。
fn sanitize_history_tool_results(history: &mut [Message]) {
    let mut pending = std::collections::HashSet::new();
    for message in history {
        match message {
            Message::Assistant(message) => {
                pending.clear();
                if let Some(uses) = &message.assistant_response_message.tool_uses {
                    pending.extend(uses.iter().map(|call| call.tool_use_id.clone()));
                }
            }
            Message::User(message) => {
                message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                    .retain(|result| {
                        let valid = pending.remove(&result.tool_use_id);
                        if !valid {
                            tracing::warn!(
                                tool_use_id = %result.tool_use_id,
                                "跳过历史中孤立、重复或跨轮次的 tool_result"
                            );
                        }
                        valid
                    });
                pending.clear();
            }
        }
    }
}

/// 验证并过滤 tool_use/tool_result 配对
///
/// 收集所有 tool_use_id，验证 tool_result 是否匹配
/// 静默跳过孤立的 tool_use 和 tool_result，输出警告日志
///
/// # Arguments
/// * `history` - 历史消息引用
/// * `tool_results` - 当前消息中的 tool_result 列表
///
/// # Returns
/// 元组：(经过验证和过滤后的 tool_result 列表, 孤立的 tool_use_id 集合)
fn validate_tool_pairing(
    history: &[Message],
    tool_results: &[ToolResult],
) -> (Vec<ToolResult>, std::collections::HashSet<String>) {
    use std::collections::HashSet;

    // 1. 收集所有历史中的 tool_use_id
    let mut all_tool_use_ids: HashSet<String> = HashSet::new();
    // 2. 收集历史中已经有 tool_result 的 tool_use_id
    let mut history_tool_result_ids: HashSet<String> = HashSet::new();

    for msg in history {
        match msg {
            Message::Assistant(assistant_msg) => {
                if let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        all_tool_use_ids.insert(tool_use.tool_use_id.clone());
                    }
                }
            }
            Message::User(user_msg) => {
                // 收集历史 user 消息中的 tool_results
                for result in &user_msg
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    history_tool_result_ids.insert(result.tool_use_id.clone());
                }
            }
        }
    }

    // 3. 计算真正未配对的 tool_use_ids（排除历史中已配对的）
    let mut unpaired_tool_use_ids: HashSet<String> = all_tool_use_ids
        .difference(&history_tool_result_ids)
        .cloned()
        .collect();

    // 4. 过滤并验证当前消息的 tool_results
    let mut filtered_results = Vec::new();
    let current_tool_use_ids: HashSet<&str> = match history.last() {
        Some(Message::Assistant(message)) => message
            .assistant_response_message
            .tool_uses
            .iter()
            .flatten()
            .map(|call| call.tool_use_id.as_str())
            .collect(),
        _ => HashSet::new(),
    };

    for result in tool_results {
        if unpaired_tool_use_ids.contains(&result.tool_use_id)
            && current_tool_use_ids.contains(result.tool_use_id.as_str())
        {
            // 配对成功
            filtered_results.push(result.clone());
            unpaired_tool_use_ids.remove(&result.tool_use_id);
        } else if all_tool_use_ids.contains(&result.tool_use_id) {
            // tool_use 存在但已经在历史中配对过了，这是重复的 tool_result
            tracing::warn!(
                "跳过重复或跨轮次的 tool_result，tool_use_id={}",
                result.tool_use_id
            );
        } else {
            // 孤立 tool_result - 找不到对应的 tool_use
            tracing::warn!(
                "跳过孤立的 tool_result：找不到对应的 tool_use，tool_use_id={}",
                result.tool_use_id
            );
        }
    }

    // 5. 检测真正孤立的 tool_use（有 tool_use 但在历史和当前消息中都没有 tool_result）
    for orphaned_id in &unpaired_tool_use_ids {
        tracing::warn!(
            "检测到孤立的 tool_use：找不到对应的 tool_result，将从历史中移除，tool_use_id={}",
            orphaned_id
        );
    }

    (filtered_results, unpaired_tool_use_ids)
}

/// 从历史消息中移除孤立的 tool_use
///
/// Kiro API 要求每个 tool_use 必须有对应的 tool_result，否则返回 400 Bad Request。
/// 此函数遍历历史中的 assistant 消息，移除没有对应 tool_result 的 tool_use。
///
/// # Arguments
/// * `history` - 可变的历史消息列表
/// * `orphaned_ids` - 需要移除的孤立 tool_use_id 集合
fn remove_orphaned_tool_uses(
    history: &mut [Message],
    orphaned_ids: &std::collections::HashSet<String>,
) {
    if orphaned_ids.is_empty() {
        return;
    }

    for msg in history.iter_mut() {
        if let Message::Assistant(assistant_msg) = msg
            && let Some(ref mut tool_uses) = assistant_msg.assistant_response_message.tool_uses
        {
            let original_len = tool_uses.len();
            tool_uses.retain(|tu| !orphaned_ids.contains(&tu.tool_use_id));

            // 如果移除后为空，设置为 None
            if tool_uses.is_empty() {
                assistant_msg.assistant_response_message.tool_uses = None;
            } else if tool_uses.len() != original_len {
                tracing::debug!(
                    "从 assistant 消息中移除了 {} 个孤立的 tool_use",
                    original_len - tool_uses.len()
                );
            }
        }
    }
}

/// Kiro API 工具名称最大长度限制
const TOOL_NAME_MAX_LEN: usize = 63;

/// 生成确定性短名称：截断前缀 + "_" + 8 位 SHA256 hex
fn shorten_tool_name(name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    let hash_hex = format!("{:x}", hasher.finalize());
    let hash_suffix = &hash_hex[..8];
    // 54 prefix + 1 underscore + 8 hash = 63
    let prefix_max = TOOL_NAME_MAX_LEN - 1 - 8;
    let mut prefix_end = name.len().min(prefix_max);
    while !name.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let prefix = &name[..prefix_end];
    format!("{}_{}", prefix, hash_suffix)
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= TOOL_NAME_MAX_LEN
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// One reversible allocation shared by definitions and history. Identity entries
/// reserve names during conversion and are removed before returning the map.
fn map_tool_name(name: &str, tool_name_map: &mut HashMap<String, String>) -> String {
    if let Some((alias, _)) = tool_name_map
        .iter()
        .find(|(_, original)| original.eq_ignore_ascii_case(name))
    {
        return alias.clone();
    }
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut alias = if clean.len() > TOOL_NAME_MAX_LEN {
        shorten_tool_name(&clean)
    } else {
        clean
    };
    if alias.is_empty() {
        alias = "tool".into();
    }
    let base = alias.clone();
    let mut attempt = 0u64;
    while tool_name_map
        .keys()
        .any(|key| key.eq_ignore_ascii_case(&alias))
    {
        let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
        let suffix = format!("_{}_{attempt}", &digest[..8]);
        alias = format!(
            "{}{}",
            &base[..base.len().min(TOOL_NAME_MAX_LEN - suffix.len())],
            suffix
        );
        attempt += 1;
    }
    if alias != name {
        tool_name_map.insert(alias.clone(), name.to_owned());
    }
    alias
}

/// 转换工具定义
fn convert_tools(
    tools: &Option<Vec<super::types::Tool>>,
    tool_name_map: &mut HashMap<String, String>,
) -> Vec<Tool> {
    let Some(tools) = tools else {
        return Vec::new();
    };

    let mut names = std::collections::HashSet::new();
    tools
        .iter()
        .filter(|tool| names.insert(tool.name.to_lowercase()))
        .map(|t| {
            let mut description = t.description.clone();

            let suffix = chunked_tool_suffix(&t.name);
            // Reserve space for the policy so long descriptions cannot erase it.
            let description_limit =
                10000 - suffix.chars().count() - usize::from(!suffix.is_empty());
            if let Some((idx, _)) = description.char_indices().nth(description_limit) {
                description.truncate(idx);
            }
            if !suffix.is_empty() {
                description.push('\n');
                description.push_str(suffix);
            }

            // 限制描述长度为 10000 字符（安全截断 UTF-8，单次遍历）
            let description = match description.char_indices().nth(10000) {
                Some((idx, _)) => description[..idx].to_string(),
                None => description,
            };

            let mut schema = normalize_json_schema(serde_json::json!(t.input_schema));
            normalize_kiro_tool_schema(&mut schema);

            Tool {
                tool_specification: ToolSpecification {
                    name: map_tool_name(&t.name, tool_name_map),
                    description,
                    input_schema: InputSchema::from_json(schema),
                },
            }
        })
        .collect()
}

/// 构建历史消息
///
/// # Arguments
/// * `req` - 原始请求，用于读取 `system`、`thinking` 等配置字段
/// * `messages` - 经过 prefill 预处理的消息切片，末尾必定是 user 消息。
///   注意：该切片与 `req.messages` 可能不同（prefill 时会截断末尾的 assistant 消息），
///   调用方应始终使用此参数而非 `req.messages`。
/// * `model_id` - 已映射的 Kiro 模型 ID
fn build_history(
    req: &MessagesRequest,
    messages: &[super::types::Message],
    model_id: &str,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();

    // 1. 处理系统消息
    let mut system_content: String = req
        .system
        .as_ref()
        .map(|system| {
            system
                .iter()
                .map(|s| s.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if req.tools.as_ref().is_some_and(|tools| !tools.is_empty()) {
        if !system_content.is_empty() {
            system_content.push('\n');
        }
        system_content.push_str(SYSTEM_CHUNKED_POLICY);
        // This is a planning hint, not a claim that Kiro accepts max_tokens.
        // Bound the whole response, since many small calls can also overflow it.
        system_content.push_str(&format!(
            " Aim to finish this entire response within {} output tokens, including reasoning and ALL tool arguments combined. Reserve room to close every JSON object. When a task needs more, return one small complete tool call and continue on the next client turn after its result; do not batch many chunks into one response.",
            req.max_tokens.clamp(1, 4096)
        ));
    }
    if !system_content.is_empty() {
        // 系统消息作为 user + assistant 配对
        let user_msg = HistoryUserMessage::new(system_content, model_id);
        history.push(Message::User(user_msg));

        let assistant_msg = HistoryAssistantMessage::new("I will follow these instructions.");
        history.push(Message::Assistant(assistant_msg));
    }

    // 2. 处理常规消息历史
    // 最后一条消息作为 currentMessage，不加入历史
    // 经过 prefill 预处理后，messages 末尾必定是 user，故直接截掉最后一条即可
    let history_end_index = messages.len().saturating_sub(1);

    // 收集并配对消息
    let mut user_buffer: Vec<&super::types::Message> = Vec::new();
    let mut assistant_buffer: Vec<&super::types::Message> = Vec::new();

    for msg in messages.iter().take(history_end_index) {
        if msg.role == "user" {
            // 先处理累积的 assistant 消息
            if !assistant_buffer.is_empty() {
                let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
                history.push(Message::Assistant(merged));
                assistant_buffer.clear();
            }
            user_buffer.push(msg);
        } else if msg.role == "assistant" {
            // 先处理累积的 user 消息
            if !user_buffer.is_empty() {
                let merged_user = merge_user_messages(&user_buffer, model_id)?;
                history.push(Message::User(merged_user));
                user_buffer.clear();
            }
            // 累积 assistant 消息（支持连续多条）
            assistant_buffer.push(msg);
        }
    }

    // 处理末尾累积的 assistant 消息
    if !assistant_buffer.is_empty() {
        let merged = merge_assistant_messages(&assistant_buffer, tool_name_map)?;
        history.push(Message::Assistant(merged));
    }

    // 处理结尾的孤立 user 消息
    if !user_buffer.is_empty() {
        let merged_user = merge_user_messages(&user_buffer, model_id)?;
        history.push(Message::User(merged_user));

        // 自动配对一个 "OK" 的 assistant 响应
        let auto_assistant = HistoryAssistantMessage::new("OK");
        history.push(Message::Assistant(auto_assistant));
    }

    Ok(history)
}

/// 合并多个 user 消息
fn merge_user_messages(
    messages: &[&super::types::Message],
    model_id: &str,
) -> Result<HistoryUserMessage, ConversionError> {
    let mut content_parts = Vec::new();
    let mut all_images = Vec::new();
    let mut all_tool_results = Vec::new();

    for msg in messages {
        let (text, images, tool_results) = process_message_content(&msg.content)?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        all_images.extend(images);
        all_tool_results.extend(tool_results);
    }

    let content = content_parts.join("\n");
    // 保留文本内容，即使有工具结果也不丢弃用户文本
    let mut user_msg = UserMessage::new(&content, model_id);

    if !all_images.is_empty() {
        user_msg = user_msg.with_images(all_images);
    }

    if !all_tool_results.is_empty() {
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(all_tool_results);
        user_msg = user_msg.with_context(ctx);
    }

    Ok(HistoryUserMessage {
        user_input_message: user_msg,
    })
}

/// 转换 assistant 消息
fn convert_assistant_message(
    msg: &super::types::Message,
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    let mut reasoning_content = None;
    let mut text_content = String::new();
    let mut tool_uses = Vec::new();

    match &msg.content {
        serde_json::Value::String(s) => {
            text_content = s.clone();
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Ok(block) = serde_json::from_value::<ContentBlock>(item.clone()) {
                    match block.block_type.as_str() {
                        "thinking" => {
                            let text = block.thinking.unwrap_or_default();
                            if !text.is_empty()
                                && let Some(signature) =
                                    block.signature.filter(|s| !s.trim().is_empty())
                            {
                                // Kiro can carry one reasoning block. Keep its
                                // text and signature together, as when merging
                                // consecutive assistant messages below.
                                reasoning_content = Some(serde_json::json!({
                                    "reasoningText": {"text": text, "signature": signature}
                                }));
                            }
                        }
                        "redacted_thinking" => {
                            if let Some(data) = block.data.filter(|s| !s.trim().is_empty()) {
                                reasoning_content =
                                    Some(serde_json::json!({"redactedContent": data}));
                            }
                        }
                        "text" => {
                            if let Some(text) = block.text {
                                text_content.push_str(&text);
                            }
                        }
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (block.id, block.name) {
                                let input = block.input.unwrap_or(serde_json::json!({}));
                                let mapped_name = map_tool_name(&name, tool_name_map);
                                tool_uses
                                    .push(ToolUseEntry::new(id, mapped_name).with_input(input));
                            }
                        }
                        "server_tool_use" | "web_search_tool_result" => {
                            // Server tools are already executed. Preserve their context on
                            // client continuation without creating pending client tool calls.
                            text_content.push_str(&item.to_string());
                            text_content.push('\n');
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }

    // Kiro stores native reasoning separately from assistant text.
    let final_content = if text_content.is_empty() && !tool_uses.is_empty() {
        " ".to_string()
    } else {
        text_content
    };

    let mut assistant = AssistantMessage::new(final_content);
    assistant.reasoning_content = reasoning_content;
    if !tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(tool_uses);
    }

    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

/// 合并多个连续的 assistant 消息为一条
/// 用于处理网络不稳定时产生的连续 assistant 消息（Issue #79）
fn merge_assistant_messages(
    messages: &[&super::types::Message],
    tool_name_map: &mut HashMap<String, String>,
) -> Result<HistoryAssistantMessage, ConversionError> {
    assert!(!messages.is_empty());
    if messages.len() == 1 {
        return convert_assistant_message(messages[0], tool_name_map);
    }

    let mut all_tool_uses: Vec<ToolUseEntry> = Vec::new();
    let mut content_parts: Vec<String> = Vec::new();
    let mut reasoning_content = None;

    for msg in messages {
        let converted = convert_assistant_message(msg, tool_name_map)?;
        let am = converted.assistant_response_message;
        if !am.content.trim().is_empty() {
            content_parts.push(am.content);
        }
        if let Some(tus) = am.tool_uses {
            all_tool_uses.extend(tus);
        }
        if am.reasoning_content.is_some() {
            // A signature authenticates one reasoning block; never join signed blocks.
            reasoning_content = am.reasoning_content;
        }
    }

    let content = if content_parts.is_empty() && !all_tool_uses.is_empty() {
        " ".to_string()
    } else {
        content_parts.join("\n\n")
    };

    let mut assistant = AssistantMessage::new(content);
    assistant.reasoning_content = reasoning_content;
    if !all_tool_uses.is_empty() {
        assistant = assistant.with_tool_uses(all_tool_uses);
    }
    Ok(HistoryAssistantMessage {
        assistant_response_message: assistant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_models_map_exactly_and_use_their_own_controls() {
        for id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "kimi-k3"] {
            assert_eq!(map_model(&id.to_uppercase()).as_deref(), Some(id));
            assert_eq!(map_model(&format!("{id}-thinking")), None);
            assert_eq!(get_context_window_size(id), 1_000_000);
            assert!(!supports_adaptive_thinking(id));
            let mut request: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":id,"max_tokens":4096,"messages":[{"role":"user","content":"hello"}],
                "output_config":{"effort":"max"}
            }))
            .unwrap();
            let fields = additional_model_request_fields(&request).unwrap();
            assert_eq!(fields["max_tokens"], 4096);
            assert!(fields.get("thinking").is_none());
            assert!(fields.get("output_config").is_none());
            if is_gpt(id) {
                assert_eq!(fields["reasoning"]["effort"], "max");
                request.output_config = None;
                request
                    .extra_controls
                    .insert("reasoning".into(), serde_json::json!({"effort":"none"}));
                assert_eq!(
                    additional_model_request_fields(&request).unwrap()["reasoning"]["effort"],
                    "none"
                );
            } else {
                assert_eq!(fields, serde_json::json!({"max_tokens":4096}));
            }
        }
        for id in ["gpt-5.6-unknown", "gpt-5.6-sol-extra", "kimi-k4"] {
            assert_eq!(map_model(id), None);
        }
    }

    #[test]
    fn test_map_model_sonnet() {
        for (model, expected) in [
            ("claude-sonnet-4-5-20250929", "claude-sonnet-4.5"),
            ("claude-sonnet-4.5", "claude-sonnet-4.5"),
            ("claude-sonnet-4-6", "claude-sonnet-4.6"),
            ("claude-sonnet-4.6", "claude-sonnet-4.6"),
        ] {
            assert_eq!(map_model(model), Some(expected.to_string()), "{model}");
        }
    }

    #[test]
    fn test_map_model_opus() {
        for (model, expected) in [
            ("claude-opus-4-5-20251101", "claude-opus-4.5"),
            ("claude-opus-4.5", "claude-opus-4.5"),
            ("claude-opus-4-6", "claude-opus-4.6"),
            ("claude-opus-4.6", "claude-opus-4.6"),
            ("claude-opus-4-7", "claude-opus-4.7"),
            ("claude-opus-4.7", "claude-opus-4.7"),
        ] {
            assert_eq!(map_model(model), Some(expected.to_string()), "{model}");
        }
    }

    #[test]
    fn test_map_model_haiku() {
        assert!(
            map_model("claude-haiku-4-20250514")
                .unwrap()
                .contains("haiku")
        );
    }

    #[test]
    fn test_map_model_unsupported() {
        for model in [
            "gpt-4",
            "claude-sonnet-4",
            "claude-sonnet-4-20250514",
            "claude-3-5-sonnet-20241022",
            "claude-opus-4",
            "claude-opus-4-20250514",
        ] {
            assert_eq!(map_model(model), None, "{model}");
        }
    }

    #[test]
    fn test_map_model_thinking_suffix_sonnet() {
        // thinking 后缀不应影响 sonnet 模型映射
        let result = map_model("claude-sonnet-4-5-20250929-thinking");
        assert_eq!(result, Some("claude-sonnet-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_5() {
        // thinking 后缀不应影响 opus 4.5 模型映射
        let result = map_model("claude-opus-4-5-20251101-thinking");
        assert_eq!(result, Some("claude-opus-4.5".to_string()));
    }

    #[test]
    fn test_map_model_thinking_suffix_opus_4_6() {
        // thinking 后缀不应影响 opus 4.6 模型映射
        let result = map_model("claude-opus-4-6-thinking");
        assert_eq!(result, Some("claude-opus-4.6".to_string()));
    }

    #[test]
    fn test_map_model_sonnet_5() {
        assert_eq!(
            map_model("claude-sonnet-5"),
            Some("claude-sonnet-5".to_string())
        );
        assert_eq!(
            map_model("claude-sonnet-5-thinking"),
            Some("claude-sonnet-5".to_string())
        );
        assert_eq!(get_context_window_size("claude-sonnet-5"), 1_000_000);
        // sonnet-4-5 不应误匹配为 sonnet-5
        assert_eq!(
            map_model("claude-sonnet-4-5-20250929"),
            Some("claude-sonnet-4.5".to_string())
        );
    }

    #[test]
    fn test_map_model_sonnet_5_5() {
        for name in [
            "claude-sonnet-5-5",
            "claude-sonnet-5.5",
            "claude-sonnet-5-5-thinking",
        ] {
            assert_eq!(map_model(name), Some("claude-sonnet-5.5".into()));
            assert_eq!(get_context_window_size(name), 1_000_000);
        }
    }

    #[test]
    fn test_map_model_fable_5_1() {
        // Anthropic 侧连字符形式（客户端实际发送的名字）
        assert_eq!(
            map_model("claude-fable-5-1"),
            Some("claude-fable-5.1".to_string())
        );
        // Kiro 侧点号形式
        assert_eq!(
            map_model("claude-fable-5.1"),
            Some("claude-fable-5.1".to_string())
        );
        // thinking 后缀不影响映射
        assert_eq!(
            map_model("claude-fable-5-1-thinking"),
            Some("claude-fable-5.1".to_string())
        );
        assert_eq!(get_context_window_size("claude-fable-5-1"), 1_000_000);
        // fable 分支不应影响其他系列
        assert_eq!(
            map_model("claude-opus-5"),
            Some("claude-opus-5".to_string())
        );
    }

    #[test]
    fn test_map_model_opus_5_5() {
        for name in [
            "claude-opus-5-5",
            "claude-opus-5.5",
            "claude-opus-5-5-thinking",
            "claude-opus-5.5-20260922",
        ] {
            assert_eq!(
                map_model(name),
                Some("claude-opus-5.5".to_string()),
                "{name} should map to claude-opus-5.5"
            );
        }
        assert_eq!(get_context_window_size("claude-opus-5-5"), 1_000_000);
        // opus-5 不应被 5.5 分支吞掉
        assert_eq!(
            map_model("claude-opus-5"),
            Some("claude-opus-5".to_string())
        );
    }

    #[test]
    fn test_map_model_opus_5() {
        assert_eq!(
            map_model("claude-opus-5"),
            Some("claude-opus-5".to_string())
        );
        assert_eq!(
            map_model("claude-opus-5-thinking"),
            Some("claude-opus-5".to_string())
        );
        assert_eq!(get_context_window_size("claude-opus-5"), 1_000_000);
        // opus-4-5 不应误匹配为 opus-5
        assert_eq!(
            map_model("claude-opus-4-5-20251101"),
            Some("claude-opus-4.5".to_string())
        );
    }

    #[test]
    fn test_map_model_opus_4_8() {
        assert_eq!(
            map_model("claude-opus-4-8"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(
            map_model("claude-opus-4-8-thinking"),
            Some("claude-opus-4.8".to_string())
        );
        assert_eq!(get_context_window_size("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn test_map_model_thinking_suffix_haiku() {
        // thinking 后缀不应影响 haiku 模型映射
        let result = map_model("claude-haiku-4-5-20251001-thinking");
        assert_eq!(result, Some("claude-haiku-4.5".to_string()));
    }

    #[test]
    fn test_determine_chat_trigger_type() {
        // 无工具时返回 MANUAL
        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };
        assert_eq!(determine_chat_trigger_type(&req), "MANUAL");
    }

    #[test]
    fn chunked_policy_survives_long_descriptions_and_missing_system() {
        for name in [
            "Write",
            "write_file",
            "mcp__files__create_file",
            "Edit",
            "apply_patch",
            "Bash",
            "exec_command",
        ] {
            for system in [
                serde_json::Value::Null,
                serde_json::json!(""),
                serde_json::json!("Original instructions"),
            ] {
                let req: MessagesRequest = serde_json::from_value(serde_json::json!({
                    "model":"claude-sonnet-5", "max_tokens":1024,
                    "messages":[{"role":"user","content":"hi"}], "system":system,
                    "tools":[{"name":name,"description":"长".repeat(11000),
                        "input_schema":{"type":"object","properties":{}}}]
                }))
                .unwrap();
                let converted = convert_request(&req).unwrap();
                let state = converted.conversation_state;
                let description = &state
                    .current_message
                    .user_input_message
                    .user_input_message_context
                    .tools[0]
                    .tool_specification
                    .description;
                assert!(description.chars().count() <= 10000);
                assert!(description.ends_with(chunked_tool_suffix(name)));
                let history = serde_json::to_string(&state.history).unwrap();
                assert!(history.contains(SYSTEM_CHUNKED_POLICY));
                if !system.is_null() && system != "" {
                    assert!(history.contains("Original instructions"));
                }
            }
        }
    }

    #[test]
    fn tool_response_budget_uses_client_limit_with_a_conservative_ceiling() {
        for (requested, budget) in [(256, 256), (4096, 4096), (64000, 4096)] {
            let req: MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"claude-sonnet-5", "max_tokens":requested,
                "messages":[{"role":"user","content":"hi"}],
                "tools":[{"name":"write_file", "input_schema":{"type":"object"}}]
            }))
            .unwrap();
            let converted = convert_request(&req).unwrap();
            let history = serde_json::to_string(&converted.conversation_state.history).unwrap();
            assert!(history.contains(&format!("within {budget} output tokens")));
            assert!(history.contains("ALL tool arguments combined"));
        }
    }

    #[test]
    fn requests_without_tools_do_not_receive_chunked_policy() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-5", "max_tokens":1024,
            "messages":[{"role":"user","content":"hi"}], "system":"Original instructions"
        }))
        .unwrap();
        let history =
            serde_json::to_string(&convert_request(&req).unwrap().conversation_state.history)
                .unwrap();
        assert!(history.contains("Original instructions"));
        assert!(!history.contains(SYSTEM_CHUNKED_POLICY));
    }

    #[test]
    fn native_model_controls_are_serialized_without_prompt_injection() {
        let req = MessagesRequest {
            model: "claude-sonnet-5-thinking".into(),
            max_tokens: 4096,
            messages: vec![super::super::types::Message {
                role: "user".into(),
                content: serde_json::json!("hi"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: Some(super::super::types::Thinking {
                thinking_type: "adaptive".into(),
                budget_tokens: 1000,
                display: Some("omitted".into()),
            }),
            output_config: Some(super::super::types::OutputConfig {
                effort: "xhigh".into(),
            }),
            metadata: None,
            compaction: None,
            context_management: None,
        };
        let converted = convert_request(&req).unwrap();
        let body = additional_model_request_fields(&req).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["thinking"]["display"], "omitted");
        assert!(
            !serde_json::to_string(&converted.conversation_state)
                .unwrap()
                .contains("thinking_mode")
        );
    }

    #[test]
    fn test_collect_history_tool_names() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 创建包含工具使用的历史消息
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
            ToolUseEntry::new("tool-2", "write")
                .with_input(serde_json::json!({"path": "/out.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_names = collect_history_tool_names(&history);
        assert_eq!(tool_names.len(), 2);
        assert!(tool_names.contains(&"read".to_string()));
        assert!(tool_names.contains(&"write".to_string()));
    }

    #[test]
    fn test_create_placeholder_tool() {
        let tool = create_placeholder_tool("my_custom_tool");

        assert_eq!(tool.tool_specification.name, "my_custom_tool");
        assert!(!tool.tool_specification.description.is_empty());

        // 验证 JSON 序列化正确
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"my_custom_tool\""));
    }

    #[test]
    fn test_shorten_tool_name_deterministic() {
        let long_name =
            "mcp__some_very_long_server_name__some_very_long_tool_name_that_exceeds_limit";
        assert!(long_name.len() > TOOL_NAME_MAX_LEN);

        let short1 = shorten_tool_name(long_name);
        let short2 = shorten_tool_name(long_name);
        assert_eq!(short1, short2, "相同输入应产生相同的短名称");
        assert!(
            short1.len() <= TOOL_NAME_MAX_LEN,
            "短名称长度应 <= 63，实际 {}",
            short1.len()
        );
    }

    #[test]
    fn test_shorten_tool_name_uniqueness() {
        let name_a = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_a";
        let name_b = "mcp__server_alpha__tool_name_that_is_very_long_and_exceeds_the_limit_b";
        let short_a = shorten_tool_name(name_a);
        let short_b = shorten_tool_name(name_b);
        assert_ne!(short_a, short_b, "不同输入应产生不同的短名称");
    }

    #[test]
    fn test_map_tool_name_short_passthrough() {
        let mut map = HashMap::new();
        let result = map_tool_name("short_name", &mut map);
        assert_eq!(result, "short_name");
        assert!(map.is_empty(), "短名称不应产生映射");
    }

    #[test]
    fn test_map_tool_name_long_creates_mapping() {
        let mut map = HashMap::new();
        let long_name = "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        let result = map_tool_name(long_name, &mut map);
        assert!(result.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(map.get(&result), Some(&long_name.to_string()));
    }

    #[test]
    fn test_tool_name_mapping_in_convert_request() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";
        assert!(long_tool_name.len() > TOOL_NAME_MAX_LEN);

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                options: Default::default(),
            }]),
            thinking: None,
            tool_choice: None,
            extra_controls: Default::default(),
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        let result = convert_request(&req).unwrap();

        // 应该有映射
        assert_eq!(result.tool_name_map.len(), 1);

        // 映射中的值应该是原始名称
        let (short, original) = result.tool_name_map.iter().next().unwrap();
        assert_eq!(original, long_tool_name);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);

        // Kiro 请求中的工具名应该是短名称
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;
        assert_eq!(tools[0].tool_specification.name, *short);
    }

    #[test]
    fn test_tool_name_mapping_in_history() {
        use super::super::types::{Message as AnthropicMessage, Tool as AnthropicTool};

        let long_tool_name =
            "mcp__plugin_very_long_server_name__extremely_long_tool_name_exceeds_63";

        let mut schema = std::collections::HashMap::new();
        schema.insert("type".to_string(), serde_json::json!("object"));
        schema.insert("properties".to_string(), serde_json::json!({}));

        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("use the tool"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "calling tool"},
                        {"type": "tool_use", "id": "toolu_01", "name": long_tool_name, "input": {}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01", "content": "done"}
                    ]),
                },
            ],
            system: None,
            stream: false,
            tools: Some(vec![AnthropicTool {
                name: long_tool_name.to_string(),
                description: "A test tool".to_string(),
                input_schema: schema,
                tool_type: None,
                max_uses: None,
                options: Default::default(),
            }]),
            thinking: None,
            tool_choice: None,
            extra_controls: Default::default(),
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        let result = convert_request(&req).unwrap();
        let short_name = result.tool_name_map.iter().next().unwrap().0.clone();

        // 历史中 assistant 消息的 tool_use name 也应该被映射
        let history = &result.conversation_state.history;
        let mut found = false;
        for msg in history {
            if let Message::Assistant(a) = msg
                && let Some(ref tool_uses) = a.assistant_response_message.tool_uses
            {
                for tu in tool_uses {
                    if tu.tool_use_id == "toolu_01" {
                        assert_eq!(tu.name, short_name, "历史中的 tool_use name 应该是短名称");
                        found = true;
                    }
                }
            }
        }
        assert!(found, "应该在历史中找到 tool_use");
    }

    #[test]
    fn test_history_tools_added_to_tools_list() {
        use super::super::types::Message as AnthropicMessage;

        // 创建一个请求，历史中有工具使用，但 tools 列表为空
        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "text", "text": "I'll read the file."},
                        {"type": "tool_use", "id": "tool-1", "name": "read", "input": {"path": "/test.txt"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "tool-1", "content": "file content"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None, // 没有提供工具定义
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        let result = convert_request(&req).unwrap();

        // 验证 tools 列表中包含了历史中使用的工具的占位符定义
        let tools = &result
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools;

        assert!(!tools.is_empty(), "tools 列表不应为空");
        assert!(
            tools.iter().any(|t| t.tool_specification.name == "read"),
            "tools 列表应包含 'read' 工具的占位符定义"
        );
    }

    #[test]
    fn test_extract_session_id_valid() {
        // 测试有效的 user_id 格式
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_8bb5523b-ec7c-4540-a9ca-beb6d79f1552";
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_format() {
        // 测试 JSON 格式的 user_id
        let user_id = r#"{"device_id":"0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd","account_uuid":"","session_id":"8bb5523b-ec7c-4540-a9ca-beb6d79f1552"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(
            session_id,
            Some("8bb5523b-ec7c-4540-a9ca-beb6d79f1552".to_string())
        );
    }

    #[test]
    fn test_extract_session_id_json_invalid_session() {
        // 测试 JSON 格式但 session_id 不是有效 UUID
        let user_id = r#"{"device_id":"abc","session_id":"not-a-uuid"}"#;
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_no_session() {
        // 测试没有 session 的 user_id
        let user_id = "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn test_extract_session_id_invalid_uuid() {
        // 测试无效的 UUID 格式
        let user_id = "user_xxx_session_invalid-uuid";
        let session_id = extract_session_id(user_id);
        assert_eq!(session_id, None);
    }

    #[test]
    fn session_metadata_rejects_unicode_and_malformed_uuids_without_panicking() {
        for candidate in [
            format!("{}🦀", "a".repeat(35)),
            "🦀".repeat(12),
            "zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz".into(),
            "12345678-1234-1234-123456789012-1234".into(),
            "short".into(),
        ] {
            assert_eq!(
                extract_session_id(&format!("user_session_{candidate}")),
                None
            );
            let metadata = serde_json::json!({"session_id":candidate}).to_string();
            assert_eq!(extract_session_id(&metadata), None);
        }
        let valid = "A0662283-7FD3-4399-A7EB-52B9A717AE88";
        assert_eq!(
            extract_session_id(&format!("用户_session_{valid}_后缀")),
            Some(valid.into())
        );
    }

    #[test]
    fn test_convert_request_with_session_metadata() {
        use super::super::types::{Message as AnthropicMessage, Metadata};

        // 测试带有 metadata 的请求，应该使用 session UUID 作为 conversationId
        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: Some(Metadata {
                user_id: Some(
                    "user_0dede55c6dcc4a11a30bbb5e7f22e6fdf86cdeba3820019cc27612af4e1243cd_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
                ),
            }),
            compaction: None,
            context_management: None,
        };

        let result = convert_request(&req).unwrap();
        assert_eq!(
            result.conversation_state.conversation_id,
            "a0662283-7fd3-4399-a7eb-52b9a717ae88"
        );
    }

    #[test]
    fn test_convert_request_without_metadata() {
        use super::super::types::Message as AnthropicMessage;

        // 测试没有 metadata 的请求，应该生成新的 UUID
        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::json!("Hello"),
            }],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        let result = convert_request(&req).unwrap();
        // 验证生成的是有效的 UUID 格式
        assert_eq!(result.conversation_state.conversation_id.len(), 36);
        assert_eq!(
            result
                .conversation_state
                .conversation_id
                .chars()
                .filter(|c| *c == '-')
                .count(),
            4
        );
    }

    fn tool_validation_request(messages: serde_json::Value) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "claude-opus-5-5", "max_tokens": 1024, "messages": messages
        }))
        .unwrap()
    }

    fn assert_wire_tool_pairs(state: &ConversationState) {
        let mut seen = std::collections::HashSet::new();
        let mut pending = std::collections::HashSet::new();
        for message in &state.history {
            match message {
                Message::Assistant(message) => {
                    assert!(pending.is_empty());
                    for call in message
                        .assistant_response_message
                        .tool_uses
                        .iter()
                        .flatten()
                    {
                        assert!(seen.insert(call.tool_use_id.clone()));
                        pending.insert(call.tool_use_id.clone());
                    }
                }
                Message::User(message) => {
                    for result in &message
                        .user_input_message
                        .user_input_message_context
                        .tool_results
                    {
                        assert!(pending.remove(&result.tool_use_id));
                    }
                    assert!(pending.is_empty());
                }
            }
        }
        for result in &state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results
        {
            assert!(pending.remove(&result.tool_use_id));
        }
        assert!(pending.is_empty());
        assert!(!seen.is_empty());
    }

    #[test]
    fn missing_and_null_input_preserve_empty_object_compatibility() {
        for call in [
            serde_json::json!({"type":"tool_use", "id":"a", "name":"read"}),
            serde_json::json!({"type":"tool_use", "id":"a", "name":"read", "input":null}),
            serde_json::json!({"type":"tool_use", "id":"a", "name":"read", "input":{}}),
        ] {
            let req = tool_validation_request(serde_json::json!([
                {"role":"assistant", "content":[call]},
                {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"a", "content":"ok"}]}
            ]));
            let state = convert_request(&req).unwrap().conversation_state;
            let wire = serde_json::to_value(&state).unwrap();
            assert_eq!(
                wire["history"][0]["assistantResponseMessage"]["toolUses"][0]["input"],
                serde_json::json!({})
            );
            assert_wire_tool_pairs(&state);
        }
    }

    #[test]
    fn conflicting_calls_in_same_turn_remain_rejected() {
        for second in [
            serde_json::json!({"type":"tool_use","id":"a","name":"write","input":{}}),
            serde_json::json!({"type":"tool_use","id":"a","name":"read","input":{"path":"other"}}),
        ] {
            let req = tool_validation_request(serde_json::json!([
                {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}}]},
                {"role":"assistant","content":[second]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"ok"}]}
            ]));
            assert!(
                convert_request(&req)
                    .unwrap_err()
                    .to_string()
                    .contains("conflicting tool_use")
            );
        }
    }

    #[test]
    fn malformed_tool_blocks_fail_locally_with_location() {
        let valid = serde_json::json!({"type":"tool_use", "id":"a", "name":"read", "input":{}});
        for (field, value) in [
            ("id", serde_json::json!("")),
            ("id", serde_json::json!(123)),
            ("name", serde_json::json!("  ")),
            ("input", serde_json::json!("{}")),
            ("input", serde_json::json!([])),
            ("input", serde_json::json!(false)),
        ] {
            let mut block = valid.clone();
            block[field] = value;
            let req = tool_validation_request(serde_json::json!([
                {"role":"assistant", "content":[block]}, {"role":"user", "content":"continue"}
            ]));
            let error = convert_request(&req).unwrap_err().to_string();
            assert!(error.contains("messages[0].content[0]"), "{error}");
        }
        for (role, block) in [
            ("user", valid),
            (
                "assistant",
                serde_json::json!({"type":"tool_result", "tool_use_id":"a"}),
            ),
            (
                "user",
                serde_json::json!({"type":"tool_result", "tool_use_id":""}),
            ),
        ] {
            let req = tool_validation_request(serde_json::json!([
                {"role":role,"content":[block]}, {"role":"user","content":"continue"}
            ]));
            assert!(matches!(
                convert_request(&req),
                Err(ConversionError::InvalidToolFormat(_))
            ));
        }
    }

    #[test]
    fn duplicate_tool_use_ids_are_normalized_within_and_across_turns() {
        let call = serde_json::json!({"type":"tool_use", "id":"a", "name":"read", "input":{}});
        let result = serde_json::json!({"type":"tool_result", "tool_use_id":"a", "content":"ok"});
        for messages in [
            serde_json::json!([
                {"role":"assistant", "content":[call, call]},
                {"role":"user", "content":[result]}
            ]),
            serde_json::json!([
                {"role":"assistant", "content":[call]},
                {"role":"assistant", "content":[call]},
                {"role":"user", "content":[result]}
            ]),
            serde_json::json!([
                {"role":"assistant", "content":[call]},
                {"role":"user", "content":[result]},
                {"role":"assistant", "content":[call]},
                {"role":"user", "content":[result]}
            ]),
        ] {
            let state = convert_request(&tool_validation_request(messages))
                .unwrap()
                .conversation_state;
            assert_wire_tool_pairs(&state);
        }
    }

    #[test]
    fn history_placeholder_names_are_unique_ignoring_case() {
        let req = tool_validation_request(serde_json::json!([
            {"role":"assistant", "content":[
                {"type":"tool_use", "id":"a", "name":"Read", "input":{}},
                {"type":"tool_use", "id":"b", "name":"read", "input":{}}
            ]},
            {"role":"user", "content":[
                {"type":"tool_result", "tool_use_id":"a", "content":"one"},
                {"type":"tool_result", "tool_use_id":"b", "content":"two"}
            ]}
        ]));
        let state = convert_request(&req).unwrap().conversation_state;
        let wire = serde_json::to_value(state).unwrap();
        let context = &wire["currentMessage"]["userInputMessage"]["userInputMessageContext"];
        assert_eq!(context["tools"].as_array().unwrap().len(), 1);
        assert_eq!(
            context["tools"][0]["toolSpecification"]["inputSchema"]["json"],
            serde_json::json!({
                "$schema":"http://json-schema.org/draft-07/schema#",
                "type":"object",
                "properties":{}
            })
        );
        assert_eq!(context["toolResults"].as_array().unwrap().len(), 2);
        let calls = wire["history"][0]["assistantResponseMessage"]["toolUses"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["toolUseId"], "a");
        assert!(calls.iter().all(|call| call["input"].is_object()));
    }

    #[test]
    fn explicit_tool_definitions_reject_empty_and_conflicting_names() {
        for names in [vec![""], vec!["Read", "read"]] {
            let mut req =
                tool_validation_request(serde_json::json!([{"role":"user","content":"hi"}]));
            req.tools = Some(
                names
                    .into_iter()
                    .enumerate()
                    .map(|(index, name)| {
                        serde_json::from_value(serde_json::json!({
                            "name":name, "description":format!("read {index}"), "input_schema":{"type":"object"}
                        }))
                        .unwrap()
                    })
                    .collect(),
            );
            assert!(matches!(
                convert_request(&req),
                Err(ConversionError::InvalidToolFormat(_))
            ));
        }
    }

    #[test]
    fn identical_tool_definitions_are_deduplicated() {
        let mut req = tool_validation_request(serde_json::json!([{"role":"user","content":"hi"}]));
        req.tools = Some(
            ["Read", "read"]
                .into_iter()
                .map(|name| {
                    serde_json::from_value(serde_json::json!({
                        "name":name,"description":"read","input_schema":{"type":"object"}
                    }))
                    .unwrap()
                })
                .collect(),
        );
        let state = convert_request(&req).unwrap().conversation_state;
        assert_eq!(
            state
                .current_message
                .user_input_message
                .user_input_message_context
                .tools
                .len(),
            1
        );
    }

    #[test]
    fn reused_ids_keep_each_turns_arguments_and_results() {
        for first_result in [true, false] {
            let mut messages = Vec::new();
            for index in 0..3 {
                messages.push(serde_json::json!({"role":"assistant","content":[
                    {"type":"tool_use","id":"a","name":"read","input":{"index":index}}
                ]}));
                messages.push(if index == 0 && !first_result {
                    serde_json::json!({"role":"user","content":"skip"})
                } else {
                    serde_json::json!({"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"a","content":format!("result {index}")}
                    ]})
                });
            }
            let req = tool_validation_request(serde_json::json!(messages));
            let state = convert_request(&req).unwrap().conversation_state;
            assert_wire_tool_pairs(&state);
            let retry = convert_request(&req).unwrap().conversation_state;
            assert_eq!(
                serde_json::to_value(&state.history).unwrap(),
                serde_json::to_value(&retry.history).unwrap()
            );
            let wire = serde_json::to_value(&state).unwrap();
            for index in 0..3 {
                let call = &wire["history"][index * 2]["assistantResponseMessage"]["toolUses"][0];
                if index == 0 && !first_result {
                    assert!(call.is_null());
                    continue;
                }
                assert_eq!(call["input"]["index"], index);
                let result = if index == 2 {
                    &wire["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"]
                        [0]
                } else {
                    &wire["history"][index * 2 + 1]["userInputMessage"]["userInputMessageContext"]["toolResults"]
                        [0]
                };
                assert_eq!(call["toolUseId"], result["toolUseId"]);
                assert_eq!(result["content"][0]["text"], format!("result {index}"));
            }
            assert_eq!(
                req.messages[4].content[0]["id"], "a",
                "client request must not be mutated"
            );
        }
    }

    #[test]
    fn remapped_ids_avoid_client_calls_and_orphan_results() {
        let call = serde_json::json!({"type":"tool_use","id":"a","name":"read","input":{}});
        let result = serde_json::json!({"type":"tool_result","tool_use_id":"a","content":"ok"});
        let messages = serde_json::json!([
            {"role":"assistant","content":[call]}, {"role":"user","content":[result]},
            {"role":"assistant","content":[call]}, {"role":"user","content":[result]}
        ]);
        let baseline = convert_request(&tool_validation_request(messages.clone()))
            .unwrap()
            .conversation_state;
        let wire = serde_json::to_value(baseline).unwrap();
        let candidate = wire["history"][2]["assistantResponseMessage"]["toolUses"][0]["toolUseId"]
            .as_str()
            .unwrap();
        for reserve_as_call in [false, true] {
            let mut messages = messages.clone();
            if reserve_as_call {
                messages.as_array_mut().unwrap().extend([
                    serde_json::json!({"role":"assistant","content":[{"type":"tool_use","id":candidate,"name":"read","input":{"path":"later"}}]}),
                    serde_json::json!({"role":"user","content":[{"type":"tool_result","tool_use_id":candidate,"content":"later result"}]}),
                ]);
            } else {
                messages[3]["content"].as_array_mut().unwrap().push(serde_json::json!(
                    {"type":"tool_result","tool_use_id":candidate,"content":"orphan must not match generated ID"}
                ));
            }
            let state = convert_request(&tool_validation_request(messages))
                .unwrap()
                .conversation_state;
            assert_wire_tool_pairs(&state);
            let wire = serde_json::to_value(state).unwrap();
            assert_ne!(
                wire["history"][2]["assistantResponseMessage"]["toolUses"][0]["toolUseId"],
                candidate
            );
            let current = &wire["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"];
            assert_eq!(current.as_array().unwrap().len(), 1);
            assert_eq!(
                current[0]["content"][0]["text"],
                if reserve_as_call {
                    "later result"
                } else {
                    "ok"
                }
            );
            if reserve_as_call {
                assert_eq!(current[0]["toolUseId"], candidate);
            }
        }
    }

    #[test]
    fn remapping_does_not_leak_past_a_result_turn() {
        let req = tool_validation_request(serde_json::json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"first"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"second"}]},
            {"role":"assistant","content":"finished"},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"stale"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"read","input":{}}]},
            {"role":"user","content":"skip this call"},
            {"role":"assistant","content":"continue"},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"too late"}]}
        ]));
        let state = convert_request(&req).unwrap().conversation_state;
        assert_wire_tool_pairs(&state);
        let wire = serde_json::to_value(state).unwrap();
        assert!(
            wire["history"][5]["userInputMessage"]["userInputMessageContext"]["toolResults"]
                .is_null()
        );
        assert!(wire["history"][6]["assistantResponseMessage"]["toolUses"].is_null());
        assert_eq!(
            wire["history"][7]["userInputMessage"]["content"],
            "skip this call"
        );
        assert!(
            wire["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"]
                .is_null()
        );
    }

    #[test]
    fn split_results_preserve_text_images_reasoning_and_error_status() {
        let mut messages = Vec::new();
        // Each user turn is split across messages; the second turn also reuses IDs.
        for turn in 0..2 {
            messages.extend([
                serde_json::json!({"role":"assistant","content":[
                    {"type":"text","text":"calling"},
                    {"type":"thinking","thinking":"reason","signature":"sig"},
                    {"type":"tool_use","id":"a","name":"read","input":{"turn":turn}},
                    {"type":"tool_use","id":"b","name":"read","input":{"turn":turn}}
                ]}),
                serde_json::json!({"role":"user","content":[
                    {"type":"text","text":"first text"},
                    {"type":"tool_result","tool_use_id":"b","is_error":true,"content":"failed"}
                ]}),
                serde_json::json!({"role":"user","content":[
                    {"type":"text","text":"second text"},
                    {"type":"tool_result","tool_use_id":"a","content":[
                        {"type":"text","text":"image result"},
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}
                    ]}
                ]}),
            ]);
        }
        let req = tool_validation_request(serde_json::json!(messages));
        let state = convert_request(&req).unwrap().conversation_state;
        assert_wire_tool_pairs(&state);
        let wire = serde_json::to_value(state).unwrap();
        for (index, user) in [&wire["history"][1], &wire["currentMessage"]]
            .iter()
            .enumerate()
        {
            let assistant = &wire["history"][index * 2]["assistantResponseMessage"];
            assert_eq!(assistant["content"], "calling");
            assert_eq!(
                assistant["reasoningContent"]["reasoningText"]["signature"],
                "sig"
            );
            let user = &user["userInputMessage"];
            assert_eq!(user["content"], "first text\nsecond text");
            assert_eq!(user["images"][0]["source"]["bytes"], "aGVsbG8=");
            let results = user["userInputMessageContext"]["toolResults"]
                .as_array()
                .unwrap();
            assert_eq!(results.len(), 2);
            assert_eq!(results[0]["status"], "error");
            assert_eq!(results[0]["content"][0]["text"], "failed");
            assert_eq!(
                results[0]["toolUseId"],
                assistant["toolUses"][1]["toolUseId"]
            );
            assert_eq!(
                results[1]["toolUseId"],
                assistant["toolUses"][0]["toolUseId"]
            );
            assert_eq!(results[1]["content"][0]["text"], "image result");
            if index == 0 {
                assert_eq!(assistant["toolUses"][0]["toolUseId"], "a");
                assert_eq!(assistant["toolUses"][1]["toolUseId"], "b");
            }
        }
    }

    #[test]
    fn unsigned_or_empty_reasoning_never_replaces_a_signed_history_block() {
        let signed = serde_json::json!({"type":"thinking","thinking":"  signed text\n", "signature":"signature-a"});
        let invalid = vec![
            serde_json::json!({"type":"thinking","thinking":"unsigned"}),
            serde_json::json!({"type":"thinking","thinking":"blank signature","signature":"  "}),
            serde_json::json!({"type":"thinking","thinking":"","signature":"orphan-signature"}),
            serde_json::json!({"type":"redacted_thinking","data":""}),
        ];
        for with_signed in [false, true] {
            for separate_messages in [false, true] {
                let mut blocks = invalid.clone();
                if with_signed {
                    blocks.insert(0, signed.clone());
                }
                let mut messages = vec![serde_json::json!({"role":"user","content":"inspect"})];
                if separate_messages {
                    for block in blocks {
                        messages.push(serde_json::json!({"role":"assistant","content":[block]}));
                    }
                } else {
                    messages.push(serde_json::json!({"role":"assistant","content":blocks}));
                }
                messages.push(serde_json::json!({"role":"user","content":"continue"}));
                let req = tool_validation_request(messages.into());
                let wire = serde_json::to_value(convert_request(&req).unwrap().conversation_state)
                    .unwrap();
                let assistant = &wire["history"][1]["assistantResponseMessage"];
                if with_signed {
                    assert_eq!(
                        assistant["reasoningContent"],
                        serde_json::json!({
                            "reasoningText":{"text":"  signed text\n", "signature":"signature-a"}
                        })
                    );
                } else {
                    assert!(assistant.get("reasoningContent").is_none());
                }
                assert!(!assistant["content"].as_str().unwrap().contains("unsigned"));
            }
        }
    }

    #[test]
    fn assistant_reasoning_keeps_one_complete_block_with_its_own_signature() {
        let first = serde_json::json!({"type":"thinking","thinking":"first thought","signature":"first-signature"});
        let second = serde_json::json!({"type":"thinking","thinking":"second thought","signature":"second-signature"});
        let redacted = serde_json::json!({"type":"redacted_thinking","data":"encrypted-thought"});
        for (blocks, expected) in [
            (
                vec![first.clone(), second.clone()],
                serde_json::json!({"reasoningText":{"text":"second thought","signature":"second-signature"}}),
            ),
            (
                vec![first.clone(), redacted.clone()],
                serde_json::json!({"redactedContent":"encrypted-thought"}),
            ),
            (
                vec![redacted, second],
                serde_json::json!({"reasoningText":{"text":"second thought","signature":"second-signature"}}),
            ),
        ] {
            // A Kiro assistant message has one reasoning union. Match the existing
            // merged-assistant policy: retain the last complete block, never join
            // text authenticated by different signatures or combine union arms.
            let req = tool_validation_request(serde_json::json!([
                {"role":"user","content":"Inspect src/main.rs"},
                {"role":"assistant","content":blocks},
                {"role":"user","content":"Continue"}
            ]));
            let wire =
                serde_json::to_value(convert_request(&req).unwrap().conversation_state).unwrap();
            assert_eq!(
                wire["history"][1]["assistantResponseMessage"]["reasoningContent"],
                expected
            );
        }
    }

    #[test]
    fn empty_signature_does_not_replay_as_thinking_or_signature() {
        let request = tool_validation_request(serde_json::json!([
            {"role":"user", "content":"start"},
            {"role":"assistant", "content":[
                {"type":"thinking", "thinking":"no signature", "signature":"  "},
                {"type":"thinking", "thinking":"", "signature":"orphan"}
            ]},
            {"role":"user", "content":"continue"}
        ]));
        let converted = convert_request(&request).unwrap();
        let history = serde_json::to_value(converted.conversation_state).unwrap();
        assert!(
            history[1]["assistantResponseMessage"]
                .get("reasoningContent")
                .is_none()
        );
    }

    #[test]
    fn programming_history_reaches_cli_without_duplicate_or_foreign_content() {
        use crate::kiro::endpoint::{CliEndpoint, KiroEndpoint, RequestContext};
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::kiro::model::requests::kiro::KiroRequest;
        use crate::model::config::Config;

        let session = "a0662283-7fd3-4399-a7eb-52b9a717ae88";
        let code = "fn main() { println!(\"hello\\nworld\"); }\n// 中文 <tag> & ${value}";
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-haiku-5-5", "max_tokens":4096,
            "metadata":{"user_id":format!("user_test_session_{session}")},
            "system":"You are a Rust programming assistant.",
            "tools":[{"name":"read_file","description":"Read a UTF-8 source file.",
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}],
            "messages":[
                {"role":"user","content":"Inspect this Rust project."},
                {"role":"user","content":"Start with src/main.rs."},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"Inspect the Rust entry point first.","signature":"thought-signature"},
                    {"type":"text","text":"I will read the entry point."},
                    {"type":"tool_use","id":"read-1","name":"read_file","input":{"path":"src/main.rs"}}
                ]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":[{"type":"text","text":code}]}]},
                {"role":"assistant","content":"The source prints a greeting."},
                {"role":"user","content":"Change the greeting."},
                {"role":"user","content":"Preserve the escaping and comments."}
            ]
        })).unwrap();
        // Interleave a separate conversion with the same session metadata. The
        // converter must use only the messages supplied in the current request.
        let mut other = tool_validation_request(serde_json::json!([
            {"role":"user","content":"foreign-conversation-sentinel"}
        ]));
        other.metadata = Some(super::super::types::Metadata {
            user_id: Some(format!("user_test_session_{session}")),
        });
        convert_request(&other).unwrap();
        for native in [false, true] {
            let credentials = KiroCredentials::default();
            let mut config = Config::default();
            config
                .endpoints
                .insert("cli".into(), serde_json::json!({"nativeThinking":native}));
            let ctx = RequestContext {
                credentials: &credentials,
                config: &config,
                token: "test",
                machine_id: "test",
            };
            let request = KiroRequest {
                conversation_state: convert_request(&req).unwrap().conversation_state,
                profile_arn: None,
                additional_model_request_fields: additional_model_request_fields(&req),
            };
            let body = CliEndpoint::new()
                .transform_api_body(&serde_json::to_string(&request).unwrap(), &ctx);
            let wire: serde_json::Value = serde_json::from_str(&body).unwrap();
            let state = &wire["conversationState"];
            assert_eq!(state["conversationId"], session);
            assert!(state.get("agentContinuationId").is_none());
            assert!(state.get("agentTaskType").is_none());
            let history = state["history"].as_array().unwrap();
            assert_eq!(history.len(), 6);
            assert!(
                history[0]["userInputMessage"]["content"]
                    .as_str()
                    .unwrap()
                    .starts_with("You are a Rust programming assistant.\n")
            );
            assert_eq!(
                history[2]["userInputMessage"]["content"],
                "Inspect this Rust project.\nStart with src/main.rs."
            );
            let assistant = &history[3]["assistantResponseMessage"];
            if native {
                assert_eq!(assistant["content"], "I will read the entry point.");
                assert_eq!(
                    assistant["reasoningContent"],
                    serde_json::json!({
                        "reasoningText":{"text":"Inspect the Rust entry point first.","signature":"thought-signature"}
                    })
                );
            } else {
                assert_eq!(assistant["content"], "I will read the entry point.");
                assert!(assistant.get("reasoningContent").is_none());
            }
            let call = &history[3]["assistantResponseMessage"]["toolUses"][0];
            assert_eq!(call["input"], serde_json::json!({"path":"src/main.rs"}));
            let result =
                &history[4]["userInputMessage"]["userInputMessageContext"]["toolResults"][0];
            assert_eq!(result["toolUseId"], call["toolUseId"]);
            assert_eq!(result["content"][0]["text"], code);
            assert_eq!(
                history[5]["assistantResponseMessage"]["content"],
                "The source prints a greeting."
            );
            let current = &state["currentMessage"]["userInputMessage"];
            assert_eq!(
                current["content"],
                "Change the greeting.\nPreserve the escaping and comments."
            );
            assert_eq!(current["modelId"], "claude-haiku-5.5");
            assert_eq!(current["origin"], "KIRO_CLI");
            assert!(!body.contains("foreign-conversation-sentinel"));
            assert_eq!(body.matches("Change the greeting.").count(), 1);
        }
    }

    #[test]
    fn shortened_unicode_tool_names_respect_byte_limit() {
        let original = "工具".repeat(40);
        let mut names = HashMap::new();
        let short = map_tool_name(&original, &mut names);
        assert!(short.len() <= TOOL_NAME_MAX_LEN);
        assert_eq!(names.get(&short), Some(&original));
    }

    #[test]
    fn test_convert_request_filters_invalid_history_results() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-5-20250929", "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "future", "content": "too early"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "future", "name": "read", "input": {}}]},
                {"role": "user", "content": [
                    {"type": "text", "text": "keep this text"},
                    {"type": "tool_result", "tool_use_id": "future", "content": "valid"},
                    {"type": "tool_result", "tool_use_id": "future", "content": "duplicate"},
                    {"type": "tool_result", "tool_use_id": "missing", "content": "orphan"}
                ]},
                {"role": "assistant", "content": "done"},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "future", "content": "late duplicate"}]}
            ]
        })).unwrap();
        let state = convert_request(&req).unwrap().conversation_state;
        let results: Vec<_> = state
            .history
            .iter()
            .filter_map(|message| match message {
                Message::User(message) => Some(
                    &message
                        .user_input_message
                        .user_input_message_context
                        .tool_results,
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool_use_id, "future");
        assert!(
            state
                .current_message
                .user_input_message
                .user_input_message_context
                .tool_results
                .is_empty()
        );
        assert!(state.history.iter().any(|message| matches!(message, Message::User(message) if message.user_input_message.content == "keep this text")));
    }

    #[test]
    fn test_convert_request_merges_split_tool_results() {
        let req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-4-5-20250929", "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "read both"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "a", "name": "read", "input": {}},
                    {"type": "tool_use", "id": "b", "name": "read", "input": {}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "first"}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b", "content": "second"}]}
            ]
        })).unwrap();
        let state = convert_request(&req).unwrap().conversation_state;
        let results = &state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_use_id, "a");
        assert_eq!(results[1].tool_use_id, "b");
        assert!(
            matches!(state.history.last(), Some(Message::Assistant(message)) if message.assistant_response_message.tool_uses.as_ref().unwrap().len() == 2)
        );
    }

    #[test]
    fn test_validate_tool_pairing_rejects_cross_turn_result() {
        let history = vec![
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: AssistantMessage::new("read")
                    .with_tool_uses(vec![ToolUseEntry::new("old", "read")]),
            }),
            Message::User(HistoryUserMessage::new("skip", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage::new("done")),
        ];
        let (results, orphaned) =
            validate_tool_pairing(&history, &[ToolResult::success("old", "late")]);
        assert!(results.is_empty());
        assert!(orphaned.contains("old"));
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_result() {
        // 测试孤立的 tool_result 被过滤
        // 历史中没有 tool_use，但 tool_results 中有 tool_result
        let history = vec![
            Message::User(HistoryUserMessage::new("Hello", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage::new("Hi there!")),
        ];

        let tool_results = vec![ToolResult::success("orphan-123", "some result")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 孤立的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "孤立的 tool_result 应该被过滤");
    }

    #[test]
    fn test_validate_tool_pairing_orphaned_use() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试孤立的 tool_use（有 tool_use 但没有对应的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-orphan", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 没有 tool_result
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空（因为没有 tool_result）
        // 同时应该返回孤立的 tool_use_id
        assert!(filtered.is_empty());
        assert!(orphaned.contains("tool-orphan"));
    }

    #[test]
    fn test_validate_tool_pairing_valid() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试正常配对的情况
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let tool_results = vec![ToolResult::success("tool-1", "file content")];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 配对成功，应该保留，无孤立
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_mixed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试混合情况：部分配对成功，部分孤立
        let mut assistant_msg = AssistantMessage::new("I'll use two tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
        ]);

        let history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // tool_results: tool-1 配对，tool-3 孤立
        let tool_results = vec![
            ToolResult::success("tool-1", "result 1"),
            ToolResult::success("tool-3", "orphan result"), // 孤立
        ];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 只有 tool-1 应该保留
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tool_use_id, "tool-1");
        // tool-2 是孤立的 tool_use（无 result），tool-3 是孤立的 tool_result
        assert!(orphaned.contains("tool-2"));
    }

    #[test]
    fn test_validate_tool_pairing_history_already_paired() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试历史中已配对的 tool_use 不应该被报告为孤立
        // 场景：多轮对话中，之前的 tool_use 已经在历史中有对应的 tool_result
        let mut assistant_msg1 = AssistantMessage::new("I'll read the file.");
        assistant_msg1 = assistant_msg1.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 构建历史中的 user 消息，包含 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            // 第一轮：用户请求
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            // 第一轮：assistant 使用工具
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg1,
            }),
            // 第二轮：用户返回工具结果（历史中已配对）
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            // 第二轮：assistant 响应
            Message::Assistant(HistoryAssistantMessage::new("The file contains...")),
        ];

        // 当前消息没有 tool_results（用户只是继续对话）
        let tool_results: Vec<ToolResult> = vec![];

        let (filtered, orphaned) = validate_tool_pairing(&history, &tool_results);

        // 结果应该为空，且不应该有孤立 tool_use
        // 因为 tool-1 已经在历史中配对了
        assert!(filtered.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn test_validate_tool_pairing_duplicate_result() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试重复的 tool_result（历史中已配对，当前消息又发送了相同的 tool_result）
        let mut assistant_msg = AssistantMessage::new("I'll read the file.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read")
                .with_input(serde_json::json!({"path": "/test.txt"})),
        ]);

        // 历史中已有 tool_result
        let mut user_msg_with_result = UserMessage::new("", "claude-sonnet-4.5");
        let mut ctx = UserInputMessageContext::new();
        ctx = ctx.with_tool_results(vec![ToolResult::success("tool-1", "file content")]);
        user_msg_with_result = user_msg_with_result.with_context(ctx);

        let history = vec![
            Message::User(HistoryUserMessage::new(
                "Read the file",
                "claude-sonnet-4.5",
            )),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
            Message::User(HistoryUserMessage {
                user_input_message: user_msg_with_result,
            }),
            Message::Assistant(HistoryAssistantMessage::new("Done")),
        ];

        // 当前消息又发送了相同的 tool_result（重复）
        let tool_results = vec![ToolResult::success("tool-1", "file content again")];

        let (filtered, _) = validate_tool_pairing(&history, &tool_results);

        // 重复的 tool_result 应该被过滤掉
        assert!(filtered.is_empty(), "重复的 tool_result 应该被过滤");
    }

    #[test]
    fn test_convert_assistant_message_tool_use_only() {
        use super::super::types::Message as AnthropicMessage;

        // 测试仅包含 tool_use 的 assistant 消息（无 text 块）
        // Kiro API 要求 content 字段不能为空
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 不为空（使用占位符）
        assert!(
            !result.assistant_response_message.content.is_empty(),
            "content 不应为空"
        );
        assert_eq!(
            result.assistant_response_message.content, " ",
            "仅 tool_use 时应使用 ' ' 占位符"
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
        assert_eq!(tool_uses[0].name, "read_file");
    }

    #[test]
    fn test_convert_assistant_message_with_text_and_tool_use() {
        use super::super::types::Message as AnthropicMessage;

        // 测试同时包含 text 和 tool_use 的 assistant 消息
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Let me read that file for you."},
                {"type": "tool_use", "id": "toolu_02XYZ", "name": "read_file", "input": {"path": "/data.json"}}
            ]),
        };

        let result = convert_assistant_message(&msg, &mut HashMap::new()).expect("应该成功转换");

        // 验证 content 使用原始文本（不是占位符）
        assert_eq!(
            result.assistant_response_message.content,
            "Let me read that file for you."
        );

        // 验证 tool_uses 被正确保留
        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应该有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_02XYZ");
    }

    #[test]
    fn test_remove_orphaned_tool_uses() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试从历史中移除孤立的 tool_use
        let mut assistant_msg = AssistantMessage::new("I'll use multiple tools.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-2", "write").with_input(serde_json::json!({})),
            ToolUseEntry::new("tool-3", "delete").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        // 移除 tool-1 和 tool-3
        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());
        orphaned.insert("tool-3".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证只剩下 tool-2
        if let Message::Assistant(ref assistant_msg) = history[1] {
            let tool_uses = assistant_msg
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("应该还有 tool_uses");
            assert_eq!(tool_uses.len(), 1);
            assert_eq!(tool_uses[0].tool_use_id, "tool-2");
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_remove_orphaned_tool_uses_all_removed() {
        use crate::kiro::model::requests::tool::ToolUseEntry;

        // 测试移除所有 tool_use 后，tool_uses 变为 None
        let mut assistant_msg = AssistantMessage::new("I'll use a tool.");
        assistant_msg = assistant_msg.with_tool_uses(vec![
            ToolUseEntry::new("tool-1", "read").with_input(serde_json::json!({})),
        ]);

        let mut history = vec![
            Message::User(HistoryUserMessage::new("Do something", "claude-sonnet-4.5")),
            Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: assistant_msg,
            }),
        ];

        let mut orphaned = std::collections::HashSet::new();
        orphaned.insert("tool-1".to_string());

        remove_orphaned_tool_uses(&mut history, &orphaned);

        // 验证 tool_uses 变为 None
        if let Message::Assistant(ref assistant_msg) = history[1] {
            assert!(
                assistant_msg.assistant_response_message.tool_uses.is_none(),
                "移除所有 tool_use 后应为 None"
            );
        } else {
            panic!("应该是 Assistant 消息");
        }
    }

    #[test]
    fn test_merge_consecutive_assistant_messages() {
        // 测试连续 assistant 消息被正确合并（Issue #79）
        use super::super::types::Message as AnthropicMessage;

        let msg1 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "Let me think about this...", "signature":"first-signature"},
                {"type": "text", "text": " "}
            ]),
        };

        let msg2 = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "thinking", "thinking": "I should read the file.", "signature":"second-signature"},
                {"type": "text", "text": "Let me read that file."},
                {"type": "tool_use", "id": "toolu_01ABC", "name": "read_file", "input": {"path": "/test.txt"}}
            ]),
        };

        let messages: Vec<&AnthropicMessage> = vec![&msg1, &msg2];
        let result = merge_assistant_messages(&messages, &mut HashMap::new()).expect("合并应成功");

        let content = &result.assistant_response_message.content;
        assert!(
            !content.contains("<thinking>"),
            "推理应与 assistant 文本分开保存"
        );
        assert!(
            result
                .assistant_response_message
                .reasoning_content
                .as_ref()
                .and_then(|value| value.pointer("/reasoningText/text"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains("I should read")),
            "签名不能对应拼接后的推理文本；保留最后一个原生推理块"
        );
        assert!(
            content.contains("Let me read that file"),
            "应包含第二条消息的 text 内容"
        );

        let tool_uses = result
            .assistant_response_message
            .tool_uses
            .expect("应有 tool_uses");
        assert_eq!(tool_uses.len(), 1);
        assert_eq!(tool_uses[0].tool_use_id, "toolu_01ABC");
    }

    #[test]
    fn test_consecutive_assistant_with_tool_use_result_pairing() {
        // 测试 Issue #79 的完整场景
        use super::super::types::Message as AnthropicMessage;

        let req = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 1024,
            messages: vec![
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!("Read the config file"),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "I need to read the file..."},
                        {"type": "text", "text": " "}
                    ]),
                },
                AnthropicMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([
                        {"type": "thinking", "thinking": "Let me read the config."},
                        {"type": "text", "text": "I'll read the config file for you."},
                        {"type": "tool_use", "id": "toolu_01XYZ", "name": "read_file", "input": {"path": "/config.json"}}
                    ]),
                },
                AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_01XYZ", "content": "{\"key\": \"value\"}"}
                    ]),
                },
            ],
            stream: false,
            system: None,
            tools: None,
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        let result = convert_request(&req);
        assert!(
            result.is_ok(),
            "连续 assistant 消息场景不应报错: {:?}",
            result.err()
        );

        let state = result.unwrap().conversation_state;
        let mut found_tool_use = false;
        for msg in &state.history {
            if let Message::Assistant(assistant_msg) = msg
                && let Some(ref tool_uses) = assistant_msg.assistant_response_message.tool_uses
                && tool_uses.iter().any(|t| t.tool_use_id == "toolu_01XYZ")
            {
                found_tool_use = true;
                break;
            }
        }
        assert!(found_tool_use, "合并后的 assistant 消息应包含 tool_use");
    }
}
