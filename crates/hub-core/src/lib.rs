//! Agent Session Hub core library (hub-core).
//!
//! Three-layer data flow: reader (source parsing) → IR (unified intermediate
//! representation) → mapper/writer (target generation).
//! Current version: Claude Code → Codex migration, the reverse Codex →
//! Claude Code migration, plus ZCode (dual SQLite database) read and write;
//! launcher provides post-migration one-click open (Terminal / ZCode), and
//! safety provides the pre-write process guard.

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
// The readers expose identically named functions: the crate root keeps the
// Claude Code versions (existing callers), while the Codex ones are exported
// under tool-prefixed aliases; module-qualified paths reader::codex::* also work.
pub use reader::claude::{read_session, scan_sessions};
pub use reader::codex::{read_session as read_codex_session, scan_sessions as scan_codex_sessions};
pub use reader::zcode::{read_session as read_zcode_session, scan_sessions as scan_zcode_sessions};
// Same idea for the two writers: the root-level write_session still refers to
// the Codex version.
pub use writer::claude::{
    write_session as write_claude_session, write_session_with as write_claude_session_with,
    ClaudeWriteOutput,
};
pub use writer::codex::{write_session, write_session_with, CodexWriteOutput, IdGen, SystemIdGen};
pub use writer::zcode::{
    write_session as write_zcode_session, write_session_with as write_zcode_session_with,
    ZcodeWriteOutput,
};
