//! Stdout-line scanner that re-emits guest fallback telemetry as structured
//! `tracing` events, plus a [`TelemetryStream`] type that plugs directly into
//! `wasmtime_wasi::p2::WasiCtxBuilder::stdout` / `.stderr`.
//!
//! Guest components built against `greentic-telemetry >= 0.5.2` emit lines in
//! the following shape when the `wit-guest` feature is not enabled (the
//! default for the messaging providers today):
//!
//! ```text
//! 2026-05-25T10:30:42.123Z [DEBUG][messaging.webchat-gui] components/foo/src/lib.rs:160 span-start: send_payload [id=1, event_kind=send_payload]
//! ```
//!
//! Without this module those lines arrive at the operator's stdout as
//! free-form text and never enter the host's `tracing` pipeline, so they
//! cannot be exported to OTLP, filtered by level, or correlated with the
//! runner's own spans. This module parses each candidate line, extracts the
//! pieces, and re-emits via `tracing::event!` at the matching level.
//!
//! Lines that fail to parse are forwarded verbatim to the real stdout (or
//! stderr) so legacy `println!` output from components remains visible.
//! That keeps the runner safe to enable in front of any component
//! (telemetry-aware or not).

#[cfg(feature = "telemetry")]
use tracing::{Level as TracingLevel, debug, error, info, trace, warn};

/// Severity level parsed from a fallback line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParsedLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl ParsedLevel {
    fn from_token(token: &str) -> Option<Self> {
        match token {
            "TRACE" => Some(Self::Trace),
            "DEBUG" => Some(Self::Debug),
            "INFO" => Some(Self::Info),
            "WARN" => Some(Self::Warn),
            "ERROR" => Some(Self::Error),
            _ => None,
        }
    }
}

/// A single parsed telemetry line.
#[derive(Debug, PartialEq, Eq)]
pub struct TelemetryLine<'a> {
    /// RFC3339 UTC timestamp string when present (may be empty for legacy
    /// lines that did not include one).
    pub timestamp: &'a str,
    pub level: ParsedLevel,
    /// Component identity from `[<name>]` after the level. Empty when no
    /// identity was registered.
    pub component: &'a str,
    /// `file:line` caller location when present.
    pub file_line: &'a str,
    /// The free-form message (everything between the location segment and
    /// the optional `[fields]` block at the tail).
    pub message: &'a str,
    /// Parsed `key=value` pairs from the tail. Empty vec when absent.
    pub fields: Vec<(&'a str, &'a str)>,
}

/// Parse one stdout line emitted by `greentic-telemetry::wasm_guest`'s
/// fallback path. Returns `None` for any line that does not match the
/// expected shape so the caller can forward it through unchanged.
pub fn parse_line(line: &str) -> Option<TelemetryLine<'_>> {
    let line = line.trim_end_matches(['\r', '\n']);

    // Optional leading RFC3339 timestamp. The format is
    // `YYYY-MM-DDTHH:MM:SS.mmmZ` (24 chars). We detect it by checking the
    // first 24 chars rather than running a full regex.
    let (timestamp, rest) = split_optional_timestamp(line);
    let rest = rest.trim_start();

    // Required `[LEVEL]` token.
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    let level = ParsedLevel::from_token(&rest[..close])?;
    let mut rest = &rest[close + 1..];

    // Optional `[component]` token immediately after the level.
    let mut component = "";
    if let Some(after_open) = rest.strip_prefix('[')
        && let Some(close) = after_open.find(']')
    {
        component = &after_open[..close];
        rest = &after_open[close + 1..];
    }

    let rest = rest.trim_start();

    // Optional `file:line` token. Detect by looking for `<word>:<digit>+`
    // followed by a space. If the next whitespace-delimited token contains a
    // colon followed by digits, treat it as the caller location.
    let (file_line, rest_after_loc) = split_optional_file_line(rest);
    let rest = rest_after_loc.trim_start();

    // Split the remainder into `<message> [fields]?` by looking for the last
    // ` [` that opens a balanced bracket block at the tail. The message can
    // contain arbitrary characters except a trailing ` [k=v, ...]`.
    let (message, fields_block) = split_message_and_fields(rest);
    let fields = fields_block.map(parse_fields).unwrap_or_default();

    Some(TelemetryLine {
        timestamp,
        level,
        component,
        file_line,
        message: message.trim_end(),
        fields,
    })
}

fn split_optional_timestamp(line: &str) -> (&str, &str) {
    // RFC3339 with millisecond precision: 24 chars, ends with 'Z', has 'T' at
    // index 10. Cheap to verify.
    if line.len() >= 24
        && line.as_bytes().get(10) == Some(&b'T')
        && line.as_bytes().get(23) == Some(&b'Z')
        && line.as_bytes().get(24).map(|c| c.is_ascii_whitespace()).unwrap_or(false)
    {
        (&line[..24], &line[24..])
    } else {
        ("", line)
    }
}

fn split_optional_file_line(rest: &str) -> (&str, &str) {
    // The location token is the next whitespace-delimited chunk if it looks
    // like `<path>:<digits>` (digits + non-digit + ...). We accept any path
    // characters before the final `:digits` suffix.
    let Some(end) = rest.find(char::is_whitespace) else {
        return ("", rest);
    };
    let token = &rest[..end];
    if let Some(colon) = token.rfind(':')
        && token[colon + 1..].chars().all(|c| c.is_ascii_digit())
        && !token[colon + 1..].is_empty()
    {
        return (token, &rest[end..]);
    }
    ("", rest)
}

fn split_message_and_fields(rest: &str) -> (&str, Option<&str>) {
    // The fields block is always at the tail, opened by ` [` and closed by
    // the final `]`. We look for the *last* ` [` and verify the line ends
    // with `]`.
    if !rest.ends_with(']') {
        return (rest, None);
    }
    let Some(open) = rest.rfind(" [") else {
        return (rest, None);
    };
    let block = &rest[open + 2..rest.len() - 1];
    // Heuristic: if the block contains `=` it's almost certainly key=value.
    // Otherwise treat as part of the message (e.g. literal markdown).
    if block.contains('=') {
        (&rest[..open], Some(block))
    } else {
        (rest, None)
    }
}

fn parse_fields(block: &str) -> Vec<(&str, &str)> {
    block.split(", ").filter_map(|kv| kv.split_once('=')).collect()
}

/// Re-emit a parsed line through the `tracing` pipeline so it lands in any
/// OTLP exporter / subscriber configured by the host. Compiles to a no-op
/// when the `telemetry` feature is disabled.
#[cfg(feature = "telemetry")]
pub fn emit_as_tracing(line: &TelemetryLine<'_>) {
    let fields_json = fields_to_json(&line.fields);
    let component = line.component;
    let file_line = line.file_line;
    let message = line.message;

    // `tracing` macros require a static `Level`; we dispatch through the
    // matching macro per variant. `provider` is the canonical attribute key
    // matching what runner-host already emits for native spans.
    match line.level {
        ParsedLevel::Trace => trace!(
            provider = %component,
            source = %file_line,
            fields = %fields_json,
            "{message}"
        ),
        ParsedLevel::Debug => debug!(
            provider = %component,
            source = %file_line,
            fields = %fields_json,
            "{message}"
        ),
        ParsedLevel::Info => info!(
            provider = %component,
            source = %file_line,
            fields = %fields_json,
            "{message}"
        ),
        ParsedLevel::Warn => warn!(
            provider = %component,
            source = %file_line,
            fields = %fields_json,
            "{message}"
        ),
        ParsedLevel::Error => error!(
            provider = %component,
            source = %file_line,
            fields = %fields_json,
            "{message}"
        ),
    }
    // Quiet the unused-variable warning for the level when telemetry is on
    // but no subscriber is installed.
    let _ = TracingLevel::INFO;
}

#[cfg(not(feature = "telemetry"))]
pub fn emit_as_tracing(_line: &TelemetryLine<'_>) {}

#[cfg(feature = "telemetry")]
fn fields_to_json(fields: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(fields.len() * 16);
    out.push('{');
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&escape_json(k));
        out.push_str("\":\"");
        out.push_str(&escape_json(v));
        out.push('"');
    }
    out.push('}');
    out
}

#[cfg(feature = "telemetry")]
fn escape_json(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '\t' => vec!['\\', 't'],
            c if c.is_control() => format!("\\u{:04x}", c as u32).chars().collect(),
            c => vec![c],
        })
        .collect()
}

// === TelemetryStream: wasmtime-wasi StdoutStream adapter ===================

use std::io::Write as _;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

/// Which stream this captures (informs where unparsed lines are forwarded).
#[derive(Clone, Copy, Debug)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// A `StdoutStream` implementation that parses every emitted line through
/// [`parse_line`] and dispatches matched lines as `tracing` events, while
/// forwarding unmatched lines to the host's real stdout/stderr.
///
/// Drop this into `WasiCtxBuilder::stdout(...)` / `.stderr(...)` instead of
/// `inherit_stdio()` to make sure guest fallback telemetry never escapes as
/// raw text.
#[derive(Clone)]
pub struct TelemetryStream {
    kind: StreamKind,
    forward_unmatched: Arc<AtomicBool>,
}

impl TelemetryStream {
    pub fn stdout() -> Self {
        Self {
            kind: StreamKind::Stdout,
            forward_unmatched: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn stderr() -> Self {
        Self {
            kind: StreamKind::Stderr,
            forward_unmatched: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Disable forwarding of unparsed lines to the underlying OS stream.
    /// Useful when the operator expects telemetry to be the only output
    /// channel and prefers to discard non-conforming chatter.
    pub fn drop_unmatched(self) -> Self {
        self.forward_unmatched.store(false, Ordering::Relaxed);
        self
    }
}

impl IsTerminal for TelemetryStream {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for TelemetryStream {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(TelemetryLineSink {
            kind: self.kind,
            forward_unmatched: self.forward_unmatched.clone(),
            buffer: Mutex::new(Vec::new()),
        })
    }
}

struct TelemetryLineSink {
    kind: StreamKind,
    forward_unmatched: Arc<AtomicBool>,
    /// Per-stream byte accumulator. Each `poll_write` appends, then the
    /// scanner drains complete lines (ending in `\n`).
    buffer: Mutex<Vec<u8>>,
}

impl TelemetryLineSink {
    fn dispatch_lines(&self, complete: &[u8]) {
        // Split on '\n' so partial trailing lines stay buffered for the next
        // write.
        for raw in complete.split(|b| *b == b'\n') {
            if raw.is_empty() {
                continue;
            }
            // Trim a CR if present (Windows-style line endings).
            let raw = if raw.last() == Some(&b'\r') {
                &raw[..raw.len() - 1]
            } else {
                raw
            };
            let line = match std::str::from_utf8(raw) {
                Ok(s) => s,
                Err(_) => {
                    // Binary chunk; forward verbatim and move on.
                    if self.forward_unmatched.load(Ordering::Relaxed) {
                        forward_bytes(self.kind, raw);
                        forward_bytes(self.kind, b"\n");
                    }
                    continue;
                }
            };
            if let Some(parsed) = parse_line(line) {
                emit_as_tracing(&parsed);
            } else if self.forward_unmatched.load(Ordering::Relaxed) {
                forward_bytes(self.kind, raw);
                forward_bytes(self.kind, b"\n");
            }
        }
    }
}

fn forward_bytes(kind: StreamKind, bytes: &[u8]) {
    match kind {
        StreamKind::Stdout => {
            let _ = std::io::stdout().write_all(bytes);
        }
        StreamKind::Stderr => {
            let _ = std::io::stderr().write_all(bytes);
        }
    }
}

impl AsyncWrite for TelemetryLineSink {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        // Accumulate, then drain complete lines.
        let drained = {
            let mut guard = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
            guard.extend_from_slice(buf);
            // Find the index of the last '\n' so we keep partial trailing
            // bytes for the next write.
            let split_at = guard.iter().rposition(|b| *b == b'\n').map(|i| i + 1);
            split_at.map(|i| guard.drain(..i).collect::<Vec<_>>())
        };
        if let Some(complete) = drained {
            self.dispatch_lines(&complete);
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        // Flush any trailing partial line that didn't end with '\n'.
        let leftover = {
            let mut guard = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *guard)
        };
        if !leftover.is_empty() {
            self.dispatch_lines(&leftover);
        }
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_line_with_all_segments() {
        let line = "2026-05-25T10:30:42.123Z [DEBUG][messaging.webchat-gui] components/foo/src/lib.rs:160 span-start: send_payload [id=1, event_kind=send_payload]";
        let p = parse_line(line).expect("should parse");
        assert_eq!(p.timestamp, "2026-05-25T10:30:42.123Z");
        assert_eq!(p.level, ParsedLevel::Debug);
        assert_eq!(p.component, "messaging.webchat-gui");
        assert_eq!(p.file_line, "components/foo/src/lib.rs:160");
        assert_eq!(p.message, "span-start: send_payload");
        assert_eq!(
            p.fields,
            vec![("id", "1"), ("event_kind", "send_payload")]
        );
    }

    #[test]
    fn parses_line_without_component() {
        let line = "[INFO] doing work [k=v]";
        let p = parse_line(line).expect("should parse");
        assert_eq!(p.timestamp, "");
        assert_eq!(p.level, ParsedLevel::Info);
        assert_eq!(p.component, "");
        assert_eq!(p.file_line, "");
        assert_eq!(p.message, "doing work");
        assert_eq!(p.fields, vec![("k", "v")]);
    }

    #[test]
    fn parses_line_without_fields_block() {
        let line = "[WARN][svc] src/lib.rs:1 some warning";
        let p = parse_line(line).expect("should parse");
        assert_eq!(p.level, ParsedLevel::Warn);
        assert_eq!(p.component, "svc");
        assert_eq!(p.file_line, "src/lib.rs:1");
        assert_eq!(p.message, "some warning");
        assert!(p.fields.is_empty());
    }

    #[test]
    fn parses_legacy_line_format() {
        // Lines emitted by greentic-telemetry 0.5.1 (no timestamp, no
        // component, no file:line).
        let line = "[DEBUG] span-start: send_payload [event_kind=send_payload, provider=messaging.webchat-gui]";
        let p = parse_line(line).expect("should parse");
        assert_eq!(p.timestamp, "");
        assert_eq!(p.level, ParsedLevel::Debug);
        assert_eq!(p.component, "");
        assert_eq!(p.file_line, "");
        assert_eq!(p.message, "span-start: send_payload");
        assert_eq!(
            p.fields,
            vec![
                ("event_kind", "send_payload"),
                ("provider", "messaging.webchat-gui"),
            ]
        );
    }

    #[test]
    fn rejects_non_telemetry_lines() {
        // Lines from runner-host's own println! / log infrastructure should
        // not match.
        assert!(parse_line("[ws notifier:memory] publish tenant=demo").is_none());
        assert!(parse_line("secrets: backend=dev-store").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("plain text").is_none());
    }

    #[test]
    fn handles_trailing_newline() {
        let line = "[INFO] hello\n";
        let p = parse_line(line).expect("should parse");
        assert_eq!(p.message, "hello");
    }

    #[test]
    fn message_with_bracket_not_followed_by_equals_stays_in_message() {
        let line = "[INFO] config loaded [final state]";
        let p = parse_line(line).expect("should parse");
        // Heuristic: no `=` in trailing block, so the bracket section stays
        // part of the message.
        assert_eq!(p.message, "config loaded [final state]");
        assert!(p.fields.is_empty());
    }

    #[test]
    fn file_line_with_relative_path() {
        let line = "[ERROR][svc] crates/foo/src/lib.rs:42 oops [code=500]";
        let p = parse_line(line).expect("should parse");
        assert_eq!(p.file_line, "crates/foo/src/lib.rs:42");
        assert_eq!(p.message, "oops");
        assert_eq!(p.fields, vec![("code", "500")]);
    }
}
