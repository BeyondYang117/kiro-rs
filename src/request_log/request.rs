//! Always-on request structure; conversation text and opaque values are excluded.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpstreamRequestDiagnostics {
    body_bytes: usize,
    thinking_transport: String,
    request_debug_enabled: bool,
    model_controls: Value,
    legacy_controls: Value,
    history_messages: usize,
    history_user_messages: usize,
    history_assistant_messages: usize,
    history_reasoning_messages: usize,
    history_signed_reasoning_messages: usize,
    history_redacted_reasoning_messages: usize,
    history_text_bytes: usize,
    recent_history: Vec<Value>,
    history_summary_truncated: bool,
    current_message: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_diagnostics: Option<Value>,
}

/// Inspect all turns; retain bounded locations and ID fingerprints, never raw IDs or inputs.
fn tool_diagnostics(state: &Value) -> Value {
    use std::collections::{HashSet, VecDeque};

    fn id_fingerprint(id: &str) -> String {
        let digest = Sha256::digest(id.as_bytes());
        digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    let mut uses = 0usize;
    let mut results = 0usize;
    let mut issue_count = 0usize;
    let mut issues = Vec::new();
    let mut pairs: Vec<Value> = Vec::new();
    let mut tool_history = Vec::new();
    let mut seen = HashSet::new();
    let mut pending: VecDeque<(String, usize)> = VecDeque::new();
    let mut issue = |path: String, reason: &str, id: Option<&str>| {
        issue_count += 1;
        if issues.len() < 24 {
            let mut entry = json!({"path":path,"reason":reason});
            if let Some(id) = id {
                entry["idFingerprint"] = json!(id_fingerprint(id));
            }
            issues.push(entry);
        }
    };
    let context = &state["currentMessage"]["userInputMessage"]["userInputMessageContext"];
    let definitions: HashSet<_> = context["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["toolSpecification"]["name"].as_str())
        .map(str::to_ascii_lowercase)
        .collect();
    let history = state["history"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (index, wrapper) in history
        .iter()
        .chain(std::iter::once(&state["currentMessage"]))
        .enumerate()
    {
        let path = if index == history.len() {
            "currentMessage".into()
        } else {
            format!("history[{index}]")
        };
        let assistant = wrapper.get("assistantResponseMessage");
        let message = assistant.unwrap_or(&wrapper["userInputMessage"]);
        let calls = message["toolUses"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let replies = message["userInputMessageContext"]["toolResults"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        if index < history.len() && (!calls.is_empty() || !replies.is_empty()) {
            let mut summary = message_summary(message, assistant.is_some());
            summary["index"] = json!(index);
            tool_history.push(summary);
        }
        uses += calls.len();
        results += replies.len();
        if assistant.is_some() {
            while let Some((id, pair_index)) = pending.pop_front() {
                pairs[pair_index]["status"] = json!("missing_result");
                issue(
                    pairs[pair_index]["callPath"]
                        .as_str()
                        .unwrap_or(&path)
                        .to_owned(),
                    "tool call has no result before the next assistant turn",
                    Some(&id),
                );
            }
            for (call_index, call) in calls.iter().enumerate() {
                let path = format!("{path}.assistantResponseMessage.toolUses[{call_index}]");
                match call["toolUseId"]
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                {
                    Some(id) => {
                        let duplicate = !seen.insert(id.to_owned());
                        let pair_index = pairs.len();
                        pairs.push(json!({
                            "idFingerprint":id_fingerprint(id),
                            "callPath":path,
                            "resultPaths":[],
                            "status":if duplicate { "duplicate_call" } else { "pending" },
                        }));
                        if duplicate {
                            issue(
                                pairs[pair_index]["callPath"].as_str().unwrap().to_owned(),
                                "duplicate toolUseId",
                                Some(id),
                            );
                        }
                        pending.push_back((id.to_owned(), pair_index));
                    }
                    None => {
                        issue(path.clone(), "missing or empty toolUseId", None);
                        pairs.push(json!({
                            "callPath":path,
                            "resultPaths":[],
                            "status":"missing_id",
                        }));
                    }
                }
                if !call["input"].is_object() {
                    issue(
                        path.clone(),
                        "input is not an object",
                        call["toolUseId"].as_str(),
                    );
                }
                if !call["name"]
                    .as_str()
                    .is_some_and(|name| definitions.contains(&name.to_ascii_lowercase()))
                {
                    issue(
                        path,
                        "tool name has no current definition",
                        call["toolUseId"].as_str(),
                    );
                }
            }
        } else {
            for (result_index, reply) in replies.iter().enumerate() {
                let result_path = format!(
                    "{path}.userInputMessage.userInputMessageContext.toolResults[{result_index}]"
                );
                let Some(id) = reply["toolUseId"]
                    .as_str()
                    .filter(|id| !id.trim().is_empty())
                else {
                    issue(result_path.clone(), "missing or empty toolUseId", None);
                    pairs.push(json!({"resultPath":result_path,"status":"missing_id"}));
                    continue;
                };
                let Some(position) = pending.iter().position(|(pending_id, _)| pending_id == id)
                else {
                    let reason = if seen.contains(id) {
                        "duplicate tool result for an already matched call"
                    } else {
                        "orphan tool result has no preceding call"
                    };
                    issue(result_path.clone(), reason, Some(id));
                    pairs.push(json!({
                        "idFingerprint":id_fingerprint(id),
                        "resultPath":result_path,
                        "status":if seen.contains(id) { "duplicate_result" } else { "orphan_result" },
                    }));
                    continue;
                };
                let out_of_order = position > 0;
                let (pending_id, pair_index) = pending.remove(position).unwrap();
                let pair = &mut pairs[pair_index];
                pair["resultPaths"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(result_path));
                if pair["status"] != "duplicate_call" {
                    pair["status"] = json!(if out_of_order {
                        "out_of_order"
                    } else {
                        "matched"
                    });
                }
                if out_of_order {
                    issue(
                        pair["callPath"].as_str().unwrap_or_default().to_owned(),
                        "tool result order does not match tool call order",
                        Some(&pending_id),
                    );
                }
            }
        }
    }
    while let Some((id, pair_index)) = pending.pop_front() {
        pairs[pair_index]["status"] = json!("missing_result");
        issue(
            pairs[pair_index]["callPath"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            "tool call has no result before the request ends",
            Some(&id),
        );
    }
    let tool_history_messages = tool_history.len();
    if tool_history_messages > 12 {
        tool_history.drain(..tool_history_messages - 12);
    }
    let mut pair_counts = serde_json::Map::new();
    for pair in &pairs {
        if let Some(status) = pair["status"].as_str() {
            let count = pair_counts
                .entry(status.to_owned())
                .or_insert_with(|| json!(0));
            *count = json!(count.as_u64().unwrap_or_default() + 1);
        }
    }
    json!({
        "toolUses":uses, "toolResults":results,
        "pairCounts":pair_counts,
        "historyMessagesWithTools":tool_history_messages,
        "recentToolHistory":tool_history,
        "historyTruncated":tool_history_messages > 12,
        "issueCount":issue_count, "issues":issues, "issuesTruncated":issue_count > 24,
        "pairs":pairs.iter().take(256).cloned().collect::<Vec<_>>(),
        "pairsTruncated":pairs.len() > 256,
    })
}

fn known(value: &Value, allowed: &[&str]) -> Value {
    value
        .as_str()
        .filter(|v| allowed.contains(v))
        .map_or(Value::Null, |v| json!(v))
}

fn legacy_tag(content: &str, name: &str) -> Value {
    let Some((_, tail)) = content.split_once(&format!("<{name}>")) else {
        return Value::Null;
    };
    let Some((value, _)) = tail.split_once(&format!("</{name}>")) else {
        return Value::Null;
    };
    if name == "max_thinking_length" {
        return value.parse::<u64>().ok().map_or(Value::Null, |n| json!(n));
    }
    known(
        &json!(value),
        &[
            "enabled",
            "adaptive",
            "between_tools",
            "disabled",
            "none",
            "low",
            "medium",
            "high",
            "xhigh",
            "max",
        ],
    )
}

fn message_summary(message: &Value, assistant: bool) -> Value {
    let context = &message["userInputMessageContext"];
    let count = |value: &Value| value.as_array().map_or(0, Vec::len);
    let reasoning = &message["reasoningContent"];
    json!({
        "role": if assistant { "assistant" } else { "user" },
        "textBytes": message["content"].as_str().map_or(0, str::len),
        "images": count(&message["images"]),
        "tools": count(&context["tools"]),
        "toolResults": count(&context["toolResults"]),
        "toolUses": count(&message["toolUses"]),
        "reasoningBytes": reasoning["reasoningText"]["text"].as_str().map_or(0, str::len),
        "hasSignature": reasoning["reasoningText"]["signature"].as_str().is_some_and(|s| !s.trim().is_empty()),
        "hasRedactedReasoning": reasoning.get("redactedContent").is_some_and(|v| !v.is_null()),
    })
}

pub(crate) fn summarize_request(
    body: &str,
    native: bool,
    debug: bool,
) -> Option<UpstreamRequestDiagnostics> {
    let request: Value = serde_json::from_str(body).ok()?;
    let state = request.get("conversationState")?;
    let fields = &request["additionalModelRequestFields"];
    let history = state["history"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    // Only inspect the leading instruction message for legacy control tags.
    // Arbitrary user text is never copied into diagnostics.
    let prefix = history
        .first()
        .and_then(|m| m.pointer("/userInputMessage/content"))
        .and_then(Value::as_str)
        .filter(|s| s.starts_with("<thinking_mode>") || s.starts_with("<thinking_effort>"))
        .unwrap_or_default();
    let mut result = UpstreamRequestDiagnostics {
        body_bytes: body.len(),
        thinking_transport: if native { "native" } else { "legacy" }.into(),
        request_debug_enabled: debug,
        model_controls: json!({
            "maxTokens": fields["max_tokens"].as_u64(),
            "thinkingType": known(&fields["thinking"]["type"], &["enabled", "adaptive", "disabled", "between_tools"]),
            "thinkingBudgetTokens": fields["thinking"]["budget_tokens"].as_u64(),
            "thinkingDisplay": known(&fields["thinking"]["display"], &["summarized", "omitted"]),
            "effort": known(&fields["output_config"]["effort"], &["low", "medium", "high", "xhigh", "max"]),
            "reasoningEffort": known(&fields["reasoning"]["effort"], &["none", "low", "medium", "high", "xhigh", "max"]),
        }),
        legacy_controls: json!({
            "thinkingType": legacy_tag(prefix, "thinking_mode"),
            "thinkingBudgetTokens": legacy_tag(prefix, "max_thinking_length"),
            "effort": legacy_tag(prefix, "thinking_effort"),
        }),
        history_messages: history.len(),
        history_user_messages: 0,
        history_assistant_messages: 0,
        history_reasoning_messages: 0,
        history_signed_reasoning_messages: 0,
        history_redacted_reasoning_messages: 0,
        history_text_bytes: 0,
        recent_history: Vec::new(),
        history_summary_truncated: history.len() > 12,
        current_message: message_summary(&state["currentMessage"]["userInputMessage"], false),
        tool_diagnostics: Some(tool_diagnostics(state)),
    };
    for (index, message) in history.iter().enumerate() {
        let (message, assistant) = if let Some(message) = message.get("assistantResponseMessage") {
            result.history_assistant_messages += 1;
            (message, true)
        } else if let Some(message) = message.get("userInputMessage") {
            result.history_user_messages += 1;
            (message, false)
        } else {
            continue;
        };
        result.history_reasoning_messages += usize::from(
            message
                .get("reasoningContent")
                .is_some_and(|v| !v.is_null()),
        );
        let mut summary = message_summary(message, assistant);
        result.history_text_bytes += summary["textBytes"].as_u64().unwrap_or(0) as usize;
        result.history_signed_reasoning_messages += usize::from(summary["hasSignature"] == true);
        result.history_redacted_reasoning_messages +=
            usize::from(summary["hasRedactedReasoning"] == true);
        if index >= history.len().saturating_sub(12) {
            summary["index"] = json!(index);
            result.recent_history.push(summary);
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_native_controls_and_history_are_visible_without_debug_or_secrets() {
        let history: Vec<_> = (0..20).map(|_| json!({"assistantResponseMessage":{
            "content":"private-answer", "toolUses":[{}],
            "reasoningContent":{"reasoningText":{"text":"private-thought","signature":"private-signature"}}
        }})).collect();
        let body = json!({"profileArn":"private-profile","conversationState":{
            "history":history,"currentMessage":{"userInputMessage":{"content":"private-user","userInputMessageContext":{"tools":[{},{}],"toolResults":[{}]}}}
        },"additionalModelRequestFields":{"max_tokens":4096,"thinking":{"type":"adaptive","display":"omitted","extra":"private-control"},"output_config":{"effort":"high"}}}).to_string();
        let diagnostic =
            serde_json::to_value(summarize_request(&body, true, false).unwrap()).unwrap();
        assert_eq!(diagnostic["thinkingTransport"], "native");
        assert_eq!(diagnostic["requestDebugEnabled"], false);
        assert_eq!(diagnostic["modelControls"]["thinkingType"], "adaptive");
        assert_eq!(diagnostic["modelControls"]["maxTokens"], 4096);
        assert_eq!(diagnostic["historySignedReasoningMessages"], 20);
        assert_eq!(diagnostic["recentHistory"].as_array().unwrap().len(), 12);
        assert_eq!(diagnostic["recentHistory"][0]["index"], 8);
        assert_eq!(diagnostic["historySummaryTruncated"], true);
        assert_eq!(diagnostic["currentMessage"]["tools"], 2);
        assert!(!diagnostic.to_string().contains("private"));
    }

    #[test]
    fn legacy_controls_are_read_from_final_wire_and_arbitrary_values_are_excluded() {
        let body = json!({"conversationState":{"history":[{"userInputMessage":{"content":"<thinking_mode>enabled</thinking_mode><max_thinking_length>2048</max_thinking_length><thinking_effort>high</thinking_effort>"}}],"currentMessage":{"userInputMessage":{"content":"private"}}}});
        let diagnostic =
            serde_json::to_value(summarize_request(&body.to_string(), false, true).unwrap())
                .unwrap();
        assert_eq!(
            diagnostic["legacyControls"],
            json!({"thinkingType":"enabled","thinkingBudgetTokens":2048,"effort":"high"})
        );
        assert!(diagnostic["modelControls"]["thinkingType"].is_null());
        let unsafe_body = body
            .to_string()
            .replace("enabled", "private-type")
            .replace("high", "private-effort");
        assert!(
            !serde_json::to_string(&summarize_request(&unsafe_body, false, false))
                .unwrap()
                .contains("private")
        );
        assert!(summarize_request("not-json", false, false).is_none());
        assert!(summarize_request(r#"{"jsonrpc":"2.0"}"#, false, false).is_none());
    }

    #[test]
    fn tool_history_before_the_recent_text_turns_remains_visible() {
        let mut history = vec![
            json!({"assistantResponseMessage":{"content":"private-answer","toolUses":[{
                "toolUseId":"private-id","name":"private-tool","input":{"secret":"private-input"}
            }]}}),
            json!({"userInputMessage":{"userInputMessageContext":{"toolResults":[{
                "toolUseId":"private-id","content":[{"text":"private-result"}]
            }]}}}),
        ];
        for _ in 0..98 {
            history.extend([
                json!({"assistantResponseMessage":{"content":"private-answer"}}),
                json!({"userInputMessage":{"content":"private-user"}}),
            ]);
        }
        let body = json!({"conversationState":{"history":history,"currentMessage":{
            "userInputMessage":{"userInputMessageContext":{"tools":[{
                "toolSpecification":{"name":"PRIVATE-TOOL"}
            }]}}
        }}})
        .to_string();
        let diagnostic =
            serde_json::to_value(summarize_request(&body, false, false).unwrap()).unwrap();
        assert_eq!(diagnostic["historyMessages"], 198);
        assert_eq!(diagnostic["recentHistory"][0]["index"], 186);
        let tools = &diagnostic["toolDiagnostics"];
        assert_eq!(tools["toolUses"], 1);
        assert_eq!(tools["toolResults"], 1);
        assert_eq!(tools["issueCount"], 0);
        assert_eq!(tools["historyMessagesWithTools"], 2);
        assert_eq!(tools["recentToolHistory"][0]["index"], 0);
        assert_eq!(tools["recentToolHistory"][1]["index"], 1);
        assert!(!diagnostic.to_string().contains("private"));
    }

    #[test]
    fn tool_diagnostics_check_adjacency_duplicates_input_and_definitions() {
        let state = json!({"history":[
            {"assistantResponseMessage":{"toolUses":[
                {"toolUseId":"private-id","name":"private-tool","input":{}}
            ]}},
            {"userInputMessage":{"content":"private-user"}},
            {"assistantResponseMessage":{"toolUses":[
                {"toolUseId":"private-id","name":"private-tool","input":null}
            ]}}
        ],"currentMessage":{"userInputMessage":{"userInputMessageContext":{"toolResults":[
            {"toolUseId":"private-id"}, {"toolUseId":"private-id"}
        ]}}}});
        let diagnostic = tool_diagnostics(&state);
        assert_eq!(diagnostic["toolUses"], 2);
        assert_eq!(diagnostic["toolResults"], 2);
        assert_eq!(diagnostic["issueCount"], 6);
        assert_eq!(
            diagnostic["issues"][0]["path"],
            "history[0].assistantResponseMessage.toolUses[0]"
        );
        assert_eq!(
            diagnostic["issues"][1]["path"],
            "history[0].assistantResponseMessage.toolUses[0]"
        );
        assert_eq!(
            diagnostic["issues"][5]["path"],
            "currentMessage.userInputMessage.userInputMessageContext.toolResults[1]"
        );
        assert!(!diagnostic.to_string().contains("private"));
    }

    #[test]
    fn tool_diagnostics_pair_ids_and_detect_order_orphans_and_duplicates() {
        let state = json!({"history":[
            {"assistantResponseMessage":{"toolUses":[
                {"toolUseId":"call-a","name":"read","input":{}},
                {"toolUseId":"call-b","name":"read","input":{}}
            ]}},
            {"userInputMessage":{"userInputMessageContext":{"toolResults":[
                {"toolUseId":"call-b"}
            ]}}},
            {"userInputMessage":{"userInputMessageContext":{"toolResults":[
                {"toolUseId":"call-a"}, {"toolUseId":"call-a"}, {"toolUseId":"unknown"}
            ]}}}
        ],"currentMessage":{"userInputMessage":{"userInputMessageContext":{"tools":[
            {"toolSpecification":{"name":"read"}}
        ]}}}});
        let diagnostic = tool_diagnostics(&state);
        assert_eq!(diagnostic["toolUses"], 2);
        assert_eq!(diagnostic["toolResults"], 4);
        assert_eq!(diagnostic["pairs"][0]["status"], "matched");
        assert_eq!(
            diagnostic["pairs"][0]["resultPaths"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(diagnostic["pairs"][1]["status"], "out_of_order");
        assert!(diagnostic["pairs"][0]["idFingerprint"].is_string());
        assert_eq!(diagnostic["pairs"][2]["status"], "duplicate_result");
        assert_eq!(diagnostic["pairs"][3]["status"], "orphan_result");
        assert_eq!(diagnostic["pairCounts"]["matched"], 1);
        assert_eq!(diagnostic["pairCounts"]["out_of_order"], 1);
        assert_eq!(diagnostic["pairCounts"]["duplicate_result"], 1);
        assert_eq!(diagnostic["pairCounts"]["orphan_result"], 1);
        assert!(
            diagnostic
                .to_string()
                .contains("tool result order does not match tool call order")
        );
        assert!(!diagnostic.to_string().contains("call-a"));
        assert!(!diagnostic.to_string().contains("call-b"));
    }

    #[test]
    fn tool_diagnostics_allow_split_results_across_consecutive_user_messages() {
        let state = json!({"history":[
            {"assistantResponseMessage":{"toolUses":[
                {"toolUseId":"call-a","name":"read","input":{}},
                {"toolUseId":"call-b","name":"read","input":{}}
            ]}},
            {"userInputMessage":{"userInputMessageContext":{"toolResults":[{"toolUseId":"call-a"}]}}},
            {"userInputMessage":{"userInputMessageContext":{"toolResults":[{"toolUseId":"call-b"}]}}}
        ],"currentMessage":{"userInputMessage":{"userInputMessageContext":{"tools":[
            {"toolSpecification":{"name":"read"}}
        ]}}}});
        let diagnostic = tool_diagnostics(&state);
        assert_eq!(diagnostic["issueCount"], 0);
        assert_eq!(diagnostic["pairs"][0]["status"], "matched");
        assert_eq!(diagnostic["pairs"][1]["status"], "matched");
    }

    #[test]
    fn tool_diagnostics_are_bounded_and_older_records_still_decode() {
        let history: Vec<_> = (0..100)
            .map(|_| {
                json!({"assistantResponseMessage":{"toolUses":[{
                    "toolUseId":"private-id","name":"private-tool","input":null
                }]}})
            })
            .collect();
        let body = json!({"conversationState":{"history":history,"currentMessage":{"userInputMessage":{}}}}).to_string();
        let mut diagnostic =
            serde_json::to_value(summarize_request(&body, false, false).unwrap()).unwrap();
        let tools = &diagnostic["toolDiagnostics"];
        assert_eq!(tools["recentToolHistory"].as_array().unwrap().len(), 12);
        assert_eq!(tools["recentToolHistory"][0]["index"], 88);
        assert_eq!(tools["historyTruncated"], true);
        assert_eq!(tools["issues"].as_array().unwrap().len(), 24);
        assert_eq!(tools["issuesTruncated"], true);
        assert!(!diagnostic.to_string().contains("private"));
        diagnostic
            .as_object_mut()
            .unwrap()
            .remove("toolDiagnostics");
        let old: UpstreamRequestDiagnostics = serde_json::from_value(diagnostic).unwrap();
        assert!(old.tool_diagnostics.is_none());
    }
}
