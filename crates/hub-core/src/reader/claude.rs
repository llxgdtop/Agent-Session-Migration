//! Claude Code session JSONL parsing.
//!
//! Source layout: `~/.claude/projects/<proj-dir>/<sessionId>.jsonl`, a
//! line-delimited JSON event stream. Bad lines are skipped and counted;
//! sidechain lines are skipped; other types such as system are ignored.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::error::HubError;
use crate::ir::{Role, SessionSummary, UnifiedMessage, UnifiedPart, UnifiedSession};

/// Title truncation length.
const TITLE_MAX_CHARS: usize = 50;

/// Scan all *.jsonl under root (usually ~/.claude/projects), ordered by
/// last_active descending, tie-broken at equal timestamps by session_id
/// lexicographic order.
///
/// A single file failing to read (including empty sessions) is skipped
/// without affecting the overall scan; if root itself does not exist,
/// SourceNotFound is returned.
pub fn scan_sessions(root: &Path) -> Result<Vec<SessionSummary>, HubError> {
    if !root.is_dir() {
        return Err(HubError::SourceNotFound(root.to_path_buf()));
    }
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files)?;
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

/// Parse a single session file into IR. Bad lines are skipped and counted;
/// sidechains are skipped; EmptySession is returned when no user/assistant
/// message remains.
pub fn read_session(path: &Path) -> Result<UnifiedSession, HubError> {
    if !path.is_file() {
        return Err(HubError::SourceNotFound(path.to_path_buf()));
    }
    let content = fs::read_to_string(path)?;

    let mut messages = Vec::new();
    let mut parse_warnings = 0usize;
    let mut project_dir = String::new();
    let mut last_active_line_ts: Option<String> = None;
    // tool_use_id → tool name, used to link a tool_result block back to its tool
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
        // Sidechain lines are skipped (no warning, no message, no last_active)
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        // project_dir always comes from the first non-empty inline cwd
        if project_dir.is_empty() {
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                if !cwd.is_empty() {
                    project_dir = cwd.to_string();
                }
            }
        }
        let role = match value.get("type").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue, // system / file-history-snapshot / mode / others: ignore, no warning
        };
        let line_ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let parts = match value.pointer("/message/content") {
            Some(Value::String(s)) => vec![UnifiedPart::Text(strip_ansi(s))],
            Some(Value::Array(blocks)) => parse_blocks(blocks, &mut tool_names),
            _ => {
                // A user/assistant line whose message.content shape cannot be
                // interpreted: count as a bad line
                parse_warnings += 1;
                continue;
            }
        };
        // Local-command wrapper messages (<command-name>/<local-command-*>)
        // are terminal-injected echoes, not real user speech: skip them
        // entirely in both preview and migration.
        let is_command_echo = role == Role::User
            && !parts.is_empty()
            && parts.iter().all(|p| match p {
                UnifiedPart::Text(t) => is_local_command_text(t),
                _ => false,
            });
        if is_command_echo {
            continue;
        }
        messages.push(UnifiedMessage {
            role,
            parts,
            timestamp: line_ts.clone(),
        });
        if line_ts.is_some() {
            last_active_line_ts = line_ts;
        }
    }

    if messages.is_empty() {
        return Err(HubError::EmptySession(path.to_path_buf()));
    }

    // Timestamp of the last user/assistant line; fall back to the file mtime
    // (RFC3339) when missing
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

/// Parse a content array block by block. Unknown block types are ignored.
fn parse_blocks(blocks: &[Value], tool_names: &mut HashMap<String, String>) -> Vec<UnifiedPart> {
    let mut parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(UnifiedPart::Text(strip_ansi(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ))),
            // thinking plaintext is kept as Reasoning; the signature never
            // enters the IR
            Some("thinking") => parts.push(UnifiedPart::Reasoning(strip_ansi(
                block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ))),
            Some("tool_use") => {
                let tool = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let input_json = block
                    .get("input")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "null".to_string());
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    tool_names.insert(id.to_string(), tool.clone());
                }
                parts.push(UnifiedPart::ToolCall { tool, input_json });
            }
            Some("tool_result") => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tool = tool_names
                    .get(tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| tool_use_id.to_string());
                parts.push(UnifiedPart::ToolResult {
                    tool,
                    content: strip_ansi(&extract_result_content(block.get("content"))),
                    is_error: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
            _ => {}
        }
    }
    parts
}

/// tool_result content shapes: a string, or an array of [text blocks];
/// anything else falls back to its JSON text.
fn extract_result_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// First 50 chars (or fewer) of the first user text; "untitled" when there
/// is no user text.
/// Skips tool-injected wrapper text (local-command notices, interrupt
/// markers, etc.) — those are not what the user actually said.
/// (reader::codex reuses the same rule, hence crate-visible.)
pub(crate) fn title_of(messages: &[UnifiedMessage]) -> String {
    for message in messages {
        if message.role != Role::User {
            continue;
        }
        for part in &message.parts {
            if let UnifiedPart::Text(text) = part {
                let trimmed = text.trim_start();
                if is_local_command_text(text)
                    || trimmed.starts_with("Caveat:")
                    || trimmed.starts_with("[Request interrupted")
                    || trimmed.starts_with("<environment_context>")
                {
                    continue;
                }
                if !text.is_empty() {
                    return text.chars().take(TITLE_MAX_CHARS).collect();
                }
            }
        }
    }
    "untitled".to_string()
}

/// Whether a user text is a terminal-injected local-command wrapper/echo.
fn is_local_command_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<command-") || t.starts_with("<local-command")
}

/// Strip ANSI escape sequences (e.g. `\x1b[1m`); they are mojibake both in
/// the UI and in migration artifacts.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            while let Some(&n) = chars.peek() {
                if n.is_ascii_digit() || n == ';' || n == '?' {
                    chars.next();
                } else {
                    break;
                }
            }
            chars.next(); // consume the terminating letter (m/A/K, etc.)
        } else {
            out.push(c);
        }
    }
    out
}

/// File mtime (RFC3339, UTC milliseconds). Reused as the last_active
/// fallback by reader::codex.
pub(crate) fn mtime_rfc3339(path: &Path) -> Result<String, HubError> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn collect_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HubError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_jsonl_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

/// Sort key: last_active parsed as UTC milliseconds; unparseable values
/// count as the minimum.
/// reader::codex reuses the same key for its scan ordering.
pub(crate) fn sort_key(summary: &SessionSummary) -> i64 {
    DateTime::parse_from_rfc3339(&summary.last_active)
        .map(|d| d.with_timezone(&Utc).timestamp_millis())
        .unwrap_or(i64::MIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// Write a single-line user session file under a tempdir (for scan
    /// ordering tests).
    fn write_session_file(
        root: &Path,
        project: &str,
        session_id: &str,
        timestamp: &str,
    ) -> PathBuf {
        let dir = root.join(project);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        let line = format!(
            r#"{{"type":"user","cwd":"/tmp/{project}","timestamp":"{timestamp}","message":{{"role":"user","content":"hi"}}}}"#
        );
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        path
    }

    fn write_lines(root: &tempfile::TempDir, lines: &[String]) -> PathBuf {
        let path = root.path().join("s.jsonl");
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    // ---------- TC-READ-01 ----------
    #[test]
    fn tc_read_01_minimal_session() {
        let session = read_session(&fixture("minimal.jsonl")).unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.parse_warnings, 0);
        assert_eq!(
            session.messages[0].parts,
            vec![UnifiedPart::Text(
                "第一个问题:你好,帮我看一下这个项目".to_string()
            )]
        );
        assert_eq!(
            session.messages[0].timestamp.as_deref(),
            Some("2026-09-10T10:00:00.000Z")
        );
        assert_eq!(
            session.messages[1].parts,
            vec![UnifiedPart::Text("好的,这是回答。".to_string())]
        );
        // Title = first user text (18 chars < 50, kept in full)
        assert_eq!(session.summary.title, "第一个问题:你好,帮我看一下这个项目");
        // project_dir = inline cwd
        assert_eq!(session.summary.project_dir, "/tmp/proj-alpha");
        // Timestamp of the last user/assistant line
        assert_eq!(session.summary.last_active, "2026-09-10T10:00:05.000Z");
        assert_eq!(session.summary.message_count, 2);
        assert_eq!(session.summary.session_id, "minimal");
        assert_eq!(session.summary.source_path, fixture("minimal.jsonl"));
    }

    // ---------- TC-READ-01 (title truncated to 50 chars) ----------
    #[test]
    fn tc_read_01_title_truncated_to_50_chars() {
        let dir = tempfile::tempdir().unwrap();
        let long_text: String = std::iter::repeat_n('题', 60).collect();
        let path = write_lines(
            &dir,
            &[format!(
                r#"{{"type":"user","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:00.000Z","message":{{"role":"user","content":"{long_text}"}}}}"#
            )],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.summary.title.chars().count(), 50);
        assert!(session.summary.title.chars().all(|c| c == '题'));
        assert_eq!(session.messages[0].parts[0], UnifiedPart::Text(long_text));
    }

    // ---------- TC-READ-02 / 03 ----------
    #[test]
    fn tc_read_02_rich_block_kinds() {
        let session = read_session(&fixture("rich.jsonl")).unwrap();
        assert_eq!(session.messages.len(), 4);

        // assistant line: thinking → Reasoning, text → Text, tool_use → ToolCall
        let assistant = &session.messages[1];
        assert_eq!(
            assistant.parts,
            vec![
                UnifiedPart::Reasoning("用户要看目录,应当调用 ls".to_string()),
                UnifiedPart::Text("我来执行 ls。".to_string()),
                UnifiedPart::ToolCall {
                    tool: "Bash".to_string(),
                    input_json: "{\"command\":\"ls -la\"}".to_string(),
                },
            ]
        );

        // user line: tool_result → ToolResult (tool name linked back via
        // tool_use_id to the tool_use)
        let tool_result_msg = &session.messages[2];
        assert_eq!(
            tool_result_msg.parts,
            vec![UnifiedPart::ToolResult {
                tool: "Bash".to_string(),
                content: "total 0\ndrwxr-xr-x 2 user staff".to_string(),
                is_error: false,
            }]
        );

        // The signature never enters the IR (invisible in any form)
        let serialized = serde_json::to_string(&session).unwrap();
        assert!(!serialized.contains("sig-should-not-appear"));
    }

    #[test]
    fn tc_read_03_bad_line_and_sidechain() {
        let session = read_session(&fixture("rich.jsonl")).unwrap();
        // Exactly one bad JSON line → parse_warnings = 1
        assert_eq!(session.parse_warnings, 1);
        // Sidechain lines produce no message and no warning (otherwise the
        // message count would be 5)
        assert_eq!(session.messages.len(), 4);
        let serialized = serde_json::to_string(&session).unwrap();
        assert!(!serialized.contains("sidechain 分支消息"));
    }

    // ---------- TC-READ-04 ----------
    #[test]
    fn tc_read_04_scan_orders_by_last_active_desc() {
        let root = tempfile::tempdir().unwrap();
        // 4 sessions across 2 project dirs; aaa and zzz share a timestamp,
        // tie-broken by session_id lexicographic order
        write_session_file(root.path(), "pa", "ccc", "2026-09-10T10:00:00.000Z");
        write_session_file(root.path(), "pa", "aaa", "2026-09-10T12:00:00.000Z");
        write_session_file(root.path(), "pb", "bbb", "2026-09-10T11:00:00.000Z");
        write_session_file(root.path(), "pb", "zzz", "2026-09-10T12:00:00.000Z");

        let summaries = scan_sessions(root.path()).unwrap();
        let ids: Vec<&str> = summaries.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "zzz", "bbb", "ccc"]);
    }

    // ---------- TC-READ-05 ----------
    #[test]
    fn tc_read_05_scan_root_not_found() {
        let err = scan_sessions(Path::new("/definitely/not/existing/dir")).unwrap_err();
        assert!(matches!(err, HubError::SourceNotFound(_)));
    }

    // ---------- TC-READ-06 ----------
    #[test]
    fn tc_read_06_system_only_is_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[r#"{"type":"system","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:00.000Z","content":"..."}"#
                .to_string()],
        );
        let err = read_session(&path).unwrap_err();
        assert!(matches!(err, HubError::EmptySession(_)));
    }

    // ---------- TC-READ-07 ----------
    #[test]
    fn tc_read_07_user_content_string_and_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[
                r#"{"type":"user","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:00.000Z","message":{"role":"user","content":"同样的话"}}"#
                    .to_string(),
                r#"{"type":"user","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"同样的话"}]}}"#
                    .to_string(),
            ],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.messages.len(), 2);
        for message in &session.messages {
            assert_eq!(
                message.parts,
                vec![UnifiedPart::Text("同样的话".to_string())]
            );
        }
    }

    // ---------- TC-READ-08 ----------
    #[test]
    fn tc_read_08_assistant_empty_content_keeps_empty_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[r#"{"type":"assistant","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:00.000Z","message":{"role":"assistant","content":[]}}"#
                .to_string()],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert!(session.messages[0].parts.is_empty());
        assert_eq!(session.messages[0].role, Role::Assistant);
    }

    // ---------- TC-READ-09 ----------
    /// Title extraction skips local-command wrapper text and takes the first
    /// real user message.
    #[test]
    fn tc_read_10_title_skips_command_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[
                r#"{"type":"user","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:00.000Z","message":{"role":"user","content":"<local-command-caveat>Caveat: The messages below were generated by the user while running local commands."}}"#
                    .to_string(),
                r#"{"type":"user","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:01.000Z","message":{"role":"user","content":"帮我修一下登录页"}}"#
                    .to_string(),
            ],
        );
        let session = read_session(&path).unwrap();
        assert_eq!(session.summary.title, "帮我修一下登录页");
    }

    /// Local-command echo messages are skipped entirely; ANSI escape
    /// sequences in text are stripped.
    /// Note: in real files ESC is stored as the JSON escape ``
    /// (bare control characters are invalid in JSON).
    #[test]
    fn tc_read_11_command_echo_skipped_and_ansi_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let esc = "\\u001b";
        let path = write_lines(
            &dir,
            &[
                r#"{"type":"user","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:00.000Z","message":{"role":"user","content":"<command-name>/model</command-name><command-message>model</command-message>"}}"#
                    .to_string(),
                format!(
                    r#"{{"type":"user","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:01.000Z","message":{{"role":"user","content":"Set model to {esc}[1mglm-5.3{esc}[22m"}}}}"#
                ),
                format!(
                    r#"{{"type":"assistant","cwd":"/tmp/p","timestamp":"2026-09-10T10:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"收到 {esc}[1mOK{esc}[0m"}}]}}}}"#
                ),
            ],
        );
        let session = read_session(&path).unwrap();
        // Command echo skipped; only 2 messages remain
        assert_eq!(session.messages.len(), 2);
        // ANSI escape sequences stripped clean
        assert_eq!(
            session.messages[0].parts,
            vec![UnifiedPart::Text("Set model to glm-5.3".to_string())]
        );
        assert_eq!(
            session.messages[1].parts,
            vec![UnifiedPart::Text("收到 OK".to_string())]
        );
    }

    // ---------- TC-READ-09 ----------
    #[test]
    fn tc_read_09_missing_timestamps_fall_back_to_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[
                r#"{"type":"user","cwd":"/tmp/p","message":{"role":"user","content":"q"}}"#.to_string(),
                r#"{"type":"assistant","cwd":"/tmp/p","message":{"role":"assistant","content":[{"type":"text","text":"a"}]}}"#
                    .to_string(),
            ],
        );
        let session = read_session(&path).unwrap();
        assert!(session.messages.iter().all(|m| m.timestamp.is_none()));
        // last_active comes from the mtime fallback; the output must be valid
        // RFC3339
        assert!(DateTime::parse_from_rfc3339(&session.summary.last_active).is_ok());
    }
}
