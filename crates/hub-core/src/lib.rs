//! Agent Session Hub 核心库(hub-core)。
//!
//! 三层数据流:reader(源解析)→ IR(统一中间表示)→ mapper/writer(目标生成)。
//! 当前版本:Claude Code → Codex 迁移、Codex → Claude Code 反向迁移,
//! 以及 ZCode(SQLite 双库)的读取与写入;
//! launcher 附带迁移后的一键打开(Terminal / ZCode),safety 提供写入前进程护栏。

pub mod error;
pub mod ir;
pub mod launcher;
pub mod mapper;
pub mod reader;
pub mod safety;
pub mod writer;

pub use error::HubError;
pub use ir::{Role, SessionSummary, Tool, UnifiedMessage, UnifiedPart, UnifiedSession};
pub use launcher::{claude_resume_command, codex_resume_command, open_in_terminal, open_zcode_app};
pub use mapper::{map_session, CodexEvent};
pub use safety::is_tool_running;
// 两个 reader 暴露同名函数:根级沿用 Claude Code 版本(既有调用方),
// Codex 版本以带工具名的别名导出;亦可用模块限定路径 reader::codex::*。
pub use reader::claude::{read_session, scan_sessions};
pub use reader::codex::{read_session as read_codex_session, scan_sessions as scan_codex_sessions};
pub use reader::zcode::{read_session as read_zcode_session, scan_sessions as scan_zcode_sessions};
// 两个 writer 同理:根级 write_session 仍指 Codex 版本。
pub use writer::claude::{
    write_session as write_claude_session, write_session_with as write_claude_session_with,
    ClaudeWriteOutput,
};
pub use writer::codex::{write_session, write_session_with, CodexWriteOutput, IdGen, SystemIdGen};
pub use writer::zcode::{
    write_session as write_zcode_session, write_session_with as write_zcode_session_with,
    ZcodeWriteOutput,
};
