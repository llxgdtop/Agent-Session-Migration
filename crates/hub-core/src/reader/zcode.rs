//! ZCode 桌面应用会话库(SQLite)读取。
//!
//! 源形态:`~/.zcode/cli/db/db.sqlite`(WAL 模式)。session/message/part 三表
//! 承载会话正文;message 按 sequence 排序,每条消息取其 parts 中第一个
//! text/reasoning/tool part 进入 IR——step-start/step-finish/timeline 等
//! 界面事件形态跳过,没有可用 part 的消息整条跳过。

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;

use crate::error::HubError;
use crate::ir::{Role, SessionSummary, UnifiedMessage, UnifiedPart, UnifiedSession};
use crate::reader::claude::title_of;

/// 扫描库中全部未归档(time_archived 为 NULL)会话,按 time_updated 倒序,
/// 同刻 tie-break 按 session_id 字典序。
///
/// 单个会话读取失败(含空会话)时跳过,不影响整体扫描;
/// 库文件本身不存在时报 SourceNotFound。
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
    // SQL 已给出目标顺序;读取失败的会话直接缺席
    let mut summaries = Vec::new();
    for id in ids {
        if let Ok(session) = read_session_on(&conn, db_path, &id) {
            summaries.push(session.summary);
        }
    }
    Ok(summaries)
}

/// 读取单个会话为 IR。会话 id 不存在或库文件不存在报 SourceNotFound;
/// 没有任何可解读消息时报 EmptySession。
pub fn read_session(db_path: &Path, session_id: &str) -> Result<UnifiedSession, HubError> {
    if !db_path.is_file() {
        return Err(HubError::SourceNotFound(db_path.to_path_buf()));
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    read_session_on(&conn, db_path, session_id)
}

/// 在已有连接上读取单个会话(scan 复用同一连接,避免逐会话重开库)。
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
                // 角色缺失/未知:该行不可解读,计坏行
                parse_warnings += 1;
                continue;
            }
        };
        let part = match first_unified_part(&mut part_stmt, &message_id, &mut parse_warnings)? {
            Some(part) => part,
            None => continue, // 没有任何 text/reasoning/tool part:整条跳过
        };
        // 消息时间优先取 data.time.created,缺失回退行级 time_created
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
        // 标题以库内 session.title 为准;为空时回退到消息推导(与其他 reader 一致)
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

/// 依序扫描消息的 parts,返回第一个可进入 IR 的内容块
/// (text/reasoning/tool);其余形态(step-start/step-finish/timeline 与
/// 未知类型)跳过;JSON 非法计坏行。tool part 只取 state.input,输出不入 IR。
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

/// 单个 part.data JSON → IR 内容块;不可用形态返回 None(调用方继续看下一个)。
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

/// epoch 毫秒 → RFC3339(UTC 毫秒,Z 结尾)。超出表示范围时退化为 Unix 纪元,
/// 保证输出始终是合法可解析时间。
fn ms_to_rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 与真机一致的 cli 库最小表结构(仅 reader/writer 触及的表,列名精确)。
    const CLI_DDL: &str = "
        CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT, revert TEXT, permission TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_compacting INTEGER, time_archived INTEGER, task_type TEXT NOT NULL DEFAULT 'interactive', title_source TEXT NOT NULL DEFAULT 'generated', title_message_id TEXT, time_title_updated INTEGER, trace_id TEXT);
        CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
        CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
        CREATE TABLE schema_migration(id TEXT PRIMARY KEY, checksum TEXT NOT NULL, app_version TEXT, time_applied INTEGER NOT NULL);
    ";

    /// 建库并落一行已验证支持的迁移记录(写入协议要求;reader 本身不校验)。
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

    /// 解析 RFC3339 回 epoch 毫秒(断言时间转换往返用)。
    fn to_ms(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    // ---------- TC-ZREAD-01 ----------
    /// text/reasoning/tool 三种 part 的读取、step/timeline 跳过、
    /// "每条消息只取第一个可用 part"、坏 JSON 计坏行、摘要字段与时间转换。
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
            // assistant:reasoning 在 tool 之前 → 只取 reasoning
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
            // assistant:timeline 之后是 tool → 只取 tool,input 序列化、output 忽略
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
            // 只有 step-start/step-finish 的消息:整条跳过
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
            // 完全没有 part 的消息:跳过
            insert_message(
                &conn,
                "m5",
                "sess-a",
                4,
                1789055538110,
                r#"{"role":"user","time":{"created":1789055538110}}"#,
            );
            // 坏 JSON part 计坏行,后续 text part 正常取用
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

        // 摘要:标题/项目目录取 session 行,last_active 为 time_updated 的 RFC3339
        assert_eq!(session.summary.session_id, "sess-a");
        assert_eq!(session.summary.title, "会话标题");
        assert_eq!(session.summary.project_dir, "/Users/x/demo");
        assert_eq!(to_ms(&session.summary.last_active), 1789055538123);
        assert_eq!(session.summary.message_count, 4);
        assert_eq!(session.summary.source_path, db);
    }

    // ---------- TC-ZREAD-02 ----------
    /// 扫描:按 time_updated 倒序、同刻按 session_id 字典序,归档会话排除。
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
            // 同刻两条,字典序 s-new-a 在 s-new-z 前;归档的一条不出现
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
    /// 库文件不存在 / 会话 id 不存在报 SourceNotFound;
    /// 会话存在但没有任何可解读消息报 EmptySession;
    /// 空标题回退为首条 user 文本推导。
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
            // 只有 step-start part:无可解读消息
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
        // 空会话在扫描中缺席(不报错)
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

        // 库存在但会话 id 不存在
        assert!(matches!(
            read_session(&db, "no-such-session"),
            Err(HubError::SourceNotFound(_))
        ));
    }

    // ---------- TC-ZREAD-04 ----------
    /// 时间转换:0 毫秒 → Unix 纪元的标准 RFC3339 字符串。
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
