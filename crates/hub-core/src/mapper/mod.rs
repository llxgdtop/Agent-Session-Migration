//! IR → Codex rollout event mapping.

use crate::error::HubError;
use crate::ir::{Role, UnifiedPart, UnifiedSession};

/// Intermediate event corresponding one-to-one with a target rollout JSONL line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexEvent {
    SessionMeta {
        id: String,
        timestamp: String,
        cwd: String,
    },
    TurnContext {
        cwd: String,
        summary: String,
    },
    Message {
        role: Role,
        text: String,
        timestamp: String,
    },
    UserMessageEvent {
        message: String,
        timestamp: String,
    },
}

/// Truncation thresholds counted in Unicode scalars (chars).
const REASONING_MAX_CHARS: usize = 1000;
const TOOL_INPUT_MAX_CHARS: usize = 500;
const TOOL_RESULT_MAX_CHARS: usize = 2000;
const ELLIPSIS: &str = "…";

/// IR → event sequence.
///
/// - session_id and meta_timestamp are supplied by the caller
///   (write_session_with takes them from the IdGen), guaranteeing
///   filename uuid == session_meta.id;
/// - messages with empty parts, or whose merged text is empty, are dropped;
///   if everything is dropped, EmptySession is returned;
/// - a user message yields exactly one response_item plus one user_message
///   event; the writer derives no events on its own;
/// - a missing message timestamp falls back to meta_timestamp.
pub fn map_session(
    ir: &UnifiedSession,
    session_id: &str,
    meta_timestamp: &str,
) -> Result<Vec<CodexEvent>, HubError> {
    let empty_session = || HubError::EmptySession(ir.summary.source_path.clone());

    if ir.messages.is_empty() {
        return Err(empty_session());
    }

    let mut events = vec![
        CodexEvent::SessionMeta {
            id: session_id.to_string(),
            timestamp: meta_timestamp.to_string(),
            cwd: ir.summary.project_dir.clone(),
        },
        CodexEvent::TurnContext {
            cwd: ir.summary.project_dir.clone(),
            summary: ir.summary.title.clone(),
        },
    ];

    let mut emitted = 0usize;
    for message in &ir.messages {
        if message.parts.is_empty() {
            continue;
        }
        let text = render_parts(&message.parts);
        if text.is_empty() {
            continue; // empty after merging: drop as well
        }
        let timestamp = message
            .timestamp
            .clone()
            .unwrap_or_else(|| meta_timestamp.to_string());
        events.push(CodexEvent::Message {
            role: message.role,
            text: text.clone(),
            timestamp: timestamp.clone(),
        });
        if message.role == Role::User {
            events.push(CodexEvent::UserMessageEvent {
                message: text,
                timestamp,
            });
        }
        emitted += 1;
    }

    if emitted == 0 {
        return Err(empty_session()); // degenerate case: no message events survived mapping
    }
    Ok(events)
}

/// Concatenate the parts of one message, in order, into a single text:
/// thinking → "> 内部推理:<t>\n\n", text → verbatim,
/// tool_use → "[调用工具 <name>] <input_json>",
/// tool_result → "[工具结果 <tool> isError=<b>] <content>".
/// Truncation thresholds: reasoning 1000 / tool input 500 / tool result 2000
/// chars, with "…" appended.
/// (writer::claude reuses the same merging rules for the Claude Code target,
/// hence crate-visible.)
pub(crate) fn render_parts(parts: &[UnifiedPart]) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            UnifiedPart::Text(text) => out.push_str(text),
            UnifiedPart::Reasoning(text) => {
                out.push_str("> 内部推理:");
                out.push_str(&truncate_chars(text, REASONING_MAX_CHARS));
                out.push_str("\n\n");
            }
            UnifiedPart::ToolCall { tool, input_json } => {
                out.push_str(&format!(
                    "[调用工具 {tool}] {}",
                    truncate_chars(input_json, TOOL_INPUT_MAX_CHARS)
                ));
            }
            UnifiedPart::ToolResult {
                tool,
                content,
                is_error,
            } => {
                out.push_str(&format!(
                    "[工具结果 {tool} isError={is_error}] {}",
                    truncate_chars(content, TOOL_RESULT_MAX_CHARS)
                ));
            }
        }
    }
    out
}

/// Truncate by chars; append an ellipsis when the text is too long.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let mut cut: String = text.chars().take(max).collect();
        cut.push_str(ELLIPSIS);
        cut
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{SessionSummary, UnifiedMessage};
    use std::path::PathBuf;

    const SID: &str = "00000000-0000-4000-8000-000000000001";
    const META_TS: &str = "2026-09-11T00:00:00.000Z";

    fn ir_with(messages: Vec<UnifiedMessage>) -> UnifiedSession {
        UnifiedSession {
            summary: SessionSummary {
                session_id: "src".to_string(),
                source_path: PathBuf::from("/tmp/src.jsonl"),
                title: "标题".to_string(),
                project_dir: "/tmp/proj".to_string(),
                last_active: "2026-09-10T10:00:00.000Z".to_string(),
                message_count: messages.len(),
            },
            messages,
            parse_warnings: 0,
        }
    }

    fn msg(role: Role, parts: Vec<UnifiedPart>) -> UnifiedMessage {
        UnifiedMessage {
            role,
            parts,
            timestamp: Some("2026-09-10T10:00:00.000Z".to_string()),
        }
    }

    // ---------- TC-MAP-01 ----------
    #[test]
    fn tc_map_01_user_text_produces_paired_events() {
        let ir = ir_with(vec![msg(
            Role::User,
            vec![UnifiedPart::Text("你好".to_string())],
        )]);
        let events = map_session(&ir, SID, META_TS).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0],
            CodexEvent::SessionMeta {
                id: SID.to_string(),
                timestamp: META_TS.to_string(),
                cwd: "/tmp/proj".to_string(),
            }
        );
        assert_eq!(
            events[1],
            CodexEvent::TurnContext {
                cwd: "/tmp/proj".to_string(),
                summary: "标题".to_string(),
            }
        );
        assert_eq!(
            events[2],
            CodexEvent::Message {
                role: Role::User,
                text: "你好".to_string(),
                timestamp: "2026-09-10T10:00:00.000Z".to_string(),
            }
        );
        assert_eq!(
            events[3],
            CodexEvent::UserMessageEvent {
                message: "你好".to_string(),
                timestamp: "2026-09-10T10:00:00.000Z".to_string(),
            }
        );
    }

    // ---------- TC-MAP-02 ----------
    #[test]
    fn tc_map_02_assistant_text_no_event_msg() {
        let ir = ir_with(vec![msg(
            Role::Assistant,
            vec![UnifiedPart::Text("回答".to_string())],
        )]);
        let events = map_session(&ir, SID, META_TS).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[2],
            CodexEvent::Message {
                role: Role::Assistant,
                text: "回答".to_string(),
                timestamp: "2026-09-10T10:00:00.000Z".to_string(),
            }
        );
        assert!(!events
            .iter()
            .any(|e| matches!(e, CodexEvent::UserMessageEvent { .. })));
    }

    // ---------- TC-MAP-03 ----------
    #[test]
    fn tc_map_03_reasoning_merged_as_prefix() {
        let ir = ir_with(vec![msg(
            Role::Assistant,
            vec![
                UnifiedPart::Reasoning("想一想".to_string()),
                UnifiedPart::Text("答案".to_string()),
            ],
        )]);
        let events = map_session(&ir, SID, META_TS).unwrap();
        let CodexEvent::Message { text, .. } = &events[2] else {
            panic!("expected Message event");
        };
        assert_eq!(text, "> 内部推理:想一想\n\n答案");
    }

    // ---------- TC-MAP-04 ----------
    #[test]
    fn tc_map_04_tool_call_and_result_textualized() {
        let ir = ir_with(vec![
            msg(
                Role::Assistant,
                vec![UnifiedPart::ToolCall {
                    tool: "Bash".to_string(),
                    input_json: "{\"command\":\"ls\"}".to_string(),
                }],
            ),
            msg(
                Role::User,
                vec![UnifiedPart::ToolResult {
                    tool: "Bash".to_string(),
                    content: "done".to_string(),
                    is_error: true,
                }],
            ),
        ]);
        let events = map_session(&ir, SID, META_TS).unwrap();
        let CodexEvent::Message {
            role: r1, text: t1, ..
        } = &events[2]
        else {
            panic!("expected Message event");
        };
        assert_eq!(
            (*r1, t1.as_str()),
            (Role::Assistant, "[调用工具 Bash] {\"command\":\"ls\"}")
        );
        let CodexEvent::Message {
            role: r2, text: t2, ..
        } = &events[3]
        else {
            panic!("expected Message event");
        };
        assert_eq!(
            (*r2, t2.as_str()),
            (Role::User, "[工具结果 Bash isError=true] done")
        );
    }

    // ---------- TC-MAP-05 ----------
    #[test]
    fn tc_map_05_empty_session_rejected() {
        let ir = ir_with(vec![]);
        assert!(matches!(
            map_session(&ir, SID, META_TS),
            Err(HubError::EmptySession(_))
        ));

        // Degenerate case: messages exist but all have empty parts (or merge
        // to empty) — rejected just the same
        let ir_all_empty = ir_with(vec![
            msg(Role::Assistant, vec![]),
            msg(Role::User, vec![UnifiedPart::Text(String::new())]),
        ]);
        assert!(matches!(
            map_session(&ir_all_empty, SID, META_TS),
            Err(HubError::EmptySession(_))
        ));
    }

    // ---------- TC-MAP-06 ----------
    #[test]
    fn tc_map_06_empty_parts_message_dropped() {
        let ir = ir_with(vec![
            msg(Role::User, vec![UnifiedPart::Text("hi".to_string())]),
            msg(Role::Assistant, vec![]),
            msg(Role::User, vec![UnifiedPart::Text("again".to_string())]),
        ]);
        let events = map_session(&ir, SID, META_TS).unwrap();
        // meta + ctx + (user msg + event) + (user msg + event) = 6
        assert_eq!(events.len(), 6);
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                CodexEvent::Message { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi", "again"]);
    }

    // ---------- TC-MAP-07 ----------
    #[test]
    fn tc_map_07_truncation_by_char_with_ellipsis() {
        let long_reasoning: String = std::iter::repeat_n('思', 1001).collect();
        let long_input: String = std::iter::repeat_n('x', 501).collect();
        let long_result: String = std::iter::repeat_n('果', 2001).collect();

        let ir = ir_with(vec![
            msg(
                Role::Assistant,
                vec![
                    UnifiedPart::Reasoning(long_reasoning),
                    UnifiedPart::ToolCall {
                        tool: "Bash".to_string(),
                        input_json: long_input,
                    },
                ],
            ),
            msg(
                Role::User,
                vec![UnifiedPart::ToolResult {
                    tool: "Bash".to_string(),
                    content: long_result,
                    is_error: false,
                }],
            ),
        ]);
        let events = map_session(&ir, SID, META_TS).unwrap();

        let CodexEvent::Message {
            text: assistant_text,
            ..
        } = &events[2]
        else {
            panic!("expected Message event");
        };
        // reasoning truncated to 1000 chars + "…", followed by "\n\n" and the
        // tool call
        assert!(assistant_text.starts_with("> 内部推理:"));
        assert!(assistant_text.contains("…\n\n[调用工具 Bash] "));
        let reasoning_part: String = std::iter::repeat_n('思', 1000).collect();
        assert!(assistant_text.contains(&reasoning_part));
        assert!(!assistant_text.contains(&(reasoning_part.clone() + "思")));
        // input_json truncated to 500 chars + "…"
        let input_500: String = std::iter::repeat_n('x', 500).collect();
        assert!(assistant_text.ends_with(&format!("[调用工具 Bash] {input_500}…")));
        assert_eq!(assistant_text.chars().filter(|c| *c == '…').count(), 2);

        let CodexEvent::Message {
            text: user_text, ..
        } = &events[3]
        else {
            panic!("expected Message event");
        };
        // tool_result content truncated to 2000 chars + "…"
        assert!(user_text.starts_with("[工具结果 Bash isError=false] "));
        let body = user_text.trim_start_matches("[工具结果 Bash isError=false] ");
        assert_eq!(body.chars().count(), 2001); // 2000 + ellipsis
        assert!(body.ends_with('…'));
    }

    /// A missing message timestamp falls back to meta_timestamp.
    #[test]
    fn tc_map_07_missing_timestamp_falls_back_to_meta() {
        let ir = ir_with(vec![UnifiedMessage {
            role: Role::User,
            parts: vec![UnifiedPart::Text("q".to_string())],
            timestamp: None,
        }]);
        let events = map_session(&ir, SID, META_TS).unwrap();
        assert_eq!(
            events[2],
            CodexEvent::Message {
                role: Role::User,
                text: "q".to_string(),
                timestamp: META_TS.to_string(),
            }
        );
        assert_eq!(
            events[3],
            CodexEvent::UserMessageEvent {
                message: "q".to_string(),
                timestamp: META_TS.to_string(),
            }
        );
    }
}
