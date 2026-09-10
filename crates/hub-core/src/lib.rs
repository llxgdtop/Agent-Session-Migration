//! Agent Session Hub 核心库(hub-core)。
//!
//! 三层数据流:reader(源解析)→ IR(统一中间表示)→ mapper/writer(目标生成)。
//! MVP 范围:Claude Code → Codex 单路线(见 MVP-DEVELOPMENT.md §1 BR-1)。

pub mod error;
pub mod ir;
pub mod mapper;
pub mod reader;
pub mod writer;

pub use error::HubError;
pub use ir::{Role, SessionSummary, Tool, UnifiedMessage, UnifiedPart, UnifiedSession};
pub use mapper::{map_session, CodexEvent};
pub use reader::claude::{read_session, scan_sessions};
pub use writer::codex::{write_session, write_session_with, CodexWriteOutput, IdGen, SystemIdGen};
