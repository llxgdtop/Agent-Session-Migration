//! Agent Session Hub desktop app.
//!
//! Layout: a slim top menu bar (app menu with "Settings…" and a direct
//! language switch), a left sidebar with the unified session list (sessions
//! from Claude Code / Codex / ZCode are scanned automatically, merged and
//! sorted by recent activity, each card carries a source badge, and the
//! source filter combines with the search box), and a right-hand
//! message-stream preview for the selected session. "Migrate to …" buttons
//! dispatch per target: Codex / Claude targets show a terminal resume
//! command that can be copied or opened in Terminal with one click; the
//! ZCode target (a desktop app) shows how to open the app. Before writing,
//! the target tool is checked for running processes and the user must
//! confirm when a conflict is detected.
//!
//! All user-facing text comes from [`crate::i18n`] (zh/en); see that module
//! for what is intentionally NOT translated (core error text, writer-format
//! preview prefixes). Code comments are English per project convention.

mod i18n;

use std::path::PathBuf;

use eframe::egui;
use egui::NumExt as _;
use hub_core::{
    is_tool_running, open_in_terminal, open_zcode_app, read_codex_session,
    read_session as read_claude_session, read_zcode_session, scan_codex_sessions,
    scan_sessions as scan_claude_sessions, scan_zcode_sessions, write_claude_session,
    write_session as write_codex_session, write_zcode_session, Role, SessionSummary, Tool,
    UnifiedPart, UnifiedSession,
};
use i18n::Language;

// ---- Color palette (centralized for easy tuning) ----
const COLOR_USER_ROLE: egui::Color32 = egui::Color32::from_rgb(37, 99, 200);
const COLOR_USER_BUBBLE: egui::Color32 = egui::Color32::from_rgb(238, 244, 253);
const COLOR_ASSISTANT_ROLE: egui::Color32 = egui::Color32::from_rgb(34, 139, 92);
const COLOR_ASSISTANT_BUBBLE: egui::Color32 = egui::Color32::from_rgb(240, 241, 243);
const COLOR_OK_BUBBLE: egui::Color32 = egui::Color32::from_rgb(232, 245, 233);
const COLOR_OK_TEXT: egui::Color32 = egui::Color32::from_rgb(31, 112, 55);
const COLOR_WARN_BUBBLE: egui::Color32 = egui::Color32::from_rgb(253, 246, 227);
const COLOR_WARN_TEXT: egui::Color32 = egui::Color32::from_rgb(146, 106, 12);
const COLOR_ERR_TEXT: egui::Color32 = egui::Color32::from_rgb(190, 45, 45);
/// Background of unselected session cards (must be visually distinct so
/// cards read as separate rows).
const COLOR_CARD_BG: egui::Color32 = egui::Color32::from_rgb(242, 243, 246);
// Source badge palette: warm orange (Claude) / dark green (Codex) /
// indigo (ZCode), all with white text for readability.
const COLOR_BADGE_CLAUDE: egui::Color32 = egui::Color32::from_rgb(194, 94, 58);
const COLOR_BADGE_CODEX: egui::Color32 = egui::Color32::from_rgb(31, 111, 84);
const COLOR_BADGE_ZCODE: egui::Color32 = egui::Color32::from_rgb(59, 91, 219);
const COLOR_BADGE_TEXT: egui::Color32 = egui::Color32::WHITE;
/// Source badge size: fixed width keeps badge right edges aligned.
const BADGE_SIZE: egui::Vec2 = egui::vec2(54.0, 18.0);

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1160.0, 760.0])
            .with_title("Agent Session Hub"),
        ..Default::default()
    };
    eframe::run_native(
        "Agent Session Hub",
        options,
        Box::new(|cc| {
            install_cjk_fonts(&cc.egui_ctx);
            Ok(Box::new(HubApp::new()))
        }),
    )
}

/// List entry: session summary plus source tool (drives badge color, the
/// detail-loading entry point and the migration dispatch).
struct SessionEntry {
    tool: Tool,
    summary: SessionSummary,
}

/// Source filter entry; combines (AND) with the search box.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceFilter {
    /// Show all three sources.
    All,
    /// Show a single source.
    Source(Tool),
}

impl SourceFilter {
    /// Is the given tool visible under this filter?
    fn matches(self, tool: Tool) -> bool {
        match self {
            Self::All => true,
            Self::Source(t) => t == tool,
        }
    }
}

/// Post-migration success state (errors are stored as Display strings).
///
/// Codex / Claude targets provide a terminal resume command and the output
/// file path; the ZCode target is a desktop app, so it gets an open-app
/// guidance text and the session id instead.
struct MigrationOk {
    /// Migration target; decides the shape of the success panel.
    target: Tool,
    /// Session title (referenced by the ZCode guidance text).
    title: String,
    session_id: String,
    /// Terminal resume command; `None` for ZCode.
    resume_command: Option<String>,
    /// Output file path; `None` for ZCode (SQLite dual-DB, no single file).
    file_path: Option<String>,
    parse_warnings: usize,
}

/// Pre-write guardrail: the target tool process is running, so migration is
/// on hold until the user confirms via "Migrate Anyway". Only the target is
/// stored; the warning text is rendered from the currently selected
/// language at draw time (a stored message would go stale on language
/// switch).
struct RunWarning {
    /// Pending migration target (used directly after confirmation, no
    /// re-check).
    target: Tool,
}

struct HubApp {
    claude_root: PathBuf,
    codex_root: PathBuf,
    /// ZCode session body database (shared by reads and writes).
    zcode_cli_db: PathBuf,
    /// ZCode desktop task-list database (migration writes).
    zcode_tasks_db: PathBuf,
    entries: Vec<SessionEntry>,
    search_query: String,
    source_filter: SourceFilter,
    selected: Option<usize>,
    session: Option<UnifiedSession>,
    detail_error: Option<String>,
    migration: Option<Result<MigrationOk, String>>,
    /// Pre-write guardrail: conflict warning while the target tool is
    /// running (when `Some`, a warning panel and a "Migrate Anyway"
    /// button are shown).
    run_warning: Option<RunWarning>,
    /// Error from one-click opens (Terminal / ZCode); shown alongside the
    /// migration success panel.
    launch_error: Option<String>,
    /// UI language preference, persisted to ~/.agent-session-hub/settings.json.
    lang: Language,
    /// Whether the settings popup window is open.
    show_settings: bool,
}

impl HubApp {
    fn new() -> Self {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let zcode_root = home.join(".zcode");
        let mut app = HubApp {
            claude_root: home.join(".claude").join("projects"),
            codex_root: home.join(".codex").join("sessions"),
            zcode_cli_db: zcode_root.join("cli").join("db").join("db.sqlite"),
            zcode_tasks_db: zcode_root.join("v2").join("tasks-index.sqlite"),
            entries: Vec::new(),
            search_query: String::new(),
            source_filter: SourceFilter::All,
            selected: None,
            session: None,
            detail_error: None,
            migration: None,
            run_warning: None,
            launch_error: None,
            lang: i18n::load_language(),
            show_settings: false,
        };
        app.rescan();
        app
    }

    /// Switch the UI language and persist it immediately. egui re-renders
    /// every frame, so the new strings take effect on the next frame.
    fn set_language(&mut self, lang: Language) {
        if self.lang != lang {
            self.lang = lang;
            i18n::store_language(lang);
        }
    }

    /// Rescan all three sources and rebuild the merged list.
    ///
    /// A missing directory/database or a read failure for one source is
    /// silently skipped (common when that tool is not installed); the other
    /// sources stay unaffected. The merged list is sorted by last_active
    /// descending — the stable sort keeps the session_id tie-break within
    /// each source, and cross-source ties follow scan order
    /// (Claude -> Codex -> ZCode).
    fn rescan(&mut self) {
        self.selected = None;
        self.session = None;
        self.detail_error = None;
        self.migration = None;
        self.run_warning = None;
        self.launch_error = None;

        let mut entries = Vec::new();
        if let Ok(summaries) = scan_claude_sessions(&self.claude_root) {
            entries.extend(summaries.into_iter().map(|summary| SessionEntry {
                tool: Tool::ClaudeCode,
                summary,
            }));
        }
        if let Ok(summaries) = scan_codex_sessions(&self.codex_root) {
            entries.extend(summaries.into_iter().map(|summary| SessionEntry {
                tool: Tool::Codex,
                summary,
            }));
        }
        if let Ok(summaries) = scan_zcode_sessions(&self.zcode_cli_db) {
            entries.extend(summaries.into_iter().map(|summary| SessionEntry {
                tool: Tool::ZCode,
                summary,
            }));
        }
        entries.sort_by(|a, b| b.summary.last_active.cmp(&a.summary.last_active));
        self.entries = entries;
    }

    /// Select a session and load its detail; the loading entry point is
    /// dispatched by source. Switching sessions switches the migration
    /// context: the guardrail warning and one-click-open error are cleared.
    fn select(&mut self, index: usize) {
        self.selected = Some(index);
        self.migration = None;
        self.run_warning = None;
        self.launch_error = None;
        let entry = &self.entries[index];
        let result = match entry.tool {
            // Claude / Codex sessions are standalone JSONL files, read by path
            Tool::ClaudeCode => read_claude_session(&entry.summary.source_path),
            Tool::Codex => read_codex_session(&entry.summary.source_path),
            // ZCode session bodies live inside the SQLite database; the
            // source_path is only the database path, read by session id
            Tool::ZCode => read_zcode_session(&self.zcode_cli_db, &entry.summary.session_id),
        };
        match result {
            Ok(session) => {
                self.session = Some(session);
                self.detail_error = None;
            }
            Err(e) => {
                self.session = None;
                self.detail_error = Some(e.to_string());
            }
        }
    }

    /// Migration entry point (pre-write guardrail): when the user clicks a
    /// migrate button, the target tool is first checked for a running
    /// process.
    ///
    /// Running -> do not write yet; record a warning and return, the UI
    /// shows the conflict risk and a "Migrate Anyway" button. Not running
    /// -> clear any old warning and migrate right away. pgrep is only
    /// invoked on button clicks, never polled in the draw loop.
    fn request_migrate(&mut self, target: Tool) {
        if is_tool_running(target) {
            self.run_warning = Some(RunWarning { target });
            return;
        }
        self.run_warning = None;
        self.do_migrate(target);
    }

    /// Run the migration, dispatching to the writer for the target tool.
    /// The writers have their own guardrails (idempotency, atomic writes /
    /// transactions + automatic backup), so the UI adds none. Entering
    /// clears the guardrail warning and the one-click-open error (this
    /// migration produces a completely fresh state).
    fn do_migrate(&mut self, target: Tool) {
        self.run_warning = None;
        self.launch_error = None;
        let Some(session) = &self.session else {
            return;
        };
        let parse_warnings = session.parse_warnings;
        let title = session.summary.title.clone();
        let result = match target {
            Tool::Codex => write_codex_session(session, &self.codex_root).map(|out| MigrationOk {
                session_id: out.session_id,
                resume_command: Some(out.resume_command),
                file_path: Some(out.file_path.display().to_string()),
                title,
                target,
                parse_warnings,
            }),
            Tool::ClaudeCode => {
                write_claude_session(session, &self.claude_root).map(|out| MigrationOk {
                    session_id: out.session_id,
                    resume_command: Some(out.resume_command),
                    file_path: Some(out.file_path.display().to_string()),
                    title,
                    target,
                    parse_warnings,
                })
            }
            Tool::ZCode => write_zcode_session(session, &self.zcode_cli_db, &self.zcode_tasks_db)
                .map(|out| MigrationOk {
                    session_id: out.session_id,
                    // ZCode is a desktop app: no terminal command, the
                    // success panel shows open-app guidance instead
                    resume_command: None,
                    file_path: None,
                    title,
                    target,
                    parse_warnings,
                }),
        };
        self.migration = Some(result.map_err(|e| e.to_string()));
    }
}

impl eframe::App for HubApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Slim top menu bar (~20px): app menu with Settings + language switch.
        egui::Panel::top("menu_bar")
            .frame(
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(8, 1))
                    .fill(ui.visuals().panel_fill),
            )
            .show(ui, |ui| {
                self.show_menu_bar(ui);
            });

        egui::Panel::left("sessions")
            .default_size(400.0)
            .show(ui, |ui| {
                self.show_sidebar(ui);
            });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.show_detail(ui);
        });

        self.show_settings_window(ui.ctx());
    }
}

impl HubApp {
    // ---------- Top: menu bar ----------

    /// macOS-style app menu: one "Agent Session Hub" menu holding
    /// "Settings…" (opens the popup window) and a direct language switch,
    /// so the language is reachable without opening Settings.
    fn show_menu_bar(&mut self, ui: &mut egui::Ui) {
        let t = self.lang.resolved().strings();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("Agent Session Hub", |ui| {
                if ui.button(t.settings_menu).clicked() {
                    self.show_settings = true;
                    ui.close();
                }
                ui.separator();
                // Direct language switch (radio semantics, applied at once).
                let mut lang = self.lang;
                ui.radio_value(&mut lang, Language::System, t.language_system);
                ui.radio_value(&mut lang, Language::Chinese, t.language_chinese);
                ui.radio_value(&mut lang, Language::English, t.language_english);
                if lang != self.lang {
                    self.set_language(lang);
                }
            });
        });
    }

    // ---------- Settings popup ----------

    /// Settings popup (`egui::Window`): language radio trio plus app info.
    /// The window owns its open flag; both the title-bar close button and
    /// the Close button dismiss it.
    fn show_settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let t = self.lang.resolved().strings();
        let mut open = true;
        let mut close_clicked = false;
        egui::Window::new(t.settings_title)
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(t.language_section).strong());
                let mut lang = self.lang;
                ui.radio_value(&mut lang, Language::System, t.language_system);
                ui.radio_value(&mut lang, Language::Chinese, t.language_chinese);
                ui.radio_value(&mut lang, Language::English, t.language_english);
                if lang != self.lang {
                    self.set_language(lang);
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(egui::RichText::new(t.about_section).strong());
                ui.label(format!(
                    "Agent Session Hub · {} {}",
                    t.version_label,
                    env!("CARGO_PKG_VERSION")
                ));
                ui.label(egui::RichText::new(t.app_description).small().weak());
                if let Some(path) = i18n::settings_path() {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {}",
                            t.settings_file_label,
                            path.display()
                        ))
                        .small()
                        .weak()
                        .monospace(),
                    );
                }
                ui.add_space(6.0);
                if ui.button(t.close).clicked() {
                    close_clicked = true;
                }
            });
        self.show_settings = open && !close_clicked;
    }

    // ---------- Left: unified session list ----------

    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        let t = self.lang.resolved().strings();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t.all_sessions).heading());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(t.rescan).clicked() {
                    self.rescan();
                }
            });
        });
        // Search box: instant filtering by title or project path
        ui.add(
            egui::TextEdit::singleline(&mut self.search_query)
                .hint_text(t.search_hint)
                .desired_width(ui.available_width()),
        );
        // Source filter: four exclusive entries, combined (AND) with search
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t.source_label).small().weak());
            ui.selectable_value(&mut self.source_filter, SourceFilter::All, t.filter_all);
            ui.selectable_value(
                &mut self.source_filter,
                SourceFilter::Source(Tool::ClaudeCode),
                "Claude",
            );
            ui.selectable_value(
                &mut self.source_filter,
                SourceFilter::Source(Tool::Codex),
                "Codex",
            );
            ui.selectable_value(
                &mut self.source_filter,
                SourceFilter::Source(Tool::ZCode),
                "ZCode",
            );
        });
        // Case-insensitive substring match, empty input shows everything;
        // source filter and search are both active at once
        let query = self.search_query.trim().to_lowercase();
        let visible: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                self.source_filter.matches(entry.tool)
                    && (query.is_empty()
                        || entry.summary.title.to_lowercase().contains(&query)
                        || entry.summary.project_dir.to_lowercase().contains(&query))
            })
            .map(|(i, _)| i)
            .collect();
        // If the selected session is filtered out, drop the selection and
        // return the right pane to the unselected state
        if let Some(sel) = self.selected {
            if !visible.contains(&sel) {
                self.selected = None;
                self.session = None;
                self.detail_error = None;
                self.migration = None;
                self.run_warning = None;
                self.launch_error = None;
            }
        }
        let filtering = self.source_filter != SourceFilter::All || !query.is_empty();
        let subtitle = if filtering {
            t.session_count_filtered(visible.len(), self.entries.len())
        } else {
            t.session_count(self.entries.len())
        };
        ui.label(egui::RichText::new(subtitle).small().weak());
        ui.add_space(4.0);

        if self.entries.is_empty() {
            // No sessions from any source (a missing directory/database for
            // one source is normal and was skipped silently)
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(t.empty_title).weak().size(15.0));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(t.empty_hint).small().weak());
            });
            return;
        }
        if visible.is_empty() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(t.no_match).weak());
            });
            return;
        }

        let mut clicked: Option<usize> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for &i in &visible {
                    let entry = &self.entries[i];
                    let summary = &entry.summary;
                    let selected = self.selected == Some(i);
                    let bg = if selected {
                        ui.visuals().selection.bg_fill
                    } else {
                        COLOR_CARD_BG
                    };
                    let frame = egui::Frame::new()
                        .fill(bg)
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .outer_margin(egui::Margin {
                            bottom: 6,
                            ..Default::default()
                        });
                    let output = frame.show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // First row: title takes all width except the badge
                        // (truncated when long), source badge top-right
                        let spacing = ui.style().spacing.item_spacing.x;
                        let title_width =
                            (ui.available_width() - BADGE_SIZE.x - spacing).at_least(60.0);
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [title_width, ui.text_style_height(&egui::TextStyle::Body)],
                                egui::Label::new(egui::RichText::new(&summary.title).strong())
                                    .truncate(),
                            );
                            source_badge(ui, entry.tool);
                        });
                        ui.add_space(2.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(t.card_meta(
                                    &friendly_time(&summary.last_active),
                                    summary.message_count,
                                ))
                                .small()
                                .weak(),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&summary.project_dir)
                                    .small()
                                    .weak()
                                    .monospace(),
                            )
                            .truncate(),
                        );
                    });
                    let resp = ui.interact(
                        output.response.rect,
                        egui::Id::new(("session-item", i)),
                        egui::Sense::click(),
                    );
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        if let Some(i) = clicked {
            self.select(i);
        }
    }

    // ---------- Right: detail ----------

    fn show_detail(&mut self, ui: &mut egui::Ui) {
        let t = self.lang.resolved().strings();
        let Some(index) = self.selected else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new(t.select_hint).weak().size(16.0));
            });
            return;
        };
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let summary = &entry.summary;
        let source = entry.tool;

        // Header: source badge + title
        ui.horizontal(|ui| {
            source_badge(ui, source);
            ui.label(egui::RichText::new(&summary.title).heading().strong());
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(friendly_time(&summary.last_active))
                    .small()
                    .weak(),
            );
            ui.separator();
            ui.label(
                egui::RichText::new(t.message_count(summary.message_count))
                    .small()
                    .weak(),
            );
        });
        ui.label(
            egui::RichText::new(&summary.project_dir)
                .small()
                .weak()
                .monospace(),
        );
        ui.add_space(8.0);

        if let Some(err) = &self.detail_error {
            // Core error text is displayed verbatim (see i18n module docs).
            ui.colored_label(COLOR_ERR_TEXT, t.read_session_failed(err));
            return;
        }

        // Take the needed data early and end the immutable borrow (the
        // button area below needs &mut self)
        let (can_migrate, parse_warnings) = match &self.session {
            Some(session) => (
                session.messages.iter().any(|m| !m.parts.is_empty()),
                session.parse_warnings,
            ),
            None => return,
        };

        if parse_warnings > 0 {
            notice_panel(ui, COLOR_WARN_BUBBLE, COLOR_WARN_TEXT, |ui| {
                ui.label(
                    egui::RichText::new(t.source_parse_warning(parse_warnings))
                        .small()
                        .color(COLOR_WARN_TEXT),
                );
            });
            ui.add_space(4.0);
        }

        // Migration action row: one button per target, excluding the current
        // source (same-source migration is pointless, no button shown)
        ui.horizontal(|ui| {
            for target in [Tool::Codex, Tool::ClaudeCode, Tool::ZCode] {
                if target == source {
                    continue;
                }
                let button = egui::Button::new(
                    egui::RichText::new(t.migrate_to(tool_display_name(target))).strong(),
                )
                .min_size(egui::vec2(150.0, 30.0));
                let response = ui.add_enabled(can_migrate, button);
                let response = if can_migrate {
                    response
                } else {
                    response.on_disabled_hover_text(t.nothing_to_migrate)
                };
                if response.clicked() {
                    self.request_migrate(target);
                }
            }
        });
        ui.add_space(4.0);
        self.show_run_warning(ui);
        self.show_migration_result(ui);

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        // Message-stream preview (borrowed again here, no &mut self conflict
        // with the button area above)
        let Some(session) = &self.session else {
            return;
        };
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for message in &session.messages {
                    let (role_name, role_color, bubble) = match message.role {
                        Role::User => (t.role_user, COLOR_USER_ROLE, COLOR_USER_BUBBLE),
                        Role::Assistant => (
                            t.role_assistant,
                            COLOR_ASSISTANT_ROLE,
                            COLOR_ASSISTANT_BUBBLE,
                        ),
                    };
                    egui::Frame::new()
                        .fill(bubble)
                        .corner_radius(8)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .outer_margin(egui::Margin {
                            bottom: 6,
                            ..Default::default()
                        })
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                egui::RichText::new(role_name)
                                    .small()
                                    .strong()
                                    .color(role_color),
                            );
                            for part in &message.parts {
                                match part {
                                    UnifiedPart::Text(text) => {
                                        ui.label(text);
                                    }
                                    UnifiedPart::Reasoning(text) => {
                                        ui.label(egui::RichText::new(text).weak().italics());
                                    }
                                    // The tool-call/result preview prefixes reuse
                                    // the writers' Chinese text format
                                    // byte-for-byte: migrating to Claude/ZCode
                                    // flattens them into exactly these strings
                                    // (asserted by hub-core e2e tests), so the
                                    // preview must keep matching the artifact
                                    // in every UI language. Only role names are
                                    // localized.
                                    UnifiedPart::ToolCall { tool, input_json } => {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "[调用工具 {tool}] {input_json}"
                                            ))
                                            .monospace()
                                            .small(),
                                        );
                                    }
                                    UnifiedPart::ToolResult {
                                        tool,
                                        content,
                                        is_error,
                                    } => {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "[工具结果 {tool} isError={is_error}] {content}"
                                            ))
                                            .monospace()
                                            .small(),
                                        );
                                    }
                                }
                            }
                        });
                }
            });
    }

    /// Pre-write guardrail warning panel: while the target tool is running,
    /// show the conflict risk; "Migrate Anyway" lets the user push this one
    /// migration through (no re-check; the warning clears immediately).
    fn show_run_warning(&mut self, ui: &mut egui::Ui) {
        let Some(warning) = &self.run_warning else {
            return;
        };
        let t = self.lang.resolved().strings();
        // Compose the message from the current language instead of storing
        // it, so it never goes stale after a language switch.
        let mut message = t.run_warning(tool_display_name(warning.target));
        if warning.target == Tool::ZCode {
            message.push_str(t.run_warning_zcode_extra);
        }
        // Land the confirmation in a local first, mutate state only after
        // the panel is drawn (avoids borrow conflicts)
        let mut confirmed: Option<Tool> = None;
        notice_panel(ui, COLOR_WARN_BUBBLE, COLOR_WARN_TEXT, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(&message).small().color(COLOR_WARN_TEXT));
            ui.add_space(2.0);
            let button = egui::Button::new(egui::RichText::new(t.migrate_anyway).strong())
                .min_size(egui::vec2(96.0, 24.0));
            if ui.add(button).clicked() {
                confirmed = Some(warning.target);
            }
        });
        if let Some(target) = confirmed {
            // The user accepted the risk: clear the warning and run this
            // migration immediately
            self.run_warning = None;
            self.do_migrate(target);
        }
    }

    fn show_migration_result(&mut self, ui: &mut egui::Ui) {
        let Some(result) = &self.migration else {
            return;
        };
        let t = self.lang.resolved().strings();
        match result {
            Ok(ok) => {
                // One-click-open error: clone the previous value before the
                // closure, land button results in locals first, then write
                // everything back after the panel (success clears it,
                // failure records the reason and shows it this same frame)
                let mut launch_error = self.launch_error.clone();
                notice_panel(ui, COLOR_OK_BUBBLE, COLOR_OK_TEXT, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new(t.migrate_ok_title)
                            .strong()
                            .color(COLOR_OK_TEXT),
                    );
                    if ok.parse_warnings > 0 {
                        ui.label(
                            egui::RichText::new(t.skipped_lines(ok.parse_warnings))
                                .small()
                                .color(COLOR_OK_TEXT),
                        );
                    }
                    if ok.target != Tool::ZCode {
                        // Codex / Claude targets: run the command in a
                        // terminal to continue the session
                        if let Some(command) = ok.resume_command.as_deref() {
                            ui.label(
                                egui::RichText::new(t.terminal_cmd_hint)
                                    .small()
                                    .color(COLOR_OK_TEXT),
                            );
                            ui.add_space(2.0);
                            egui::Frame::new()
                                .fill(ui.visuals().extreme_bg_color)
                                .corner_radius(4)
                                .inner_margin(egui::Margin::symmetric(8, 5))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(command).monospace())
                                            .truncate(),
                                    );
                                });
                            // Primary button: open in Terminal with one
                            // click; secondary: copy the command as fallback
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                let open_button = egui::Button::new(
                                    egui::RichText::new(t.open_in_terminal).strong(),
                                )
                                .min_size(egui::vec2(132.0, 26.0));
                                if ui.add(open_button).clicked() {
                                    match open_in_terminal(command) {
                                        Ok(()) => launch_error = None,
                                        Err(e) => launch_error = Some(e.to_string()),
                                    }
                                }
                                if ui.button(t.copy_command).clicked() {
                                    ui.ctx().copy_text(command.to_string());
                                }
                            });
                            if let Some(err) = &launch_error {
                                ui.colored_label(COLOR_ERR_TEXT, t.open_terminal_failed(err));
                            }
                        }
                        if let Some(path) = ok.file_path.as_deref() {
                            ui.label(
                                egui::RichText::new(t.output_file(path))
                                    .small()
                                    .weak()
                                    .monospace(),
                            );
                        }
                    } else {
                        // ZCode target: desktop app without a terminal
                        // command, show open-app guidance plus one-click launch
                        ui.label(
                            egui::RichText::new(t.zcode_success_hint(&ok.title))
                                .small()
                                .color(COLOR_OK_TEXT),
                        );
                        ui.add_space(4.0);
                        let open_button =
                            egui::Button::new(egui::RichText::new(t.open_zcode).strong())
                                .min_size(egui::vec2(132.0, 26.0));
                        if ui.add(open_button).clicked() {
                            match open_zcode_app() {
                                Ok(()) => launch_error = None,
                                Err(e) => launch_error = Some(e.to_string()),
                            }
                        }
                        if let Some(err) = &launch_error {
                            ui.colored_label(COLOR_ERR_TEXT, t.open_zcode_failed(err));
                        }
                        ui.label(
                            egui::RichText::new(t.session_id(&ok.session_id))
                                .small()
                                .weak()
                                .monospace(),
                        );
                    }
                });
                self.launch_error = launch_error;
            }
            Err(err) => {
                // Core error text is displayed verbatim (see i18n docs).
                ui.colored_label(COLOR_ERR_TEXT, t.migrate_failed(err));
            }
        }
    }
}

// ---------- Small widgets ----------

/// Notice panel (success / warning and similar).
fn notice_panel(
    ui: &mut egui::Ui,
    bg: egui::Color32,
    _fg: egui::Color32,
    content: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, content);
}

/// Source badge on cards / detail header: a fixed-size colored chip with
/// centered white text. Drawn with the painter directly so the width is
/// known, letting the title truncate against "remaining width".
fn source_badge(ui: &mut egui::Ui, tool: Tool) {
    let (text, color) = match tool {
        Tool::ClaudeCode => ("Claude", COLOR_BADGE_CLAUDE),
        Tool::Codex => ("Codex", COLOR_BADGE_CODEX),
        Tool::ZCode => ("ZCode", COLOR_BADGE_ZCODE),
    };
    let (rect, _) = ui.allocate_exact_size(BADGE_SIZE, egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(4), color);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::TextStyle::Small.resolve(ui.style()),
        COLOR_BADGE_TEXT,
    );
}

/// Short tool name for UI text (warnings etc.), matching the badge text.
/// Language-neutral product name, not translated.
fn tool_display_name(tool: Tool) -> &'static str {
    match tool {
        Tool::ClaudeCode => "Claude",
        Tool::Codex => "Codex",
        Tool::ZCode => "ZCode",
    }
}

/// Render an RFC3339 timestamp as "MM-DD HH:MM" (raw text if parsing fails).
fn friendly_time(rfc3339: &str) -> String {
    if rfc3339.len() >= 16 {
        rfc3339[5..16].replace('T', " ")
    } else {
        rfc3339.to_string()
    }
}

// ---------- Fonts ----------

/// Load a CJK font so Chinese and Latin text share one baseline.
///
/// Key point: the CJK font is inserted at the **front** of the
/// Proportional family, so Latin glyphs are rendered by it as well —
/// metrics within a single font align naturally. If Latin text went
/// through the built-in font while Chinese fell back to another family,
/// the differing line heights of the two fonts would make mixed text
/// wobble. The Monospace family keeps the built-in monospace font first
/// and appends the CJK font at the end (monospace content stays aligned,
/// Chinese still has a fallback).
///
/// Candidates: known system fonts with Simplified Chinese (in priority
/// order); if all are missing, the user font directories are scanned.
/// User directories are NOT scanned first because they commonly hold pure
/// Latin monospace fonts (e.g. JetBrains Mono); matching one of those
/// first would render Chinese as boxes.
fn install_cjk_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    #[cfg(target_os = "macos")]
    let known_cjk_fonts = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
    ];
    #[cfg(not(target_os = "macos"))]
    let known_cjk_fonts: [&str; 0] = [];

    let mut candidates: Vec<PathBuf> = known_cjk_fonts.iter().map(PathBuf::from).collect();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        for dir in [home.join("Library/Fonts"), home.join(".fonts")] {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                candidates.extend(entries.flatten().map(|e| e.path()).filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| matches!(e, "ttf" | "ttc" | "otf"))
                        .unwrap_or(false)
                }));
            }
        }
    }

    for path in candidates {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let name = format!(
            "hub-cjk-{}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(bytes).into());
        // Proportional: insert at the front, one font for CJK+Latin -> one
        // baseline
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, name.clone());
        // Monospace: monospace font stays first, CJK appended as fallback
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(name.clone());
        break;
    }
    ctx.set_fonts(fonts);
}
