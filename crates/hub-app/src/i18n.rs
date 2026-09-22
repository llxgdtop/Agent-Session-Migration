//! UI localization for the app layer (Chinese / English).
//!
//! Every user-facing string of the app goes through [`Strings`]: plain
//! labels are fields, text with interpolation lives in methods that branch
//! on the resolved language. Scope notes:
//!
//! - Errors raised by `hub-core` (`HubError` Display, currently Chinese)
//!   are shown verbatim; translating the library is out of scope here.
//! - Message-stream preview prefixes (`[调用工具 …]` / `[工具结果 …]`)
//!   deliberately stay in the writers' Chinese format: migrating to
//!   Claude/ZCode flattens tool calls into exactly these strings (asserted
//!   by hub-core e2e tests), so the preview must keep matching the artifact
//!   text in every UI language. Only the role names are localized.
//! - Brand and technical names ("Agent Session Hub", "Claude", "Codex",
//!   "ZCode") are language-neutral and stay inline in `main.rs`.

use std::path::{Path, PathBuf};

/// Persisted language preference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    /// Follow the OS locale (`LC_ALL` / `LANG`).
    System,
    Chinese,
    English,
}

impl Language {
    /// Serialized form used in `settings.json` (`system` / `zh` / `en`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Chinese => "zh",
            Self::English => "en",
        }
    }

    /// Parse the serialized form; anything unknown falls back to `System`.
    pub fn from_str(raw: &str) -> Self {
        match raw {
            "zh" => Self::Chinese,
            "en" => Self::English,
            _ => Self::System,
        }
    }

    /// Effective UI language; `System` resolves via the locale env vars.
    pub fn resolved(self) -> ResolvedLang {
        match self {
            Self::Chinese => ResolvedLang::Chinese,
            Self::English => ResolvedLang::English,
            Self::System if system_prefers_chinese() => ResolvedLang::Chinese,
            Self::System => ResolvedLang::English,
        }
    }
}

/// Effective UI language after resolving the `System` preference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolvedLang {
    Chinese,
    English,
}

impl ResolvedLang {
    /// All UI strings for this language.
    pub fn strings(self) -> &'static Strings {
        match self {
            Self::Chinese => &STRINGS_ZH,
            Self::English => &STRINGS_EN,
        }
    }
}

/// Best-effort OS locale detection: `LC_ALL` overrides `LANG`; the first
/// non-empty value wins and any value containing "zh" counts as Chinese.
/// Good enough for a desktop app; falls back to English.
fn system_prefers_chinese() -> bool {
    ["LC_ALL", "LANG"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.is_empty())
        .is_some_and(|value| is_zh_locale(&value))
}

/// Does a locale string (`zh_CN.UTF-8`, `en_US`, …) mean Chinese?
fn is_zh_locale(value: &str) -> bool {
    value.to_ascii_lowercase().contains("zh")
}

/// All user-facing strings for one language.
///
/// Plain labels are `pub` fields; formatted text lives in `pub` methods.
/// Both languages must define every entry (guarded by a unit test).
#[derive(Debug)]
pub struct Strings {
    lang: ResolvedLang,

    // ---- Sidebar ----
    /// Heading above the unified session list.
    pub all_sessions: &'static str,
    /// Button that rescans all three sources.
    pub rescan: &'static str,
    /// Search box hint text.
    pub search_hint: &'static str,
    /// Label in front of the source filter row.
    pub source_label: &'static str,
    /// Source filter entry meaning "no filter".
    pub filter_all: &'static str,
    /// Empty state: no sessions found at all.
    pub empty_title: &'static str,
    /// Empty state: what the app can read.
    pub empty_hint: &'static str,
    /// Empty state: filters matched nothing.
    pub no_match: &'static str,

    // ---- Detail ----
    /// Placeholder when no session is selected.
    pub select_hint: &'static str,
    /// Hover text for a disabled migrate button.
    pub nothing_to_migrate: &'static str,
    /// Confirmation button in the running-tool warning panel.
    pub migrate_anyway: &'static str,
    /// Title of the migration success panel.
    pub migrate_ok_title: &'static str,
    /// Intro line above the resume command.
    pub terminal_cmd_hint: &'static str,
    /// Button that opens the resume command in Terminal.
    pub open_in_terminal: &'static str,
    /// Button that copies the resume command.
    pub copy_command: &'static str,
    /// Button that launches the ZCode app.
    pub open_zcode: &'static str,
    /// Message-stream role name for user turns.
    pub role_user: &'static str,
    /// Message-stream role name for assistant turns.
    pub role_assistant: &'static str,
    /// Extra sentence appended to the run warning for the ZCode SQLite target.
    pub run_warning_zcode_extra: &'static str,

    // ---- Menu bar / settings ----
    /// App-menu entry that opens the settings window.
    pub settings_menu: &'static str,
    /// Settings window title.
    pub settings_title: &'static str,
    /// Section label above the language radio group.
    pub language_section: &'static str,
    /// Radio label for "follow the OS locale".
    pub language_system: &'static str,
    /// Radio label for Chinese (shown as "中文" in both languages).
    pub language_chinese: &'static str,
    /// Radio label for English (shown as "English" in both languages).
    pub language_english: &'static str,
    /// Section label for the app info block.
    pub about_section: &'static str,
    /// Word in front of the version number.
    pub version_label: &'static str,
    /// One-line app description in the settings window.
    pub app_description: &'static str,
    /// Word in front of the settings file path.
    pub settings_file_label: &'static str,
    /// Button that closes the settings window.
    pub close: &'static str,
}

static STRINGS_ZH: Strings = Strings::zh();
static STRINGS_EN: Strings = Strings::en();

impl Strings {
    const fn zh() -> Self {
        Self {
            lang: ResolvedLang::Chinese,
            all_sessions: "全部会话",
            rescan: "重新扫描",
            search_hint: "搜索标题或路径…",
            source_label: "来源:",
            filter_all: "全部",
            empty_title: "未发现任何会话",
            empty_hint: "支持自动读取 Claude Code、Codex、ZCode 的本地会话",
            no_match: "没有符合当前条件的会话",
            select_hint: "在左侧选择一个会话进行预览",
            nothing_to_migrate: "该会话没有可迁移的内容",
            migrate_anyway: "仍要迁移",
            migrate_ok_title: "迁移成功",
            terminal_cmd_hint: "在终端执行以下命令,继续这个会话:",
            open_in_terminal: "在终端中打开",
            copy_command: "复制命令",
            open_zcode: "打开 ZCode",
            role_user: "用户",
            role_assistant: "助手",
            run_warning_zcode_extra: "ZCode 运行中写入有数据损坏风险,强烈建议先退出。",
            settings_menu: "设置…",
            settings_title: "设置",
            language_section: "语言",
            language_system: "系统默认",
            language_chinese: "中文",
            language_english: "English",
            about_section: "应用信息",
            version_label: "版本",
            app_description: "跨 Claude Code / Codex / ZCode 的本地会话迁移工具",
            settings_file_label: "设置文件:",
            close: "关闭",
        }
    }

    const fn en() -> Self {
        Self {
            lang: ResolvedLang::English,
            all_sessions: "All Sessions",
            rescan: "Rescan",
            search_hint: "Search title or path…",
            source_label: "Source:",
            filter_all: "All",
            empty_title: "No sessions found",
            empty_hint: "Automatically reads local sessions from Claude Code, Codex and ZCode",
            no_match: "No sessions match the current filters",
            select_hint: "Select a session on the left to preview it",
            nothing_to_migrate: "This session has nothing to migrate",
            migrate_anyway: "Migrate Anyway",
            migrate_ok_title: "Migration successful",
            terminal_cmd_hint: "Run the following command in a terminal to continue this session:",
            open_in_terminal: "Open in Terminal",
            copy_command: "Copy Command",
            open_zcode: "Open ZCode",
            role_user: "User",
            role_assistant: "Assistant",
            run_warning_zcode_extra: " Writing while ZCode is running risks data \
            corruption; quitting it first is strongly recommended.",
            settings_menu: "Settings…",
            settings_title: "Settings",
            language_section: "Language",
            language_system: "System Default",
            language_chinese: "中文",
            language_english: "English",
            about_section: "About",
            version_label: "Version",
            app_description: "Local session migrator across Claude Code / Codex / ZCode",
            settings_file_label: "Settings file:",
            close: "Close",
        }
    }

    // ---- Sidebar ----

    /// List subtitle without active filters.
    pub fn session_count(&self, count: usize) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("共 {count} 个会话,按最近活跃排序"),
            ResolvedLang::English => format!("{count} sessions, sorted by recent activity"),
        }
    }

    /// List subtitle while filters narrow the list.
    pub fn session_count_filtered(&self, shown: usize, total: usize) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("共 {shown} / {total} 个会话,按最近活跃排序"),
            ResolvedLang::English => {
                format!("{shown} / {total} sessions, sorted by recent activity")
            }
        }
    }

    /// Second line of a session card: last active time and message count.
    pub fn card_meta(&self, time: &str, messages: usize) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("{time} · {messages} 条消息"),
            ResolvedLang::English => format!("{time} · {messages} messages"),
        }
    }

    // ---- Detail ----

    /// Message count in the detail header.
    pub fn message_count(&self, count: usize) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("{count} 条消息"),
            ResolvedLang::English => format!("{count} messages"),
        }
    }

    /// Detail load failure (core error appended verbatim).
    pub fn read_session_failed(&self, err: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("读取会话失败:{err}"),
            ResolvedLang::English => format!("Failed to read session: {err}"),
        }
    }

    /// Parse-warning panel above the migrate buttons.
    pub fn source_parse_warning(&self, lines: usize) -> String {
        match self.lang {
            ResolvedLang::Chinese => {
                format!("源中有 {lines} 行无法解析,迁移时将跳过(不影响其余内容)")
            }
            ResolvedLang::English => format!(
                "{lines} source lines could not be parsed and will be skipped \
                during migration (the rest is unaffected)"
            ),
        }
    }

    /// Migrate button label for the given target name.
    pub fn migrate_to(&self, tool: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("迁移到 {tool}  →"),
            ResolvedLang::English => format!("Migrate to {tool}  →"),
        }
    }

    /// Pre-write guardrail warning; `tool` is the language-neutral short name.
    pub fn run_warning(&self, tool: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!(
                "检测到 {tool} 正在运行,迁移写入可能与它冲突。请先退出 {tool},或点击「仍要迁移」继续。"
            ),
            ResolvedLang::English => format!(
                "{tool} is currently running, so the migration write may conflict \
                with it. Quit {tool} first, or click \"Migrate Anyway\" to continue."
            ),
        }
    }

    /// Success panel note when source lines were skipped.
    pub fn skipped_lines(&self, lines: usize) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("已跳过 {lines} 行无法解析的源内容"),
            ResolvedLang::English => format!("Skipped {lines} unparsable source lines"),
        }
    }

    /// Written artifact path (Codex / Claude targets).
    pub fn output_file(&self, path: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("产物文件:{path}"),
            ResolvedLang::English => format!("Output file: {path}"),
        }
    }

    /// ZCode success guidance referencing the session title.
    pub fn zcode_success_hint(&self, title: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => {
                format!("迁移完成,打开 ZCode 应用即可在任务列表中看到(会话标题:{title})")
            }
            ResolvedLang::English => format!(
                "Migration complete. Open the ZCode app and find it in the task \
                list (session title: {title})"
            ),
        }
    }

    /// "Open in Terminal" failure guidance.
    pub fn open_terminal_failed(&self, err: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => {
                format!("在终端中打开失败:{err}。请复制上方命令到终端手动执行。")
            }
            ResolvedLang::English => format!(
                "Failed to open a terminal: {err}. Copy the command above and \
                run it in your terminal manually."
            ),
        }
    }

    /// "Open ZCode" failure guidance.
    pub fn open_zcode_failed(&self, err: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => {
                format!("打开 ZCode 失败:{err}。请手动启动 ZCode 应用查看迁移结果。")
            }
            ResolvedLang::English => format!(
                "Failed to open ZCode: {err}. Start the ZCode app manually to \
                view the migration result."
            ),
        }
    }

    /// Technical session id line (kept in English in both languages).
    pub fn session_id(&self, id: &str) -> String {
        format!("session id: {id}")
    }

    /// Migration failure (core error appended verbatim).
    pub fn migrate_failed(&self, err: &str) -> String {
        match self.lang {
            ResolvedLang::Chinese => format!("迁移失败:{err}"),
            ResolvedLang::English => format!("Migration failed: {err}"),
        }
    }
}

// ---------- Settings persistence ----------

/// Path of the persisted settings file (`None` when `HOME` is unset).
pub fn settings_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        let home = PathBuf::from(home);
        home.join(".agent-session-hub").join("settings.json")
    })
}

/// Load the language preference; a missing file, unreadable JSON or an
/// unknown field all fall back to `System` (first launch / bad data).
pub fn load_language() -> Language {
    match settings_path() {
        Some(path) => load_language_from(&path),
        None => Language::System,
    }
}

/// Best-effort persist of the language preference; write failures are only
/// logged so the app keeps running with the in-memory language.
pub fn store_language(language: Language) {
    let Some(path) = settings_path() else {
        return;
    };
    if let Err(err) = save_language_to(&path, language) {
        eprintln!("failed to write {}: {err}", path.display());
    }
}

fn load_language_from(path: &Path) -> Language {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Language::System;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("language")
                .and_then(|field| field.as_str())
                .map(Language::from_str)
        })
        .unwrap_or(Language::System)
}

fn save_language_to(path: &Path, language: Language) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Hand-written serde_json usage: the schema is a single field, so no
    // derive machinery is needed.
    let json = serde_json::json!({ "language": language.as_str() });
    std::fs::write(path, json.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialized language values round-trip; junk falls back to System.
    #[test]
    fn language_values_roundtrip() {
        for lang in [Language::System, Language::Chinese, Language::English] {
            assert_eq!(Language::from_str(lang.as_str()), lang);
        }
        assert_eq!(Language::from_str("bogus"), Language::System);
        assert_eq!(Language::from_str(""), Language::System);
    }

    /// Explicit preferences resolve without consulting the environment.
    #[test]
    fn explicit_language_resolves_directly() {
        assert_eq!(Language::Chinese.resolved(), ResolvedLang::Chinese);
        assert_eq!(Language::English.resolved(), ResolvedLang::English);
    }

    /// Locale detection accepts zh regions/encodings, rejects others.
    #[test]
    fn zh_locale_detection() {
        for value in ["zh_CN.UTF-8", "zh_TW", "zh", "ZH-Hans"] {
            assert!(is_zh_locale(value), "{value} should count as Chinese");
        }
        for value in ["en_US.UTF-8", "ja_JP", "C", ""] {
            assert!(!is_zh_locale(value), "{value} should not count as Chinese");
        }
    }

    /// Every label exists (non-empty) in every language; guards against
    /// adding a field to one language but forgetting the other.
    #[test]
    fn all_labels_present_in_both_languages() {
        for lang in [ResolvedLang::Chinese, ResolvedLang::English] {
            let t = lang.strings();
            let labels = [
                t.all_sessions,
                t.rescan,
                t.search_hint,
                t.source_label,
                t.filter_all,
                t.empty_title,
                t.empty_hint,
                t.no_match,
                t.select_hint,
                t.nothing_to_migrate,
                t.migrate_anyway,
                t.migrate_ok_title,
                t.terminal_cmd_hint,
                t.open_in_terminal,
                t.copy_command,
                t.open_zcode,
                t.role_user,
                t.role_assistant,
                t.run_warning_zcode_extra,
                t.settings_menu,
                t.settings_title,
                t.language_section,
                t.language_system,
                t.language_chinese,
                t.language_english,
                t.about_section,
                t.version_label,
                t.app_description,
                t.settings_file_label,
                t.close,
            ];
            assert!(
                labels.iter().all(|label| !label.is_empty()),
                "empty label in {lang:?}"
            );
        }
    }

    /// Formatted entries produce distinct, non-empty text per language.
    #[test]
    fn formatted_strings_differ_per_language() {
        let zh = ResolvedLang::Chinese.strings();
        let en = ResolvedLang::English.strings();
        let pairs = [
            (zh.session_count(3), en.session_count(3)),
            (
                zh.session_count_filtered(1, 3),
                en.session_count_filtered(1, 3),
            ),
            (
                zh.card_meta("09-23 10:00", 7),
                en.card_meta("09-23 10:00", 7),
            ),
            (zh.message_count(7), en.message_count(7)),
            (
                zh.read_session_failed("boom"),
                en.read_session_failed("boom"),
            ),
            (zh.source_parse_warning(2), en.source_parse_warning(2)),
            (zh.migrate_to("Codex"), en.migrate_to("Codex")),
            (zh.run_warning("Codex"), en.run_warning("Codex")),
            (zh.skipped_lines(2), en.skipped_lines(2)),
            (zh.output_file("/a.jsonl"), en.output_file("/a.jsonl")),
            (zh.zcode_success_hint("t"), en.zcode_success_hint("t")),
            (
                zh.open_terminal_failed("boom"),
                en.open_terminal_failed("boom"),
            ),
            (zh.open_zcode_failed("boom"), en.open_zcode_failed("boom")),
            (zh.migrate_failed("boom"), en.migrate_failed("boom")),
        ];
        for (zh_text, en_text) in pairs {
            assert!(!zh_text.is_empty() && !en_text.is_empty());
            assert_ne!(zh_text, en_text);
        }
        // The technical session-id line stays identical in both languages.
        assert_eq!(zh.session_id("abc"), en.session_id("abc"));
        assert!(zh.session_id("abc").contains("abc"));
    }

    /// settings.json save/load round-trip via a temp path, including the
    /// exact single-field file format.
    #[test]
    fn settings_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("hub-app-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        for lang in [Language::System, Language::Chinese, Language::English] {
            save_language_to(&path, lang).expect("write settings");
            assert_eq!(load_language_from(&path), lang);
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text, format!("{{\"language\":\"{}\"}}", lang.as_str()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing file or broken content falls back to System.
    #[test]
    fn settings_load_falls_back_to_system() {
        let dir = std::env::temp_dir().join(format!("hub-app-broken-{}", std::process::id()));
        let path = dir.join("settings.json");
        assert_eq!(load_language_from(&path), Language::System);

        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "not json at all").unwrap();
        assert_eq!(load_language_from(&path), Language::System);

        std::fs::write(&path, r#"{"language":"klingon"}"#).unwrap();
        assert_eq!(load_language_from(&path), Language::System);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
