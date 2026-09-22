//! ZCode 桌面应用会话库(SQLite)写入。
//!
//! 目标形态:双库——`~/.zcode/cli/db/db.sqlite`(session/message/part 三表,
//! 会话正文)与 `~/.zcode/v2/tasks-index.sqlite`(tasks 表,桌面任务列表)。
//!
//! 写入协议(护栏,缺一不可):
//! 1. 校验 cli 库 schema_migration 最新 id 以已验证支持的前缀开头,
//!    不认识则拒绝写入(更高版本可能改表结构);
//! 2. 两个库各自动备份为 `<原路径>.hub-backup-<时间戳>`,备份失败即中止;
//! 3. cli 库单事务写 session+message+part,任一失败整体回滚;
//!    tasks 库单事务写任务行;
//! 4. 幂等:session.id 已存在报 TargetExists,不动任何数据(含不产生备份)。
//!
//! 我们只在既有 ZCode 安装上追加会话,绝不初始化新库;库文件不存在直接拒绝。

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::HubError;
use crate::ir::{Role, UnifiedSession};
use crate::mapper::render_parts;
use crate::writer::codex::{IdGen, SystemIdGen};

/// 固定字段值(与 2026-09 真机观测一致)。
const AGENT: &str = "zcode-agent";
const PROVIDER_ID: &str = "builtin:bigmodel-coding-plan";
const MODEL_ID: &str = "GLM-5.3-Flash";
const MODEL_VARIANT: &str = "low";
const MODE: &str = "build";
const FINISH: &str = "completed";
const TASK_TYPE: &str = "interactive";
const TITLE_SOURCE: &str = "generated";
/// 桌面任务列表的状态值:迁移进来的都是已完结的对话。
const TASK_STATUS: &str = "completed";
/// 已验证支持的 schema_migration 版本前缀;更高版本可能改表结构,直接拒绝。
const SUPPORTED_MIGRATION_PREFIX: &str = "0018";
/// schema_migration 表缺失/无记录时的占位描述(进入 SchemaUnsupported 文案)。
const NO_MIGRATION: &str = "无迁移记录";

#[derive(Debug)]
pub struct ZcodeWriteOutput {
    pub session_id: String,
    /// ZCode 为桌面应用,没有终端续聊命令;此处暂为空串,
    /// 「迁移后打开会话」由应用层后续处理。
    pub resume_command: String,
}

/// 便捷入口:系统时刻与随机 uuid。确定性测试请用 [`write_session_with`]。
pub fn write_session(
    ir: &UnifiedSession,
    cli_db: &Path,
    tasks_db: &Path,
) -> Result<ZcodeWriteOutput, HubError> {
    write_session_with(ir, cli_db, tasks_db, &SystemIdGen)
}

/// 注入 IdGen 的写入入口:会话/消息/部件 id 与兜底时间由 gen 提供。
pub fn write_session_with(
    ir: &UnifiedSession,
    cli_db: &Path,
    tasks_db: &Path,
    gen: &dyn IdGen,
) -> Result<ZcodeWriteOutput, HubError> {
    // ---- 1. 纯内存准备:任何输入问题都在触碰库文件之前报出 ----
    if ir.summary.project_dir.is_empty() {
        return Err(HubError::InvalidInput(
            "源会话缺少项目目录(project_dir),无法确定 ZCode 工作区".to_string(),
        ));
    }
    let project_dir = ir.summary.project_dir.clone();
    let now_ms = now_millis(gen)?;
    let session_id = format!("sess_{}", gen.uuid_v4());

    // 逐消息渲染:每条 IR 消息 → 1 message + 1 text part(内容用统一合并规则);
    // 空 parts 或合并后为空的消息跳过;全部为空报 EmptySession。
    // parentID 指向前一条落库消息,首条不带该字段。
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

    // 会话时间忠实于源:创建取首条消息时间,更新取 IR last_active
    //(无法解析时退化为末条消息时间)。
    let time_created = planned[0].ts_ms;
    let time_updated =
        parse_ms(&ir.summary.last_active).unwrap_or(planned[planned.len() - 1].ts_ms);

    // ---- 2. 目标库必须已存在:绝不初始化/覆盖一个新库 ----
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

    // ---- 3. schema 校验:只写入已验证支持的库版本 ----
    let mut cli_conn = Connection::open(cli_db)?;
    let (migration_id, app_version) = latest_migration(&cli_conn)?;
    if !migration_id.starts_with(SUPPORTED_MIGRATION_PREFIX) {
        return Err(HubError::SchemaUnsupported(migration_id));
    }

    // ---- 4. 幂等:session.id 已存在即拒绝,不动任何数据(也不产生备份)----
    let exists: i64 = cli_conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
        params![session_id],
        |row| row.get(0),
    )?;
    if exists != 0 {
        return Err(HubError::TargetExists(cli_db.to_path_buf()));
    }

    // ---- 5. 自动备份:任何写入动作之前,两个库各复制一份带时间戳的副本 ----
    let backup_stamp = gen.now_rfc3339(); // 文件名友好格式(冒号转连字符)
    backup_db(cli_db, &backup_stamp)?;
    backup_db(tasks_db, &backup_stamp)?;

    // ---- 6. cli 库单事务:session + message + part,任一失败整体回滚 ----
    let project_id = project_id_of(&project_dir);
    let title = ir.summary.title.clone();
    {
        let tx = cli_conn.transaction()?;
        tx.execute(
            INSERT_SESSION_SQL,
            params![
                session_id,
                project_id,
                None::<&str>, // workspace_id:库允许为空
                None::<&str>, // parent_id:顶层会话
                session_id,   // slug 规则:等于 id
                project_dir,
                None::<&str>, // path:可空的遗留字段
                title,
                app_version.unwrap_or_default(), // 版本沿用库内迁移记录的应用版本
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
                None::<i64>, // time_archived:必须为空,否则桌面端不显示
                TASK_TYPE,
                TITLE_SOURCE,
                None::<&str>, // title_message_id
                None::<i64>,  // time_title_updated
                None::<&str>, // trace_id
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
                    0 // 每条消息恰好一个 part
                ],
            )?;
        }
        tx.commit()?;
    }

    // ---- 7. tasks 库单事务:桌面任务列表行(task_id 必须等于 session.id)----
    {
        let mut tasks_conn = Connection::open(tasks_db)?;
        let tx = tasks_conn.transaction()?;
        tx.execute(
            INSERT_TASK_SQL,
            params![
                project_dir,  // workspace_key = 项目绝对路径
                project_dir,  // workspace_path = 项目绝对路径
                None::<&str>, // workspace_identity
                session_id,
                title,
                TASK_STATUS,
                PROVIDER_ID,
                MODE,
                MODEL_ID,
                None::<&str>, // migration_source:ZCode 自身迁移用
                None::<&str>, // forked_from_task_id
                time_created,
                time_updated,
                None::<i64>, // unread_at
                0,           // last_unread_at
                0,           // pinned
                0,           // archived:必须为 0,否则列表不显示
                0,           // deleted
                0,           // title_overridden
                task_meta_json(
                    &session_id,
                    &title,
                    &project_dir,
                    time_created,
                    time_updated
                ),
                title,        // searchable_text:以标题参与搜索
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

/// 写入前组装完成的一条消息(1 message 行 + 1 text part 行)。
/// parentID 已在 data_json 内,无需单独携带。
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

/// 取 schema_migration 最新一条(按 time_applied,同刻按 id 兜底)。
/// 表缺失或无记录时返回占位描述,由调用方按「不受支持」拒绝。
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

/// 把库文件复制为 `<原路径>.hub-backup-<时间戳>`;文件不存在则跳过。
/// 备份失败即中止写入:绝不无备份地改动目标库。
fn backup_db(db: &Path, stamp: &str) -> Result<(), HubError> {
    if !db.exists() {
        return Ok(());
    }
    let target = PathBuf::from(format!("{}.hub-backup-{stamp}", db.display()));
    fs::copy(db, &target)?;
    Ok(())
}

/// project_id 规则(真机验证):去掉前导斜杠,路径内斜线转连字符,转小写,
/// 前缀 "proj_"。例:/Users/x/MyProj → proj_users-x-myproj。
fn project_id_of(directory: &str) -> String {
    format!(
        "proj_{}",
        directory
            .trim_start_matches('/')
            .replace('/', "-")
            .to_lowercase()
    )
}

/// 从 IdGen 的 RFC3339 时刻取 epoch 毫秒。
fn now_millis(gen: &dyn IdGen) -> Result<i64, HubError> {
    DateTime::parse_from_rfc3339(&gen.now_rfc3339_colon())
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
        .map_err(|_| HubError::InvalidInput("时间源返回值不是合法 RFC3339".to_string()))
}

/// RFC3339 字符串 → epoch 毫秒;无法解析返回 None。
fn parse_ms(rfc3339: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
}

// ---- message.data / part.data / meta_json 的线上形态 ----

/// message.data 公共时间字段:user 只有 created,assistant 另带 completed。
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
    /// 迁移发生时的平台;源机器的其余环境信息不可知,不臆造。
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
    /// 前一条消息的 id,构成链;首条消息不带该字段。
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

/// tasks.meta_json:桌面列表展示用的冗余快照,字段与真机观测一致。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskMeta<'a> {
    task_id: &'a str,
    title: &'a str,
    workspace_path: &'a str,
    created_at: i64,
    updated_at: i64,
    mode: &'a str,
    model: &'a str,
    provider: &'a str,
    status: &'a str,
}

/// user message.data:真实用户发言,带环境快照。
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

/// assistant message.data:模型回复;token/cost 计数迁移时不可知,置零。
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
            // 精确枚举值未观测到,此值经实验验证 UI 可正常显示
            kind: "model_response",
            ui_visibility: "visible",
            provider_visibility: "visible",
            transcript_visibility: "visible",
        },
    };
    serde_json::to_string(&data).expect("serialize zcode assistant message")
}

/// part.data:合并后的全文以单一 text part 落库。
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
    title: &str,
    project_dir: &str,
    created: i64,
    updated: i64,
) -> String {
    let meta = TaskMeta {
        task_id: session_id,
        title,
        workspace_path: project_dir,
        created_at: created,
        updated_at: updated,
        mode: MODE,
        model: MODEL_ID,
        provider: PROVIDER_ID,
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

    /// 与真机一致的 cli 库最小表结构(列名精确)。
    const CLI_DDL: &str = "
        CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT NOT NULL, version TEXT NOT NULL, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT, revert TEXT, permission TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_compacting INTEGER, time_archived INTEGER, task_type TEXT NOT NULL DEFAULT 'interactive', title_source TEXT NOT NULL DEFAULT 'generated', title_message_id TEXT, time_title_updated INTEGER, trace_id TEXT);
        CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
        CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL, sequence INTEGER);
        CREATE TABLE schema_migration(id TEXT PRIMARY KEY, checksum TEXT NOT NULL, app_version TEXT, time_applied INTEGER NOT NULL);
    ";
    /// 与真机一致的桌面任务列表库最小表结构。
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

    /// uuid 递增:第 1 个给会话,其后逐消息(先 message 后 part);时间恒定。
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

    /// 建一个带 0018 迁移记录的空 cli 库。
    fn build_cli_db(dir: &tempfile::TempDir) -> PathBuf {
        build_cli_db_with_migration(dir, "0018_baseline")
    }

    /// 建指定最新迁移 id 的 cli 库(测试不受支持的版本用)。
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

    /// 双库典型 IR:user 文本 + assistant(推理+文本)。
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
    /// 双库写入后的逐字段断言:session 行、message 链 parentID、
    /// part data JSON、tasks 行、workspace_key、meta_json。
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

        // ---- session 行 ----
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
        assert_eq!(version, "1.4.2"); // 沿用库内迁移记录的应用版本
        assert_eq!(time_created, ms("2026-09-10T10:00:00.000Z")); // 首条消息时间
        assert_eq!(time_updated, ms("2026-09-10T10:00:05.000Z")); // IR last_active
        assert_eq!(time_archived, None); // 未归档,桌面端才可见
        assert_eq!(task_type, "interactive");
        assert_eq!(title_source, "generated");

        // ---- message 行:链式 parentID、角色、时间 ----
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
        assert_eq!(m1_id, &format!("msg_{}", uuid(2)));
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
        assert_eq!(m2_id, &format!("msg_{}", uuid(4)));
        assert_eq!(*m2_seq, 1);
        let m2: Value = serde_json::from_str(m2_data).unwrap();
        assert_eq!(m2["parentID"], format!("msg_{}", uuid(2)));
        assert_eq!(m2["role"], "assistant");
        assert_eq!(m2["modelID"], "GLM-5.3-Flash");
        assert_eq!(m2["mode"], "build");
        assert_eq!(m2["finish"], "completed");
        assert_eq!(m2["tokens"]["input"], 0);
        assert_eq!(m2["semantics"]["kind"], "model_response");

        // ---- part 行:每条消息恰好一个 text part,内容为统一合并结果 ----
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
        assert_eq!(p1_id, &format!("part_{}", uuid(3)));
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
        // 合并规则与 mapper::render_parts 一致:推理做前缀
        assert_eq!(p2["text"], "> 内部推理:想一想\n\n回答一");
        drop(conn);

        // ---- tasks 行 ----
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
        assert_eq!(ws_key, "/Users/x/MyProj"); // workspace_key = 项目绝对路径
        assert_eq!(ws_path, "/Users/x/MyProj");
        assert_eq!(task_id, sid); // task_id 必须等于 session.id
        assert_eq!(task_title, "迁移会话");
        assert_eq!(status, "completed");
        assert_eq!(provider, "builtin:bigmodel-coding-plan");
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
        assert_eq!(created_at, ms("2026-09-10T10:00:00.000Z"));
        assert_eq!(updated_at, ms("2026-09-10T10:00:05.000Z"));
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
        assert_eq!(meta["createdAt"], ms("2026-09-10T10:00:00.000Z"));
        assert_eq!(meta["updatedAt"], ms("2026-09-10T10:00:05.000Z"));
        assert_eq!(meta["mode"], "build");
        assert_eq!(meta["model"], "GLM-5.3-Flash");
        assert_eq!(meta["provider"], "builtin:bigmodel-coding-plan");
        assert_eq!(meta["status"], "completed");
    }

    // ---------- TC-ZWRITE-02 ----------
    /// 幂等:同 id 二次写入报 TargetExists,两库数据不变,且不产生新备份。
    #[test]
    fn tc_zwrite_02_idempotent_target_exists_data_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);

        write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new()).unwrap();
        let err = write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new())
            .unwrap_err();
        assert!(matches!(err, HubError::TargetExists(_)));

        // 数据完全不变:cli 库仍 1 session / 2 message / 2 part,tasks 库仍 1 行
        let conn = Connection::open(&cli_db).unwrap();
        assert_eq!(count(&conn, "session"), 1);
        assert_eq!(count(&conn, "message"), 2);
        assert_eq!(count(&conn, "part"), 2);
        drop(conn);
        let tasks_conn = Connection::open(&tasks_db).unwrap();
        assert_eq!(count(&tasks_conn, "tasks"), 1);
        drop(tasks_conn);

        // 幂等拒绝发生在备份之前:仍然只有第一次写入产生的 1 份备份
        assert_eq!(backup_files(dir.path(), "db.sqlite").len(), 1);
        assert_eq!(backup_files(dir.path(), "tasks-index.sqlite").len(), 1);
    }

    // ---------- TC-ZWRITE-03 ----------
    /// schema 不认识(更高版本/无记录)拒绝写入,且无任何落库与备份;
    /// 目标库文件不存在同样拒绝。
    #[test]
    fn tc_zwrite_03_unsupported_schema_and_missing_db_rejected() {
        // 未来版本 0099:拒绝
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

        // schema_migration 有表无记录:同样拒绝
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

        // 目标库文件不存在:拒绝,且不得顺手创建新库
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
        // cli 库未被触碰
        let conn4 = Connection::open(&cli_db4).unwrap();
        assert_eq!(count(&conn4, "session"), 0);
    }

    // ---------- TC-ZWRITE-04 ----------
    /// 备份:两个库写入前各复制一份 `<原名>.hub-backup-<时间戳>`,
    /// 内容等于写入前的原文件字节。
    #[test]
    fn tc_zwrite_04_backup_files_created_from_pre_write_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);
        // 让两个库内容非空,备份才有比对意义
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
    /// 输入防线:project_dir 为空报 InvalidInput;
    /// 全部消息为空报 EmptySession。两者都发生在触碰库文件之前。
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
        // 全部消息合并后为空同样拒绝
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

        // 空消息被跳过,其余消息照常落库,parentID 链不断裂
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
    /// 事务回滚:cli 库写入中途失败(主键冲突模拟),session/message/part
    /// 整体回滚,tasks 库也未被写入。
    #[test]
    fn tc_zwrite_06_transaction_rollback_on_midway_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cli_db = build_cli_db(&dir);
        let tasks_db = build_tasks_db(&dir);
        // 预置一条与写入方第一条消息同 id 的行(CountingIdGen 的第 2 个 uuid)
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
                params![format!("msg_{}", uuid(2))],
            )
            .unwrap();
        }

        let result = write_session_with(&sample_ir(), &cli_db, &tasks_db, &CountingIdGen::new());
        assert!(result.is_err(), "主键冲突必须报错");

        // cli 库:新会话整体回滚,只剩预置数据
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
        // tasks 库:cli 事务失败后不再触碰
        let tasks_conn = Connection::open(&tasks_db).unwrap();
        assert_eq!(count(&tasks_conn, "tasks"), 0);
    }

    // ---------- TC-ZWRITE-07 ----------
    /// 消息 timestamp 缺失时用 IdGen 的当前时刻兜底。
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
        // 会话创建时间同样取兜底时刻(首条消息无时间戳)
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
