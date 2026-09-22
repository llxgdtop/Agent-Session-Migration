//! 端到端集成测试:Codex → Claude Code 反向迁移全链路。
//!
//! fixture 全部在 tempdir 内手写构造,不拷贝任何真实 ~/.codex / ~/.claude 数据。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hub_core::{read_codex_session, read_session, write_claude_session_with, IdGen};
use serde_json::Value;

const FIXED_TS_COLON: &str = "2026-09-11T00:00:00.000Z";

/// uuid 递增:第 1 个给会话,其后逐消息;时间恒定。
struct CountingIdGen {
    counter: AtomicUsize,
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
        uuid(n)
    }
}

fn uuid(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

/// 手写一份典型 Codex rollout:meta + turn_context + user 消息 + 加密 reasoning
/// (应丢弃)+ function_call + function_call_output + assistant 消息 + event_msg + 坏行。
fn write_codex_fixture(root: &Path) -> PathBuf {
    let path = root
        .join("2026/09/10")
        .join("rollout-2026-09-10T10-00-00-11111111-2222-4333-8444-555555555555.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines = [
        r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"session_meta","payload":{"id":"11111111-2222-4333-8444-555555555555","timestamp":"2026-09-10T10:00:00.000Z","cwd":"/tmp/proj-gamma","originator":"codex_tui","cli_version":"0.146.0","source":"cli","thread_source":"user","model_provider":"openai","base_instructions":"应被整体忽略的系统提示"}}"#,
        r#"{"timestamp":"2026-09-10T10:00:00.000Z","type":"turn_context","payload":{"cwd":"/tmp/proj-gamma","model":"gpt-5","approval_policy":"on-request","summary":"旧摘要"}}"#,
        r#"{"timestamp":"2026-09-10T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"帮我看一下这个目录"}]}}"#,
        r#"{"timestamp":"2026-09-10T10:00:02.000Z","type":"response_item","payload":{"type":"reasoning","content":null,"summary":[],"encrypted_content":"eyJlbmNyeXB0ZWQtc2hvdWxkLW5vdC1taWdyYXRlIn0="}}"#,
        r#"{"timestamp":"2026-09-10T10:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"call-1","arguments":"{\"command\":[\"ls -la\"]}"}}"#,
        r#"{"timestamp":"2026-09-10T10:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"total 0"}}"#,
        r#"{"timestamp":"2026-09-10T10:00:05.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"目录是空的"}]}}"#,
        r#"{"timestamp":"2026-09-10T10:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1}}}}"#,
        "{not-valid-json",
    ];
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

// ---------- TC-CE2E-01:read codex → write claude 逐行断言 ----------

#[test]
fn tc_ce2e_01_codex_to_claude_full_pipeline() {
    let src_root = tempfile::tempdir().unwrap();
    let codex_path = write_codex_fixture(src_root.path());
    let source_bytes_before = fs::read(&codex_path).unwrap();

    let ir = read_codex_session(&codex_path).unwrap();
    assert_eq!(ir.summary.project_dir, "/tmp/proj-gamma");
    assert_eq!(ir.messages.len(), 4);
    assert_eq!(ir.parse_warnings, 1);

    let target_root = tempfile::tempdir().unwrap();
    let out = write_claude_session_with(
        &ir,
        target_root.path(),
        &CountingIdGen {
            counter: AtomicUsize::new(0),
        },
    )
    .unwrap();

    // 产物路径:<root>/<project 转义目录>/<新 uuid>.jsonl
    assert_eq!(
        out.file_path,
        target_root
            .path()
            .join("-tmp-proj-gamma")
            .join(format!("{}.jsonl", uuid(1)))
    );

    let content = fs::read_to_string(&out.file_path).unwrap();
    let values: Vec<Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).expect("每行合法 JSON"))
        .collect();
    assert_eq!(values.len(), 4);

    // 行级公共字段
    for value in &values {
        assert_eq!(value["sessionId"], uuid(1));
        assert_eq!(value["isSidechain"], false);
        assert_eq!(value["userType"], "external");
        assert_eq!(value["cwd"], "/tmp/proj-gamma");
        assert_eq!(value["version"], "2.1.212");
        assert_eq!(value["gitBranch"], "");
        assert!(value["uuid"].is_string());
    }

    // parentUuid 链:首条 null,后续逐条指向前一条
    assert_eq!(values[0]["parentUuid"], Value::Null);
    for i in 1..values.len() {
        assert_eq!(values[i]["parentUuid"], values[i - 1]["uuid"]);
    }

    // 角色与内容形态
    assert_eq!(values[0]["type"], "user");
    assert_eq!(values[0]["message"]["role"], "user");
    assert_eq!(values[0]["message"]["content"], "帮我看一下这个目录");
    assert_eq!(values[0]["timestamp"], "2026-09-10T10:00:01.000Z");

    assert_eq!(values[1]["type"], "assistant");
    assert_eq!(
        values[1]["message"]["content"][0]["text"],
        "[调用工具 shell] {\"command\":[\"ls -la\"]}"
    );

    assert_eq!(values[2]["type"], "user");
    assert_eq!(
        values[2]["message"]["content"],
        "[工具结果 shell isError=false] total 0"
    );

    assert_eq!(values[3]["type"], "assistant");
    assert_eq!(values[3]["message"]["content"][0]["text"], "目录是空的");
    assert_eq!(values[3]["timestamp"], "2026-09-10T10:00:05.000Z");

    // 加密推理绝不进入产物
    assert!(!content.contains("eyJlbmNyeXB0"));

    // resume 命令
    assert_eq!(
        out.resume_command,
        format!("cd '/tmp/proj-gamma' && claude --resume {}", uuid(1))
    );

    // 源文件逐字节不变(只读迁移)
    assert_eq!(source_bytes_before, fs::read(&codex_path).unwrap());
}

// ---------- TC-CE2E-02:产物可被本库 Claude Code reader 载回 ----------

/// 反向产物用正向 reader 读回,验证行形态与 claude 解析器兼容:
/// 4 条消息角色正确、标题与 project_dir 语义保留。
#[test]
fn tc_ce2e_02_artifact_readable_by_claude_reader() {
    let src_root = tempfile::tempdir().unwrap();
    let codex_path = write_codex_fixture(src_root.path());
    let ir = read_codex_session(&codex_path).unwrap();

    let target_root = tempfile::tempdir().unwrap();
    let out = write_claude_session_with(
        &ir,
        target_root.path(),
        &CountingIdGen {
            counter: AtomicUsize::new(0),
        },
    )
    .unwrap();

    let back = read_session(&out.file_path).unwrap();
    assert_eq!(back.messages.len(), 4);
    assert_eq!(back.summary.project_dir, "/tmp/proj-gamma");
    assert_eq!(back.summary.title, "帮我看一下这个目录");
    assert_eq!(back.summary.message_count, 4);
    // 读回的消息保留了文本化工具内容
    let texts: Vec<&str> = back
        .messages
        .iter()
        .filter_map(|m| match &m.parts[0] {
            hub_core::UnifiedPart::Text(t) | hub_core::UnifiedPart::Reasoning(t) => {
                Some(t.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(texts.contains(&"[调用工具 shell] {\"command\":[\"ls -la\"]}"));
    assert!(texts.contains(&"[工具结果 shell isError=false] total 0"));
}
