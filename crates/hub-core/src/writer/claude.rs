//! Claude Code session JSONL writing.
//!
//! Behavior: render IR messages one by one → write
//! `<target>/<escaped project dir>/<new uuid>.jsonl.tmp` → rename for an
//! atomic commit; TargetExists when the target already exists; EmptySession
//! when every message is empty.
//! A minimal two lines (user + assistant) is enough for `claude --resume` to
//! load the session — verified on a real device.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::HubError;
use crate::ir::{Role, UnifiedSession};
use crate::mapper::render_parts;
use crate::writer::codex::{IdGen, SystemIdGen};

/// Target Claude Code version (written into the inline version field,
/// matching real-device artifacts).
const CLI_VERSION: &str = "2.1.212";
/// Fixed inline userType value: a real session from an external user.
const USER_TYPE: &str = "external";

#[derive(Debug)]
pub struct ClaudeWriteOutput {
    pub session_id: String,
    pub file_path: PathBuf,
    pub resume_command: String,
}

/// Convenience entry point: system time and random uuid. Use
/// [`write_session_with`] for deterministic tests.
pub fn write_session(
    ir: &UnifiedSession,
    target_root: &Path,
) -> Result<ClaudeWriteOutput, HubError> {
    write_session_with(ir, target_root, &SystemIdGen)
}

/// Write entry point with an injected IdGen: the session uuid, per-message
/// uuids, and fallback timestamps come from gen.
pub fn write_session_with(
    ir: &UnifiedSession,
    target_root: &Path,
    gen: &dyn IdGen,
) -> Result<ClaudeWriteOutput, HubError> {
    let session_id = gen.uuid_v4();
    let now_ts = gen.now_rfc3339_colon();

    // Render message by message; messages with empty parts or empty merged
    // text are skipped; uuids are allocated in actual on-disk order, and
    // parentUuid points at the previous persisted message (null for the first)
    let mut lines = Vec::new();
    let mut parent_uuid: Option<String> = None;
    for message in &ir.messages {
        if message.parts.is_empty() {
            continue;
        }
        let text = render_parts(&message.parts);
        if text.is_empty() {
            continue;
        }
        let message_uuid = gen.uuid_v4();
        let timestamp = message.timestamp.clone().unwrap_or_else(|| now_ts.clone());
        lines.push(serialize_line(
            parent_uuid.as_deref(),
            message.role,
            &text,
            &message_uuid,
            &timestamp,
            &session_id,
            &ir.summary.project_dir,
        ));
        parent_uuid = Some(message_uuid);
    }
    if lines.is_empty() {
        return Err(HubError::EmptySession(ir.summary.source_path.clone()));
    }

    // Target: <root>/<project_dir slashes → hyphens>/<session uuid>.jsonl
    let dir = target_root.join(escape_project_dir(&ir.summary.project_dir));
    let target = dir.join(format!("{session_id}.jsonl"));
    // Idempotency key = target file path; refuse when it already exists
    if target.exists() {
        return Err(HubError::TargetExists(target));
    }
    if fs::create_dir_all(&dir).is_err() {
        return Err(HubError::NoWritableTarget(dir));
    }

    let mut body = lines.join("\n");
    body.push('\n');

    // Write the .tmp then rename; any failure before the rename cleans up the
    // tmp — no partially-written state is ever left behind
    let tmp = dir.join(format!("{session_id}.jsonl.tmp"));
    if let Err(e) = fs::write(&tmp, body.as_bytes()) {
        let _ = fs::remove_file(&tmp);
        return Err(match e.kind() {
            io::ErrorKind::PermissionDenied => HubError::NoWritableTarget(dir),
            _ => HubError::Io(e),
        });
    }
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(HubError::Io(e));
    }

    // project_dir comes from the source session (externally controllable);
    // the single-quote hardening in the command-generation layer kicks in here
    let resume_command =
        crate::launcher::claude_resume_command(&session_id, &ir.summary.project_dir);
    Ok(ClaudeWriteOutput {
        session_id,
        file_path: target,
        resume_command,
    })
}

/// Claude Code project directory-name rule: every slash in the path becomes
/// a hyphen.
/// (project_dir is only used as a directory name and as the inline cwd data
/// field — it never reaches a shell; single quotes are safe for the file
/// system.)
fn escape_project_dir(project_dir: &str) -> String {
    project_dir.replace('/', "-")
}

/// One message → one JSON line. user content takes the string form;
/// assistant content takes the array form
/// [{"type":"text","text": merged text}].
fn serialize_line(
    parent_uuid: Option<&str>,
    role: Role,
    text: &str,
    message_uuid: &str,
    timestamp: &str,
    session_id: &str,
    cwd: &str,
) -> String {
    let (kind, role_str, content) = match role {
        Role::User => ("user", "user", WireContent::Text(text)),
        Role::Assistant => (
            "assistant",
            "assistant",
            WireContent::Blocks(vec![TextBlock { kind: "text", text }]),
        ),
    };
    let line = WireLine {
        parent_uuid,
        is_sidechain: false,
        user_type: USER_TYPE,
        cwd,
        session_id,
        version: CLI_VERSION,
        git_branch: "",
        kind,
        uuid: message_uuid,
        timestamp,
        message: WireMessage {
            role: role_str,
            content,
        },
    };
    serde_json::to_string(&line).expect("serialize claude line")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireLine<'a> {
    parent_uuid: Option<&'a str>,
    is_sidechain: bool,
    user_type: &'a str,
    cwd: &'a str,
    session_id: &'a str,
    version: &'a str,
    git_branch: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    uuid: &'a str,
    timestamp: &'a str,
    message: WireMessage<'a>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: WireContent<'a>,
}

/// The two content shapes: a plain string for user, an array of text blocks
/// for assistant.
#[derive(Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
    Text(&'a str),
    Blocks(Vec<TextBlock<'a>>),
}

#[derive(Serialize)]
struct TextBlock<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{SessionSummary, UnifiedMessage, UnifiedPart};
    use serde_json::Value;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FIXED_TS_COLON: &str = "2026-09-11T00:00:00.000Z";

    /// Incrementing uuids: the 1st goes to the session, then one per message;
    /// time is constant.
    struct CountingIdGen {
        counter: AtomicUsize,
    }

    impl CountingIdGen {
        fn new() -> Self {
            Self {
                counter: AtomicUsize::new(0),
            }
        }
    }

    impl IdGen for CountingIdGen {
        fn now_rfc3339(&self) -> String {
            "unused".to_string()
        }
        fn now_rfc3339_colon(&self) -> String {
            FIXED_TS_COLON.to_string()
        }
        fn now_date_path(&self) -> String {
            "unused".to_string()
        }
        fn uuid_v4(&self) -> String {
            let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
            format!("00000000-0000-4000-8000-{n:012}")
        }
    }

    fn uuid(n: usize) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }

    fn ir_with(project_dir: &str, messages: Vec<UnifiedMessage>) -> UnifiedSession {
        UnifiedSession {
            summary: SessionSummary {
                session_id: "src".to_string(),
                source_path: PathBuf::from("/tmp/src.jsonl"),
                title: "标题".to_string(),
                project_dir: project_dir.to_string(),
                last_active: "2026-09-10T10:00:00.000Z".to_string(),
                message_count: messages.len(),
            },
            messages,
            parse_warnings: 0,
        }
    }

    fn msg(role: Role, parts: Vec<UnifiedPart>, ts: Option<&str>) -> UnifiedMessage {
        UnifiedMessage {
            role,
            parts,
            timestamp: ts.map(str::to_string),
        }
    }

    fn lines_of(path: &Path) -> Vec<Value> {
        let content = fs::read_to_string(path).unwrap();
        assert!(content.ends_with('\n'), "文件以换行结尾");
        content
            .lines()
            .map(|l| serde_json::from_str(l).expect("每行合法 JSON"))
            .collect()
    }

    // ---------- TC-CWRITE-01 ----------
    /// Path escaping, line schema, uuid/parentUuid chain, the two content
    /// forms (user string vs assistant array), and the resume command.
    #[test]
    fn tc_cwrite_01_path_schema_chain_and_content_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let ir = ir_with(
            "/Users/x/proj",
            vec![
                msg(
                    Role::User,
                    vec![UnifiedPart::Text("问题一".to_string())],
                    Some("2026-09-10T10:00:00.000Z"),
                ),
                msg(
                    Role::Assistant,
                    vec![UnifiedPart::Text("回答一".to_string())],
                    Some("2026-09-10T10:00:05.000Z"),
                ),
            ],
        );
        let out = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap();

        // Path: slashes → hyphens; file name = the session's new uuid
        let expected = tmp
            .path()
            .join("-Users-x-proj")
            .join(format!("{}.jsonl", uuid(1)));
        assert_eq!(out.file_path, expected);
        assert_eq!(out.session_id, uuid(1));
        assert_eq!(
            out.resume_command,
            format!("cd '/Users/x/proj' && claude --resume {}", uuid(1))
        );

        let values = lines_of(&out.file_path);
        assert_eq!(values.len(), 2);

        let first = &values[0];
        assert_eq!(first["parentUuid"], Value::Null);
        assert_eq!(first["isSidechain"], false);
        assert_eq!(first["userType"], "external");
        assert_eq!(first["cwd"], "/Users/x/proj");
        assert_eq!(first["sessionId"], uuid(1));
        assert_eq!(first["version"], "2.1.212");
        assert_eq!(first["gitBranch"], "");
        assert_eq!(first["type"], "user");
        assert_eq!(first["uuid"], uuid(2));
        assert_eq!(first["timestamp"], "2026-09-10T10:00:00.000Z");
        assert_eq!(first["message"]["role"], "user");
        // user content takes the string form
        assert_eq!(first["message"]["content"], "问题一");

        let second = &values[1];
        assert_eq!(second["parentUuid"], uuid(2));
        assert_eq!(second["type"], "assistant");
        assert_eq!(second["uuid"], uuid(3));
        assert_eq!(second["sessionId"], uuid(1));
        assert_eq!(second["timestamp"], "2026-09-10T10:00:05.000Z");
        // assistant content takes the array form
        assert_eq!(
            second["message"]["content"],
            serde_json::json!([{"type": "text", "text": "回答一"}])
        );
    }

    // ---------- TC-CWRITE-02 ----------
    /// Merging rules match the mapper: reasoning prefix, textualized tool
    /// calls/results.
    #[test]
    fn tc_cwrite_02_merge_rules_match_mapper() {
        let tmp = tempfile::tempdir().unwrap();
        let ir = ir_with(
            "/tmp/p",
            vec![
                msg(
                    Role::Assistant,
                    vec![
                        UnifiedPart::Reasoning("想一想".to_string()),
                        UnifiedPart::ToolCall {
                            tool: "Bash".to_string(),
                            input_json: "{\"command\":\"ls\"}".to_string(),
                        },
                    ],
                    Some("2026-09-10T10:00:02.000Z"),
                ),
                msg(
                    Role::User,
                    vec![UnifiedPart::ToolResult {
                        tool: "Bash".to_string(),
                        content: "done".to_string(),
                        is_error: true,
                    }],
                    Some("2026-09-10T10:00:04.000Z"),
                ),
            ],
        );
        let out = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap();
        let values = lines_of(&out.file_path);
        assert_eq!(values.len(), 2);
        assert_eq!(
            values[0]["message"]["content"][0]["text"],
            "> 内部推理:想一想\n\n[调用工具 Bash] {\"command\":\"ls\"}"
        );
        // The tool result lands on a user line, in string form
        assert_eq!(
            values[1]["message"]["content"],
            "[工具结果 Bash isError=true] done"
        );
    }

    // ---------- TC-CWRITE-03 ----------
    /// Idempotency: an existing target reports TargetExists, and the original
    /// file stays byte-for-byte identical.
    #[test]
    fn tc_cwrite_03_target_exists_and_bytes_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let ir = ir_with(
            "/tmp/p",
            vec![msg(
                Role::User,
                vec![UnifiedPart::Text("q".to_string())],
                None,
            )],
        );
        let out = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap();
        let bytes_before = fs::read(&out.file_path).unwrap();

        let err = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap_err();
        assert!(matches!(err, HubError::TargetExists(_)));

        let bytes_after = fs::read(&out.file_path).unwrap();
        assert_eq!(bytes_before, bytes_after, "第一次产物逐字节未变");
    }

    // ---------- TC-CWRITE-04 ----------
    /// Atomic write: no .tmp residue; a pre-existing garbage .tmp with the
    /// target's name is overwritten and disappears with the rename.
    #[test]
    fn tc_cwrite_04_atomic_no_tmp_residue() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("-tmp-p");
        fs::create_dir_all(&dir).unwrap();
        // Pre-plant a garbage file with the same name as the target tmp
        let garbage = dir.join(format!("{}.jsonl.tmp", uuid(1)));
        fs::write(&garbage, "garbage").unwrap();

        let ir = ir_with(
            "/tmp/p",
            vec![msg(
                Role::User,
                vec![UnifiedPart::Text("q".to_string())],
                None,
            )],
        );
        let out = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap();
        assert!(out.file_path.is_file());
        assert!(!garbage.exists(), "垃圾 .tmp 被覆盖并随 rename 消失");

        let has_tmp = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!has_tmp, "无 .tmp 残留");
    }

    // ---------- TC-CWRITE-05 ----------
    /// Injection hardening: a project_dir containing a single quote degrades
    /// the resume command (the cd segment is dropped), while directory
    /// escaping proceeds as usual (quotes are safe for the file system).
    #[test]
    fn tc_cwrite_05_quoted_project_dir_degrades_resume_command() {
        let tmp = tempfile::tempdir().unwrap();
        let ir = ir_with(
            "/tmp/x'; rm -rf ~",
            vec![msg(
                Role::User,
                vec![UnifiedPart::Text("q".to_string())],
                None,
            )],
        );
        let out = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap();
        // The cd segment is dropped; the session_id is a generated uuid and
        // inherently safe
        assert_eq!(out.resume_command, format!("claude --resume {}", uuid(1)));
        // The inline cwd keeps the original path (a data field, never reaches
        // a shell)
        let values = lines_of(&out.file_path);
        assert_eq!(values[0]["cwd"], "/tmp/x'; rm -rf ~");
        // The file lands under the escaped directory
        assert!(out
            .file_path
            .starts_with(tmp.path().join("-tmp-x'; rm -rf ~")));
    }

    // ---------- TC-CWRITE-06 ----------
    /// Empty session / all messages empty after merging → EmptySession.
    #[test]
    fn tc_cwrite_06_empty_session_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = ir_with("/tmp/p", vec![]);
        assert!(matches!(
            write_session_with(&empty, tmp.path(), &CountingIdGen::new()),
            Err(HubError::EmptySession(_))
        ));

        let all_blank = ir_with(
            "/tmp/p",
            vec![
                msg(Role::Assistant, vec![], None),
                msg(Role::User, vec![UnifiedPart::Text(String::new())], None),
            ],
        );
        assert!(matches!(
            write_session_with(&all_blank, tmp.path(), &CountingIdGen::new()),
            Err(HubError::EmptySession(_))
        ));
    }

    /// Blank messages are dropped; the remaining messages' parentUuid chain
    /// stays intact.
    #[test]
    fn tc_cwrite_06_blank_message_dropped_chain_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let ir = ir_with(
            "/tmp/p",
            vec![
                msg(Role::User, vec![UnifiedPart::Text("hi".to_string())], None),
                msg(Role::Assistant, vec![], None),
                msg(
                    Role::Assistant,
                    vec![UnifiedPart::Text("again".to_string())],
                    None,
                ),
            ],
        );
        let out = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap();
        let values = lines_of(&out.file_path);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["uuid"], uuid(2));
        assert_eq!(values[1]["uuid"], uuid(3));
        assert_eq!(values[1]["parentUuid"], uuid(2));
    }

    // ---------- TC-CWRITE-07 ----------
    /// A missing message timestamp falls back to the generation time
    /// (supplied by the IdGen).
    #[test]
    fn tc_cwrite_07_missing_timestamp_falls_back_to_now() {
        let tmp = tempfile::tempdir().unwrap();
        let ir = ir_with(
            "/tmp/p",
            vec![msg(
                Role::User,
                vec![UnifiedPart::Text("q".to_string())],
                None,
            )],
        );
        let out = write_session_with(&ir, tmp.path(), &CountingIdGen::new()).unwrap();
        let values = lines_of(&out.file_path);
        assert_eq!(values[0]["timestamp"], FIXED_TS_COLON);
    }
}
