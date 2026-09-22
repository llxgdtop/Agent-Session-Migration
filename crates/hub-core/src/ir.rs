//! IR (unified intermediate representation) type definitions.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The three adapted tools. Currently used by `ClaudeCode` (read) and `Codex` (write).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Tool {
    ClaudeCode,
    Codex,
    ZCode,
}

/// Message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

/// Content block within a message. The current version keeps tool calls/results
/// in textual form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnifiedPart {
    /// Plain text block.
    Text(String),
    /// Plaintext reasoning.
    Reasoning(String),
    /// Tool call; `input_json` is the JSON text of the call arguments.
    ToolCall { tool: String, input_json: String },
    /// Tool result.
    ToolResult {
        tool: String,
        content: String,
        is_error: bool,
    },
}

/// A single unified message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedMessage {
    pub role: Role,
    pub parts: Vec<UnifiedPart>,
    /// Inline timestamp (RFC3339) passed through; None when absent.
    pub timestamp: Option<String>,
}

/// Session list entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Source session file name without the `.jsonl` extension.
    pub session_id: String,
    pub source_path: PathBuf,
    /// First 50 chars of the first user text; "untitled" when there is no user message.
    pub title: String,
    /// Always the first non-empty inline `cwd` from the JSONL lines.
    pub project_dir: String,
    /// Timestamp (RFC3339) of the last user/assistant line; falls back to file mtime when missing.
    pub last_active: String,
    /// Number of user+assistant messages.
    pub message_count: usize,
}

/// Full session IR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedSession {
    pub summary: SessionSummary,
    pub messages: Vec<UnifiedMessage>,
    /// Number of skipped bad lines.
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

    /// TC-IR-01: serialize with serde then deserialize; no field loss.
    #[test]
    fn tc_ir_01_serde_roundtrip_preserves_fields() {
        let original = sample_session();
        let json = serde_json::to_string(&original).expect("serialize IR");
        let back: UnifiedSession = serde_json::from_str(&json).expect("deserialize IR");
        assert_eq!(back, original);
    }

    /// TC-IR-01: Tool enum serialization names include "ClaudeCode"/"Codex"/"ZCode".
    #[test]
    fn tc_ir_01_tool_enum_serialization_names() {
        assert_eq!(
            serde_json::to_string(&Tool::ClaudeCode).unwrap(),
            "\"ClaudeCode\""
        );
        assert_eq!(serde_json::to_string(&Tool::Codex).unwrap(), "\"Codex\"");
        assert_eq!(serde_json::to_string(&Tool::ZCode).unwrap(), "\"ZCode\"");
        // And the reverse direction
        let tool: Tool = serde_json::from_str("\"ClaudeCode\"").unwrap();
        assert_eq!(tool, Tool::ClaudeCode);
    }
}
