//! IR(统一中间表示)类型定义(契约见 MVP-DEVELOPMENT.md §2.1)。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 适配的三家工具。MVP 只使用 `ClaudeCode`(读)与 `Codex`(写)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Tool {
    ClaudeCode,
    Codex,
    ZCode,
}

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// 消息内内容块。MVP 文本化策略下工具调用/结果以原始文本保留(BR-7)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnifiedPart {
    /// 普通文本块。
    Text(String),
    /// 明文推理(BR-4:signature 不进入 IR)。
    Reasoning(String),
    /// 工具调用;`input_json` 为入参的 JSON 文本。
    ToolCall { tool: String, input_json: String },
    /// 工具结果。
    ToolResult {
        tool: String,
        content: String,
        is_error: bool,
    },
}

/// 单条统一消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedMessage {
    pub role: Role,
    pub parts: Vec<UnifiedPart>,
    /// 行内 timestamp(RFC3339)透传;缺失时为 None(BR-20)。
    pub timestamp: Option<String>,
}

/// 会话列表项(BR-2/16/18/19)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 源会话文件名去掉 `.jsonl`。
    pub session_id: String,
    pub source_path: PathBuf,
    /// BR-16:首条 user 文本前 50 个 char;无 user 消息则为 "untitled"。
    pub title: String,
    /// BR-19:一律取 JSONL 行内首个非空 `cwd`。
    pub project_dir: String,
    /// BR-18:最后一条 user/assistant 行的 timestamp(RFC3339);缺失走文件 mtime。
    pub last_active: String,
    /// user+assistant 消息数。
    pub message_count: usize,
}

/// 完整会话 IR。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedSession {
    pub summary: SessionSummary,
    pub messages: Vec<UnifiedMessage>,
    /// BR-10:跳过的坏行数。
    pub parse_warnings: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> UnifiedSession {
        UnifiedSession {
            summary: SessionSummary {
                session_id: "11111111-2222-4333-8444-555555555555".to_string(),
                source_path: PathBuf::from("/tmp/x/11111111-2222-4333-8444-555555555555.jsonl"),
                title: "标题示例".to_string(),
                project_dir: "/tmp/x".to_string(),
                last_active: "2026-09-10T10:00:05.000Z".to_string(),
                message_count: 3,
            },
            messages: vec![
                UnifiedMessage {
                    role: Role::User,
                    parts: vec![UnifiedPart::Text("你好".to_string())],
                    timestamp: Some("2026-09-10T10:00:00.000Z".to_string()),
                },
                UnifiedMessage {
                    role: Role::Assistant,
                    parts: vec![
                        UnifiedPart::Reasoning("想一想".to_string()),
                        UnifiedPart::ToolCall {
                            tool: "Bash".to_string(),
                            input_json: "{\"command\":\"ls\"}".to_string(),
                        },
                    ],
                    timestamp: None,
                },
                UnifiedMessage {
                    role: Role::User,
                    parts: vec![UnifiedPart::ToolResult {
                        tool: "Bash".to_string(),
                        content: "ok".to_string(),
                        is_error: false,
                    }],
                    timestamp: None,
                },
            ],
            parse_warnings: 2,
        }
    }

    /// TC-IR-01:serde 序列化再反序列化,字段无丢失。
    #[test]
    fn tc_ir_01_serde_roundtrip_preserves_fields() {
        let original = sample_session();
        let json = serde_json::to_string(&original).expect("serialize IR");
        let back: UnifiedSession = serde_json::from_str(&json).expect("deserialize IR");
        assert_eq!(back, original);
    }

    /// TC-IR-01:Tool 枚举序列化名含 "ClaudeCode"/"Codex"/"ZCode"。
    #[test]
    fn tc_ir_01_tool_enum_serialization_names() {
        assert_eq!(
            serde_json::to_string(&Tool::ClaudeCode).unwrap(),
            "\"ClaudeCode\""
        );
        assert_eq!(serde_json::to_string(&Tool::Codex).unwrap(), "\"Codex\"");
        assert_eq!(serde_json::to_string(&Tool::ZCode).unwrap(), "\"ZCode\"");
        // 反向亦然
        let tool: Tool = serde_json::from_str("\"ClaudeCode\"").unwrap();
        assert_eq!(tool, Tool::ClaudeCode);
    }
}
