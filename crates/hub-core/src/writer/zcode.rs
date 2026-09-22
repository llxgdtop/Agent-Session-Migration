//! ZCode desktop-app session database (SQLite) writing.
//!
//! Target layout: dual databases — `~/.zcode/cli/db/db.sqlite`
//! (session/message/part tables, the session body) and
//! `~/.zcode/v2/tasks-index.sqlite` (tasks table, the desktop task list).
//!
//! Write protocol (guardrails, all mandatory):
//! 1. Verify that the cli database's latest schema_migration id starts with a
//!    verified-supported prefix; refuse to write on anything unrecognized (a
//!    newer version may have changed the table layout);
//! 2. Back up both databases as `<original path>.hub-backup-<timestamp>`;
//!    abort immediately if a backup fails;
//! 3. Write session+message+part to the cli database in a single transaction,
//!    rolling back entirely on any failure; write the task row to the tasks
//!    database in its own single transaction;
//! 4. Idempotent: an already-existing session.id reports TargetExists and
//!    touches no data (including creating no backup).
//!
//! We only ever append sessions to an existing ZCode installation and never
//! initialize a new database; a missing database file is rejected outright.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::HubError;
use crate::ir::{Role, UnifiedSession};
use crate::mapper::render_parts;
use crate::writer::codex::{IdGen, SystemIdGen};

/// Fixed field values (matching real-device observations from 2026-09).
const AGENT: &str = "zcode-agent";
const PROVIDER_ID: &str = "builtin:bigmodel-coding-plan";
const MODEL_ID: &str = "GLM-5.3-Flash";
const MODEL_VARIANT: &str = "low";
const MODE: &str = "build";
/// The provider in the tasks row and meta_json must be one of ZCode's
/// validated enums (claude|opencode|gemini|codex|glm); otherwise the whole
/// row is silently dropped at load time (real-device log: task-index-repo
/// "读取 task index meta_json 非法").
const TASKS_PROVIDER: &str = "glm";
const FINISH: &str = "completed";
const TASK_TYPE: &str = "interactive";
const TITLE_SOURCE: &str = "generated";
/// Status value for the desktop task list: everything migrated in is an
/// already-finished conversation.
const TASK_STATUS: &str = "completed";
/// Verified-supported schema_migration version prefix; a newer version may
/// have changed the table layout — reject outright.
const SUPPORTED_MIGRATION_PREFIX: &str = "0018";
/// Placeholder description when the schema_migration table is missing or has
/// no records (surfaces in the SchemaUnsupported message).
const NO_MIGRATION: &str = "无迁移记录";

#[derive(Debug)]
pub struct ZcodeWriteOutput {
    pub session_id: String,
    /// ZCode is a desktop app with no terminal resume command; this stays an
    /// empty string for now — "open the session after migration" is handled
    /// later at the application layer.
    pub resume_command: String,
}

/// Convenience entry point: system time and random uuid. Use
/// [`write_session_with`] for deterministic tests.
pub fn write_session(
    ir: &UnifiedSession,
    cli_db: &Path,
    tasks_db: &Path,
) -> Result<ZcodeWriteOutput, HubError> {
    write_session_with(ir, cli_db, tasks_db, &SystemIdGen)
}

/// Write entry point with an injected IdGen: session/message/part ids and
/// fallback timestamps come from gen.
pub fn write_session_with(
    ir: &UnifiedSession,
    cli_db: &Path,
    tasks_db: &Path,
    gen: &dyn IdGen,
) -> Result<ZcodeWriteOutput, HubError> {
    // ---- 1. Pure in-memory preparation: report any input problem before
    //         touching the database files ----
    if ir.summary.project_dir.is_empty() {
        return Err(HubError::InvalidInput(
            "源会话缺少项目目录(project_dir),无法确定 ZCode 工作区".to_string(),
        ));
    }
    let project_dir = ir.summary.project_dir.clone();
    let now_ms = now_millis(gen)?;
    let session_id = format!("sess_{}", gen.uuid_v4());
    // ZCode's task list validates meta_json on load; traceId is mandatory
    let trace_id = gen.uuid_v4();

    // Render message by message: each IR message → 1 message + 1 text part
    // (content follows the unified merging rules); messages with empty parts
    // or empty merged text are skipped; EmptySession when all are skipped.
    // parentID points at the previous persisted message; the first message
    // omits the field.
    let mut planned: Vec<PlannedMessage> = Vec::new();
    let mut parent_id: Option<String> = None;
    for message in &ir.messages {
        if message.parts.is_empty() {
            continue;
        }
        let text = render_parts(&message.parts);
        if text.is_empty() {
            continue;
        }
        let message_id = format!("msg_{}", gen.uuid_v4());
        let part_id = format!("part_{}", gen.uuid_v4());
        let ts_ms = message
            .timestamp
            .as_deref()
            .and_then(parse_ms)
            .unwrap_or(now_ms);
        let data_json = match message.role {
            Role::User => serialize_user_message(&project_dir, ts_ms),
            Role::Assistant => {
                serialize_assistant_message(&project_dir, ts_ms, parent_id.as_deref())
            }
        };
        planned.push(PlannedMessage {
            message_id: message_id.clone(),
            part_id,
            ts_ms,
            data_json,
            part_data_json: text_part_json(&text, ts_ms),
        });
        parent_id = Some(message_id);
    }
    if planned.is_empty() {
        return Err(HubError::EmptySession(ir.summary.source_path.clone()));
    }

    // Session creation/update times take the migration moment: a freshly
    // migrated session must be immediately visible at the top of ZCode's task
    // list. Keeping the source session's activity time would sink newly
    // migrated sessions to the bottom under their old dates (real-data
    // validation showed users concluding the migration had failed). Messages
    // inside the session keep their original timestamps.
    let time_created = now_ms;
    let time_updated = now_ms;

    // ---- 2. Both target databases must already exist: never initialize or
    //         overwrite a new database ----
    if !cli_db.is_file() {
        return Err(HubError::InvalidInput(format!(
            "目标会话库不存在: {}",
            cli_db.display()
        )));
    }
    if !tasks_db.is_file() {
        return Err(HubError::InvalidInput(format!(
            "目标任务库不存在: {}",
            tasks_db.display()
        )));
    }

    // ---- 3. Schema check: write only into verified-supported database versions ----
    let mut cli_conn = Connection::open(cli_db)?;
    let (migration_id, app_version) = latest_migration(&cli_conn)?;
    if !migration_id.starts_with(SUPPORTED_MIGRATION_PREFIX) {
        return Err(HubError::SchemaUnsupported(migration_id));
    }

    // ---- 4. Idempotent: refuse if session.id already exists, touching no
    //         data (and creating no backup) ----
    let exists: i64 = cli_conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
        params![session_id],
        |row| row.get(0),
    )?;
    if exists != 0 {
        return Err(HubError::TargetExists(cli_db.to_path_buf()));
    }

    // ---- 5. Automatic backup: before any write action, copy both databases
    //         to timestamped replicas ----
    let backup_stamp = gen.now_rfc3339(); // filename-friendly format (colons → hyphens)
    backup_db(cli_db, &backup_stamp)?;
    backup_db(tasks_db, &backup_stamp)?;

    // ---- 6. cli database single transaction: session + message + part;
    //         any failure rolls back the whole thing ----
    let project_id = project_id_of(&project_dir);
    let title = ir.summary.title.clone();
    {
        let tx = cli_conn.transaction()?;
        tx.execute(
            INSERT_SESSION_SQL,
            params![
                session_id,
                project_id,
                None::<&str>, // workspace_id: the database allows NULL
                None::<&str>, // parent_id: top-level session
                session_id,   // slug rule: equals the id
                project_dir,
                None::<&str>, // path: nullable legacy field
                title,
                app_version.unwrap_or_default(), // version follows the app version of the database's migration record
                None::<&str>,                    // share_url
                None::<i64>,                     // summary_additions
                None::<i64>,                     // summary_deletions
                None::<i64>,                     // summary_files
                None::<&str>,                    // summary_diffs
                None::<&str>,                    // revert
                None::<&str>,                    // permission
                time_created,
                time_updated,
                None::<i64>, // time_compacting
                None::<i64>, // time_archived: must be NULL, or the desktop app won't show it
                TASK_TYPE,
                TITLE_SOURCE,
                None::<&str>, // title_message_id
                None::<i64>,  // time_title_updated
                Some(trace_id.as_str()),
            ],
        )?;
        for (index, message) in planned.iter().enumerate() {
            tx.execute(
                "INSERT INTO message(id, session_id, time_created, time_updated, data, sequence) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    message.message_id,
                    session_id,
                    message.ts_ms,
                    message.ts_ms,
                    message.data_json,
                    index as i64
                ],
            )?;
            tx.execute(
                "INSERT INTO part(id, message_id, session_id, time_created, time_updated, \
                 data, sequence) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    message.part_id,
                    message.message_id,
                    session_id,
                    message.ts_ms,
                    message.ts_ms,
                    message.part_data_json,
                    0 // exactly one part per message
                ],
            )?;
        }
        tx.commit()?;
    }

    // ---- 7. tasks database single transaction: the desktop task-list row
    //         (task_id must equal session.id) ----
    {
        let mut tasks_conn = Connection::open(tasks_db)?;
        let tx = tasks_conn.transaction()?;
        tx.execute(
            INSERT_TASK_SQL,
            params![
                project_dir,  // workspace_key = absolute project path
                project_dir,  // workspace_path = absolute project path
                None::<&str>, // workspace_identity
                session_id,
                title,
                TASK_STATUS,
                TASKS_PROVIDER,
                MODE,
                MODEL_ID,
                None::<&str>, // migration_source: reserved for ZCode's own migrations
                None::<&str>, // forked_from_task_id
                time_created,
                time_updated,
                None::<i64>, // unread_at
                0,           // last_unread_at
                0,           // pinned
                0,           // archived: must be 0, or the list won't show it
                0,           // deleted
                0,           // title_overridden
                task_meta_json(
                    &session_id,
                    &trace_id,
                    &title,
                    &project_dir,
                    time_created,
                    time_updated,
                ),
                title,        // searchable_text: the title participates in search
                None::<&str>, // cron_automation_id
                None::<&str>, // off_peak_task_id
            ],
        )?;
        tx.commit()?;
    }

    Ok(ZcodeWriteOutput {
        session_id,
        resume_command: String::new(),
    })
}

/// A message fully assembled before writing (1 message row + 1 text part row).
/// parentID already lives inside data_json, so it needs no separate field.
struct PlannedMessage {
    message_id: String,
    part_id: String,
    ts_ms: i64,
    data_json: String,
    part_data_json: String,
}

const INSERT_SESSION_SQL: &str = "
    INSERT INTO session(
        id, project_id, workspace_id, parent_id, slug, directory, path, title, version,
        share_url, summary_additions, summary_deletions, summary_files, summary_diffs,
        revert, permission, time_created, time_updated, time_compacting, time_archived,
        task_type, title_source, title_message_id, time_title_updated, trace_id
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
        ?10, ?11, ?12, ?13, ?14,
        ?15, ?16, ?17, ?18, ?19, ?20,
        ?21, ?22, ?23, ?24, ?25
    )";

const INSERT_TASK_SQL: &str = "
    INSERT INTO tasks(
        workspace_key, workspace_path, workspace_identity, task_id, title, task_status,
        provider, mode, model, migration_source, forked_from_task_id,
        created_at, updated_at, unread_at, last_unread_at,
        pinned, archived, deleted, title_overridden, meta_json, searchable_text,
        cron_automation_id, off_peak_task_id
    ) VALUES (
        ?1, ?2, ?3, ?4, ?5, ?6,
        ?7, ?8, ?9, ?10, ?11,
        ?12, ?13, ?14, ?15,
        ?16, ?17, ?18, ?19, ?20, ?21,
        ?22, ?23
    )";

/// Take the latest schema_migration record (by time_applied, tie-broken by id
/// at equal timestamps). A missing table or no records yields the placeholder
/// description, which the caller rejects as "unsupported".
fn latest_migration(conn: &Connection) -> Result<(String, Option<String>), HubError> {
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_migration'",
        [],
        |row| row.get(0),
    )?;
    if has_table == 0 {
        return Ok((NO_MIGRATION.to_string(), None));
    }
    match conn.query_row(
        "SELECT id, app_version FROM schema_migration \
         ORDER BY time_applied DESC, id DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
        Ok(pair) => Ok(pair),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok((NO_MIGRATION.to_string(), None)),
        Err(e) => Err(e.into()),
    }
}

/// Copy the database file to `<original path>.hub-backup-<timestamp>`; skip
/// when the file does not exist. A failed backup aborts the write: never
/// modify a target database without a backup.
fn backup_db(db: &Path, stamp: &str) -> Result<(), HubError> {
    if !db.exists() {
        return Ok(());
    }
    let target = PathBuf::from(format!("{}.hub-backup-{stamp}", db.display()));
    fs::copy(db, &target)?;
    Ok(())
}

/// project_id rule (verified on a real device): strip the leading slash,
/// turn inner slashes into hyphens, lowercase, prefix "proj_".
/// Example: /Users/x/MyProj → proj_users-x-myproj.
fn project_id_of(directory: &str) -> String {
    format!(
        "proj_{}",
        directory
            .trim_start_matches('/')
            .replace('/', "-")
            .to_lowercase()
    )
}

/// Epoch milliseconds from the IdGen's RFC3339 time.
fn now_millis(gen: &dyn IdGen) -> Result<i64, HubError> {
    DateTime::parse_from_rfc3339(&gen.now_rfc3339_colon())
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
        .map_err(|_| HubError::InvalidInput("时间源返回值不是合法 RFC3339".to_string()))
}

/// RFC3339 string → epoch milliseconds; None when unparseable.
fn parse_ms(rfc3339: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
}

// ---- Wire shapes of message.data / part.data / meta_json ----

/// Shared time fields of message.data: user has only created; assistant also
/// carries completed.
#[derive(Serialize)]
struct WireTime {
    created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed: Option<i64>,
}

#[derive(Serialize)]
struct WireUserModel<'a> {
    #[serde(rename = "providerID")]
    provider_id: &'a str,
    #[serde(rename = "modelID")]
    model_id: &'a str,
    variant: &'a str,
}

#[derive(Serialize)]
struct WireEnvInfo<'a> {
    cwd: &'a str,
    /// The platform at migration time; the source machine's remaining
    /// environment is unknowable, so don't fabricate it.
    platform: &'a str,
}

#[derive(Serialize)]
struct WireContextSnapshot<'a> {
    #[serde(rename = "envInfo")]
    env_info: WireEnvInfo<'a>,
}

#[derive(Serialize)]
struct WireSemantics<'a> {
    origin: &'a str,
    kind: &'a str,
    ui_visibility: &'a str,
    provider_visibility: &'a str,
    transcript_visibility: &'a str,
}

#[derive(Serialize)]
struct WireUserMessageData<'a> {
    role: &'a str,
    time: WireTime,
    agent: &'a str,
    model: WireUserModel<'a>,
    #[serde(rename = "contextSnapshot")]
    context_snapshot: WireContextSnapshot<'a>,
    semantics: WireSemantics<'a>,
}

#[derive(Serialize)]
struct WireTokenCache {
    read: i64,
    write: i64,
}

#[derive(Serialize)]
struct WireTokens {
    input: i64,
    output: i64,
    reasoning: i64,
    cache: WireTokenCache,
}

#[derive(Serialize)]
struct WirePath<'a> {
    cwd: &'a str,
    root: &'a str,
}

#[derive(Serialize)]
struct WireAssistantMessageData<'a> {
    role: &'a str,
    time: WireTime,
    /// The previous message's id, forming the chain; the first message omits
    /// the field.
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    parent_id: Option<&'a str>,
    #[serde(rename = "modelID")]
    model_id: &'a str,
    #[serde(rename = "providerID")]
    provider_id: &'a str,
    mode: &'a str,
    agent: &'a str,
    path: WirePath<'a>,
    cost: i64,
    tokens: WireTokens,
    finish: &'a str,
    semantics: WireSemantics<'a>,
}

#[derive(Serialize)]
struct WireTimeStartEnd {
    start: i64,
    end: i64,
}

#[derive(Serialize)]
struct WireTextPartData<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
    time: WireTimeStartEnd,
}

/// tasks.meta_json: a redundant snapshot for the desktop list display, with
/// fields matching real-device observations. traceId is mandatory (if absent,
/// ZCode's load-time validation fails and the whole row is dropped).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskMeta<'a> {
    task_id: &'a str,
    trace_id: &'a str,
    title: &'a str,
    workspace_path: &'a str,
    created_at: i64,
    updated_at: i64,
    mode: &'a str,
    model: &'a str,
    provider: &'a str,
    status: &'a str,
}

/// user message.data: a real user utterance, with an environment snapshot.
fn serialize_user_message(project_dir: &str, ts_ms: i64) -> String {
    let data = WireUserMessageData {
        role: "user",
        time: WireTime {
            created: ts_ms,
            completed: None,
        },
        agent: AGENT,
        model: WireUserModel {
            provider_id: PROVIDER_ID,
            model_id: MODEL_ID,
            variant: MODEL_VARIANT,
        },
        context_snapshot: WireContextSnapshot {
            env_info: WireEnvInfo {
                cwd: project_dir,
                platform: std::env::consts::OS,
            },
        },
        semantics: WireSemantics {
            origin: "real_user",
            kind: "user_prompt",
            ui_visibility: "visible",
            provider_visibility: "visible",
            transcript_visibility: "visible",
        },
    };
    serde_json::to_string(&data).expect("serialize zcode user message")
}

/// assistant message.data: the model's reply; token/cost counts are
/// unknowable at migration time and set to zero.
fn serialize_assistant_message(project_dir: &str, ts_ms: i64, parent_id: Option<&str>) -> String {
    let data = WireAssistantMessageData {
        role: "assistant",
        time: WireTime {
            created: ts_ms,
            completed: Some(ts_ms),
        },
        parent_id,
        model_id: MODEL_ID,
        provider_id: PROVIDER_ID,
        mode: MODE,
        agent: AGENT,
        path: WirePath {
            cwd: project_dir,
            root: project_dir,
        },
        cost: 0,
        tokens: WireTokens {
            input: 0,
            output: 0,
            reasoning: 0,
            cache: WireTokenCache { read: 0, write: 0 },
        },
        finish: FINISH,
        semantics: WireSemantics {
            origin: "system",
            // The exact enum value was never observed; this one was verified
            // experimentally to render correctly in the UI
            kind: "model_response",
            ui_visibility: "visible",
            provider_visibility: "visible",
            transcript_visibility: "visible",
        },
    };
    serde_json::to_string(&data).expect("serialize zcode assistant message")
}

/// part.data: the fully merged text is persisted as a single text part.
fn text_part_json(text: &str, ts_ms: i64) -> String {
    let data = WireTextPartData {
        kind: "text",
        text,
        time: WireTimeStartEnd {
            start: ts_ms,
            end: ts_ms,
        },
    };
    serde_json::to_string(&data).expect("serialize zcode text part")
}

fn task_meta_json(
    session_id: &str,
    trace_id: &str,
    title: &str,
    project_dir: &str,
    created: i64,
    updated: i64,
) -> String {
    let meta = TaskMeta {
        task_id: session_id,
        trace_id,
        title,
        workspace_path: project_dir,
        created_at: created,
        updated_at: updated,
        mode: MODE,
        model: MODEL_ID,
        provider: TASKS_PROVIDER,
        status: TASK_STATUS,
    };
    serde_json::to_string(&meta).expect("serialize zcode task meta")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{SessionSummary, UnifiedMessage, UnifiedPart};
    use chrono::DateTime;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn ms(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    /// Incrementing uuids: the 1st goes to the session, then one per message
    /// (message first, then part); time is constant.
    struct CountingIdGen {
        counter: AtomicUsize,
    }

    impl CountingIdGen {
        fn new() -> Self {
            Self {
                counter: AtomicUsize::new(0),
            }
        }
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
            uuid(n)
        }
    }

    fn uuid(n: usize) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }

    /// Build an empty cli database carrying an 0018 migration record.
    fn build_cli_db(dir: &tempfile::TempDir) -> PathBuf {
        build_cli_db_with_migration(dir, "0018_baseline")
    }

    /// Build a cli database with the given latest migration id (for testing
    /// unsupported versions).
    fn build_cli_db_with_migration(dir: &tempfile::TempDir, migration: &str) -> PathBuf {
        let path = dir.path().join("db.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(CLI_DDL).unwrap();
        conn.execute(
            "INSERT INTO schema_migration(id, checksum, app_version, time_applied) \
             VALUES(?1, 'deadbeef', '1.4.2', 1)",
            params![migration],
        )
        .unwrap();
        path
    }

    fn build_tasks_db(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("tasks-index.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch(TASKS_DDL)
            .unwrap();
        path
    }

    fn ir_with(project_dir: &str, title: &str, messages: Vec<UnifiedMessage>) -> UnifiedSession {
        UnifiedSession {
            summary: SessionSummary {
                session_id: "src".to_string(),
                source_path: PathBuf::from("/tmp/src.jsonl"),
                title: title.to_string(),
                project_dir: project_dir.to_string(),
                last_active: "2026-09-10T10:00:05.000Z".to_string(),
                message_count: messages.len(),
            },
            messages,
            parse_warnings: 0,
        }
    }

    fn msg(role: Role, parts: Vec<UnifiedPart>, ts: Option<&str>) -> UnifiedMessage {
        UnifiedMessage {
            role,
            parts,
            timestamp: ts.map(str::to_string),
        }
    }

    /// Typical IR across both databases: user text + assistant (reasoning + text).
    fn sample_ir() -> UnifiedSession {
        ir_with(
            "/Users/x/MyProj",
            "迁移会话",
            vec![
                msg(
                    Role::User,
                    vec![UnifiedPart::Text("问题一".to_string())],
                    Some("2026-09-10T10:00:00.000Z"),
                ),
                msg(
                    Role::Assistant,
                    vec![
                        UnifiedPart::Reasoning("想一想".to_string()),
                        UnifiedPart::Text("回答一".to_string()),
                    ],
                    Some("2026-09-10T10:00:03.000Z"),
                ),
            ],
        )
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn backup_files(dir: &Path, db_name: &str) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| {
                n.to_string_lossy()
                    .starts_with(&format!("{db_name}.hub-backup-"))
            })
            .collect();
        names.sort();
        names
    }

    // ---------- TC-ZWRITE-01 ----------
    /// Field-by-field assertions after the dual-database write: the session
    /// row, the message chain parentID, part data JSON, the tasks row,
    /// workspace_key, and meta_json.
    #[test]
    fn tc_zwrite_01_both_dbs_full_shape() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);

        let out =
            write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new()).unwrap();
        let sid = out.session_id;
        assert_eq!(sid, format!("sess_{}", uuid(1)));
        assert_eq!(out.resume_command, "");

        // ---- session row ----
        let conn = Connection::open(&cli_db).unwrap();
        let (
            project_id,
            slug,
            directory,
            title,
            version,
            time_created,
            time_updated,
            time_archived,
            task_type,
            title_source,
        ): (
            String,
            String,
            String,
            String,
            String,
            i64,
            i64,
            Option<i64>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT project_id, slug, directory, title, version, time_created, \
                 time_updated, time_archived, task_type, title_source FROM session WHERE id = ?1",
                params![sid],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(project_id, "proj_users-x-myproj");
        assert_eq!(slug, sid);
        assert_eq!(directory, "/Users/x/MyProj");
        assert_eq!(title, "迁移会话");
        assert_eq!(version, "1.4.2"); // follows the database's migration-record app version
        assert_eq!(time_created, ms("2026-09-11T00:00:00.000Z")); // migration moment (visible at top)
        assert_eq!(time_updated, ms("2026-09-11T00:00:00.000Z")); // migration moment
        assert_eq!(time_archived, None); // not archived, so the desktop app shows it
        assert_eq!(task_type, "interactive");
        assert_eq!(title_source, "generated");

        // ---- message rows: chained parentID, roles, times ----
        let messages: Vec<(String, i64, String)> = conn
            .prepare(
                "SELECT id, sequence, data FROM message WHERE session_id = ?1 \
                 ORDER BY sequence ASC",
            )
            .unwrap()
            .query_map(params![sid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(messages.len(), 2);

        let (m1_id, m1_seq, m1_data) = &messages[0];
        assert_eq!(m1_id, &format!("msg_{}", uuid(3)));
        assert_eq!(*m1_seq, 0);
        let m1: Value = serde_json::from_str(m1_data).unwrap();
        assert!(m1.get("parentID").is_none(), "首条消息无 parentID");
        assert_eq!(m1["role"], "user");
        assert_eq!(m1["agent"], "zcode-agent");
        assert_eq!(m1["time"]["created"], ms("2026-09-10T10:00:00.000Z"));
        assert_eq!(m1["model"]["providerID"], "builtin:bigmodel-coding-plan");
        assert_eq!(m1["model"]["modelID"], "GLM-5.3-Flash");
        assert_eq!(m1["contextSnapshot"]["envInfo"]["cwd"], "/Users/x/MyProj");
        assert_eq!(m1["semantics"]["kind"], "user_prompt");
        assert_eq!(m1["semantics"]["origin"], "real_user");

        let (m2_id, m2_seq, m2_data) = &messages[1];
        assert_eq!(m2_id, &format!("msg_{}", uuid(5)));
        assert_eq!(*m2_seq, 1);
        let m2: Value = serde_json::from_str(m2_data).unwrap();
        assert_eq!(m2["parentID"], format!("msg_{}", uuid(3)));
        assert_eq!(m2["role"], "assistant");
        assert_eq!(m2["modelID"], "GLM-5.3-Flash");
        assert_eq!(m2["mode"], "build");
        assert_eq!(m2["finish"], "completed");
        assert_eq!(m2["tokens"]["input"], 0);
        assert_eq!(m2["semantics"]["kind"], "model_response");

        // ---- part rows: exactly one text part per message, content being the
        // unified merge result ----
        let parts: Vec<(String, String, i64, String)> = conn
            .prepare(
                "SELECT id, message_id, sequence, data FROM part WHERE session_id = ?1 \
                 ORDER BY id ASC",
            )
            .unwrap()
            .query_map(params![sid], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(parts.len(), 2);
        let (p1_id, p1_msg, p1_seq, p1_data) = &parts[0];
        assert_eq!(p1_id, &format!("part_{}", uuid(4)));
        assert_eq!(p1_msg, m1_id);
        assert_eq!(*p1_seq, 0);
        let p1: Value = serde_json::from_str(p1_data).unwrap();
        assert_eq!(p1["type"], "text");
        assert_eq!(p1["text"], "问题一");
        assert_eq!(p1["time"]["start"], ms("2026-09-10T10:00:00.000Z"));

        let (_, p2_msg, p2_seq, p2_data) = &parts[1];
        assert_eq!(p2_msg, m2_id);
        assert_eq!(*p2_seq, 0);
        let p2: Value = serde_json::from_str(p2_data).unwrap();
        // Merging rules match mapper::render_parts: reasoning as a prefix
        assert_eq!(p2["text"], "> 内部推理:想一想\n\n回答一");
        drop(conn);

        // ---- tasks row ----
        let tasks_conn = Connection::open(&tasks_db).unwrap();
        let (ws_key, ws_path, task_id, task_title, status, provider, mode, model): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ) = tasks_conn
            .query_row(
                "SELECT workspace_key, workspace_path, task_id, title, task_status, \
                 provider, mode, model FROM tasks WHERE task_id = ?1",
                params![sid],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(ws_key, "/Users/x/MyProj"); // workspace_key = absolute project path
        assert_eq!(ws_path, "/Users/x/MyProj");
        assert_eq!(task_id, sid); // task_id must equal session.id
        assert_eq!(task_title, "迁移会话");
        assert_eq!(status, "completed");
        assert_eq!(provider, "glm"); // must be one of ZCode's validated enums
        assert_eq!(mode, "build");
        assert_eq!(model, "GLM-5.3-Flash");
        let (created_at, updated_at, last_unread, pinned, archived, deleted, title_overridden): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = tasks_conn
            .query_row(
                "SELECT created_at, updated_at, last_unread_at, pinned, archived, \
                 deleted, title_overridden FROM tasks WHERE task_id = ?1",
                params![sid],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(created_at, ms("2026-09-11T00:00:00.000Z")); // migration moment
        assert_eq!(updated_at, ms("2026-09-11T00:00:00.000Z")); // migration moment
        assert_eq!(
            (last_unread, pinned, archived, deleted, title_overridden),
            (0, 0, 0, 0, 0)
        );
        let (meta_json, searchable): (String, String) = tasks_conn
            .query_row(
                "SELECT meta_json, searchable_text FROM tasks WHERE task_id = ?1",
                params![sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(searchable, "迁移会话");
        let meta: Value = serde_json::from_str(&meta_json).unwrap();
        assert_eq!(meta["taskId"], sid);
        assert_eq!(meta["title"], "迁移会话");
        assert_eq!(meta["workspacePath"], "/Users/x/MyProj");
        assert_eq!(meta["createdAt"], ms("2026-09-11T00:00:00.000Z")); // migration moment
        assert_eq!(meta["updatedAt"], ms("2026-09-11T00:00:00.000Z")); // migration moment
        assert_eq!(meta["mode"], "build");
        assert_eq!(meta["model"], "GLM-5.3-Flash");
        assert_eq!(meta["provider"], "glm");
        // traceId is mandatory for ZCode's load validation (if absent, the
        // whole row is silently dropped)
        assert!(
            meta["traceId"].is_string() && !meta["traceId"].as_str().unwrap().is_empty(),
            "meta_json 必须含非空 traceId"
        );
        assert_eq!(meta["status"], "completed");
    }

    // ---------- TC-ZWRITE-02 ----------
    /// Idempotency: a second write with the same id reports TargetExists,
    /// both databases stay unchanged, and no new backups are created.
    #[test]
    fn tc_zwrite_02_idempotent_target_exists_data_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);

        write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new()).unwrap();
        let err = write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new())
            .unwrap_err();
        assert!(matches!(err, HubError::TargetExists(_)));

        // Data completely unchanged: the cli database still has 1 session /
        // 2 messages / 2 parts; the tasks database still 1 row
        let conn = Connection::open(&cli_db).unwrap();
        assert_eq!(count(&conn, "session"), 1);
        assert_eq!(count(&conn, "message"), 2);
        assert_eq!(count(&conn, "part"), 2);
        drop(conn);
        let tasks_conn = Connection::open(&tasks_db).unwrap();
        assert_eq!(count(&tasks_conn, "tasks"), 1);
        drop(tasks_conn);

        // The idempotent rejection happens before the backup: still only the
        // 1 backup created by the first write
        assert_eq!(backup_files(dir.path(), "db.sqlite").len(), 1);
        assert_eq!(backup_files(dir.path(), "tasks-index.sqlite").len(), 1);
    }

    // ---------- TC-ZWRITE-03 ----------
    /// An unrecognized schema (newer version / no records) is rejected with
    /// no writes and no backups; a missing target database file is rejected
    /// the same way.
    #[test]
    fn tc_zwrite_03_unsupported_schema_and_missing_db_rejected() {
        // A future version 0099: rejected
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db_with_migration(&dir, "0099_future_schema");
        let tasks_db = build_tasks_db(&dir);
        let err = write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new())
            .unwrap_err();
        match err {
            HubError::SchemaUnsupported(id) => assert!(id.contains("0099_future_schema")),
            other => panic!("期望 SchemaUnsupported,得到 {other:?}"),
        }
        let conn = Connection::open(&cli_db).unwrap();
        assert_eq!(count(&conn, "session"), 0, "未知版本绝不写入");
        drop(conn);
        assert!(
            backup_files(dir.path(), "db.sqlite").is_empty(),
            "拒绝时不产生备份"
        );

        // schema_migration table present but no records: rejected as well
        let dir2 = tempfile::tempdir().unwrap();
        let cli_db2 = {
            let path = dir2.path().join("db.sqlite");
            Connection::open(&path)
                .unwrap()
                .execute_batch(CLI_DDL)
                .unwrap();
            path
        };
        let tasks_db2 = build_tasks_db(&dir2);
        let err = write_session_with(&sample_ir(), &cli_db2, &tasks_db2, &CountingIdGen::new())
            .unwrap_err();
        match err {
            HubError::SchemaUnsupported(id) => assert!(id.contains("无迁移记录")),
            other => panic!("期望 SchemaUnsupported,得到 {other:?}"),
        }

        // Missing target database file: rejected, and must not create a new
        // database on the side
        let dir3 = tempfile::tempdir().unwrap();
        let missing_cli = dir3.path().join("db.sqlite");
        let tasks_db3 = build_tasks_db(&dir3);
        assert!(matches!(
            write_session_with(
                &sample_ir(),
                &missing_cli,
                &tasks_db3,
                &CountingIdGen::new()
            ),
            Err(HubError::InvalidInput(_))
        ));
        assert!(!missing_cli.exists(), "绝不初始化新库");

        let dir4 = tempfile::tempdir().unwrap();
        let cli_db4 = build_cli_db(&dir4);
        let missing_tasks = dir4.path().join("tasks-index.sqlite");
        assert!(matches!(
            write_session_with(
                &sample_ir(),
                &cli_db4,
                &missing_tasks,
                &CountingIdGen::new()
            ),
            Err(HubError::InvalidInput(_))
        ));
        assert!(!missing_tasks.exists());
        // The cli database was never touched
        let conn4 = Connection::open(&cli_db4).unwrap();
        assert_eq!(count(&conn4, "session"), 0);
    }

    // ---------- TC-ZWRITE-04 ----------
    /// Backup: both databases are copied to `<original
    /// name>.hub-backup-<timestamp>` before writing; the contents equal the
    /// original file's bytes before the write.
    #[test]
    fn tc_zwrite_04_backup_files_created_from_pre_write_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);
        // Give both databases non-empty contents so the backup comparison is
        // meaningful
        {
            let tasks_conn = Connection::open(&tasks_db).unwrap();
            tasks_conn
                .execute(
                    "INSERT INTO tasks(workspace_key, workspace_path, task_id, title, \
                     created_at, updated_at) VALUES('wk', 'wp', 'pre-existing', '旧任务', 1, 1)",
                    [],
                )
                .unwrap();
        }
        let cli_bytes_before = fs::read(&cli_db).unwrap();
        let tasks_bytes_before = fs::read(&tasks_db).unwrap();

        write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new()).unwrap();

        let cli_backup = dir
            .path()
            .join(format!("db.sqlite.hub-backup-{FIXED_TS_FILE}"));
        let tasks_backup = dir
            .path()
            .join(format!("tasks-index.sqlite.hub-backup-{FIXED_TS_FILE}"));
        assert!(cli_backup.is_file(), "cli 库备份存在");
        assert!(tasks_backup.is_file(), "tasks 库备份存在");
        assert_eq!(fs::read(&cli_backup).unwrap(), cli_bytes_before);
        assert_eq!(fs::read(&tasks_backup).unwrap(), tasks_bytes_before);
    }

    // ---------- TC-ZWRITE-05 ----------
    /// Input defenses: an empty project_dir reports InvalidInput; a session
    /// with all messages empty reports EmptySession. Both happen before any
    /// database file is touched.
    #[test]
    fn tc_zwrite_05_invalid_input_and_empty_session() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);

        let no_dir = ir_with("", "标题", sample_ir().messages.clone());
        assert!(matches!(
            write_session_with(&no_dir, &cli_db, &tasks_db, &CountingIdGen::new()),
            Err(HubError::InvalidInput(_))
        ));

        let empty = ir_with("/tmp/p", "标题", vec![]);
        assert!(matches!(
            write_session_with(&empty, &cli_db, &tasks_db, &CountingIdGen::new()),
            Err(HubError::EmptySession(_))
        ));
        // All messages empty after merging: rejected just the same
        let all_blank = ir_with(
            "/tmp/p",
            "标题",
            vec![
                msg(Role::Assistant, vec![], None),
                msg(Role::User, vec![UnifiedPart::Text(String::new())], None),
            ],
        );
        assert!(matches!(
            write_session_with(&all_blank, &cli_db, &tasks_db, &CountingIdGen::new()),
            Err(HubError::EmptySession(_))
        ));

        // Blank messages are skipped; the remaining messages persist as usual
        // and the parentID chain stays unbroken
        let with_blank = ir_with(
            "/tmp/p",
            "标题",
            vec![
                msg(Role::User, vec![UnifiedPart::Text("hi".to_string())], None),
                msg(Role::Assistant, vec![], None),
                msg(
                    Role::Assistant,
                    vec![UnifiedPart::Text("again".to_string())],
                    None,
                ),
            ],
        );
        write_session_with(&with_blank, &cli_db, &tasks_db, &CountingIdGen::new()).unwrap();
        let conn = Connection::open(&cli_db).unwrap();
        assert_eq!(count(&conn, "session"), 1);
        assert_eq!(count(&conn, "message"), 2);
        assert_eq!(count(&conn, "part"), 2);
        drop(conn);
        let tasks_conn = Connection::open(&tasks_db).unwrap();
        assert_eq!(count(&tasks_conn, "tasks"), 1);
    }

    // ---------- TC-ZWRITE-06 ----------
    /// Transaction rollback: a mid-write failure in the cli database
    /// (simulated with a primary-key conflict) rolls back
    /// session/message/part entirely; the tasks database is never written.
    #[test]
    fn tc_zwrite_06_transaction_rollback_on_midway_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);
        // Pre-plant a row with the same id as the writer's first message
        // (CountingIdGen's 2nd uuid)
        {
            let conn = Connection::open(&cli_db).unwrap();
            conn.execute(
                "INSERT INTO session(id, project_id, slug, directory, title, version, \
                 time_created, time_updated) \
                 VALUES('sess-existing', 'proj_p', 'sess-existing', '/tmp/p', '既有', '1.4.2', 1, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message(id, session_id, time_created, time_updated, data, sequence) \
                 VALUES(?1, 'sess-existing', 1, 1, '{}', 0)",
                params![format!("msg_{}", uuid(3))],
            )
            .unwrap();
        }

        let result = write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new());
        assert!(result.is_err(), "主键冲突必须报错");

        // cli database: the new session rolled back entirely; only the
        // pre-planted data remains
        let conn = Connection::open(&cli_db).unwrap();
        assert_eq!(count(&conn, "session"), 1);
        assert_eq!(count(&conn, "message"), 1);
        assert_eq!(count(&conn, "part"), 0);
        let not_migrated: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session WHERE id = ?1",
                params![format!("sess_{}", uuid(1))],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(not_migrated, 0, "失败的会话行没有写入");
        drop(conn);
        // tasks database: never touched after the cli transaction failed
        let tasks_conn = Connection::open(&tasks_db).unwrap();
        assert_eq!(count(&tasks_conn, "tasks"), 0);
    }

    // ---------- TC-ZWRITE-07 ----------
    /// A missing message timestamp falls back to the IdGen's current time.
    #[test]
    fn tc_zwrite_07_missing_timestamp_falls_back_to_now() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);

        let ir = ir_with(
            "/tmp/p",
            "标题",
            vec![msg(
                Role::User,
                vec![UnifiedPart::Text("q".to_string())],
                None,
            )],
        );
        let out = write_session_with(&ir, &cli_db, &tasks_db, &CountingIdGen::new()).unwrap();
        let now = ms(FIXED_TS_COLON);

        let conn = Connection::open(&cli_db).unwrap();
        let (created, data): (i64, String) = conn
            .query_row(
                "SELECT time_created, data FROM message WHERE session_id = ?1",
                params![out.session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(created, now);
        let value: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(value["time"]["created"], now);
        // The session creation time is the migration moment by design (which
        // is also the fallback time here, as the first message has no timestamp)
        let session_created: i64 = conn
            .query_row(
                "SELECT time_created FROM session WHERE id = ?1",
                params![out.session_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(session_created, now);
    }
}
