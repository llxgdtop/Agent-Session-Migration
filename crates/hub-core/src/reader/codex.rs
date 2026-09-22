//! Codex session rollout JSONL parsing.
//!
//! Source layout: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`, a
//! line-delimited `{timestamp, type, payload}` event stream. Only
//! `response_item` payloads are consumed; `event_msg` duplicates information
//! already in `response_item`, so it is skipped on read; `session_meta` /
//! `turn_context` contribute metadata only; encrypted reasoning
//! (encrypted_content) cannot migrate across tools and is dropped. Bad lines
//! are skipped and counted.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::HubError;
use crate::ir::{Role, SessionSummary, UnifiedMessage, UnifiedPart, UnifiedSession};
use crate::reader::claude::{mtime_rfc3339, sort_key, title_of};

/// Scan all `rollout-*.jsonl` under root (usually ~/.codex/sessions,
/// recursing through date directories), ordered by last_active descending,
/// tie-broken at equal timestamps by session_id lexicographic order — the
/// same as reader::claude::scan_sessions. A single file failing to read
/// (including empty sessions) is skipped without affecting the overall scan;
/// if root itself does not exist, SourceNotFound is returned.
pub fn scan_sessions(root: &Path) -> Result<Vec<SessionSummary>, HubError> {
    if !root.is_dir() {
        return Err(HubError::SourceNotFound(root.to_path_buf()));
    }
    let mut files = Vec::new();
    collect_rollout_files(root, &mut files)?;
    let mut summaries = Vec::new();
    for file in files {
        if let Ok(session) = read_session(&file) {
            summaries.push(session.summary);
        }
    }
    summaries.sort_by(|a, b| {
        sort_key(b)
            .cmp(&sort_key(a))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    Ok(summaries)
}

/// Parse a single rollout file into IR. Bad lines are skipped and counted;
/// event_msg/session_meta/turn_context and other types produce no messages;
/// EmptySession is returned when no user/assistant message remains.
pub fn read_session(path: &Path) -> Result<UnifiedSession, HubError> {
    if !path.is_file() {
        return Err(HubError::SourceNotFound(path.to_path_buf()));
    }
    let content = fs::read_to_string(path)?;

    let mut messages = Vec::new();
    let mut parse_warnings = 0usize;
    // project_dir prefers the first turn_context.cwd, falling back to
    // session_meta.cwd when absent
    let mut project_dir = String::new();
    let mut meta_cwd: Option<String> = None;
    let mut last_active_line_ts: Option<String> = None;
    // call_id → function name, used to link a function_call_output back to
    // its tool name
    let mut tool_names: HashMap<String, String> = HashMap::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                parse_warnings += 1;
                continue;
            }
        };
        let line_ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let payload = value.get("payload").cloned().unwrap_or(Value::Null);
        let mut produced: Option<UnifiedMessage> = None;
        match value.get("type").and_then(Value::as_str) {
            // Record the cwd only; produces no message
            Some("session_meta") => {
                if meta_cwd.is_none() {
                    meta_cwd = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                }
            }
            Some("turn_context") => {
                if project_dir.is_empty() {
                    if let Some(cwd) = payload
                        .get("cwd")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        project_dir = cwd.to_string();
                    }
                }
            }
            // UI events duplicate response_item information; skip on read
            Some("event_msg") => {}
            Some("response_item") => {
                produced = match payload.get("type").and_then(Value::as_str) {
                    Some("message") => parse_message(&payload, &mut parse_warnings).map(|mut m| {
                        m.timestamp = line_ts.clone();
                        m
                    }),
                    Some("function_call") => {
                        let tool = payload
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let input_json = match payload.get("arguments") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => "null".to_string(),
                        };
                        if let Some(id) = payload.get("call_id").and_then(Value::as_str) {
                            tool_names.insert(id.to_string(), tool.clone());
                        }
                        Some(UnifiedMessage {
                            role: Role::Assistant,
                            parts: vec![UnifiedPart::ToolCall { tool, input_json }],
                            timestamp: line_ts.clone(),
                        })
                    }
                    Some("function_call_output") => {
                        let call_id = payload
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let tool = tool_names
                            .get(call_id)
                            .cloned()
                            .unwrap_or_else(|| call_id.to_string());
                        let output = match payload.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        Some(UnifiedMessage {
                            role: Role::User,
                            parts: vec![UnifiedPart::ToolResult {
                                tool,
                                content: output,
                                is_error: false,
                            }],
                            timestamp: line_ts.clone(),
                        })
                    }
                    Some("reasoning") => reasoning_part(&payload).map(|text| UnifiedMessage {
                        role: Role::Assistant,
                        parts: vec![UnifiedPart::Reasoning(text)],
                        timestamp: line_ts.clone(),
                    }),
                    // web_search_call and unknown payloads: the information is
                    // either non-migratable or uninterpretable — ignore
                    // without counting a bad line
                    _ => None,
                };
            }
            // compacted and other top-level types: ignore
            _ => {}
        }
        if let Some(message) = produced {
            // The environment-context wrapper (<environment_context>) is an
            // echo of the runtime environment injected by Codex, not real
            // user speech: skip it entirely in both preview and migration.
            let is_env_echo = message.role == Role::User
                && !message.parts.is_empty()
                && message.parts.iter().all(|p| match p {
                    UnifiedPart::Text(t) => t.trim_start().starts_with("<environment_context>"),
                    _ => false,
                });
            if is_env_echo {
                continue;
            }
            if line_ts.is_some() {
                last_active_line_ts = line_ts;
            }
            messages.push(message);
        }
    }

    if messages.is_empty() {
        return Err(HubError::EmptySession(path.to_path_buf()));
    }
    if project_dir.is_empty() {
        project_dir = meta_cwd.unwrap_or_default();
    }

    // Timestamp of the last line that produced a message; fall back to the
    // file mtime (RFC3339) when missing
    let last_active = match last_active_line_ts {
        Some(ts) => ts,
        None => mtime_rfc3339(path)?,
    };

    let summary = SessionSummary {
        session_id: path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default(),
        source_path: path.to_path_buf(),
        title: title_of(&messages),
        project_dir,
        last_active,
        message_count: messages.len(),
    };
    Ok(UnifiedSession {
        summary,
        messages,
        parse_warnings,
    })
}

/// message payload → unified message. An uninterpretable content shape
/// counts as a bad line.
fn parse_message(payload: &Value, parse_warnings: &mut usize) -> Option<UnifiedMessage> {
    let role = match payload.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return None, // system and other roles: ignore
    };
    let blocks = match payload.get("content") {
        Some(Value::Array(blocks)) => blocks,
        _ => {
            *parse_warnings += 1;
            return None;
        }
    };
    let parts = blocks
        .iter()
        .filter_map(|block| {
            match block.get("type").and_then(Value::as_str) {
                Some("input_text") | Some("output_text") => Some(UnifiedPart::Text(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )),
                _ => None, // unknown block types: ignore
            }
        })
        .collect();
    Some(UnifiedMessage {
        role,
        parts,
        timestamp: None, // overwritten by the caller with the line-level timestamp
    })
}

/// Reasoning payload: prefer the plaintext content, fall back to summary when
/// it is empty; when both are empty (only encrypted_content remains, which
/// cannot migrate across tools) return None and drop the line.
fn reasoning_part(payload: &Value) -> Option<String> {
    extract_block_text(payload.get("content"))
        .or_else(|| extract_block_text(payload.get("summary")))
}

/// The two shapes of the content/summary fields: a string, or an array of
/// [text blocks]; empty text counts as no content.
fn extract_block_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => (!s.is_empty()).then(|| s.clone()),
        Some(Value::Array(blocks)) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// Recursively collect rollout-*.jsonl (date directories at any depth);
/// jsonl files without the rollout prefix are not collected.
fn collect_rollout_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HubError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_rollout_files(&path, out)?;
        } else {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                out.push(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use std::io::Write;

    fn rollout_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_lines(root: &tempfile::TempDir, name: &str, lines: &[String]) -> PathBuf {
        let path = root.path().join(name);
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    /// A typical session: meta + turn_context + user message + encrypted
    /// reasoning (dropped) + function_call + function_call_output + assistant
    /// message + event_msg + a bad line.
    fn typical_lines() -> Vec<String> {
        vec![
            r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"session_meta","payload":{"id":"11111111-2222-4333-8444-555555555555","timestamp":"2026-09-10T10:00:00.000Z","cwd":"/tmp/proj-gamma","originator":"codex_tui","cli_version":"0.146.0","source":"cli","thread_source":"user","model_provider":"openai","base_instructions":"巨长的系统提示,应被整体忽略"}}"#
                .to_string(),
            r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"turn_context","payload":{"cwd":"/tmp/proj-gamma","model":"gpt-5","approval_policy":"on-request","summary":"旧摘要"}}"#
                .to_string(),
            r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"帮我看一下这个目录"}]}}"#
                .to_string(),
            r#"{"timestamp":"2026-09-10T10:00:02.000Z","type":"response_item","payload":{"type":"reasoning","content":null,"summary":[],"encrypted_content":"eyJlbmNyeXB0ZWQtc2hvdWxkLW5vdC1taWdyYXRlIn0="}}"#
                .to_string(),
            r#"{"timestamp":"2026-09-10T10:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"call-1","arguments":"{\"command\":[\"ls -la\"]}"}}"#
                .to_string(),
            r#"{"timestamp":"2026-09-10T10:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"total 0"}}"#
                .to_string(),
            r#"{"timestamp":"2026-09-10T10:00:05.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"目录是空的"}]}}"#
                .to_string(),
            r#"{"timestamp":"2026-09-10T10:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1}}}}"#
                .to_string(),
            "{not-valid-json".to_string(),
        ]
    }

    /// Environment-context wrapper messages (<environment_context>) are
    /// skipped entirely, including for the title.
    #[test]
    fn tc_cread_env_context_echo_skipped() {
        let dir = rollout_dir();
        let path = write_lines(
            &dir,
            "rollout-2026-09-10T10-00-00-11111111-2222-4333-8444-555555555556.jsonl",
            &[
                r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"session_meta","payload":{"id":"x","cwd":"/tmp/p"}}"#.to_string(),
                r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n<cwd>/tmp/p</cwd>\n</environment_context>"}]}}"#.to_string(),
                r#"{"timestamp":"2026-09-10T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"帮我修一下登录页"}]}}"#.to_string(),
            ],
        );
        let session = read_session(&path).unwrap();
        // Environment echo skipped, only 1 message left; the title comes from
        // the real user message
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.summary.title, "帮我修一下登录页");
    }

    // ---------- TC-CREAD-01 ----------
    #[test]
    fn tc_cread_01_typical_rollout_pipeline() {
        let dir = rollout_dir();
        let path = write_lines(
            &dir,
            "rollout-2026-09-10T10-00-00-11111111-2222-4333-8444-555555555555.jsonl",
            &typical_lines(),
        );
        let session = read_session(&path).unwrap();

        // meta/turn_context/event_msg/bad lines produce no message; encrypted
        // reasoning is dropped
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.parse_warnings, 1);

        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(
            session.messages[0].parts,
            vec![UnifiedPart::Text("帮我看一下这个目录".to_string())]
        );
        assert_eq!(
            session.messages[0].timestamp.as_deref(),
            Some("2026-09-10T10:00:01.000Z")
        );

        // function_call → ToolCall (name is the tool, arguments kept verbatim
        // as JSON text)
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(
            session.messages[1].parts,
            vec![UnifiedPart::ToolCall {
                tool: "shell".to_string(),
                input_json: "{\"command\":[\"ls -la\"]}".to_string(),
            }]
        );

        // function_call_output → ToolResult (call_id linked back to the tool name)
        assert_eq!(session.messages[2].role, Role::User);
        assert_eq!(
            session.messages[2].parts,
            vec![UnifiedPart::ToolResult {
                tool: "shell".to_string(),
                content: "total 0".to_string(),
                is_error: false,
            }]
        );

        assert_eq!(session.messages[3].role, Role::Assistant);
        assert_eq!(
            session.messages[3].parts,
            vec![UnifiedPart::Text("目录是空的".to_string())]
        );

        // Summary fields
        assert_eq!(session.summary.title, "帮我看一下这个目录");
        assert_eq!(session.summary.project_dir, "/tmp/proj-gamma");
        // Timestamp of the last message-bearing line (event_msg/bad lines
        // don't count)
        assert_eq!(session.summary.last_active, "2026-09-10T10:00:05.000Z");
        assert_eq!(session.summary.message_count, 4);
        assert_eq!(
            session.summary.session_id,
            "rollout-2026-09-10T10-00-00-11111111-2222-4333-8444-555555555555"
        );
        assert_eq!(session.summary.source_path, path);

        // Encrypted reasoning never enters the IR
        let serialized = serde_json::to_string(&session).unwrap();
        assert!(!serialized.contains("encrypted"));
        assert!(!serialized.contains("eyJlbmNyeXB0ZWQ"));
    }

    // ---------- TC-CREAD-02 ----------
    /// Plaintext reasoning is kept; an empty content falls back to summary;
    /// both empty means the line is dropped.
    #[test]
    fn tc_cread_02_reasoning_plaintext_and_summary_fallback() {
        let dir = rollout_dir();
        let path = write_lines(
            &dir,
            "rollout-r.jsonl",
            &[
                r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"reasoning","content":[{"type":"reasoning_text","text":"先想清楚再动手"}],"encrypted_content":"enc"}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:02.000Z","type":"response_item","payload":{"type":"reasoning","content":null,"summary":[{"type":"summary_text","text":"查了一下目录结构"}]}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:03.000Z","type":"response_item","payload":{"type":"reasoning","content":null,"summary":[]}}"#
                    .to_string(),
            ],
        );
        let session = read_session(&path).unwrap();
        // The third reasoning (content and summary both empty) produces no
        // message
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::Assistant);
        assert_eq!(
            session.messages[0].parts,
            vec![UnifiedPart::Reasoning("先想清楚再动手".to_string())]
        );
        assert_eq!(
            session.messages[1].parts,
            vec![UnifiedPart::Reasoning("查了一下目录结构".to_string())]
        );
    }

    // ---------- TC-CREAD-03 ----------
    /// project_dir: first turn_context.cwd, falling back to session_meta.cwd.
    #[test]
    fn tc_cread_03_project_dir_turn_context_first_meta_fallback() {
        let dir = rollout_dir();
        // No turn_context: fall back to session_meta.cwd
        let path = write_lines(
            &dir,
            "rollout-a.jsonl",
            &[
                r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"session_meta","payload":{"id":"a","timestamp":"2026-09-10T10:00:00.000Z","cwd":"/tmp/from-meta","originator":"codex_tui"}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#
                    .to_string(),
            ],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.summary.project_dir, "/tmp/from-meta");

        // Both missing: project_dir is the empty string (the writer side
        // degrades on its own)
        let path2 = write_lines(
            &dir,
            "rollout-b.jsonl",
            &[r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#
                .to_string()],
        );
        let session2 = read_session(&path2).unwrap();
        assert_eq!(session2.summary.project_dir, "");
    }

    // ---------- TC-CREAD-04 ----------
    /// Title: skip wrapper texts, take the first real user input, truncate to
    /// 50 chars.
    #[test]
    fn tc_cread_04_title_skips_wrappers_and_truncates() {
        let dir = rollout_dir();
        let long: String = std::iter::repeat_n('题', 60).collect();
        let path = write_lines(
            &dir,
            "rollout-t.jsonl",
            &[
                r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<command-name>/model</command-name>"}]}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<local-command-stdout>ok</local-command-stdout>"}]}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:03.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Caveat: 本条是注入提示"}]}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"[Request interrupted by user]"}]}}"#
                    .to_string(),
                format!(
                    r#"{{"timestamp":"2026-09-10T10:00:05.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{long}"}}]}}}}"#
                ),
            ],
        );
        let session = read_session(&path).unwrap();
        // All four wrapper texts skipped; the title is the 5th line,
        // truncated to 50 chars
        assert_eq!(session.summary.title.chars().count(), 50);
        assert!(session.summary.title.chars().all(|c| c == '题'));
        // The wrapper texts themselves stay in the messages (they only affect
        // title extraction)
        assert_eq!(session.messages.len(), 5);
    }

    /// A session without user text gets the title "untitled".
    #[test]
    fn tc_cread_04_title_untitled_without_user_text() {
        let dir = rollout_dir();
        let path = write_lines(
            &dir,
            "rollout-u.jsonl",
            &[r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"只有助手"}]}}"#
                .to_string()],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.summary.title, "untitled");
    }

    // ---------- TC-CREAD-05 ----------
    #[test]
    fn tc_cread_05_meta_only_is_empty_session() {
        let dir = rollout_dir();
        let path = write_lines(
            &dir,
            "rollout-e.jsonl",
            &[
                r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"session_meta","payload":{"id":"e","cwd":"/tmp/x"}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"turn_context","payload":{"cwd":"/tmp/x","model":"gpt-5"}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"界面回显"}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:02.000Z","type":"compacted","payload":{}}"#.to_string(),
            ],
        );
        let err = read_session(&path).unwrap_err();
        assert!(matches!(err, HubError::EmptySession(_)));
    }

    // ---------- TC-CREAD-06 ----------
    /// When message lines carry no timestamp, last_active falls back to the
    /// file mtime.
    #[test]
    fn tc_cread_06_missing_timestamps_fall_back_to_mtime() {
        let dir = rollout_dir();
        let path = write_lines(
            &dir,
            "rollout-m.jsonl",
            &[r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]}}"#
                .to_string()],
        );
        let session = read_session(&path).unwrap();
        assert!(session.messages.iter().all(|m| m.timestamp.is_none()));
        assert!(DateTime::parse_from_rfc3339(&session.summary.last_active).is_ok());
    }

    // ---------- TC-CREAD-07 ----------
    /// Scan: recurses date directories, accepts only the rollout-* prefix,
    /// skips bad files, orders by last_active desc with session_id
    /// lexicographic tie-break.
    #[test]
    fn tc_cread_07_scan_recurses_filters_and_orders() {
        let root = rollout_dir();
        let one_user = |ts: &str| {
            format!(
                r#"{{"timestamp":"{ts}","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}}}"#
            )
        };
        let write_rollout = |rel: &str, ts: &str| {
            let path = root.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut f = fs::File::create(&path).unwrap();
            writeln!(f, "{}", one_user(ts)).unwrap();
        };
        write_rollout(
            "2026/09/10/rollout-old-ccc.jsonl",
            "2026-09-10T10:00:00.000Z",
        );
        write_rollout(
            "2026/09/11/rollout-new-aaa.jsonl",
            "2026-09-11T12:00:00.000Z",
        );
        write_rollout(
            "2026/09/11/rollout-new-zzz.jsonl",
            "2026-09-11T12:00:00.000Z",
        );
        // A jsonl without the rollout prefix: ignored
        let other = root.path().join("notes.jsonl");
        fs::write(&other, one_user("2026-09-12T00:00:00.000Z") + "\n").unwrap();
        // rollout prefix but empty content: read fails, scan skips it
        let bad = root.path().join("2026/09/10/rollout-bad.jsonl");
        fs::create_dir_all(bad.parent().unwrap()).unwrap();
        fs::write(&bad, "").unwrap();

        let summaries = scan_sessions(root.path()).unwrap();
        let ids: Vec<&str> = summaries.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["rollout-new-aaa", "rollout-new-zzz", "rollout-old-ccc"]
        );
    }

    // ---------- TC-CREAD-08 ----------
    #[test]
    fn tc_cread_08_scan_root_not_found() {
        let err = scan_sessions(Path::new("/definitely/not/existing/dir")).unwrap_err();
        assert!(matches!(err, HubError::SourceNotFound(_)));
        let err = read_session(Path::new("/definitely/not/existing.jsonl")).unwrap_err();
        assert!(matches!(err, HubError::SourceNotFound(_)));
    }

    /// When a function_call_output's call_id cannot be matched to a function,
    /// the tool name degrades to the call_id; other response_item payloads
    /// such as web_search_call produce no message.
    #[test]
    fn tc_cread_09_unknown_call_id_and_ignored_payload_kinds() {
        let dir = rollout_dir();
        let path = write_lines(
            &dir,
            "rollout-w.jsonl",
            &[
                r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-orphan","output":"孤儿结果"}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:02.000Z","type":"response_item","payload":{"type":"web_search_call","action":{"type":"search","query":"rust"}}}"#
                    .to_string(),
                r#"{"timestamp":"2026-09-10T10:00:03.000Z","type":"response_item","payload":{"type":"something_new","data":{}}}"#
                    .to_string(),
            ],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(
            session.messages[0].parts,
            vec![UnifiedPart::ToolResult {
                tool: "call-orphan".to_string(),
                content: "孤儿结果".to_string(),
                is_error: false,
            }]
        );
        // Unknown payloads do not count as bad lines
        assert_eq!(session.parse_warnings, 0);
    }
}
