//! End-to-end integration tests: the full ZCode read → write → read-back
//! pipeline.
//!
//! Both the source and target databases are hand-built with rusqlite inside
//! tempdirs (minimal table layouts with real-device column names); no real
//! ~/.zcode is touched and no real data is copied.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hub_core::{
    read_zcode_session, scan_zcode_sessions, write_zcode_session_with, IdGen, Role, UnifiedPart,
};
use rusqlite::{params, Connection};

/// Minimal cli-database schema matching the real device (exact column names).
const CLI_DDL: &str = "
    CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT, revert TEXT, permission TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_compacting INTEGER, time_archived INTEGER, task_type TEXT NOT NULL DEFAULT 'interactive', title_source TEXT NOT NULL DEFAULT 'generated', title_message_id TEXT, time_title_updated INTEGER, trace_id TEXT);
    CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
    CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
    CREATE TABLE schema_migration(id TEXT PRIMARY KEY, checksum TEXT NOT NULL, app_version TEXT, time_applied INTEGER NOT NULL);
";
/// Minimal desktop task-list database schema matching the real device.
const TASKS_DDL: &str = "
    CREATE TABLE tasks(workspace_key TEXT NOT NULL, workspace_path TEXT NOT NULL, workspace_identity TEXT, task_id TEXT NOT NULL, title TEXT NOT NULL DEFAULT '', task_status TEXT, provider TEXT, mode TEXT NOT NULL DEFAULT 'build', model TEXT, migration_source TEXT, forked_from_task_id TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, unread_at INTEGER, last_unread_at INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0, deleted INTEGER NOT NULL DEFAULT 0, title_overridden INTEGER NOT NULL DEFAULT 0, meta_json TEXT NOT NULL DEFAULT '{}', searchable_text TEXT NOT NULL DEFAULT '', cron_automation_id TEXT, off_peak_task_id TEXT, PRIMARY KEY(workspace_key, task_id));
";

const FIXED_TS_FILE: &str = "2026-09-11T00-00-00";
const FIXED_TS_COLON: &str = "2026-09-11T00:00:00.000Z";

/// Incrementing uuids: the 1st goes to the session, then one per message
/// (message first, then part); time is constant.
struct CountingIdGen {
    counter: AtomicUsize,
}

impl IdGen for CountingIdGen {
    fn now_rfc3339(&self) -> String {
        FIXED_TS_FILE.to_string()
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

fn build_cli_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(CLI_DDL).unwrap();
    conn.execute(
        "INSERT INTO schema_migration(id, checksum, app_version, time_applied) \
         VALUES('0018_baseline', 'deadbeef', '1.4.2', 1)",
        [],
    )
    .unwrap();
}

fn build_tasks_db(path: &Path) {
    Connection::open(path)
        .unwrap()
        .execute_batch(TASKS_DDL)
        .unwrap();
}

fn insert_session(conn: &Connection, id: &str, title: &str, directory: &str, updated_ms: i64) {
    conn.execute(
        "INSERT INTO session(id, project_id, slug, directory, title, version, \
         time_created, time_updated) VALUES(?1, 'proj_x', ?2, ?3, ?4, '1.4.2', ?5, ?6)",
        params![id, id, directory, title, updated_ms, updated_ms],
    )
    .unwrap();
}

fn insert_message(conn: &Connection, id: &str, session: &str, seq: i64, data: &str) {
    conn.execute(
        "INSERT INTO message(id, session_id, time_created, time_updated, data, sequence) \
         VALUES(?1, ?2, 1789055537517, 1789055537517, ?3, ?4)",
        params![id, session, data, seq],
    )
    .unwrap();
}

fn insert_part(conn: &Connection, id: &str, message: &str, session: &str, seq: i64, data: &str) {
    conn.execute(
        "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data, sequence) \
         VALUES(?1, ?2, ?3, 1789055537517, 1789055537517, ?4, ?5)",
        params![id, message, session, data, seq],
    )
    .unwrap();
}

/// Build a source database containing typical message shapes:
/// the target session (user text / assistant reasoning / assistant tool /
/// UI-event parts), plus one archived session (which the scan must exclude).
fn build_source_db(root: &Path) -> PathBuf {
    let db = root.join("db.sqlite");
    build_cli_db(&db);
    {
        let conn = Connection::open(&db).unwrap();
        insert_session(
            &conn,
            "sess-live",
            "源会话标题",
            "/Users/x/MyProj",
            1789055538123,
        );
        insert_session(
            &conn,
            "sess-archived",
            "归档会话",
            "/Users/x/MyProj",
            1789055599999,
        );
        conn.execute(
            "UPDATE session SET time_archived = 1789055600000 WHERE id = 'sess-archived'",
            [],
        )
        .unwrap();

        // m1: user text (the step-start is skipped)
        insert_message(
            &conn,
            "m1",
            "sess-live",
            0,
            r#"{"role":"user","time":{"created":1789055537517}}"#,
        );
        insert_part(
            &conn,
            "p1",
            "m1",
            "sess-live",
            0,
            r#"{"type":"step-start"}"#,
        );
        insert_part(
            &conn,
            "p2",
            "m1",
            "sess-live",
            1,
            r#"{"type":"text","text":"你好,帮我看一下这个项目"}"#,
        );
        // m2: assistant reasoning (reasoning precedes the tool; reading takes
        // only the reasoning)
        insert_message(
            &conn,
            "m2",
            "sess-live",
            1,
            r#"{"role":"assistant","time":{"created":1789055537900,"completed":1789055538000},"parentID":"m1"}"#,
        );
        insert_part(
            &conn,
            "p3",
            "m2",
            "sess-live",
            0,
            r#"{"type":"reasoning","text":"用户要看目录,应当先 ls"}"#,
        );
        insert_part(
            &conn,
            "p4",
            "m2",
            "sess-live",
            1,
            r#"{"type":"tool","callID":"call_1","tool":"Bash","state":{"status":"completed","input":{"command":"ls -la"},"output":"total 0"}}"#,
        );
        // m3: assistant tool (the first usable part after the timeline is the
        // tool)
        insert_message(
            &conn,
            "m3",
            "sess-live",
            2,
            r#"{"role":"assistant","time":{"created":1789055538000}}"#,
        );
        insert_part(
            &conn,
            "p5",
            "m3",
            "sess-live",
            0,
            r#"{"type":"timeline","title":"执行命令"}"#,
        );
        insert_part(
            &conn,
            "p6",
            "m3",
            "sess-live",
            1,
            r#"{"type":"tool","callID":"call_2","tool":"Read","state":{"status":"completed","input":{"path":"src/main.rs"},"output":"fn main() {}"}}"#,
        );
    }
    db
}

// ---------- TC-ZE2E-01: read → write → read-back, content preserved ----------

#[test]
fn tc_ze2e_01_roundtrip_content_preserved() {
    let src_dir = tempfile::tempdir().unwrap();
    let src_db = build_source_db(src_dir.path());

    // The source database is read-only: byte-for-byte identical before and
    // after migration
    let src_bytes_before = fs::read(&src_db).unwrap();

    // Scan: the archived session is excluded; only the target session shows
    let summaries = scan_zcode_sessions(&src_db).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session_id, "sess-live");

    let source_ir = read_zcode_session(&src_db, "sess-live").unwrap();
    assert_eq!(source_ir.summary.project_dir, "/Users/x/MyProj");
    assert_eq!(source_ir.summary.title, "源会话标题");
    assert_eq!(source_ir.messages.len(), 3);
    assert_eq!(source_ir.parse_warnings, 0);
    assert_eq!(fs::read(&src_db).unwrap(), src_bytes_before, "源库只读");

    // Write into a fresh pair of temporary databases
    let dst_dir = tempfile::tempdir().unwrap();
    let dst_cli = dst_dir.path().join("db.sqlite");
    let dst_tasks = dst_dir.path().join("tasks-index.sqlite");
    build_cli_db(&dst_cli);
    build_tasks_db(&dst_tasks);

    let out = write_zcode_session_with(
        &source_ir,
        &dst_cli,
        &dst_tasks,
        &CountingIdGen {
            counter: AtomicUsize::new(0),
        },
    )
    .unwrap();
    assert!(out.session_id.starts_with("sess_"));
    assert_eq!(out.resume_command, "");

    // The target databases got their backups
    assert!(dst_dir
        .path()
        .join(format!("db.sqlite.hub-backup-{FIXED_TS_FILE}"))
        .is_file());
    assert!(dst_dir
        .path()
        .join(format!("tasks-index.sqlite.hub-backup-{FIXED_TS_FILE}"))
        .is_file());

    // Read back: content matches the source (message level)
    let back = read_zcode_session(&dst_cli, &out.session_id).unwrap();
    assert_eq!(back.summary.session_id, out.session_id);
    assert_eq!(back.summary.project_dir, "/Users/x/MyProj");
    assert_eq!(back.summary.title, "源会话标题");
    // The writer deliberately sets session time_updated to the migration
    // moment (so migrated sessions surface at the top of the task list);
    // only per-message timestamps round-trip from the source.
    assert_eq!(back.summary.last_active, FIXED_TS_COLON);
    assert_eq!(back.messages.len(), 3);
    assert_eq!(back.parse_warnings, 0);

    assert_eq!(back.messages[0].role, Role::User);
    assert_eq!(
        back.messages[0].parts,
        vec![UnifiedPart::Text("你好,帮我看一下这个项目".to_string())]
    );

    // Source message [reasoning] → written merged as "> 内部推理:…\n\n" text
    // → read back as Text
    assert_eq!(back.messages[1].role, Role::Assistant);
    assert_eq!(
        back.messages[1].parts,
        vec![UnifiedPart::Text(
            "> 内部推理:用户要看目录,应当先 ls\n\n".to_string()
        )]
    );

    // Source message [tool] → written merged as "[调用工具 …] <input>" text
    // → read back as Text
    assert_eq!(back.messages[2].role, Role::Assistant);
    assert_eq!(
        back.messages[2].parts,
        vec![UnifiedPart::Text(
            "[调用工具 Read] {\"path\":\"src/main.rs\"}".to_string()
        )]
    );

    // The role sequence as a whole matches
    let roles: Vec<Role> = back.messages.iter().map(|m| m.role).collect();
    let source_roles: Vec<Role> = source_ir.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, source_roles);

    // The tasks row is in place, with workspace_key pointing at the project
    // path
    let tasks_conn = Connection::open(&dst_tasks).unwrap();
    let (task_id, ws_key): (String, String) = tasks_conn
        .query_row("SELECT task_id, workspace_key FROM tasks", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(task_id, out.session_id);
    assert_eq!(ws_key, "/Users/x/MyProj");

    // Scanning the target databases: the migrated session is visible (not
    // archived, not deleted)
    let dst_summaries = scan_zcode_sessions(&dst_cli).unwrap();
    assert_eq!(dst_summaries.len(), 1);
    assert_eq!(dst_summaries[0].session_id, out.session_id);
}
