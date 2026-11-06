//! Durable request metadata and bounded response previews. Request bodies are never stored.
mod response;
pub(crate) use response::append_preview;
use response::{ResponsePreview, StreamResponsePreview};
#[cfg(test)]
pub(crate) mod tests;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, OnceLock},
    time::Instant,
};

use axum::{
    Json,
    body::Body,
    extract::{Extension, Path as AxumPath, State},
    http::{HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use futures::StreamExt;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{
    Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context};

const MAX_ROWS: usize = 100_000;
const MAX_CAPTURE: usize = 1024 * 1024;

/// Response structure, counts, bounded assistant text and upstream stop reason.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpstreamResponseDiagnostics {
    pub outcome: String,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub body_bytes: u64,
    pub chunks: u64,
    pub frames: u64,
    pub event_types: BTreeMap<String, u64>,
    /// Bounded field paths sampled from the first frame of each event type.
    pub event_fields: BTreeMap<String, Vec<String>>,
    pub unknown_events: u64,
    pub text_bytes: u64,
    pub non_whitespace_text_bytes: u64,
    pub reasoning_bytes: u64,
    pub redacted_reasoning_bytes: u64,
    pub tool_input_bytes: u64,
    /// None when decoding stopped early with unexamined bytes remaining.
    pub incomplete_frame: Option<bool>,
    /// None for older records that did not distinguish buffered from partial frames.
    pub buffered_bytes: Option<u64>,
    pub pending_tool_calls: usize,
    pub thinking_enabled: bool,
    pub content: String,
    pub content_truncated: bool,
    pub stop_reason: Option<String>,
    pub refusal_category: Option<String>,
    pub refusal_explanation: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    timestamp: i64,
    level: String,
    target: String,
    message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    timestamp: i64,
    credential_id: u64,
    endpoint: String,
    status: Option<u16>,
    error: Option<String>,
    upstream_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_headers_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    first_byte_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_bytes: Option<u64>,
    /// Transport EOF only; response validation can still fail afterward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body_complete: Option<bool>,
    /// Latest valid credit usage reported by this upstream call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kiro_credits: Option<f64>,
    #[serde(skip)]
    started: Option<Instant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response_diagnostics: Option<UpstreamResponseDiagnostics>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    id: String,
    timestamp: i64,
    method: String,
    path: String,
    model: Option<String>,
    stream: bool,
    status: String,
    http_status: u16,
    duration_ms: u64,
    first_byte_ms: Option<u64>,
    /// 最后一次带会话标识的调度结果；旧日志没有此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    affinity: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    /// Sum of reported upstream credit usage, including retries and compaction.
    #[serde(default)]
    kiro_credits: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<ResponsePreview>,
    credential_id: Option<u64>,
    attempts: usize,
    error: Option<String>,
    upstream_attempts: Vec<Attempt>,
    diagnostics: Vec<Diagnostic>,
}

impl Record {
    fn new(method: &str, path: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now().timestamp_millis(),
            method: method.into(),
            path: path.into(),
            model: None,
            stream: false,
            status: "running".into(),
            http_status: 0,
            duration_ms: 0,
            first_byte_ms: None,
            affinity: None,
            input_tokens: None,
            output_tokens: None,
            kiro_credits: None,
            response: None,
            credential_id: None,
            attempts: 0,
            error: None,
            upstream_attempts: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

type RequestContext = Arc<Mutex<Record>>;
tokio::task_local! { static CURRENT: RequestContext; }

/// Redact common token formats, secret fields and proxy URL credentials in error text.
pub(crate) fn redact(message: &str) -> String {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| [
        r#"(?i)\b(?:authorization|x-api-key|(?:access|refresh)[_-]?token|client[_-]?secret|(?:(?:admin|kiro)[_-]?)?api[_-]?key|proxy[_-]?password|password)\b["']?\s*[:=]\s*(?:"(?:\\.|[^"\\])*(?:"|\\?$)|'(?:\\.|[^'\\])*(?:'|\\?$)|(?:(?:bearer|basic)\s+)?[^\s,;"'}]+)"#,
        r#"(?i)\bbearer\s+[^\s,;"}]+"#,
        r"\b(?:sk-|aik_|ksk_)[A-Za-z0-9_\-]+",
        r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
        r"(?i)(?:https?|socks5h?)://[^\s/@]+@",
    ].iter().map(|pattern| regex::Regex::new(pattern).expect("valid redaction regex")).collect());
    let mut result = message.to_owned();
    for pattern in patterns {
        result = pattern.replace_all(&result, "[REDACTED]").into_owned();
    }
    result
}

/// Apply the same redaction to JSON and SSE errors; bound untrusted diagnostic text.
pub(crate) fn public_error_message(message: &str) -> String {
    redact(message)
        .chars()
        .take(4096)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

pub fn request_metadata(model: &str, stream: bool) {
    let _ = CURRENT.try_with(|context| {
        let mut record = context.lock();
        if record.model.is_none() {
            record.model = Some(redact(&model.chars().take(256).collect::<String>()));
            record.stream = stream;
        }
    });
}

pub(crate) fn affinity_selection(outcome: &str) {
    let _ = CURRENT.try_with(|context| {
        context.lock().affinity = Some(outcome.to_owned());
    });
}

pub fn attempt_started(credential_id: u64, endpoint: &str) {
    let _ = CURRENT.try_with(|context| {
        let mut record = context.lock();
        record.credential_id = Some(credential_id);
        record.attempts += 1;
        record.upstream_attempts.push(Attempt {
            timestamp: Utc::now().timestamp_millis(),
            credential_id,
            endpoint: endpoint.into(),
            status: None,
            error: None,
            upstream_request_id: None,
            response_headers_ms: None,
            first_byte_ms: None,
            duration_ms: None,
            body_bytes: None,
            body_complete: None,
            kiro_credits: None,
            started: Some(Instant::now()),
            response_diagnostics: None,
        });
    });
}

pub fn attempt_result(status: Option<u16>, error: Option<&str>, upstream_id: Option<&str>) {
    let _ = CURRENT.try_with(|context| {
        if let Some(attempt) = context.lock().upstream_attempts.last_mut() {
            if status.is_some() && attempt.response_headers_ms.is_none() {
                attempt.response_headers_ms = attempt.elapsed_ms();
                attempt.body_bytes = Some(0);
                attempt.body_complete = Some(false);
            }
            if error.is_some() {
                attempt.duration_ms = attempt.elapsed_ms();
            }
            attempt.status = status;
            attempt.error = error.map(redact);
            attempt.upstream_request_id = upstream_id.map(redact);
        }
    });
}

impl Attempt {
    fn elapsed_ms(&self) -> Option<u64> {
        self.started
            .map(|started| started.elapsed().as_millis() as u64)
    }
}

/// Capture metering immediately so cancellation or a later decode error cannot discard it.
/// Repeated reports replace this call's value rather than double-counting it.
pub(crate) fn attempt_kiro_credits(credits: f64) {
    if !credits.is_finite() || credits < 0.0 {
        return;
    }
    let _ = CURRENT.try_with(|context| {
        let mut record = context.lock();
        let Some((attempt, previous)) = record.upstream_attempts.split_last_mut() else {
            return;
        };
        let total = previous
            .iter()
            .filter_map(|attempt| attempt.kiro_credits)
            .sum::<f64>()
            + credits;
        if total.is_finite() {
            attempt.kiro_credits = Some(credits);
            record.kiro_credits = Some(total);
        }
    });
}

/// Track raw upstream bytes independently of decoded content or downstream SSE pings.
pub(crate) fn attempt_body_chunk(chunk: &[u8]) {
    let _ = CURRENT.try_with(|context| {
        if let Some(attempt) = context.lock().upstream_attempts.last_mut() {
            if !chunk.is_empty() && attempt.first_byte_ms.is_none() {
                attempt.first_byte_ms = attempt.elapsed_ms();
            }
            let bytes = attempt.body_bytes.get_or_insert(0);
            *bytes = bytes.saturating_add(chunk.len() as u64);
        }
    });
}

pub(crate) fn attempt_body_finished(error: Option<&str>) {
    let _ = CURRENT.try_with(|context| {
        if let Some(attempt) = context.lock().upstream_attempts.last_mut() {
            attempt.duration_ms = attempt.elapsed_ms();
            attempt.body_complete = Some(error.is_none());
            if let Some(error) = error {
                attempt.error = Some(redact(error));
            }
        }
    });
}

/// Body validation happens after HTTP success. Preserve that HTTP status and request ID.
pub fn attempt_response_finished(diagnostics: UpstreamResponseDiagnostics, error: Option<&str>) {
    let _ = CURRENT.try_with(|context| {
        if let Some(attempt) = context.lock().upstream_attempts.last_mut() {
            attempt.duration_ms = attempt.elapsed_ms();
            attempt.response_diagnostics = Some(diagnostics);
            if let Some(error) = error {
                attempt.error = Some(redact(error));
            }
        }
    });
}

pub fn error_details(error: impl std::fmt::Display) {
    let _ = CURRENT.try_with(|context| context.lock().error = Some(redact(&error.to_string())));
}

pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn apply_usage(record: &mut Record, usage: &Value) {
    if let Some(iterations) = usage["iterations"].as_array().filter(|v| !v.is_empty()) {
        record.input_tokens = Some(
            iterations
                .iter()
                .filter_map(|v| v["input_tokens"].as_u64())
                .sum(),
        );
        record.output_tokens = Some(
            iterations
                .iter()
                .filter_map(|v| v["output_tokens"].as_u64())
                .sum(),
        );
    } else {
        if let Some(n) = usage["input_tokens"].as_u64() {
            record.input_tokens = Some(n);
        }
        if let Some(n) = usage["output_tokens"].as_u64() {
            record.output_tokens = Some(n);
        }
    }
}

/// Capture usage and a bounded response preview even when the JSON exceeds MAX_CAPTURE.
pub fn response_usage(message: &Value) {
    let _ = CURRENT.try_with(|context| {
        let mut record = context.lock();
        apply_usage(&mut record, &message["usage"]);
        if let Some(response) = ResponsePreview::from_message(message) {
            record.response = Some(response);
        }
    });
}

/// Capture warnings and full error chains inside the request, including stream polling.
pub struct DiagnosticLayer;
impl<S: Subscriber> Layer<S> for DiagnosticLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        struct Visitor(String);
        impl Visit for Visitor {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if !self.0.is_empty() {
                    self.0.push(' ');
                }
                if field.name() != "message" {
                    self.0.push_str(field.name());
                    self.0.push('=');
                }
                self.0.push_str(&format!("{value:?}"));
            }
        }
        let _ = CURRENT.try_with(|context| {
            let mut record = context.lock();
            if record.diagnostics.len() == 256 {
                record.diagnostics.push(Diagnostic {
                    timestamp: Utc::now().timestamp_millis(),
                    level: "WARN".into(),
                    target: "request_log".into(),
                    message: "诊断记录超过 256 条，后续警告省略；最终错误和上游错误仍单独保存"
                        .into(),
                });
                return;
            }
            if record.diagnostics.len() > 256 {
                return;
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            record.diagnostics.push(Diagnostic {
                timestamp: Utc::now().timestamp_millis(),
                level: event.metadata().level().to_string(),
                target: event.metadata().target().into(),
                message: redact(&visitor.0),
            });
        });
    }
}

pub struct LogStore {
    connection: Mutex<Connection>,
    retention_days: u32,
    writes: std::sync::atomic::AtomicUsize,
}

impl LogStore {
    pub fn open(path: &Path, retention_days: u32) -> anyhow::Result<Arc<Self>> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        // Create privately before SQLite opens it (also protects WAL/SHM creation on Unix).
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS request_logs (
                id TEXT PRIMARY KEY, timestamp INTEGER NOT NULL, model TEXT,
                credential_id INTEGER, status TEXT NOT NULL, http_status INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL, input_tokens INTEGER, output_tokens INTEGER,
                data TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS logs_time ON request_logs(timestamp DESC);
            CREATE INDEX IF NOT EXISTS logs_status_time ON request_logs(status, timestamp DESC);
            CREATE INDEX IF NOT EXISTS logs_model_time ON request_logs(model, timestamp DESC);
            CREATE INDEX IF NOT EXISTS logs_credential_time ON request_logs(credential_id, timestamp DESC);
            UPDATE request_logs SET status='interrupted', data=json_set(data,
                '$.status','interrupted','$.error','服务重启，调用结果未知') WHERE status='running';
        ")?;
        let store = Arc::new(Self {
            connection: Mutex::new(connection),
            retention_days: retention_days.max(1),
            writes: std::sync::atomic::AtomicUsize::new(0),
        });
        store.prune(&store.connection.lock())?;
        Ok(store)
    }

    fn prune(&self, connection: &Connection) -> anyhow::Result<()> {
        let cutoff = Utc::now().timestamp_millis() - i64::from(self.retention_days) * 86_400_000;
        connection.execute("DELETE FROM request_logs WHERE timestamp < ?", [cutoff])?;
        connection.execute(
            "DELETE FROM request_logs WHERE id IN (
            SELECT id FROM request_logs ORDER BY timestamp DESC, id DESC LIMIT -1 OFFSET ?)",
            [MAX_ROWS],
        )?;
        Ok(())
    }

    fn save(&self, record: &Record) -> anyhow::Result<()> {
        let connection = self.connection.lock();
        connection.execute("INSERT INTO request_logs
            (id,timestamp,model,credential_id,status,http_status,duration_ms,input_tokens,output_tokens,data)
            VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET
                timestamp=excluded.timestamp,model=excluded.model,credential_id=excluded.credential_id,
                status=excluded.status,http_status=excluded.http_status,duration_ms=excluded.duration_ms,
                input_tokens=excluded.input_tokens,output_tokens=excluded.output_tokens,data=excluded.data
            WHERE request_logs.status='running' OR excluded.status!='running'", params![record.id, record.timestamp, record.model,
                record.credential_id, record.status, record.http_status, record.duration_ms,
                record.input_tokens, record.output_tokens, serde_json::to_string(record)?])?;
        if self
            .writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .is_multiple_of(128)
        {
            self.prune(&connection)?;
        }
        Ok(())
    }

    async fn save_async(self: &Arc<Self>, record: Record) {
        let store = self.clone();
        match tokio::task::spawn_blocking(move || store.save(&record)).await {
            Ok(Ok(())) => {}
            error => tracing::error!("写入调用日志失败: {error:?}"),
        }
    }

    fn query(&self, query: LogQuery) -> anyhow::Result<Value> {
        let page = query.page.unwrap_or(1).clamp(1, 1_000_000);
        let page_size = query.page_size.unwrap_or(20).clamp(1, 100);
        let mut clauses = vec!["timestamp >= ?".to_owned()];
        let mut values: Vec<SqlValue> = vec![
            (Utc::now().timestamp_millis() - i64::from(self.retention_days) * 86_400_000).into(),
        ];
        for (column, value) in [("model", query.model), ("status", query.status)] {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                clauses.push(format!("{column} = ?"));
                values.push(value.into());
            }
        }
        if let Some(id) = query.credential_id {
            clauses.push("(credential_id = ? OR EXISTS (SELECT 1 FROM json_each(request_logs.data, '$.upstreamAttempts') a
                WHERE json_extract(a.value, '$.credentialId') = ?))".into());
            values.push(id.into());
            values.push(id.into());
        }
        for (clause, value) in [
            ("http_status = ?", query.http_status),
            ("timestamp >= ?", query.from),
            ("timestamp <= ?", query.to),
        ] {
            if let Some(value) = value {
                clauses.push(clause.into());
                values.push(value.into());
            }
        }
        if let Some(search) = query.search.filter(|v| !v.is_empty()) {
            clauses.push("instr(lower(data), lower(?)) > 0".into());
            values.push(search.into());
        }
        let filter = clauses.join(" AND ");
        let connection = self.connection.lock();
        let summary = connection.query_row(
            &format!(
                "SELECT COUNT(*),
            COALESCE(SUM(status='success'),0), COALESCE(SUM(status='error'),0),
            COALESCE(SUM(status='interrupted'),0), COALESCE(SUM(status='running'),0),
            COALESCE(SUM(input_tokens),0),COALESCE(SUM(output_tokens),0),
            COALESCE(AVG(CASE WHEN status!='running' THEN duration_ms END),0),
            SUM(json_extract(data, '$.kiroCredits')),
            COUNT(json_extract(data, '$.kiroCredits'))
            FROM request_logs WHERE {filter}"
            ),
            params_from_iter(values.iter()),
            |row| {
                Ok(
                    json!({"total":row.get::<_,u64>(0)?, "success":row.get::<_,u64>(1)?,
                    "error":row.get::<_,u64>(2)?, "interrupted":row.get::<_,u64>(3)?,
                    "running":row.get::<_,u64>(4)?, "inputTokens":row.get::<_,u64>(5)?,
                    "outputTokens":row.get::<_,u64>(6)?, "averageDurationMs":row.get::<_,f64>(7)?,
                    "kiroCredits":row.get::<_,Option<f64>>(8)?, "meteredRequests":row.get::<_,u64>(9)?}),
                )
            },
        )?;
        let mut statement = connection.prepare(&format!(
            "SELECT data FROM request_logs WHERE {filter}
            ORDER BY timestamp DESC, id DESC LIMIT ? OFFSET ?"
        ))?;
        values.push(page_size.into());
        values.push(((page - 1) * page_size).into());
        let rows = statement.query_map(params_from_iter(values.iter()), |row| {
            row.get::<_, String>(0)
        })?;
        let mut items = Vec::new();
        for row in rows {
            let mut item: Value = serde_json::from_str(&row?)?;
            item.as_object_mut().unwrap().remove("diagnostics");
            item.as_object_mut().unwrap().remove("upstreamAttempts");
            item.as_object_mut().unwrap().remove("response");
            items.push(item);
        }
        Ok(
            json!({"items":items,"summary":summary,"page":page,"pageSize":page_size,
            "retentionDays":self.retention_days,"maxRows":MAX_ROWS}),
        )
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    model: Option<String>,
    status: Option<String>,
    credential_id: Option<i64>,
    http_status: Option<i64>,
    from: Option<i64>,
    to: Option<i64>,
    search: Option<String>,
}

fn query_error(error: impl std::fmt::Display) -> Response {
    tracing::error!("查询调用日志失败: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":{"message":"日志查询失败，请查看服务运行日志"}})),
    )
        .into_response()
}

pub async fn list(
    Extension(store): Extension<Arc<LogStore>>,
    crate::admin::extract::AdminQuery(query): crate::admin::extract::AdminQuery<LogQuery>,
) -> Response {
    if query.from.zip(query.to).is_some_and(|(from, to)| from > to) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"开始时间不能晚于结束时间"}})),
        )
            .into_response();
    }
    match tokio::task::spawn_blocking(move || store.query(query)).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(error)) => query_error(error),
        Err(error) => query_error(error),
    }
}

pub async fn detail(
    Extension(store): Extension<Arc<LogStore>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> {
        let content: Option<String> = store
            .connection
            .lock()
            .query_row(
                "SELECT data FROM request_logs WHERE id=? AND timestamp>=?",
                params![
                    id,
                    Utc::now().timestamp_millis() - i64::from(store.retention_days) * 86_400_000
                ],
                |r| r.get(0),
            )
            .optional()?;
        content
            .map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    })
    .await
    {
        Ok(Ok(Some(record))) => Json(record).into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":{"message":"日志不存在或已过期"}})),
        )
            .into_response(),
        Ok(Err(error)) => query_error(error),
        Err(error) => query_error(error),
    }
}

struct Observer {
    store: Arc<LogStore>,
    context: RequestContext,
    started: Instant,
    is_sse: bool,
    buffer: Vec<u8>,
    line: Vec<u8>,
    discard_line: bool,
    stop: bool,
    finished: bool,
    response_preview: StreamResponsePreview,
}

impl Observer {
    fn observe_json(&mut self, value: &Value) {
        if self.is_sse {
            self.response_preview.observe(value);
        }
        let mut record = self.context.lock();
        apply_usage(&mut record, &value["usage"]);
        if !self.is_sse
            && let Some(response) = ResponsePreview::from_message(value)
        {
            record.response = Some(response);
        }
        if record.path.ends_with("/count_tokens") {
            record.input_tokens = value["input_tokens"].as_u64();
        }
        if value["error"].is_object() || value["type"] == "error" {
            record.status = "error".into();
            if record.error.is_none() {
                record.error = Some(redact(&value["error"].to_string()));
            }
        }
        match value["type"].as_str() {
            Some("message_start") => apply_usage(&mut record, &value["message"]["usage"]),
            Some("message_stop") => self.stop = true,
            _ => {}
        }
    }

    fn observe(&mut self, chunk: &[u8]) {
        if !chunk.is_empty() && self.context.lock().first_byte_ms.is_none() {
            self.context.lock().first_byte_ms = Some(self.started.elapsed().as_millis() as u64);
        }
        if !self.is_sse {
            if self.discard_line {
                return;
            }
            if self.buffer.len() + chunk.len() <= MAX_CAPTURE {
                self.buffer.extend_from_slice(chunk);
            } else {
                self.discard_line = true;
                self.buffer.clear();
            }
            return;
        }
        for &byte in chunk {
            if byte == b'\n' {
                if !self.discard_line {
                    let line = std::mem::take(&mut self.line);
                    if let Some(data) = line.strip_prefix(b"data:")
                        && let Ok(value) = serde_json::from_slice::<Value>(data)
                    {
                        self.observe_json(&value);
                    }
                }
                self.line.clear();
                self.discard_line = false;
            } else if !self.discard_line {
                if self.line.len() < MAX_CAPTURE {
                    self.line.push(byte);
                } else {
                    self.line.clear();
                    self.discard_line = true;
                    self.response_preview.mark_truncated();
                }
            }
        }
    }

    fn finish(&mut self, eof: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.is_sse && !self.buffer.is_empty() && !self.discard_line {
            if let Ok(value) = serde_json::from_slice::<Value>(&self.buffer) {
                self.observe_json(&value);
            } else {
                let mut record = self.context.lock();
                if record.http_status >= 400 && record.error.is_none() {
                    record.error = Some(redact(&String::from_utf8_lossy(&self.buffer)));
                }
            }
        }
        let record = {
            let mut record = self.context.lock();
            if self.is_sse {
                record.response = std::mem::take(&mut self.response_preview).finish();
            }
            record.duration_ms = self.started.elapsed().as_millis() as u64;
            if record.status != "error" {
                record.status = if (self.is_sse && !self.stop) || (!eof && !self.stop) {
                    if record.error.is_none() {
                        record.error = Some(if record.http_status == 0 {
                            "请求在返回客户端响应头前被取消；请检查客户端或反向代理超时（非流式请求需等待完整生成）".into()
                        } else {
                            "响应未完整结束：客户端/反向代理断开或流被取消".into()
                        });
                    }
                    "interrupted"
                } else {
                    "success"
                }
                .into();
            }
            if record.status == "error" && record.error.is_none() {
                record.error = Some(format!("HTTP {}，响应未提供错误详情", record.http_status));
            }
            let interrupted = record.status == "interrupted";
            if let Some(attempt) = record.upstream_attempts.last_mut()
                && attempt.duration_ms.is_none()
            {
                attempt.duration_ms = attempt.elapsed_ms();
                if interrupted && attempt.error.is_none() {
                    attempt.error = Some(if attempt.status.is_none() {
                        "调用结束时尚未取得上游响应头".into()
                    } else {
                        "调用结束时上游响应体尚未完整读取".into()
                    });
                }
            }
            record.clone()
        };
        let store = self.store.clone();
        // Blocking writes finish during Tokio runtime shutdown as well.
        tokio::task::spawn_blocking(move || {
            if let Err(error) = store.save(&record) {
                tracing::error!("写入调用日志失败: {error:#}");
            }
        });
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.finish(false);
    }
}

pub async fn middleware(
    State(store): State<Arc<LogStore>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if !(path.starts_with("/v1/") || path.starts_with("/cc/v1/")) || request.method() == "OPTIONS" {
        return next.run(request).await;
    }
    let started = Instant::now();
    let context = Arc::new(Mutex::new(Record::new(request.method().as_str(), path)));
    let mut observer = Observer {
        store,
        context: context.clone(),
        started,
        is_sse: false,
        buffer: Vec::new(),
        line: Vec::new(),
        discard_line: false,
        stop: false,
        finished: false,
        response_preview: StreamResponsePreview::default(),
    };
    let initial = context.lock().clone();
    observer.store.save_async(initial).await;
    let mut response = CURRENT.scope(context.clone(), next.run(request)).await;
    {
        let mut record = context.lock();
        record.http_status = response.status().as_u16();
        if response.status().is_client_error() || response.status().is_server_error() {
            record.status = "error".into();
        }
        response
            .headers_mut()
            .insert("x-request-id", HeaderValue::from_str(&record.id).unwrap());
    }
    observer.is_sse = response
        .headers()
        .get("content-type")
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    let mut active = context.lock().clone();
    active.status = "running".into();
    observer.store.save_async(active).await;
    let (parts, body) = response.into_parts();
    let stream = futures::stream::unfold(
        (body.into_data_stream(), observer),
        move |(mut body, mut observer)| {
            let context = context.clone();
            async move {
                if observer.finished {
                    return None;
                }
                match CURRENT.scope(context, body.next()).await {
                    Some(Ok(bytes)) => {
                        observer.observe(&bytes);
                        Some((Ok(bytes), (body, observer)))
                    }
                    Some(Err(error)) => {
                        {
                            let mut record = observer.context.lock();
                            record.status = "error".into();
                            record.error =
                                Some(redact(&format!("响应读取失败: {}", error_chain(&error))));
                        }
                        observer.finish(false);
                        Some((Err(error), (body, observer)))
                    }
                    None => {
                        observer.finish(true);
                        None
                    }
                }
            }
        },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}
