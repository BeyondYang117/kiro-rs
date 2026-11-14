//! Offline regressions: abu-api -> converter -> CLI -> account shaping.

use super::{converter, types::MessagesRequest};
use crate::kiro::endpoint::{CliEndpoint, KiroEndpoint, RequestContext};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Fixture {
    name: String,
    request: Value,
}

fn fixtures() -> Vec<Fixture> {
    serde_json::from_str(include_str!(
        "../../testdata/abu_api_tool_compatibility.json"
    ))
    .unwrap()
}

fn fixture(name: &str) -> Value {
    fixtures()
        .into_iter()
        .find(|f| f.name == name)
        .unwrap()
        .request
}

// Exercise production methods; no HTTP calls, refreshes, credential files or real keys.
fn wire(request: &Value, native: bool) -> (Value, std::collections::HashMap<String, String>) {
    let mut request = request.clone();
    for tool in request["tools"].as_array_mut().into_iter().flatten() {
        if let Some(schema) = tool.get_mut("input_schema") {
            drop_unsupported_additional_properties(schema);
        }
    }
    let request: MessagesRequest = serde_json::from_value(request).unwrap();
    let converted = converter::convert_request(&request).unwrap();
    let mut body = json!({"conversationState": converted.conversation_state});
    if let Some(fields) = converter::additional_model_request_fields(&request) {
        body["additionalModelRequestFields"] = fields;
    }
    let credentials = KiroCredentials {
        id: Some(1),
        endpoint: Some("cli".into()),
        kiro_api_key: Some("synthetic-offline-key".into()),
        ..Default::default()
    };
    let mut config = Config::default();
    config
        .endpoints
        .insert("cli".into(), json!({"nativeThinking": native}));
    let ctx = RequestContext {
        credentials: &credentials,
        config: &config,
        token: "synthetic",
        machine_id: "synthetic",
    };
    let endpoint_body = CliEndpoint::new().transform_api_body(&body.to_string(), &ctx);
    let endpoint: Value = serde_json::from_str(&endpoint_body).unwrap();
    // Both CLI modes must leave the converter's tool fields intact. Check the
    // actual final account-shaping method too, rather than assuming it is a no-op.
    assert_eq!(context(&endpoint), context(&body));
    assert_eq!(calls(&endpoint), calls(&body));
    let manager = MultiTokenManager::new(config, vec![credentials], None, None, true).unwrap();
    let final_body: Value =
        serde_json::from_str(&manager.model_request_body(1, endpoint_body)).unwrap();
    assert_eq!(context(&final_body), context(&endpoint));
    assert_eq!(calls(&final_body), calls(&endpoint));
    assert_eq!(
        final_body.pointer("/conversationState/currentMessage/userInputMessage/origin"),
        Some(&json!("KIRO_CLI"))
    );
    (final_body, converted.tool_name_map)
}

fn context(body: &Value) -> &Value {
    body.pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext")
        .unwrap()
}

fn tools(body: &Value) -> Vec<&Value> {
    context(body)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| &t["toolSpecification"])
        .collect()
}

fn calls(body: &Value) -> Vec<&Value> {
    body.pointer("/conversationState/history")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.pointer("/assistantResponseMessage/toolUses"))
        .flat_map(|uses| uses.as_array().unwrap())
        .collect()
}

fn assert_response_name(
    mapping: &std::collections::HashMap<String, String>,
    alias: &str,
    original: &str,
) {
    use super::stream::StreamContext;
    use crate::kiro::model::events::ToolUseEvent;
    let mut stream = StreamContext::new_with_thinking("claude-opus-5-5", 1, false, mapping.clone());
    stream.generate_initial_events();
    let events =
        stream.process_kiro_event(&crate::kiro::model::events::Event::ToolUse(ToolUseEvent {
            name: alias.into(),
            tool_use_id: "synthetic-response".into(),
            input: "{}".into(),
            stop: true,
        }));
    let start = events
        .iter()
        .find(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
        })
        .unwrap();
    assert_eq!(start.data["content_block"]["name"], original);
}

#[test]
fn compatibility_fixed_function_tools_and_nested_schemas_survive_both_cli_modes() {
    let names = [
        "lookup",
        "bash",
        "web_search",
        "text_editor",
        "str_replace_editor",
        "computer",
        "computer_search",
    ];
    for name in names {
        let request = fixture(name);
        for native in [false, true] {
            let decoded: MessagesRequest = serde_json::from_value(request.clone()).unwrap();
            assert!(!super::websearch::has_web_search_tool(&decoded));
            let (body, mapping) = wire(&request, native);
            let definitions = tools(&body);
            assert_eq!(definitions.len(), 1, "{name}, native={native}");
            assert_eq!(definitions[0]["name"], name);
            assert_eq!(
                definitions[0]["inputSchema"]["json"],
                normalized_tool_schema(&request["tools"][0]["input_schema"])
            );
            let uses = calls(&body);
            assert_eq!(uses.len(), 1);
            assert_eq!(uses[0]["name"], name);
            assert_eq!(uses[0]["input"]["mode"], "read");
            assert!(mapping.is_empty());
            assert_eq!(
                context(&body)["toolResults"][0]["toolUseId"],
                uses[0]["toolUseId"]
            );
            assert!(context(&body)["toolResults"][0].get("isError").is_none());
        }
    }
}

#[test]
fn compatibility_custom_web_search_stays_on_generation_route() {
    let request: MessagesRequest = serde_json::from_value(fixture("web_search")).unwrap();
    let tool = &request.tools.as_ref().unwrap()[0];
    assert!(tool.tool_type.is_none());
    assert!(!tool.input_schema.is_empty());
    // Both /v1/messages and /cc/v1/messages use this predicate before convert_request.
    assert!(!super::websearch::has_web_search_tool(&request));
    // The current user contains tool results, not a textual MCP search query.
    assert!(super::websearch::extract_search_query(&request).is_none());
    let mut mixed = fixture("web_search");
    mixed["tools"]
        .as_array_mut()
        .unwrap()
        .push(fixture("lookup")["tools"][0].clone());
    let request: MessagesRequest = serde_json::from_value(mixed).unwrap();
    assert!(!super::websearch::has_web_search_tool(&request));
    let mut typed = fixture("web_search");
    typed["tools"][0]["type"] = "web_search_20250305".into();
    let request: MessagesRequest = serde_json::from_value(typed).unwrap();
    // An explicit native type must not silently discard a client schema either.
    assert!(!super::websearch::has_web_search_tool(&request));
    assert!(converter::convert_request(&request).is_err());
}

#[test]
fn compatibility_native_web_search_appended_after_57_functions_uses_server_route() {
    let value = fixture("function tools with native web search");
    assert_eq!(value["tools"].as_array().unwrap().len(), 58);
    assert_eq!(value["tools"][57]["type"], "web_search_20250305");
    let request: MessagesRequest = serde_json::from_value(value).unwrap();
    assert!(!super::websearch::has_web_search_tool(&request));
    assert!(super::mixed_search::is_mixed_search(&request));
    let error = converter::convert_request(&request)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("tools[57].type"));
    assert!(error.contains("web_search_20250305"));
    assert!(error.contains("name=\"web_search\""));
    assert!(error.contains("server execution route"));
}

#[test]
fn compatibility_invalid_tool_names_are_reversible_and_legal() {
    for name in ["functions.exec_command", "mcp/server/read", "read tool"] {
        assert!(
            !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        );
        for native in [false, true] {
            let (body, mapping) = wire(&fixture(name), native);
            let alias = tools(&body)[0]["name"].as_str().unwrap();
            assert_ne!(alias, name);
            assert!(
                alias
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            );
            assert_eq!(calls(&body)[0]["name"], alias);
            assert_eq!(mapping.get(alias).map(String::as_str), Some(name));
            assert_response_name(&mapping, alias, name);
        }
    }
}

#[test]
fn compatibility_shortening_collision_keeps_names_and_reverse_map_distinct() {
    let request = fixture("shortening collision");
    let long = request["tools"][0]["name"].as_str().unwrap();
    let alias = request["tools"][1]["name"].as_str().unwrap();
    let digest = hex::encode(Sha256::digest(long.as_bytes()));
    assert_eq!(alias, format!("{}_{}", &long[..54], &digest[..8]));
    for native in [false, true] {
        let (body, mapping) = wire(&request, native);
        let definitions = tools(&body);
        assert_eq!(definitions.len(), 2);
        let shortened = definitions[0]["name"].as_str().unwrap();
        assert_ne!(shortened, alias);
        assert_eq!(definitions[1]["name"], alias);
        assert_eq!(calls(&body)[0]["name"], shortened);
        assert_eq!(mapping.get(shortened).map(String::as_str), Some(long));
        assert!(!mapping.contains_key(alias));
        assert_response_name(&mapping, shortened, long);
        assert_response_name(&mapping, alias, alias);
    }
}

#[test]
fn compatibility_omitted_type_preserves_schema_and_unsupported_native_is_rejected() {
    let request = fixture("omitted type");
    for native in [false, true] {
        let (body, _) = wire(&request, native);
        let definitions = tools(&body);
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["name"], "text_editor");
        assert_eq!(
            definitions[0]["inputSchema"]["json"],
            normalized_tool_schema(&request["tools"][0]["input_schema"])
        );
    }
    let mut request = request;
    request["tools"] = json!([{"name":"str_replace_editor","type":"text_editor_20250124"}]);
    let request: MessagesRequest = serde_json::from_value(request).unwrap();
    assert!(
        converter::convert_request(&request)
            .err()
            .unwrap()
            .to_string()
            .contains("tools[0].type")
    );
}

fn normalized_tool_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
    drop_unsupported_additional_properties(&mut schema);
    let request: MessagesRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-5.5", "max_tokens":16,
        "messages":[{"role":"user","content":"test"}],
        "tools":[{"name":"test","description":"test","input_schema":schema}]
    }))
    .unwrap();
    let converted = converter::convert_request(&request).unwrap();
    serde_json::to_value(
        converted
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tools[0]
            .tool_specification
            .input_schema
            .json
            .clone(),
    )
    .unwrap()
}

fn drop_unsupported_additional_properties(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    object.remove("additionalProperties");
    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                drop_unsupported_additional_properties(child);
            }
        }
    }
    if let Some(dependencies) = object
        .get_mut("dependencies")
        .and_then(Value::as_object_mut)
    {
        for child in dependencies.values_mut().filter(|value| value.is_object()) {
            drop_unsupported_additional_properties(child);
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
                    drop_unsupported_additional_properties(child);
                }
            } else {
                drop_unsupported_additional_properties(child);
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            for child in children {
                drop_unsupported_additional_properties(child);
            }
        }
    }
}

#[test]
fn kiro_tool_schema_removes_unsupported_keywords_recursively() {
    let schema = json!({
        "type":"object",
        "properties": {
            "nested": {"type":"object", "required":[], "additionalProperties":true,
                "properties":{"value":{"type":"string", "required":["kept"], "additionalProperties":true}}},
            "items": {"type":"array", "items":[{"type":"object", "required":[], "additionalProperties":true}]},
            "union": {"oneOf":[{"type":"string", "additionalProperties":true}, {"type":"null", "required":[]}]},
            "dependency": {"type":"object", "dependencies":{"other":{"type":"object", "additionalProperties":true}}},
            "example": {"enum":[{"additionalProperties":false,"required":[]}]}
        },
        "required":["nested"], "additionalProperties":true
    });
    let normalized = normalized_tool_schema(&schema);
    assert_eq!(normalized["required"], json!(["nested"]));
    assert!(normalized.get("additionalProperties").is_none());
    assert!(normalized["properties"]["nested"].get("required").is_none());
    assert!(
        normalized["properties"]["nested"]
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(
        normalized["properties"]["nested"]["properties"]["value"]["required"],
        json!(["kept"])
    );
    assert!(
        normalized["properties"]["nested"]["properties"]["value"]
            .get("additionalProperties")
            .is_none()
    );
    assert!(
        normalized["properties"]["items"]["items"][0]
            .get("additionalProperties")
            .is_none()
    );
    assert!(
        normalized["properties"]["union"]["oneOf"][0]
            .get("additionalProperties")
            .is_none()
    );
    assert!(
        normalized["properties"]["union"]["oneOf"][1]
            .get("required")
            .is_none()
    );
    assert!(
        normalized["properties"]["dependency"]["dependencies"]["other"]
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(
        normalized["properties"]["example"]["enum"][0]["additionalProperties"],
        false
    );
    assert_eq!(
        normalized["properties"]["example"]["enum"][0]["required"],
        json!([])
    );
}

#[test]
fn kiro_tool_schema_rejects_restrictions_it_cannot_preserve() {
    for schema in [
        json!({"type":"object", "properties":{"nested":{"type":"object", "additionalProperties":{"type":"string"}}}}),
    ] {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-5.5", "max_tokens":16,
            "messages":[{"role":"user","content":"test"}],
            "tools":[{"name":"test","description":"test","input_schema":schema}]
        }))
        .unwrap();
        let error = converter::convert_request(&request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("additionalProperties"), "{error}");
    }
}

#[test]
fn internal_tool_schema_retains_false_and_omits_true_additional_properties() {
    let schema = json!({
        "type":"object",
        "additionalProperties":false,
        "properties": {
            "nested":{"type":"object", "additionalProperties":false},
            "items":{"type":"array", "items":{"type":"object", "additionalProperties":true}}
        }
    });
    let request: MessagesRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-5.5", "max_tokens":16,
        "messages":[{"role":"user","content":"test"}],
        "tools":[{"name":"test","description":"test","input_schema":schema}]
    }))
    .unwrap();
    let converted = converter::convert_request(&request).unwrap();
    let normalized = &converted
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .tools[0]
        .tool_specification
        .input_schema
        .json;
    assert_eq!(normalized["additionalProperties"], false);
    assert_eq!(
        normalized["properties"]["nested"]["additionalProperties"],
        false
    );
    assert!(
        normalized["properties"]["items"]["items"]
            .get("additionalProperties")
            .is_none()
    );
}

#[test]
fn compatibility_error_result_uses_status_without_extra_wire_field() {
    for (name, failed) in [
        ("messages success result", false),
        ("messages error result", true),
    ] {
        for native in [false, true] {
            let (body, _) = wire(&fixture(name), native);
            let result = &context(&body)["toolResults"][0];
            assert_eq!(result["toolUseId"], calls(&body)[0]["toolUseId"]);
            assert_eq!(result["status"], if failed { "error" } else { "success" });
            assert!(result.get("isError").is_none());
            let mut keys: Vec<_> = result
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            let expected = vec!["content", "status", "toolUseId"];
            assert_eq!(keys, expected);
        }
    }
}

#[test]
fn compatibility_already_corrupted_nested_schema_fails_before_upstream() {
    let mut request = fixture("lookup");
    // Reproduce pre-84dde51f7 gateway corruption without reimplementing its sanitizer.
    let broken = json!({"type":"object", "enum":["read", "write"]});
    request["tools"][0]["input_schema"]["properties"]["mode"] = broken.clone();
    let request: MessagesRequest = serde_json::from_value(request).unwrap();
    let error = converter::convert_request(&request)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("tools[0].input_schema.properties.mode"));
    assert!(error.contains("type contradicts enum/const"));
}

#[test]
fn compatibility_name_collisions_include_history_only_and_case_insensitive_names() {
    for reverse in [false, true] {
        let mut request = fixture("functions.exec_command");
        let mut legal = fixture("lookup")["tools"][0].clone();
        legal["name"] = "FUNCTIONS_EXEC_COMMAND".into();
        request["tools"].as_array_mut().unwrap().push(legal);
        let mut third = fixture("lookup")["tools"][0].clone();
        third["name"] = "functions/exec_command".into();
        request["tools"].as_array_mut().unwrap().push(third);
        if reverse {
            request["tools"].as_array_mut().unwrap().reverse();
        }
        for native in [false, true] {
            let (body, mapping) = wire(&request, native);
            let names: Vec<_> = tools(&body)
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_ascii_lowercase())
                .collect();
            let unique: std::collections::HashSet<_> = names.iter().collect();
            assert_eq!(unique.len(), 3);
            let alias = calls(&body)[0]["name"].as_str().unwrap();
            assert_response_name(&mapping, alias, "functions.exec_command");
            assert_response_name(&mapping, "FUNCTIONS_EXEC_COMMAND", "FUNCTIONS_EXEC_COMMAND");
        }
    }
    let mut request = fixture("functions.exec_command");
    request["tools"] = json!([]);
    let (body, mapping) = wire(&request, false);
    assert_eq!(tools(&body)[0]["name"], calls(&body)[0]["name"]);
    assert_response_name(
        &mapping,
        calls(&body)[0]["name"].as_str().unwrap(),
        "functions.exec_command",
    );
}

#[test]
fn compatibility_schema_validation_keeps_data_and_valid_unions() {
    let mut request = fixture("lookup");
    request["tools"][0]["input_schema"] = json!({"type":"object","properties":{
        "value":{"type":["string","null"],"enum":[null,"read"]},
        "data":{"type":"object","enum":[{"type":"object","enum":["this is data"]}]},
        "number":{"type":"integer","enum":[1.0]},
        "mixed":{"type":"string","enum":[1,"read"]}
    }});
    for native in [false, true] {
        wire(&request, native);
    }
    request["tools"][0]["input_schema"]["properties"]["secret"] =
        json!({"type":"object","const":"do not log"});
    let request: MessagesRequest = serde_json::from_value(request).unwrap();
    let error = converter::convert_request(&request)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("properties.secret"));
    assert!(!error.contains("do not log"));
}
