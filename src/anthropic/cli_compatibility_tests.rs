//! Offline characterization of abu-api 84dde51f7 -> converter -> CLI -> account shaping.
//! Gap tests assert current output, not a promise that the upstream accepts it.

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
    let request: MessagesRequest = serde_json::from_value(request.clone()).unwrap();
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
            let mut dispatched_request = request.clone();
            if name == "web_search" {
                // A single tool named web_search is intercepted before conversion.
                dispatched_request["tools"]
                    .as_array_mut()
                    .unwrap()
                    .push(fixture("lookup")["tools"][0].clone());
            }
            let decoded: MessagesRequest =
                serde_json::from_value(dispatched_request.clone()).unwrap();
            assert!(!super::websearch::has_web_search_tool(&decoded));
            let (body, mapping) = wire(&dispatched_request, native);
            let definitions = tools(&body);
            assert_eq!(
                definitions.len(),
                if name == "web_search" { 2 } else { 1 },
                "{name}, native={native}"
            );
            assert_eq!(definitions[0]["name"], name);
            assert_eq!(
                definitions[0]["inputSchema"]["json"],
                request["tools"][0]["input_schema"]
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
fn compatibility_gap_single_custom_web_search_is_dispatched_to_mcp_by_name() {
    let request: MessagesRequest = serde_json::from_value(fixture("web_search")).unwrap();
    let tool = &request.tools.as_ref().unwrap()[0];
    assert!(tool.tool_type.is_none());
    assert!(!tool.input_schema.is_empty());
    // Both /v1/messages and /cc/v1/messages use this predicate before convert_request.
    assert!(super::websearch::has_web_search_tool(&request));
    // The current user contains tool results, not a textual MCP search query.
    assert!(super::websearch::extract_search_query(&request).is_none());
    let mut mixed = fixture("web_search");
    mixed["tools"]
        .as_array_mut()
        .unwrap()
        .push(fixture("lookup")["tools"][0].clone());
    let request: MessagesRequest = serde_json::from_value(mixed).unwrap();
    assert!(!super::websearch::has_web_search_tool(&request));
}

#[test]
fn compatibility_gap_channel_overrides_can_rebreak_fixed_schema_or_tool_identity() {
    for native in [false, true] {
        let (body, _) = wire(&fixture("override nested schema"), native);
        assert_eq!(
            tools(&body)[0]["inputSchema"]["json"]["properties"]["mode"],
            json!({"type":"object","enum":["read","write"]})
        );
        assert_eq!(calls(&body)[0]["input"]["mode"], "read");

        let (body, _) = wire(&fixture("override tool name"), native);
        let definitions = tools(&body);
        assert_eq!(definitions.len(), 2);
        assert_eq!(definitions[0]["name"], "renamed_lookup");
        assert_eq!(definitions[1]["name"], "lookup");
        assert_eq!(
            definitions[1]["inputSchema"]["json"]["properties"],
            json!({})
        );
        assert_eq!(calls(&body)[0]["name"], "lookup");
    }
}

#[test]
fn compatibility_gap_invalid_tool_name_characters_reach_final_cli_body() {
    for name in ["functions.exec_command", "mcp/server/read", "read tool"] {
        assert!(
            !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        );
        for native in [false, true] {
            let (body, mapping) = wire(&fixture(name), native);
            assert_eq!(tools(&body)[0]["name"], name);
            assert_eq!(calls(&body)[0]["name"], name);
            assert!(mapping.is_empty());
        }
    }
}

#[test]
fn compatibility_gap_shortening_collision_duplicates_definitions_and_ambiguous_reverse_map() {
    let request = fixture("shortening collision");
    let long = request["tools"][0]["name"].as_str().unwrap();
    let alias = request["tools"][1]["name"].as_str().unwrap();
    let digest = hex::encode(Sha256::digest(long.as_bytes()));
    assert_eq!(alias, format!("{}_{}", &long[..54], &digest[..8]));
    for native in [false, true] {
        let (body, mapping) = wire(&request, native);
        let definitions = tools(&body);
        assert_eq!(definitions.len(), 2);
        assert_eq!(definitions[0]["name"], alias);
        assert_eq!(definitions[1]["name"], alias);
        assert_eq!(calls(&body)[0]["name"], alias);
        // A response choosing the client's real short-name tool would also be
        // restored to the long name by the same response mapping.
        assert_eq!(mapping.get(alias).map(String::as_str), Some(long));
    }
}

#[test]
fn compatibility_gap_legacy_and_explicit_native_tools_lose_schema_and_get_placeholders() {
    for name in ["omitted type", "custom native type"] {
        let request = fixture(name);
        assert!(request["tools"][0].get("input_schema").is_none());
        for native in [false, true] {
            let (body, _) = wire(&request, native);
            let definitions = tools(&body);
            assert_eq!(definitions.len(), 2);
            assert_eq!(definitions[0]["name"], "str_replace_editor");
            assert_eq!(definitions[1]["name"], "text_editor");
            for definition in definitions {
                let schema = &definition["inputSchema"]["json"];
                assert_eq!(schema["properties"], json!({}));
                assert_eq!(schema["required"], json!([]));
                assert_eq!(schema["additionalProperties"], true);
            }
            assert_eq!(calls(&body)[0]["name"], "text_editor");
        }
    }
}

#[test]
fn compatibility_gap_error_result_has_extra_is_error_wire_field() {
    for (name, failed) in [
        ("messages success result", false),
        ("messages error result", true),
    ] {
        for native in [false, true] {
            let (body, _) = wire(&fixture(name), native);
            let result = &context(&body)["toolResults"][0];
            assert_eq!(result["toolUseId"], calls(&body)[0]["toolUseId"]);
            assert_eq!(result["status"], if failed { "error" } else { "success" });
            if failed {
                assert_eq!(result["isError"], true);
            } else {
                assert!(result.get("isError").is_none());
            }
            let mut keys: Vec<_> = result
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            let expected = if failed {
                vec!["content", "isError", "status", "toolUseId"]
            } else {
                vec!["content", "status", "toolUseId"]
            };
            assert_eq!(keys, expected);
        }
    }
}

#[test]
fn compatibility_gap_already_corrupted_nested_schema_is_not_repaired() {
    let mut request = fixture("lookup");
    // Reproduce pre-84dde51f7 gateway corruption without reimplementing its sanitizer.
    let broken = json!({"type":"object", "enum":["read", "write"]});
    request["tools"][0]["input_schema"]["properties"]["mode"] = broken.clone();
    for native in [false, true] {
        let (body, _) = wire(&request, native);
        assert_eq!(
            tools(&body)[0]["inputSchema"]["json"]["properties"]["mode"],
            broken
        );
        assert!(calls(&body)[0]["input"]["mode"].is_string());
    }
}
