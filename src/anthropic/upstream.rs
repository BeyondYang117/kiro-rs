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
// Bound memory while withholding tool calls until their arguments are valid.
const MAX_TOOL_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Exact counters can precede an error in the same network chunk. Carry only
/// reported usage through validation errors; never bill local estimates on failure.
#[derive(Debug)]
pub(super) struct UpstreamReportedUsage(pub serde_json::Value);

impl std::fmt::Display for UpstreamReportedUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Upstream reported usage before failure")
    }
}

pub(super) fn with_reported_usage(
    error: anyhow::Error,
    reported: &crate::kiro::model::events::ReportedUsage,
) -> anyhow::Error {
    if error.downcast_ref::<UpstreamReportedUsage>().is_some() {
        return error;
    }
    let mut usage = serde_json::json!({});
    reported.apply(&mut usage);
    if usage.as_object().is_some_and(|fields| !fields.is_empty()) {
        error.context(UpstreamReportedUsage(usage))
    } else {
        error
    }
}

/// An explicit upstream refusal is terminal; replaying the request cannot repair it.
#[derive(Debug)]
pub(super) struct UpstreamRefusalError(String);

impl UpstreamRefusalError {
    fn from_metadata(metadata: &crate::kiro::model::events::MetadataEvent) -> Self {
        let mut message = "Upstream refused the request".to_owned();
        if let Some(refusal) = metadata
            .stop_details
            .as_ref()
            .and_then(|details| details.refusal.as_ref())
        {
            if let Some(category) = &refusal.category {
                message.push_str(&format!(" (category: {category})"));
            }
            if let Some(explanation) = &refusal.explanation {
                message.push_str(": ");
                message.push_str(explanation);
            }
        }
        if let Some(reason) = &metadata.stop_reason {
            message.push_str(&format!(" [stopReason: {}]", diagnostic_label(reason)));
        }
        if metadata
            .stop_details
            .as_ref()
            .and_then(|details| details.refusal.as_ref())
            .and_then(|refusal| refusal.explanation.as_deref())
            .is_none_or(|explanation| explanation.trim().is_empty())
        {
            message.push_str(
                ". Review the request and conversation history; start a new conversation or select a different model if appropriate.",
            );
        }
        Self(
            crate::request_log::redact(&message)
                .chars()
                .take(2048)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
        )
    }
}

impl std::fmt::Display for UpstreamRefusalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UpstreamRefusalError {}

#[derive(Debug)]
pub(super) enum ToolCallError {
    Invalid(String),
    Truncated(String),
}

impl std::fmt::Display for ToolCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}. The invalid tool call was not forwarded for execution. Split file writes, edits, or shell commands into smaller chunks; do not retry the same oversized call unchanged.",
            match self {
                Self::Invalid(message) | Self::Truncated(message) => message,
            }
        )
    }
}

impl std::error::Error for ToolCallError {}

#[derive(Default)]
struct PendingTool {
    name: String,
    input: String,
}

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
    let required_fields = required_tool_fields(&request_body);
    struct State {
        request_body: String,
        body: Option<BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>>,
        credential_id: u64,
        affinity_key: Option<crate::kiro::token_manager::AffinityKey>,
        decoder: UpstreamResponseDecoder,
        prefix: Vec<Event>,
        prefix_bytes: usize,
        summary: String,
        text_prefix: String,
        has_content: bool,
        limited: bool,
        retries: usize,
        recoveries: u8,
        emitted_content: bool,
        done: bool,
        response_bytes: usize,
        response_events: usize,
        first_content_deadline: tokio::time::Instant,
    }
    impl State {
        async fn recover_tool_output(
            &mut self,
            error: &anyhow::Error,
            provider: &KiroProvider,
            is_stream: bool,
            policy: ResponsePolicy,
        ) -> anyhow::Result<bool> {
            if !matches!(
                error.downcast_ref::<ToolCallError>(),
                Some(ToolCallError::Truncated(_))
            ) || !matches!(policy, ResponsePolicy::Conversation { .. })
                || self.emitted_content
                || self.decoder.context_full
                || self.recoveries >= provider.tool_output_recovery_attempts()
                || self.decoder.pending_tools.is_empty()
                || !self.decoder.pending_tools.values().all(|tool| {
                    self.decoder
                        .required_fields
                        .contains_key(&tool.name.to_ascii_lowercase())
                })
            {
                return Ok(false);
            }
            let Some(body) = tool_recovery_request(&self.request_body, self.recoveries + 1) else {
                return Ok(false);
            };
            // No response content has left the decoder. Release capacity before
            // regenerating with a smaller chunk and discard the failed prefix.
            self.body.take();
            self.recoveries += 1;
            tracing::warn!(
                recovery = self.recoveries,
                "Recovering truncated tool output with smaller chunks"
            );
            let response = if is_stream {
                provider.call_api_stream(&body).await?
            } else {
                provider.call_api(&body).await?
            };
            self.request_body = body;
            self.credential_id = response.credential_id();
            self.affinity_key = response.affinity_key();
            let required_fields = std::mem::take(&mut self.decoder.required_fields);
            self.decoder =
                UpstreamResponseDecoder::for_response(&response, policy, required_fields);
            self.body = Some(response.bytes_stream());
            self.prefix.clear();
            self.prefix_bytes = 0;
            self.summary.clear();
            self.text_prefix.clear();
            self.has_content = false;
            self.limited = false;
            Ok(true)
        }
    }
    let limits = provider.request_limits().clone();
    let state = State {
        request_body,
        credential_id: response.credential_id(),
        affinity_key: response.affinity_key(),
        decoder: UpstreamResponseDecoder::for_response(&response, policy, required_fields),
        body: Some(response.bytes_stream()),
        prefix: Vec::new(),
        prefix_bytes: 0,
        summary: String::new(),
        text_prefix: String::new(),
        has_content: false,
        limited: false,
        retries: 0,
        recoveries: 0,
        emitted_content: false,
        done: false,
        response_bytes: 0,
        response_events: 0,
        first_content_deadline: tokio::time::Instant::now()
            + Duration::from_secs(limits.first_content_timeout_seconds),
    };
    futures::stream::try_unfold(state, move |mut state| {
        let provider = provider.clone();
        let limits = limits.clone();
        async move {
            if state.done {
                return Ok(None);
            }
            loop {
                let idle_deadline =
                    tokio::time::Instant::now() + Duration::from_secs(limits.idle_timeout_seconds);
                let deadline = if state.has_content {
                    idle_deadline
                } else {
                    idle_deadline.min(state.first_content_deadline)
                };
                let next = tokio::time::timeout_at(deadline, state.body.as_mut().unwrap().next())
                    .await
                    .map_err(|_| {
                        let message = if !state.has_content
                            && tokio::time::Instant::now() >= state.first_content_deadline
                        {
                            "Upstream first content deadline exceeded"
                        } else {
                            "Upstream idle timeout exceeded"
                        };
                        state.decoder.record_failure(
                            state.credential_id,
                            state.retries + usize::from(state.recoveries) + 1,
                            "timeout",
                            message,
                        );
                        with_reported_usage(anyhow::anyhow!(message), &state.decoder.reported_usage)
                    })?;
                if let Some(chunk) = next {
                    let chunk = chunk
                        .inspect_err(|error| {
                            state.decoder.record_failure(
                                state.credential_id,
                                state.retries + usize::from(state.recoveries) + 1,
                                "read_error",
                                crate::request_log::error_chain(error),
                            );
                        })
                        .map_err(|error| {
                            with_reported_usage(error.into(), &state.decoder.reported_usage)
                        })?;
                    state.response_bytes = state.response_bytes.saturating_add(chunk.len());
                    if state.response_bytes > limits.max_response_bytes {
                        state.decoder.record_failure(
                            state.credential_id,
                            state.retries + usize::from(state.recoveries) + 1,
                            "response_limit",
                            "Upstream response byte limit exceeded",
                        );
                        bail!("Upstream response byte limit exceeded");
                    }
                    let events = match state.decoder.feed(&chunk) {
                        Ok(events) => events,
                        Err(error) => {
                            state.decoder.record_failure(
                                state.credential_id,
                                state.retries + usize::from(state.recoveries) + 1,
                                if error.is::<UpstreamRefusalError>() {
                                    "refusal"
                                } else {
                                    "decode_error"
                                },
                                format!("{error:#}"),
                            );
                            if state
                                .recover_tool_output(&error, &provider, is_stream, policy)
                                .await?
                            {
                                continue;
                            }
                            return Err(error);
                        }
                    };
                    // Pending tools count as generated content, but remain eligible
                    // for bounded recovery until any response events are released.
                    state.has_content |= !state.decoder.pending_tools.is_empty();
                    state.response_events = state.response_events.saturating_add(events.len());
                    if state.response_events > limits.max_response_events {
                        bail!("Upstream response event limit exceeded");
                    }
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
                            Event::Metadata(metadata) if metadata.output_limited() => {
                                state.limited = true
                            }
                            _ => {}
                        }
                    }
                    state.prefix.extend(events);
                    if state.has_content && matches!(policy, ResponsePolicy::Conversation { .. }) {
                        // Empty batches contain only withheld tools, not client-visible content.
                        state.emitted_content |= !state.prefix.is_empty();
                        return Ok(Some((std::mem::take(&mut state.prefix), state)));
                    }
                    state.prefix_bytes += chunk.len();
                    if matches!(policy, ResponsePolicy::Conversation { .. })
                        && state.prefix_bytes > MAX_EMPTY_PREFIX_BYTES
                    {
                        state.decoder.record_failure(
                            state.credential_id,
                            state.retries + usize::from(state.recoveries) + 1,
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
                        state.retries + usize::from(state.recoveries) + 1,
                        "incomplete",
                        format!("{error:#}"),
                    );
                    if state
                        .recover_tool_output(&error, &provider, is_stream, policy)
                        .await?
                    {
                        continue;
                    }
                    return Err(with_reported_usage(error, &state.decoder.reported_usage));
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
                    provider.report_response_success(state.credential_id, state.affinity_key);
                    state.done = true;
                    return Ok(Some((std::mem::take(&mut state.prefix), state)));
                }
                state.decoder.record_failure(
                    state.credential_id,
                    state.retries + usize::from(state.recoveries) + 1,
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
                    provider.call_api_stream(&state.request_body).await?
                } else {
                    provider.call_api(&state.request_body).await?
                };
                state.credential_id = response.credential_id();
                state.affinity_key = response.affinity_key();
                let required_fields = std::mem::take(&mut state.decoder.required_fields);
                state.decoder =
                    UpstreamResponseDecoder::for_response(&response, policy, required_fields);
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

/// Keep history, tool results and schemas intact. Only strengthen the current
/// user instruction; never manufacture a tool result for an unexecuted call.
fn tool_recovery_request(request_body: &str, recovery: u8) -> Option<String> {
    use crate::kiro::model::requests::kiro::KiroRequest;
    let mut request: KiroRequest = serde_json::from_str(request_body).ok()?;
    let message = &mut request
        .conversation_state
        .current_message
        .user_input_message;
    let (lines, characters) = if recovery == 1 {
        (20, 4000)
    } else {
        (10, 2000)
    };
    message.content.push_str(&format!(
        "\n\n[Proxy tool output recovery {recovery}] The previous tool arguments were truncated upstream and were discarded before any response content was sent to the client. No tools from that attempt were executed. Regenerate a smaller complete call, not a continuation of partial JSON. Make only ONE tool call in this response, with at most {lines} lines and {characters} characters of generated content. For a large file, create a small skeleton first; subsequent client turns can fill it in. Do not claim the whole file is complete. Do not move the large payload into shell commands or encoded strings. Include every required argument and close the JSON object."
    ));
    serde_json::to_string(&request).ok()
}

/// Use the actual upstream names (including shortened MCP names) and normalized
/// schemas. Only check top-level required fields, not the full JSON Schema.
fn required_tool_fields(request_body: &str) -> HashMap<String, Vec<String>> {
    let Ok(request) =
        serde_json::from_str::<crate::kiro::model::requests::kiro::KiroRequest>(request_body)
    else {
        return HashMap::new();
    };
    request
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .tools
        .into_iter()
        .map(|tool| {
            let spec = tool.tool_specification;
            let required = spec
                .input_schema
                .json
                .get("required")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            (spec.name.to_ascii_lowercase(), required)
        })
        .collect()
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
pub(crate) struct UpstreamResponseDecoder {
    frames: EventStreamDecoder,
    decoding_stopped_early: bool,
    pending_tools: HashMap<String, PendingTool>,
    required_fields: HashMap<String, Vec<String>>,
    tool_events: Vec<Event>,
    tool_buffer_bytes: usize,
    context_full: bool,
    diagnostics: UpstreamResponseDiagnostics,
    reported_usage: crate::kiro::model::events::ReportedUsage,
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

    fn for_response(
        response: &UpstreamResponse,
        policy: ResponsePolicy,
        required_fields: HashMap<String, Vec<String>>,
    ) -> Self {
        Self {
            required_fields,
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
            content: crate::request_log::redact(&self.diagnostics.content),
            incomplete_frame: if self.decoding_stopped_early && self.frames.has_pending_data() {
                None
            } else {
                Some(self.frames.has_pending_data())
            },
            buffered_bytes: Some(self.frames.pending_bytes() as u64),
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
        self.decode_chunk(chunk)
            .map_err(|error| with_reported_usage(error, &self.reported_usage))
    }

    fn decode_chunk(&mut self, chunk: &[u8]) -> anyhow::Result<Vec<Event>> {
        self.diagnostics.body_bytes += chunk.len() as u64;
        self.diagnostics.chunks += 1;
        // An early error may leave complete frames in the buffer. Only call
        // them incomplete after the decode loop has examined all available frames.
        self.decoding_stopped_early = true;
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
            let payload_bytes = frame.payload.len();
            let event = Event::from_frame(frame)?;
            match &event {
                Event::Metering {
                    credits: Some(credits),
                } => {
                    crate::request_log::attempt_kiro_credits(*credits);
                }
                Event::AssistantResponse(response) => {
                    self.diagnostics.content_truncated |= crate::request_log::append_preview(
                        &mut self.diagnostics.content,
                        &response.content,
                        4096,
                    );
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
                Event::Metadata(metadata) => {
                    self.reported_usage.observe(metadata);
                    if let Some(reason) = metadata.stop_reason.as_deref() {
                        self.diagnostics.stop_reason = Some(diagnostic_label(reason));
                    }
                }
                Event::Unknown {} => self.diagnostics.unknown_events += 1,
                _ => {}
            }
            match &event {
                Event::Metadata(metadata) if metadata.is_refusal() => {
                    if let Some(refusal) = metadata
                        .stop_details
                        .as_ref()
                        .and_then(|details| details.refusal.as_ref())
                    {
                        self.diagnostics.refusal_category =
                            refusal.category.as_deref().map(diagnostic_label);
                        self.diagnostics.refusal_explanation =
                            refusal.explanation.as_deref().map(|explanation| {
                                crate::request_log::public_error_message(explanation)
                                    .chars()
                                    .take(2048)
                                    .collect()
                            });
                    }
                    return Err(UpstreamRefusalError::from_metadata(metadata).into());
                }
                Event::ContextUsage(usage) if usage.context_usage_percentage >= 100.0 => {
                    self.context_full = true;
                }
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
                Event::Exception { exception_type, .. }
                    if exception_type == "ContentLengthExceededException"
                        && !self.pending_tools.is_empty() =>
                {
                    return Err(ToolCallError::Truncated(
                        "Upstream output limit reached during tool generation".into(),
                    )
                    .into());
                }
                Event::Metadata(metadata)
                    if metadata.output_limited() && !self.pending_tools.is_empty() =>
                {
                    return Err(ToolCallError::Truncated(
                        "Upstream metadata reports max_tokens during tool generation".into(),
                    )
                    .into());
                }
                Event::ToolUse(tool) => {
                    if tool.tool_use_id.is_empty() || tool.name.is_empty() {
                        return Err(ToolCallError::Invalid(
                            "Upstream tool call has an empty ID or name".into(),
                        )
                        .into());
                    }
                    let pending = self
                        .pending_tools
                        .entry(tool.tool_use_id.clone())
                        .or_insert_with(|| PendingTool {
                            name: tool.name.clone(),
                            input: String::new(),
                        });
                    if pending.name != tool.name {
                        return Err(ToolCallError::Invalid(
                            "Upstream tool name changed during generation".into(),
                        )
                        .into());
                    }
                    // Check before copying payloads into the tool buffers.
                    if self.tool_buffer_bytes.saturating_add(payload_bytes) > MAX_TOOL_BUFFER_BYTES
                    {
                        return Err(ToolCallError::Invalid(
                            "Upstream tool call exceeds the 4 MiB buffer limit".into(),
                        )
                        .into());
                    }
                    pending.input.push_str(&tool.input);
                    if tool.stop {
                        let input = if pending.input.is_empty() {
                            serde_json::json!({})
                        } else {
                            serde_json::from_str::<serde_json::Value>(&pending.input).map_err(
                                |error| {
                                    let message = format!(
                                        "Invalid input for upstream tool {}: {error}",
                                        tool.tool_use_id
                                    );
                                    if error.is_eof() {
                                        ToolCallError::Truncated(message)
                                    } else {
                                        ToolCallError::Invalid(message)
                                    }
                                },
                            )?
                        };
                        let Some(input) = input.as_object() else {
                            return Err(ToolCallError::Invalid(
                                "Upstream tool input must be a JSON object".into(),
                            )
                            .into());
                        };
                        if let Some(required) =
                            self.required_fields.get(&pending.name.to_ascii_lowercase())
                            && required.iter().any(|key| !input.contains_key(key))
                        {
                            return Err(ToolCallError::Invalid(format!(
                                "Upstream tool {} is missing required arguments",
                                pending.name
                            ))
                            .into());
                        }
                        self.pending_tools.remove(&tool.tool_use_id);
                    }
                }
                _ => {}
            }
            // Preserve event order, including text between tool fragments. Never
            // expose an empty/partial tool_use that a client could try to execute.
            if matches!(event, Event::ToolUse(_)) || !self.tool_events.is_empty() {
                self.tool_buffer_bytes = self.tool_buffer_bytes.saturating_add(payload_bytes);
                if self.tool_buffer_bytes > MAX_TOOL_BUFFER_BYTES {
                    return Err(ToolCallError::Invalid(
                        "Upstream tool call exceeds the 4 MiB buffer limit".into(),
                    )
                    .into());
                }
                self.tool_events.push(event);
                if self.pending_tools.is_empty() {
                    events.append(&mut self.tool_events);
                    self.tool_buffer_bytes = 0;
                }
            } else {
                events.push(event);
            }
        }
        self.decoding_stopped_early = false;
        Ok(events)
    }

    pub fn finish(&self) -> anyhow::Result<()> {
        if self.frames.has_pending_data() {
            bail!("Incomplete upstream response frame");
        }
        if !self.pending_tools.is_empty() {
            return Err(ToolCallError::Truncated(
                "Incomplete upstream tool call (output truncated or missing stop event)".into(),
            )
            .into());
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
