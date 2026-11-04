//! Strict response decoding shared by buffered, streaming and JSON responses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use futures::{StreamExt, stream::BoxStream};

use crate::kiro::model::events::Event;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::{KiroProvider, UpstreamResponse};
use crate::request_log::{UpstreamResponseDiagnostics, attempt_response_finished};

/// Empty completions get two additional attempts. Never replay emitted content.
const MAX_EMPTY_RESPONSE_RETRIES: usize = 2;
const MAX_EMPTY_PREFIX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum ResponsePolicy {
    Conversation { thinking_enabled: bool },
    Summary,
}

/// Decode and validate a response before treating EOF as a successful completion.
/// Withhold metadata/blank prefixes so retries cannot leak stale usage or content.
/// Summaries are already buffered by their caller; validate their extracted text.
pub(super) fn response_events(
    provider: Arc<KiroProvider>,
    request_body: String,
    is_stream: bool,
    response: UpstreamResponse,
    policy: ResponsePolicy,
) -> BoxStream<'static, anyhow::Result<Vec<Event>>> {
    struct State {
        body: Option<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>,
        credential_id: u64,
        decoder: UpstreamResponseDecoder,
        prefix: Vec<Event>,
        prefix_bytes: usize,
        summary: String,
        text_prefix: String,
        has_content: bool,
        limited: bool,
        retries: usize,
        done: bool,
    }
    let state = State {
        credential_id: response.credential_id(),
        decoder: UpstreamResponseDecoder::for_response(&response, policy),
        body: Some(response.bytes_stream()),
        prefix: Vec::new(),
        prefix_bytes: 0,
        summary: String::new(),
        text_prefix: String::new(),
        has_content: false,
        limited: false,
        retries: 0,
        done: false,
    };
    let request_body: Arc<str> = request_body.into();
    futures::stream::try_unfold(state, move |mut state| {
        let provider = provider.clone();
        let request_body = request_body.clone();
        async move {
            if state.done {
                return Ok(None);
            }
            loop {
                if let Some(chunk) = state.body.as_mut().unwrap().next().await {
                    let chunk = chunk.inspect_err(|error| {
                        state.decoder.record_failure(
                            state.credential_id,
                            state.retries + 1,
                            "read_error",
                            crate::request_log::error_chain(error),
                        );
                    })?;
                    let events = match state.decoder.feed(&chunk) {
                        Ok(events) => events,
                        Err(error) => {
                            state.decoder.record_failure(
                                state.credential_id,
                                state.retries + 1,
                                "decode_error",
                                format!("{error:#}"),
                            );
                            return Err(error);
                        }
                    };
                    for event in &events {
                        match event {
                            Event::AssistantResponse(response) => match policy {
                                ResponsePolicy::Summary => {
                                    state.summary.push_str(&response.content)
                                }
                                ResponsePolicy::Conversation { thinking_enabled } => {
                                    if !state.has_content {
                                        state.text_prefix.push_str(&response.content);
                                        state.has_content = has_meaningful_text(
                                            &state.text_prefix,
                                            thinking_enabled,
                                        );
                                        if state.has_content {
                                            state.text_prefix.clear();
                                        }
                                    }
                                }
                            },
                            Event::ReasoningContent(reasoning)
                                if matches!(
                                    policy,
                                    ResponsePolicy::Conversation {
                                        thinking_enabled: true
                                    }
                                ) =>
                            {
                                state.has_content |= !reasoning.text.trim().is_empty()
                                    || reasoning
                                        .redacted_content
                                        .as_ref()
                                        .is_some_and(|s| !s.trim().is_empty());
                            }
                            // Even partial tools commit the attempt: an incomplete tool
                            // must fail validation, never be silently replayed.
                            Event::ToolUse(_) => state.has_content = true,
                            Event::ContextUsage(usage)
                                if usage.context_usage_percentage >= 100.0 =>
                            {
                                state.limited = true;
                            }
                            Event::Exception { exception_type, .. }
                                if exception_type == "ContentLengthExceededException" =>
                            {
                                state.limited = true
                            }
                            _ => {}
                        }
                    }
                    state.prefix.extend(events);
                    if state.has_content && matches!(policy, ResponsePolicy::Conversation { .. }) {
                        return Ok(Some((std::mem::take(&mut state.prefix), state)));
                    }
                    state.prefix_bytes += chunk.len();
                    if matches!(policy, ResponsePolicy::Conversation { .. })
                        && state.prefix_bytes > MAX_EMPTY_PREFIX_BYTES
                    {
                        state.decoder.record_failure(
                            state.credential_id,
                            state.retries + 1,
                            "prefix_limit",
                            "Upstream response exceeded the empty prefix limit",
                        );
                        bail!("Upstream response exceeded the empty prefix limit");
                    }
                    continue;
                }

                if let Err(error) = state.decoder.finish() {
                    state.decoder.record_failure(
                        state.credential_id,
                        state.retries + 1,
                        "incomplete",
                        format!("{error:#}"),
                    );
                    return Err(error);
                }
                // Release the credential reservation before any retry/backoff.
                state.body.take();
                if matches!(policy, ResponsePolicy::Summary) {
                    let (_, text) =
                        super::stream::extract_thinking_from_complete_text(&state.summary);
                    state.has_content |= !text.trim().is_empty();
                }
                if state.has_content || state.limited {
                    attempt_response_finished(
                        state.decoder.diagnostics(if state.limited {
                            "limited"
                        } else {
                            "success"
                        }),
                        None,
                    );
                    provider.report_response_success(state.credential_id);
                    state.done = true;
                    return Ok(Some((std::mem::take(&mut state.prefix), state)));
                }
                state.decoder.record_failure(
                    state.credential_id,
                    state.retries + 1,
                    "empty",
                    format!("Empty upstream response: {}", state.decoder.empty_reason()),
                );
                if state.retries == MAX_EMPTY_RESPONSE_RETRIES {
                    bail!(
                        "Empty upstream response after {} attempts",
                        state.retries + 1
                    );
                }
                state.retries += 1;
                tracing::warn!(
                    credential_id = state.credential_id,
                    retry = state.retries,
                    max_retries = MAX_EMPTY_RESPONSE_RETRIES,
                    "Empty upstream response; retrying"
                );
                tokio::time::sleep(Duration::from_millis(200 * state.retries as u64)).await;
                let response = if is_stream {
                    provider.call_api_stream(&request_body).await?
                } else {
                    provider.call_api(&request_body).await?
                };
                state.credential_id = response.credential_id();
                state.decoder = UpstreamResponseDecoder::for_response(&response, policy);
                state.body = Some(response.bytes_stream());
                state.prefix.clear();
                state.prefix_bytes = 0;
                state.summary.clear();
                state.text_prefix.clear();
                state.has_content = false;
                state.limited = false;
            }
        }
    })
    .boxed()
}

fn has_meaningful_text(text: &str, thinking_enabled: bool) -> bool {
    let text = text.trim();
    if !thinking_enabled {
        return !text.is_empty();
    }
    // Wait for a split opening/closing tag instead of treating markup as output.
    if "<thinking>".starts_with(text) {
        return false;
    }
    if let Some(after) = text.strip_prefix("<thinking>")
        && "</thinking>".starts_with(after.trim())
    {
        return false;
    }
    let (thinking, remaining) = super::stream::extract_thinking_from_complete_text(text);
    thinking.is_some_and(|s| !s.trim().is_empty()) || !remaining.trim().is_empty()
}

#[derive(Default)]
pub(super) struct UpstreamResponseDecoder {
    frames: EventStreamDecoder,
    pending_tools: HashMap<String, String>,
    diagnostics: UpstreamResponseDiagnostics,
}

impl UpstreamResponseDecoder {
    fn empty_reason(&self) -> &'static str {
        let diagnostics = &self.diagnostics;
        if diagnostics.body_bytes == 0 {
            "zero body bytes received"
        } else if diagnostics.unknown_events > 0 {
            "no meaningful content; unrecognized events received"
        } else if diagnostics.text_bytes > 0 && diagnostics.non_whitespace_text_bytes == 0 {
            "only whitespace text received"
        } else if diagnostics.text_bytes > 0 {
            "text did not contain meaningful output (e.g. empty thinking markup)"
        } else if diagnostics.reasoning_bytes > 0 || diagnostics.redacted_reasoning_bytes > 0 {
            "reasoning content did not satisfy the response policy"
        } else {
            "only metadata or empty content events received"
        }
    }

    fn for_response(response: &UpstreamResponse, policy: ResponsePolicy) -> Self {
        Self {
            diagnostics: UpstreamResponseDiagnostics {
                content_type: response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(diagnostic_label),
                content_length: response
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok()),
                thinking_enabled: matches!(
                    policy,
                    ResponsePolicy::Conversation {
                        thinking_enabled: true
                    }
                ),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn diagnostics(&self, outcome: &str) -> UpstreamResponseDiagnostics {
        UpstreamResponseDiagnostics {
            outcome: outcome.into(),
            incomplete_frame: self.frames.has_pending_data(),
            pending_tool_calls: self.pending_tools.len(),
            ..self.diagnostics.clone()
        }
    }

    fn record_failure(
        &self,
        credential_id: u64,
        attempt: usize,
        outcome: &str,
        error: impl std::fmt::Display,
    ) {
        let diagnostics = self.diagnostics(outcome);
        let error = crate::request_log::redact(&error.to_string());
        attempt_response_finished(diagnostics.clone(), Some(&error));
        tracing::warn!(
            credential_id,
            attempt,
            error = %error,
            response_diagnostics = %serde_json::to_string(&diagnostics).unwrap_or_default(),
            "Upstream response validation failed"
        );
    }

    pub fn feed(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<Event>> {
        self.diagnostics.body_bytes += chunk.len() as u64;
        self.diagnostics.chunks += 1;
        self.frames.feed(chunk)?;
        let mut events = Vec::new();
        // Response content must not silently skip corrupt frames or payloads.
        for frame in self.frames.decode_iter() {
            let frame = frame?;
            self.diagnostics.frames += 1;
            // Cap cardinality and label sizes even if an upstream emits arbitrary names.
            let kind = match frame.message_type().unwrap_or("event") {
                "event" => frame.event_type().unwrap_or("unknown"),
                "exception" => frame.headers.exception_type().unwrap_or("exception"),
                "error" => frame.headers.error_code().unwrap_or("error"),
                other => other,
            };
            let mut kind = diagnostic_label(kind);
            if !self.diagnostics.event_types.contains_key(&kind)
                && self.diagnostics.event_types.len() >= MAX_DIAGNOSTIC_EVENT_TYPES
            {
                kind = "[other]".into();
            }
            *self
                .diagnostics
                .event_types
                .entry(kind.clone())
                .or_default() += 1;
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.diagnostics.event_fields.entry(kind)
            {
                let mut fields = Vec::new();
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&frame.payload) {
                    diagnostic_fields(&value, "", 0, &mut fields);
                }
                entry.insert(fields);
            }
            let event = Event::from_frame(frame)?;
            match &event {
                Event::AssistantResponse(response) => {
                    self.diagnostics.text_bytes += response.content.len() as u64;
                    self.diagnostics.non_whitespace_text_bytes += response
                        .content
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .map(|c| c.len_utf8() as u64)
                        .sum::<u64>();
                }
                Event::ReasoningContent(reasoning) => {
                    self.diagnostics.reasoning_bytes += reasoning.text.len() as u64;
                    self.diagnostics.redacted_reasoning_bytes += reasoning
                        .redacted_content
                        .as_ref()
                        .map_or(0, |s| s.len() as u64);
                }
                Event::ToolUse(tool) => {
                    self.diagnostics.tool_input_bytes += tool.input.len() as u64
                }
                Event::Unknown {} => self.diagnostics.unknown_events += 1,
                _ => {}
            }
            match &event {
                Event::Error {
                    error_code,
                    error_message,
                } => bail!("{error_code}: {error_message}"),
                Event::Exception {
                    exception_type,
                    message,
                } if exception_type != "ContentLengthExceededException" => {
                    bail!("{exception_type}: {message}");
                }
                Event::ToolUse(tool) => {
                    let input = self
                        .pending_tools
                        .entry(tool.tool_use_id.clone())
                        .or_default();
                    input.push_str(&tool.input);
                    if tool.stop {
                        if !input.is_empty() {
                            serde_json::from_str::<serde_json::Value>(input).map_err(|error| {
                                anyhow::anyhow!(
                                    "Invalid input for upstream tool {}: {error}",
                                    tool.tool_use_id
                                )
                            })?;
                        }
                        self.pending_tools.remove(&tool.tool_use_id);
                    }
                }
                _ => {}
            }
            events.push(event);
        }
        Ok(events)
    }

    pub fn finish(&self) -> anyhow::Result<()> {
        if self.frames.has_pending_data() {
            bail!("Incomplete upstream response frame");
        }
        if !self.pending_tools.is_empty() {
            bail!("Incomplete upstream tool call");
        }
        Ok(())
    }
}

const MAX_DIAGNOSTIC_EVENT_TYPES: usize = 32;
const MAX_DIAGNOSTIC_FIELDS: usize = 32;

fn diagnostic_label(name: &str) -> String {
    crate::request_log::redact(name)
        .chars()
        .take(96)
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// Sample object structure only. Arrays and scalar values may contain user data.
fn diagnostic_fields(
    value: &serde_json::Value,
    prefix: &str,
    depth: usize,
    fields: &mut Vec<String>,
) {
    if depth >= 3 {
        return;
    }
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            if fields.len() >= MAX_DIAGNOSTIC_FIELDS {
                break;
            }
            let path = if prefix.is_empty() {
                diagnostic_label(key)
            } else {
                format!("{prefix}/{}", diagnostic_label(key))
            };
            fields.push(path.clone());
            diagnostic_fields(value, &path, depth + 1, fields);
        }
    }
}
