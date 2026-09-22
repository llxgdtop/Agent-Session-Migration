//! ZCode desktop-app session database (SQLite) reading.
//!
//! Source layout: `~/.zcode/cli/db/db.sqlite` (WAL mode). The session/message/part
//! tables carry the session body; messages are ordered by sequence, and for
//! each message the first text/reasoning/tool part among its parts enters the
//! IR — UI-event shapes such as step-start/step-finish/timeline are skipped,
//! and a message with no usable part is skipped whole.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use crate::error::HubError;
use crate::ir::{Role, SessionSummary, UnifiedMessage, UnifiedPart, UnifiedSession};
use crate::reader::claude::title_of;

/// Scan all non-archived (time_archived IS NULL) sessions in the database,
/// ordered by time_updated descending, tie-broken at equal timestamps by
/// session_id lexicographic order.
///
/// A single session failing to read (including empty sessions) is skipped
/// without affecting the overall scan; if the database file itself does not
/// exist, SourceNotFound is returned.
pub fn scan_sessions(db_path: &Path) -> Result<Vec<SessionSummary>, HubError> {
    if !db_path.is_file() {
        return Err(HubError::SourceNotFound(db_path.to_path_buf()));
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT id FROM session WHERE time_archived IS NULL ORDER BY time_updated DESC, id ASC",
    )?;
    let ids: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    // The SQL already yields the target order; sessions that fail to read
    // are simply absent
    let mut summaries = Vec::new();
    for id in ids {
        if let Ok(session) = read_session_on(&conn, db_path, &id) {
            summaries.push(session.summary);
        }
    }
    Ok(summaries)
}

/// Read a single session into IR. Returns SourceNotFound when the session id
/// or the database file does not exist; EmptySession when no message is
/// interpretable.
pub fn read_session(db_path: &Path, session_id: &str) -> Result<UnifiedSession, HubError> {
    if !db_path.is_file() {
        return Err(HubError::SourceNotFound(db_path.to_path_buf()));
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    read_session_on(&conn, db_path, session_id)
}

/// Read a single session on an existing connection (scan reuses one
/// connection instead of reopening the database per session).
fn read_session_on(
    conn: &Connection,
    db_path: &Path,
    session_id: &str,
) -> Result<UnifiedSession, HubError> {
    let (title, directory, updated_ms): (String, String, i64) = match conn.query_row(
        "SELECT title, directory, time_updated FROM session WHERE id = ?1",
        params![session_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ) {
        Ok(row) => row,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(HubError::SourceNotFound(db_path.to_path_buf()));
        }
        Err(e) => return Err(e.into()),
    };

    let mut messages = Vec::new();
    let mut parse_warnings = 0usize;
    let mut msg_stmt = conn.prepare(
        "SELECT id, data, time_created FROM message WHERE session_id = ?1 \
         ORDER BY sequence ASC, id ASC",
    )?;
    let mut part_stmt =
        conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY sequence ASC, id ASC")?;
    let rows = msg_stmt.query_map(params![session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (message_id, data_json, row_created_ms) = row?;
        let data: Value = match serde_json::from_str(&data_json) {
            Ok(v) => v,
            Err(_) => {
                parse_warnings += 1;
                continue;
            }
        };
        let role = match data.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => {
                // Missing/unknown role: the row is uninterpretable, count it
                // as a bad line
                parse_warnings += 1;
                continue;
            }
        };
        let part = match first_unified_part(&mut part_stmt, &message_id, &mut parse_warnings)? {
            Some(part) => part,
            None => continue, // no text/reasoning/tool part at all: skip whole
        };
        // Message time prefers data.time.created, falling back to the
        // row-level time_created
        let created_ms = data
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(row_created_ms);
        messages.push(UnifiedMessage {
            role,
            parts: vec![part],
            timestamp: Some(ms_to_rfc3339(created_ms)),
        });
    }

    if messages.is_empty() {
        return Err(HubError::EmptySession(db_path.to_path_buf()));
    }

    let summary = SessionSummary {
        session_id: session_id.to_string(),
        source_path: db_path.to_path_buf(),
        // The title defers to session.title in the database; when empty it
        // falls back to derivation from messages (same as the other readers)
        title: if title.is_empty() {
            title_of(&messages)
        } else {
            title
        },
        project_dir: directory,
        last_active: ms_to_rfc3339(updated_ms),
        message_count: messages.len(),
    };
    Ok(UnifiedSession {
        summary,
        messages,
        parse_warnings,
    })
}

/// Scan a message's parts in order and return the first content block
/// eligible for the IR (text/reasoning/tool); other shapes
/// (step-start/step-finish/timeline and unknown types) are skipped;
/// invalid JSON counts as a bad line. A tool part contributes only
/// state.input — output never enters the IR.
fn first_unified_part(
    part_stmt: &mut rusqlite::Statement<'_>,
    message_id: &str,
    parse_warnings: &mut usize,
) -> Result<Option<UnifiedPart>, HubError> {
    let rows = part_stmt.query_map(params![message_id], |row| row.get::<_, String>(0))?;
    for row in rows {
        let data_json = row?;
        if let Some(part) = unified_part_of(&data_json, parse_warnings) {
            return Ok(Some(part));
        }
    }
    Ok(None)
}

/// A single part.data JSON → IR content block; unusable shapes return None
/// (the caller moves on to the next part).
fn unified_part_of(data_json: &str, parse_warnings: &mut usize) -> Option<UnifiedPart> {
    let value: Value = match serde_json::from_str(data_json) {
        Ok(v) => v,
        Err(_) => {
            *parse_warnings += 1;
            return None;
        }
    };
    match value.get("type").and_then(Value::as_str) {
        Some("text") => Some(UnifiedPart::Text(
            value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        Some("reasoning") => Some(UnifiedPart::Reasoning(
            value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        Some("tool") => Some(UnifiedPart::ToolCall {
            tool: value
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input_json: value
                .pointer("/state/input")
                .map(Value::to_string)
                .unwrap_or_else(|| "null".to_string()),
        }),
        _ => None,
    }
}

/// Epoch milliseconds → RFC3339 (UTC milliseconds, Z suffix). Values outside
/// the representable range degrade to the Unix epoch so the output is always
/// a valid, parseable timestamp.
fn ms_to_rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Minimal cli-database schema matching the real device (only the tables
    /// reader/writer touch, with exact column names).
    const CLI_DDL: &str = "
        CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT, revert TEXT, permission TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_compacting INTEGER, time_archived INTEGER, task_type TEXT NOT NULL DEFAULT 'interactive', title_source TEXT NOT NULL DEFAULT 'generated', title_message_id TEXT, time_title_updated INTEGER, trace_id TEXT);
        CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
        CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
        CREATE TABLE schema_migration(id TEXT PRIMARY KEY, checksum TEXT NOT NULL, app_version TEXT, time_applied INTEGER NOT NULL);
    ";

    /// Build a database and insert one verified-supported migration record
    /// (required by the write protocol; the reader itself does not check it).
    fn build_cli_db(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("db.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(CLI_DDL).unwrap();
        conn.execute(
            "INSERT INTO schema_migration(id, checksum, app_version, time_applied) \
             VALUES('0018_baseline', 'deadbeef', '1.4.2', 1)",
            [],
        )
        .unwrap();
        path
    }

    fn insert_session(
        conn: &Connection,
        id: &str,
        title: &str,
        directory: &str,
        updated_ms: i64,
        archived: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO session(id, project_id, slug, directory, title, version, \
             time_created, time_updated, time_archived) \
             VALUES(?1, ?2, ?3, ?4, ?5, '1.4.2', ?6, ?7, ?8)",
            params![
                id,
                format!("proj_{id}"),
                id,
                directory,
                title,
                updated_ms,
                updated_ms,
                archived
            ],
        )
        .unwrap();
    }

    fn insert_message(
        conn: &Connection,
        id: &str,
        session: &str,
        seq: i64,
        created_ms: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO message(id, session_id, time_created, time_updated, data, sequence) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, session, created_ms, created_ms, data, seq],
        )
        .unwrap();
    }

    fn insert_part(
        conn: &Connection,
        id: &str,
        message: &str,
        session: &str,
        seq: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data, sequence) \
             VALUES(?1, ?2, ?3, 0, 0, ?4, ?5)",
            params![id, message, session, data, seq],
        )
        .unwrap();
    }

    /// Parse an RFC3339 string back to epoch milliseconds (for asserting
    /// time-conversion round trips).
    fn to_ms(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    // ---------- TC-ZREAD-01 ----------
    /// Reading the three part kinds text/reasoning/tool, skipping
    /// step/timeline, "only the first usable part per message", bad JSON
    /// counting as a bad line, and summary fields plus time conversion.
    #[test]
    fn tc_zread_01_part_kinds_first_match_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let db = build_cli_db(&dir);
        {
            let conn = Connection::open(&db).unwrap();
            insert_session(
                &conn,
                "sess-a",
                "会话标题",
                "/Users/x/demo",
                1789055538123,
                None,
            );
            insert_message(
                &conn,
                "m1",
                "sess-a",
                0,
                1789055537517,
                r#"{"role":"user","time":{"created":1789055537517}}"#,
            );
            insert_part(
                &conn,
                "p1",
                "m1",
                "sess-a",
                0,
                r#"{"type":"step-start","time":{"start":1789055537517}}"#,
            );
            insert_part(
                &conn,
                "p2",
                "m1",
                "sess-a",
                1,
                r#"{"type":"text","text":"帮我看一下这个项目","time":{"start":1789055537517,"end":1789055537520}}"#,
            );
            // assistant: reasoning precedes tool → only reasoning is taken
            insert_message(
                &conn,
                "m2",
                "sess-a",
                1,
                1789055537900,
                r#"{"role":"assistant","time":{"created":1789055537900,"completed":1789055538000},"parentID":"m1","modelID":"GLM-5.3-Flash"}"#,
            );
            insert_part(
                &conn,
                "p3",
                "m2",
                "sess-a",
                0,
                r#"{"type":"reasoning","text":"用户要看目录,应当先 ls","time":{"start":1789055537900,"end":1789055537950}}"#,
            );
            insert_part(
                &conn,
                "p4",
                "m2",
                "sess-a",
                1,
                r#"{"type":"tool","callID":"call_1","tool":"Bash","state":{"status":"completed","input":{"command":"ls -la"},"output":"total 0"}}"#,
            );
            // assistant: a tool follows the timeline → only the tool is
            // taken, with input serialized and output ignored
            insert_message(
                &conn,
                "m3",
                "sess-a",
                2,
                1789055538000,
                r#"{"role":"assistant","time":{"created":1789055538000}}"#,
            );
            insert_part(
                &conn,
                "p5",
                "m3",
                "sess-a",
                0,
                r#"{"type":"timeline","title":"执行命令"}"#,
            );
            insert_part(
                &conn,
                "p6",
                "m3",
                "sess-a",
                1,
                r#"{"type":"tool","callID":"call_2","tool":"Read","state":{"status":"completed","input":{"path":"src/main.rs"},"output":"fn main() {}"}}"#,
            );
            // A message with only step-start/step-finish: skipped whole
            insert_message(
                &conn,
                "m4",
                "sess-a",
                3,
                1789055538100,
                r#"{"role":"assistant","time":{"created":1789055538100}}"#,
            );
            insert_part(&conn, "p7", "m4", "sess-a", 0, r#"{"type":"step-start"}"#);
            insert_part(&conn, "p8", "m4", "sess-a", 1, r#"{"type":"step-finish"}"#);
            // A message with no parts at all: skipped
            insert_message(
                &conn,
                "m5",
                "sess-a",
                4,
                1789055538110,
                r#"{"role":"user","time":{"created":1789055538110}}"#,
            );
            // A bad-JSON part counts as a bad line; the text part after it
            // is taken normally
            insert_message(
                &conn,
                "m6",
                "sess-a",
                5,
                1789055538120,
                r#"{"role":"user","time":{"created":1789055538120}}"#,
            );
            insert_part(&conn, "p9", "m6", "sess-a", 0, "{not-valid-json");
            insert_part(
                &conn,
                "p10",
                "m6",
                "sess-a",
                1,
                r#"{"type":"text","text":"第二个问题"}"#,
            );
        }

        let session = read_session(&db, "sess-a").unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.parse_warnings, 1);

        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(
            session.messages[0].parts,
            vec![UnifiedPart::Text("帮我看一下这个项目".to_string())]
        );
        assert_eq!(
            to_ms(session.messages[0].timestamp.as_deref().unwrap()),
            1789055537517
        );

        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(
            session.messages[1].parts,
            vec![UnifiedPart::Reasoning("用户要看目录,应当先 ls".to_string())]
        );

        assert_eq!(session.messages[2].role, Role::Assistant);
        assert_eq!(
            session.messages[2].parts,
            vec![UnifiedPart::ToolCall {
                tool: "Read".to_string(),
                input_json: r#"{"path":"src/main.rs"}"#.to_string(),
            }]
        );

        assert_eq!(
            session.messages[3].parts,
            vec![UnifiedPart::Text("第二个问题".to_string())]
        );

        // Summary: title/project_dir come from the session row; last_active
        // is time_updated rendered as RFC3339
        assert_eq!(session.summary.session_id, "sess-a");
        assert_eq!(session.summary.title, "会话标题");
        assert_eq!(session.summary.project_dir, "/Users/x/demo");
        assert_eq!(to_ms(&session.summary.last_active), 1789055538123);
        assert_eq!(session.summary.message_count, 4);
        assert_eq!(session.summary.source_path, db);
    }

    // ---------- TC-ZREAD-02 ----------
    /// Scan: ordered by time_updated desc, tie-broken by session_id
    /// lexicographic order; archived sessions excluded.
    #[test]
    fn tc_zread_02_scan_order_and_archived_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let db = build_cli_db(&dir);
        {
            let conn = Connection::open(&db).unwrap();
            let one_text = |conn: &Connection, sid: &str| {
                let mid = format!("m-{sid}");
                let pid = format!("p-{sid}");
                insert_message(
                    conn,
                    &mid,
                    sid,
                    0,
                    1,
                    r#"{"role":"user","time":{"created":1}}"#,
                );
                insert_part(conn, &pid, &mid, sid, 0, r#"{"type":"text","text":"hi"}"#);
            };
            insert_session(&conn, "s-old", "旧", "/p", 3000, None);
            one_text(&conn, "s-old");
            insert_session(&conn, "s-mid", "中", "/p", 4000, None);
            one_text(&conn, "s-mid");
            // Two sessions share a timestamp: s-new-a sorts before s-new-z
            // lexicographically; the archived one never appears
            insert_session(&conn, "s-new-z", "新z", "/p", 5000, None);
            one_text(&conn, "s-new-z");
            insert_session(&conn, "s-new-a", "新a", "/p", 5000, None);
            one_text(&conn, "s-new-a");
            insert_session(&conn, "s-archived", "归档", "/p", 9000, Some(9999));
            one_text(&conn, "s-archived");
        }

        let summaries = scan_sessions(&db).unwrap();
        let ids: Vec<&str> = summaries.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["s-new-a", "s-new-z", "s-mid", "s-old"]);
    }

    // ---------- TC-ZREAD-03 ----------
    /// A missing database file or unknown session id → SourceNotFound;
    /// an existing session with no interpretable message → EmptySession;
    /// an empty title falls back to derivation from the first user text.
    #[test]
    fn tc_zread_03_not_found_empty_session_and_title_fallback() {
        let missing = PathBuf::from("/definitely/not/existing/db.sqlite");
        assert!(matches!(
            scan_sessions(&missing),
            Err(HubError::SourceNotFound(_))
        ));
        assert!(matches!(
            read_session(&missing, "any"),
            Err(HubError::SourceNotFound(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        let db = build_cli_db(&dir);
        {
            let conn = Connection::open(&db).unwrap();
            insert_session(&conn, "s-empty", "", "/p", 100, None);
            // Only a step-start part: no interpretable message
            insert_message(
                &conn,
                "m",
                "s-empty",
                0,
                100,
                r#"{"role":"user","time":{"created":100}}"#,
            );
            insert_part(&conn, "p", "m", "s-empty", 0, r#"{"type":"step-start"}"#);
        }
        let err = read_session(&db, "s-empty").unwrap_err();
        assert!(matches!(err, HubError::EmptySession(_)));
        // The empty session is absent from the scan (no error)
        assert!(scan_sessions(&db).unwrap().is_empty());

        {
            let conn = Connection::open(&db).unwrap();
            insert_session(&conn, "s-untitled", "", "/p", 200, None);
            insert_message(
                &conn,
                "m2",
                "s-untitled",
                0,
                200,
                r#"{"role":"user","time":{"created":200}}"#,
            );
            insert_part(
                &conn,
                "p2",
                "m2",
                "s-untitled",
                0,
                r#"{"type":"text","text":"首条提问作为标题"}"#,
            );
        }
        let session = read_session(&db, "s-untitled").unwrap();
        assert_eq!(session.summary.title, "首条提问作为标题");

        // Database exists but the session id does not
        assert!(matches!(
            read_session(&db, "no-such-session"),
            Err(HubError::SourceNotFound(_))
        ));
    }

    // ---------- TC-ZREAD-04 ----------
    /// Time conversion: 0 milliseconds → the canonical RFC3339 string of the
    /// Unix epoch.
    #[test]
    fn tc_zread_04_epoch_zero_renders_unix_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let db = build_cli_db(&dir);
        {
            let conn = Connection::open(&db).unwrap();
            insert_session(&conn, "s0", "t", "/p", 0, None);
            insert_message(
                &conn,
                "m0",
                "s0",
                0,
                0,
                r#"{"role":"user","time":{"created":0}}"#,
            );
            insert_part(&conn, "p0", "m0", "s0", 0, r#"{"type":"text","text":"q"}"#);
        }
        let session = read_session(&db, "s0").unwrap();
        assert_eq!(session.summary.last_active, "1970-01-01T00:00:00.000Z");
        assert_eq!(
            session.messages[0].timestamp.as_deref(),
            Some("1970-01-01T00:00:00.000Z")
        );
    }
}
