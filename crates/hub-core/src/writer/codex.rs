//! Codex rollout 写入(契约见 MVP-DEVELOPMENT.md §2.1,schema 见 §3.2.1,命名见 §3.3)。
//!
//! 行为:调 map_session → 逐行序列化 → 写 `<target>/<date>/rollout-<ts>-<uuid>.jsonl.tmp`
//! → rename 原子落盘;目标已存在报 TargetExists(BR-9);map 出 EmptySession 报 EmptySession。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{Local, Utc};
use serde::Serialize;

use crate::error::HubError;
use crate::ir::{Role, UnifiedSession};
use crate::mapper::{map_session, CodexEvent};

/// 固定字段值(§3.2.1:照抄实验已验证的最小集,不引入未验证变量)。
const ORIGINATOR: &str = "codex-tui";
const CLI_VERSION: &str = "0.146.0";
const SOURCE: &str = "cli";
const THREAD_SOURCE: &str = "user";
const MODEL_PROVIDER: &str = "openai";
const MODEL: &str = "gpt-5";
const APPROVAL_POLICY: &str = "on-request";

#[derive(Debug)]
pub struct CodexWriteOutput {
    pub session_id: String,
    pub file_path: PathBuf,
    pub resume_command: String,
}

/// 时间与 uuid 注入点(BR-22):生产用 SystemIdGen,测试用固定实现保证黄金文件对比确定性。
pub trait IdGen: Send + Sync {
    /// 文件名用:如 `2026-09-11T01-02-03`(本地时间,冒号以 `-` 分隔)。
    fn now_rfc3339(&self) -> String;
    /// 行内 timestamp 用:如 `2026-09-11T01:02:03.000Z`(UTC)。
    fn now_rfc3339_colon(&self) -> String;
    /// 日期目录用:如 `2026/09/11`(与文件名同源本地时间)。
    fn now_date_path(&self) -> String;
    fn uuid_v4(&self) -> String;
}

/// 默认系统实现:每次调用读取当前时刻。
///
/// 注:§2.1 契约规定 SystemIdGen 为单元结构体,无法在构造时缓存单一时刻快照;
/// write_session_with 内文件名时间与日期目录为连续两次调用,跨秒错位窗口为纳秒级
/// (§3.3 同刻要求在生产实现上的已知残余风险,见汇报)。
pub struct SystemIdGen;

impl IdGen for SystemIdGen {
    fn now_rfc3339(&self) -> String {
        Local::now().format("%Y-%m-%dT%H-%M-%S").to_string()
    }
    fn now_rfc3339_colon(&self) -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }
    fn now_date_path(&self) -> String {
        Local::now().format("%Y/%m/%d").to_string()
    }
    fn uuid_v4(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

pub fn write_session(
    ir: &UnifiedSession,
    target_root: &Path,
) -> Result<CodexWriteOutput, HubError> {
    write_session_with(ir, target_root, &SystemIdGen)
}

pub fn write_session_with(
    ir: &UnifiedSession,
    target_root: &Path,
    gen: &dyn IdGen,
) -> Result<CodexWriteOutput, HubError> {
    // §3.3:文件名 uuid == session_meta.id,同一次 uuid_v4 结果
    let session_id = gen.uuid_v4();
    let meta_timestamp = gen.now_rfc3339_colon();
    let events = map_session(ir, &session_id, &meta_timestamp)?;

    let file_name = format!("rollout-{}-{}.jsonl", gen.now_rfc3339(), session_id);
    let dir = target_root.join(gen.now_date_path());
    let target = dir.join(&file_name);

    // BR-9:幂等键 = 目标文件路径,已存在即拒绝
    if target.exists() {
        return Err(HubError::TargetExists(target));
    }
    if fs::create_dir_all(&dir).is_err() {
        return Err(HubError::NoWritableTarget(dir));
    }

    let mut body = String::new();
    for event in &events {
        body.push_str(&serialize_event(event, &meta_timestamp));
        body.push('\n');
    }

    // §2.3:写 .tmp 成功后 rename;rename 前任何失败都清理 tmp,无部分写入状态
    let tmp = dir.join(format!("{file_name}.tmp"));
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

    let resume_command = format!("cd {} && codex resume {session_id}", ir.summary.project_dir);
    Ok(CodexWriteOutput {
        session_id,
        file_path: target,
        resume_command,
    })
}

/// 事件 → 一行 JSON(§3.2.1 权威 schema;TurnContext 行 timestamp 取 IdGen 生成时刻,同 meta)。
fn serialize_event(event: &CodexEvent, meta_timestamp: &str) -> String {
    match event {
        CodexEvent::SessionMeta { id, timestamp, cwd } => {
            let line = WireLine {
                timestamp,
                kind: "session_meta",
                payload: SessionMetaPayload {
                    id,
                    timestamp,
                    cwd,
                    originator: ORIGINATOR,
                    cli_version: CLI_VERSION,
                    source: SOURCE,
                    thread_source: THREAD_SOURCE,
                    model_provider: MODEL_PROVIDER,
                },
            };
            serde_json::to_string(&line).expect("serialize session_meta")
        }
        CodexEvent::TurnContext { cwd, summary } => {
            let line = WireLine {
                timestamp: meta_timestamp,
                kind: "turn_context",
                payload: TurnContextPayload {
                    cwd,
                    model: MODEL,
                    approval_policy: APPROVAL_POLICY,
                    summary,
                },
            };
            serde_json::to_string(&line).expect("serialize turn_context")
        }
        CodexEvent::Message {
            role,
            text,
            timestamp,
        } => {
            let (role_str, content_type) = match role {
                Role::User => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };
            let line = WireLine {
                timestamp,
                kind: "response_item",
                payload: MessagePayload {
                    kind: "message",
                    role: role_str,
                    content: vec![ContentBlock {
                        kind: content_type,
                        text,
                    }],
                },
            };
            serde_json::to_string(&line).expect("serialize response_item")
        }
        CodexEvent::UserMessageEvent { message, timestamp } => {
            let line = WireLine {
                timestamp,
                kind: "event_msg",
                payload: UserMessagePayload {
                    kind: "user_message",
                    message,
                },
            };
            serde_json::to_string(&line).expect("serialize event_msg")
        }
    }
}

#[derive(Serialize)]
struct WireLine<'a, P> {
    timestamp: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    payload: P,
}

#[derive(Serialize)]
struct SessionMetaPayload<'a> {
    id: &'a str,
    timestamp: &'a str,
    cwd: &'a str,
    originator: &'a str,
    cli_version: &'a str,
    source: &'a str,
    thread_source: &'a str,
    model_provider: &'a str,
}

#[derive(Serialize)]
struct TurnContextPayload<'a> {
    cwd: &'a str,
    model: &'a str,
    approval_policy: &'a str,
    summary: &'a str,
}

#[derive(Serialize)]
struct MessagePayload<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    role: &'a str,
    content: Vec<ContentBlock<'a>>,
}

#[derive(Serialize)]
struct ContentBlock<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct UserMessagePayload<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    message: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{SessionSummary, UnifiedMessage, UnifiedPart};
    use chrono::DateTime;
    use serde_json::Value;

    const FIXED_UUID: &str = "00000000-0000-4000-8000-000000000001";
    const FIXED_TS_FILE: &str = "2026-09-11T00-00-00";
    const FIXED_TS_COLON: &str = "2026-09-11T00:00:00.000Z";
    const FIXED_DATE: &str = "2026/09/11";

    struct FixedIdGen;

    impl IdGen for FixedIdGen {
        fn now_rfc3339(&self) -> String {
            FIXED_TS_FILE.to_string()
        }
        fn now_rfc3339_colon(&self) -> String {
            FIXED_TS_COLON.to_string()
        }
        fn now_date_path(&self) -> String {
            FIXED_DATE.to_string()
        }
        fn uuid_v4(&self) -> String {
            FIXED_UUID.to_string()
        }
    }

    fn sample_ir() -> UnifiedSession {
        UnifiedSession {
            summary: SessionSummary {
                session_id: "src".to_string(),
                source_path: PathBuf::from("/tmp/src.jsonl"),
                title: "标题".to_string(),
                project_dir: "/tmp/proj".to_string(),
                last_active: "2026-09-10T10:00:00.000Z".to_string(),
                message_count: 2,
            },
            messages: vec![
                UnifiedMessage {
                    role: Role::User,
                    parts: vec![UnifiedPart::Text("问题一".to_string())],
                    timestamp: Some("2026-09-10T10:00:00.000Z".to_string()),
                },
                UnifiedMessage {
                    role: Role::Assistant,
                    parts: vec![UnifiedPart::Text("回答一".to_string())],
                    timestamp: Some("2026-09-10T10:00:05.000Z".to_string()),
                },
            ],
            parse_warnings: 0,
        }
    }

    fn expected_path(root: &Path) -> PathBuf {
        root.join(FIXED_DATE)
            .join(format!("rollout-{FIXED_TS_FILE}-{FIXED_UUID}.jsonl"))
    }

    // ---------- TC-WRITE-01 ----------
    #[test]
    fn tc_write_01_filename_and_head_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let out = write_session_with(&sample_ir(), tmp.path(), &FixedIdGen).unwrap();
        assert_eq!(out.file_path, expected_path(tmp.path()));
        assert_eq!(out.session_id, FIXED_UUID);
        assert_eq!(
            out.resume_command,
            format!("cd /tmp/proj && codex resume {FIXED_UUID}")
        );

        let content = fs::read_to_string(&out.file_path).unwrap();
        let mut lines = content.lines();
        let first: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(first["type"], "session_meta");
        assert_eq!(first["payload"]["id"], FIXED_UUID); // §3.3:文件名 uuid == session_meta.id
        assert_eq!(first["payload"]["cwd"], "/tmp/proj");
        assert_eq!(first["payload"]["originator"], "codex-tui");
        assert_eq!(first["payload"]["cli_version"], "0.146.0");
        let second: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(second["type"], "turn_context");
        assert_eq!(second["payload"]["summary"], "标题");
        assert_eq!(second["payload"]["model"], "gpt-5");
    }

    /// §2.1:map 出 EmptySession 报 EmptySession。
    #[test]
    fn tc_write_01_empty_session_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ir = sample_ir();
        ir.messages.clear();
        assert!(matches!(
            write_session_with(&ir, tmp.path(), &FixedIdGen),
            Err(HubError::EmptySession(_))
        ));
    }

    // ---------- TC-WRITE-02 ----------
    #[test]
    fn tc_write_02_second_write_target_exists_and_bytes_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let out = write_session_with(&sample_ir(), tmp.path(), &FixedIdGen).unwrap();
        let bytes_before = fs::read(&out.file_path).unwrap();

        let err = write_session_with(&sample_ir(), tmp.path(), &FixedIdGen).unwrap_err();
        assert!(matches!(err, HubError::TargetExists(_)));

        let bytes_after = fs::read(&out.file_path).unwrap();
        assert_eq!(bytes_before, bytes_after, "第一次产物逐字节未变");
    }

    // ---------- TC-WRITE-03 ----------
    #[test]
    fn tc_write_03_line_schema_validity_and_user_pairing() {
        let tmp = tempfile::tempdir().unwrap();
        let out = write_session_with(&sample_ir(), tmp.path(), &FixedIdGen).unwrap();
        let content = fs::read_to_string(&out.file_path).unwrap();
        let values: Vec<Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).expect("每行合法 JSON"))
            .collect();
        assert_eq!(values.len(), 5);

        for value in &values {
            let ts = value["timestamp"].as_str().expect("timestamp 存在");
            assert!(DateTime::parse_from_rfc3339(ts).is_ok(), "RFC3339: {ts}");
        }

        // user 消息 response_item 与 event_msg 恰好成对
        let mut pairs = 0;
        for (i, value) in values.iter().enumerate() {
            if value["type"] == "response_item" && value["payload"]["role"] == "user" {
                let next = &values[i + 1];
                assert_eq!(next["type"], "event_msg");
                assert_eq!(next["payload"]["type"], "user_message");
                assert_eq!(
                    next["payload"]["message"],
                    value["payload"]["content"][0]["text"]
                );
                assert_eq!(next["timestamp"], value["timestamp"]);
                pairs += 1;
            }
        }
        assert_eq!(pairs, 1);
    }

    // ---------- TC-WRITE-04 ----------
    #[cfg(unix)]
    #[test]
    fn tc_write_04_readonly_target_dir_is_no_writable_target() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(FIXED_DATE);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();

        let result = write_session_with(&sample_ir(), tmp.path(), &FixedIdGen);

        // 先恢复权限(tempdir 清理需要),再断言
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result, Err(HubError::NoWritableTarget(_))));

        let has_tmp = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!has_tmp, "无 .tmp 残留");
    }

    // ---------- TC-WRITE-05 ----------
    #[test]
    fn tc_write_05_preexisting_tmp_is_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let target = expected_path(tmp.path());
        let tmp_file = target
            .with_file_name(target.file_name().unwrap().to_string_lossy().to_string() + ".tmp");
        fs::create_dir_all(tmp_file.parent().unwrap()).unwrap();
        fs::write(&tmp_file, "garbage").unwrap();

        let out = write_session_with(&sample_ir(), tmp.path(), &FixedIdGen).unwrap();
        assert!(out.file_path.exists());
        assert!(!tmp_file.exists(), "无 .tmp 残留");
        let content = fs::read_to_string(&out.file_path).unwrap();
        assert!(content.starts_with('{'));
    }
}
