//! Test-only raw requests. No credentials, actual tools or production rewrites.
use serde_json::{Value, json};

pub(super) fn candidates() -> anyhow::Result<Vec<(String, Value, Value)>> {
    let user = |content: Value| json!({"role":"user","content":content});
    let assistant = |content: Value| json!({"role":"assistant","content":content});
    let call = |id: &str, value: &str| json!({"type":"tool_use","id":id,"name":"probe_echo","input":{"value":value}});
    let result =
        |id: &str, value: &str| json!({"type":"tool_result","tool_use_id":id,"content":value});
    let prompt = "Do not call tools. Report every audit marker from the history in order, including tool input and result markers. Keep the answer short.";
    let end = || user(prompt.into());
    let mut inputs: Vec<(String, Vec<Value>)> = Vec::new();
    for (label, second_id) in [("id-unique-turns", "second"), ("id-reused-turns", "same")] {
        inputs.push((
            label.into(),
            vec![
                user("Inspect both turns.".into()),
                assistant(json!([call("same", "INPUT_A_31")])),
                user(json!([result("same", "RESULT_A_41")])),
                assistant(json!([call(second_id, "INPUT_B_53")])),
                user(json!([result(second_id,"RESULT_B_67"), {"type":"text","text":prompt}])),
            ],
        ));
    }
    for (label, id) in [
        ("id-unicode", "调用_甲".to_owned()),
        ("id-punctuation", "id :/ x!?".to_owned()),
        ("id-long-256", "x".repeat(256)),
    ] {
        inputs.push((
            label.into(),
            vec![
                assistant(json!([call(&id, "INPUT_A_31")])),
                user(json!([result(&id,"RESULT_A_41"),{"type":"text","text":prompt}])),
            ],
        ));
    }
    for (label, separate, conflict, twice) in [
        ("duplicate-identical-one-result", false, false, false),
        ("duplicate-identical-two-results", false, false, true),
        ("duplicate-conflicting-one-result", false, true, false),
        ("duplicate-conflicting-two-results", false, true, true),
        ("duplicate-separate-one-result", true, false, false),
        ("duplicate-separate-two-results", true, false, true),
    ] {
        let a = call("same", "INPUT_A_31");
        let b = call("same", if conflict { "INPUT_B_53" } else { "INPUT_A_31" });
        let mut messages = vec![user("Inspect calls.".into())];
        if separate {
            messages.extend([assistant(json!([a])), assistant(json!([b]))]);
        } else {
            messages.push(assistant(json!([a, b])));
        }
        let mut results = vec![result("same", "RESULT_A_41")];
        if twice {
            results.push(result("same", "RESULT_B_67"));
        }
        results.push(json!({"type":"text","text":prompt}));
        messages.push(user(json!(results)));
        inputs.push((label.into(), messages));
    }
    for (label, messages) in [
        (
            "users-current-separated",
            vec![user("USER_A_17".into()), user("USER_B_29".into()), end()],
        ),
        (
            "users-current-merged",
            vec![user(format!("USER_A_17\nUSER_B_29\n{prompt}").into())],
        ),
        (
            "users-history-separated",
            vec![
                user("USER_A_17".into()),
                user("USER_B_29".into()),
                assistant("ASSISTANT_C_43".into()),
                end(),
            ],
        ),
        (
            "users-history-merged",
            vec![
                user("USER_A_17\nUSER_B_29".into()),
                assistant("ASSISTANT_C_43".into()),
                end(),
            ],
        ),
        (
            "assistants-separated",
            vec![
                user("Remember the markers.".into()),
                assistant("ASSISTANT_A_17".into()),
                assistant("ASSISTANT_B_29".into()),
                end(),
            ],
        ),
        (
            "assistants-merged",
            vec![
                user("Remember the markers.".into()),
                assistant("ASSISTANT_A_17\n\nASSISTANT_B_29".into()),
                end(),
            ],
        ),
        (
            "assistants-whitespace",
            vec![
                user("USER_A_17".into()),
                assistant(" \n\t ".into()),
                assistant("ASSISTANT_B_29".into()),
                end(),
            ],
        ),
        (
            "users-empty",
            vec![
                user("USER_A_17".into()),
                user("".into()),
                assistant("ASSISTANT_B_29".into()),
                end(),
            ],
        ),
        (
            "assistants-empty",
            vec![
                user("USER_A_17".into()),
                assistant("".into()),
                assistant("ASSISTANT_B_29".into()),
                end(),
            ],
        ),
        (
            "calls-separated",
            vec![
                assistant(json!([call("a", "INPUT_A_31")])),
                assistant(json!([call("b", "INPUT_B_53")])),
                user(
                    json!([result("a","RESULT_A_41"),result("b","RESULT_B_67"),{"type":"text","text":prompt}]),
                ),
            ],
        ),
        (
            "results-current-separated",
            vec![
                assistant(json!([call("a", "INPUT_A_31"), call("b", "INPUT_B_53")])),
                user(json!([result("a", "RESULT_A_41")])),
                user(json!([result("b","RESULT_B_67"),{"type":"text","text":prompt}])),
            ],
        ),
        (
            "results-current-merged",
            vec![
                assistant(json!([call("a", "INPUT_A_31"), call("b", "INPUT_B_53")])),
                user(
                    json!([result("a","RESULT_A_41"),result("b","RESULT_B_67"),{"type":"text","text":prompt}]),
                ),
            ],
        ),
        (
            "results-history-separated",
            vec![
                assistant(json!([call("a", "INPUT_A_31"), call("b", "INPUT_B_53")])),
                user(json!([result("a", "RESULT_A_41")])),
                user(json!([result("b", "RESULT_B_67")])),
                assistant("Completed.".into()),
                end(),
            ],
        ),
        (
            "result-then-text-user",
            vec![
                assistant(json!([call("a", "INPUT_A_31")])),
                user(json!([result("a", "RESULT_A_41")])),
                end(),
            ],
        ),
    ] {
        inputs.push((label.into(), messages));
    }
    for (label, messages) in [
        ("recall-users-current", vec![user("My name is Alice.".into()), user("My friend's name is Beatrice.".into()), user("What are our two names? Answer only the names.".into())]),
        ("recall-users-current-merged", vec![user("My name is Alice.\nMy friend's name is Beatrice.\nWhat are our two names? Answer only the names.".into())]),
        ("recall-users-history", vec![user("My name is Alice.".into()), user("My friend's name is Beatrice.".into()), assistant("Nice to meet you both.".into()), user("What are our two names? Answer only the names.".into())]),
        ("recall-assistants", vec![user("Name two French cities.".into()), assistant("Lyon.".into()), assistant("Marseille.".into()), user("Which two cities did you just name? Answer only the cities.".into())]),
        ("recall-assistants-merged", vec![user("Name two French cities.".into()), assistant("Lyon.\n\nMarseille.".into()), user("Which two cities did you just name? Answer only the cities.".into())]),
        ("recall-results-split", vec![assistant(json!([call("a","apple"),call("b","pear")])),user(json!([result("a","red")])),user(json!([result("b","green"),{"type":"text","text":"What fruit and color did each echo call return? Answer briefly."}]))]),
        ("recall-calls-split", vec![assistant(json!([call("a","apple")])),assistant(json!([call("b","pear")])),user(json!([result("a","red"),result("b","green"),{"type":"text","text":"What fruit and color did each echo call return? Answer briefly."}]))]),
        ("recall-empty-assistant", vec![user("My name is Alice.".into()),assistant(json!([])),assistant("Hello Alice.".into()),user("What is my name? Answer only the name.".into())]),
    ] { inputs.push((label.into(), messages)); }
    let mut mixed_rounds = vec![user("Echo fruit names.".into())];
    for (name, value, color) in [
        ("kiro_native_web_search", "apple", "red"),
        ("KIRO_NATIVE_WEB_SEARCH", "pear", "green"),
        ("lookup", "lemon", "yellow"),
    ] {
        let mut block = call("call-1", value);
        block["name"] = name.into();
        mixed_rounds.extend([
            assistant(json!([block])),
            user(json!([result("call-1", color)])),
        ]);
    }
    mixed_rounds.push(user(
        "Which three fruits and colors appeared in the tool calls? Answer briefly.".into(),
    ));
    inputs.push(("id-reused-mixed-rounds".into(), mixed_rounds));
    let selected = std::env::var("KIRO_CONTROLS_PROBE_CASES").ok();
    let mut candidates = Vec::new();
    for (label, messages) in inputs {
        let input = json!({"model":"claude-haiku-5.5","max_tokens":1024,"messages":messages});
        for converted in [false, true] {
            let label = format!("{}-{label}", if converted { "converted" } else { "raw" });
            if selected
                .as_ref()
                .is_some_and(|s| !s.split(',').any(|x| x == label))
            {
                continue;
            }
            let mut state = if converted {
                serde_json::to_value(
                    super::super::converter::convert_request(&serde_json::from_value(
                        input.clone(),
                    )?)?
                    .conversation_state,
                )?
            } else {
                let mut wire: Vec<_> = messages.iter().map(raw_message).collect();
                let current = wire.pop().unwrap();
                json!({"conversationId":uuid::Uuid::new_v4().to_string(),"chatTriggerType":"MANUAL","history":wire,"currentMessage":current})
            };
            for m in state["history"].as_array_mut().into_iter().flatten() {
                if let Some(u) = m.get_mut("userInputMessage") {
                    u["origin"] = "KIRO_CLI".into();
                }
            }
            state["currentMessage"]["userInputMessage"]["origin"] = "KIRO_CLI".into();
            candidates.push((label,input.clone(),json!({"conversationState":state,"additionalModelRequestFields":{"max_tokens":1024,"thinking":{"type":"disabled"}}})));
        }
    }
    Ok(candidates)
}

// Deliberately independent of the production converter: preserve IDs, duplicates,
// message boundaries, empty content and whitespace for real upstream comparison.
fn raw_message(message: &Value) -> Value {
    let is_user = message["role"] == "user";
    let mut text = String::new();
    let mut tools = Vec::new();
    if let Some(s) = message["content"].as_str() {
        text = s.into();
    } else if let Some(blocks) = message["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("text")=>text.push_str(block["text"].as_str().unwrap()),
                Some("tool_use")=>tools.push(json!({"toolUseId":block["id"],"name":block["name"],"input":block["input"]})),
                Some("tool_result")=>tools.push(json!({"toolUseId":block["tool_use_id"],"status":"success","content":[{"text":block["content"]}]})),
                _=>panic!("Unexpected audit block"),
            }
        }
    }
    if is_user {
        let mut user = json!({"content":text,"modelId":"claude-haiku-5.5","origin":"KIRO_CLI"});
        if !tools.is_empty() {
            user["userInputMessageContext"] = json!({"toolResults":tools});
        }
        json!({"userInputMessage":user})
    } else {
        let mut assistant = json!({"content":text});
        if !tools.is_empty() {
            assistant["toolUses"] = json!(tools);
        }
        json!({"assistantResponseMessage":assistant})
    }
}

#[test]
fn original_ids_duplicates_and_message_boundaries_match_raw_fixtures() {
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("conversationId");
                map.remove("agentContinuationId");
                map.remove("agentTaskType");
                if map
                    .get("history")
                    .is_some_and(|h| h.is_null() || h == &json!([]))
                {
                    map.remove("history");
                }
                if map.get("userInputMessageContext") == Some(&json!({})) {
                    map.remove("userInputMessageContext");
                }
                for child in map.values_mut() {
                    normalize(child);
                }
            }
            Value::Array(items) => {
                for child in items {
                    normalize(child);
                }
            }
            _ => {}
        }
    }
    let cases = candidates().unwrap();
    let mut raw = std::collections::HashMap::new();
    for (label, _, mut candidate) in cases {
        normalize(&mut candidate);
        if let Some(label) = label.strip_prefix("raw-") {
            raw.insert(label.to_owned(), candidate);
        } else {
            let label = label.strip_prefix("converted-").unwrap();
            assert_eq!(candidate, raw[label], "wire fixture: {label}");
        }
    }
    assert!(!raw.is_empty());
}
