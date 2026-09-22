//! Claude Code 会话 JSONL 解析。
//!
//! 源形态:`~/.claude/projects/<proj-dir>/<sessionId>.jsonl`,逐行 JSON 事件流。
//! 坏行跳过并计数;sidechain 行跳过;system 等其他 type 忽略。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::error::HubError;
use crate::ir::{Role, SessionSummary, UnifiedMessage, UnifiedPart, UnifiedSession};

/// 标题截断长度。
const TITLE_MAX_CHARS: usize = 50;

/// 扫描 root(通常 ~/.claude/projects)下所有 *.jsonl,按 last_active 倒序,
/// 同刻 tie-break 按 session_id 字典序。
///
/// 单个文件读取失败(含空会话)时跳过该文件,不影响整体扫描;
/// root 本身不存在时报 SourceNotFound。
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

/// 解析单个会话文件为 IR。坏行跳过并计数;sidechain 跳过;
/// 无任何 user/assistant 消息时报 EmptySession。
pub fn read_session(path: &Path) -> Result<UnifiedSession, HubError> {
    if !path.is_file() {
        return Err(HubError::SourceNotFound(path.to_path_buf()));
    }
    let content = fs::read_to_string(path)?;

    let mut messages = Vec::new();
    let mut parse_warnings = 0usize;
    let mut project_dir = String::new();
    let mut last_active_line_ts: Option<String> = None;
    // tool_use_id → 工具名,用于把 tool_result 块关联到对应工具名
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
        // sidechain 行跳过(不计 warning、不计消息、不计 last_active)
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        // project_dir 一律取首个非空行内 cwd
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
            _ => continue, // system / file-history-snapshot / mode / 其他:忽略,不计 warning
        };
        let line_ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let parts = match value.pointer("/message/content") {
            Some(Value::String(s)) => vec![UnifiedPart::Text(strip_ansi(s))],
            Some(Value::Array(blocks)) => parse_blocks(blocks, &mut tool_names),
            _ => {
                // user/assistant 行但 message.content 形态无法解读:按坏行计
                parse_warnings += 1;
                continue;
            }
        };
        // 本地命令包装消息(<command-name>/<local-command-*>)是终端注入的回显,
        // 不是用户真实发言:预览与迁移都跳过整条。
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

    // 最后一条 user/assistant 行 timestamp;缺失 fallback 文件 mtime(RFC3339)
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

/// 逐块解析 content 数组。未知块类型忽略。
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
            // thinking 明文保留为 Reasoning,signature 不进入 IR
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

/// tool_result 的 content 形态:string 或 [text 块] 数组;其他以 JSON 文本兜底。
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

/// 首条 user 文本前 50 个 char(不足全取);无 user 文本用 "untitled"。
/// 跳过工具注入的包装文本(本地命令提示、中断标记等),它们不是用户真正说的话。
fn title_of(messages: &[UnifiedMessage]) -> String {
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

/// 判断 user 文本是否为终端注入的本地命令包装/回显。
fn is_local_command_text(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<command-") || t.starts_with("<local-command")
}

/// 剥离 ANSI 转义序列(如 `\x1b[1m`),它们在界面与迁移产物中都是乱码。
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // 消费 '['
            while let Some(&n) = chars.peek() {
                if n.is_ascii_digit() || n == ';' || n == '?' {
                    chars.next();
                } else {
                    break;
                }
            }
            chars.next(); // 消费终结字母(m/A/K 等)
        } else {
            out.push(c);
        }
    }
    out
}

fn mtime_rfc3339(path: &Path) -> Result<String, HubError> {
    let modified = fs::metadata(path)?.modified()?;
    Ok(DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn collect_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HubError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_jsonl_files(&path, out)?;
        } else if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

/// 排序键:last_active 解析为 UTC 毫秒;不可解析视为最小值。
fn sort_key(summary: &SessionSummary) -> i64 {
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

    /// 在 tempdir 下写一个单行 user 会话文件(扫描排序用)。
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
        // 标题 = 首条 user 文本(18 char < 50,全取)
        assert_eq!(session.summary.title, "第一个问题:你好,帮我看一下这个项目");
        // project_dir = 行内 cwd
        assert_eq!(session.summary.project_dir, "/tmp/proj-alpha");
        // 最后一条 user/assistant 行 timestamp
        assert_eq!(session.summary.last_active, "2026-09-10T10:00:05.000Z");
        assert_eq!(session.summary.message_count, 2);
        assert_eq!(session.summary.session_id, "minimal");
        assert_eq!(session.summary.source_path, fixture("minimal.jsonl"));
    }

    // ---------- TC-READ-01(标题 50 char 截断)----------
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

        // assistant 行:thinking → Reasoning、text → Text、tool_use → ToolCall
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

        // user 行:tool_result → ToolResult(tool 名由 tool_use_id 关联回 tool_use)
        let tool_result_msg = &session.messages[2];
        assert_eq!(
            tool_result_msg.parts,
            vec![UnifiedPart::ToolResult {
                tool: "Bash".to_string(),
                content: "total 0\ndrwxr-xr-x 2 user staff".to_string(),
                is_error: false,
            }]
        );

        // signature 不进入 IR(任何形式都不可见)
        let serialized = serde_json::to_string(&session).unwrap();
        assert!(!serialized.contains("sig-should-not-appear"));
    }

    #[test]
    fn tc_read_03_bad_line_and_sidechain() {
        let session = read_session(&fixture("rich.jsonl")).unwrap();
        // 坏 JSON 行恰好 1 行 → parse_warnings = 1
        assert_eq!(session.parse_warnings, 1);
        // sidechain 行不产生消息、不计 warning(否则消息数为 5)
        assert_eq!(session.messages.len(), 4);
        let serialized = serde_json::to_string(&session).unwrap();
        assert!(!serialized.contains("sidechain 分支消息"));
    }

    // ---------- TC-READ-04 ----------
    #[test]
    fn tc_read_04_scan_orders_by_last_active_desc() {
        let root = tempfile::tempdir().unwrap();
        // 4 个会话跨 2 个项目目录;aaa 与 zzz 同刻,tie-break 按 session_id 字典序
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
    /// 标题提取跳过本地命令包装文本,取第一条真实用户消息。
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

    /// 本地命令回显消息被整条跳过;文本中的 ANSI 转义序列被剥离。
    /// 注:真实文件中 ESC 以 JSON 转义 `` 存储(裸控制字符在 JSON 中非法)。
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
        // 命令回显被跳过,仅剩 2 条消息
        assert_eq!(session.messages.len(), 2);
        // ANSI 转义序列剥离干净
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
        // last_active 走 mtime fallback,输出为合法 RFC3339
        assert!(DateTime::parse_from_rfc3339(&session.summary.last_active).is_ok());
    }
}
