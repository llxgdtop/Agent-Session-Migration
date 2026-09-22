//! Agent Session Hub 桌面应用。
//!
//! 界面:左侧为统一会话列表(自动扫描 Claude Code / Codex / ZCode 三家来源,
//! 合并后按最近活跃排序,卡片右上角带来源徽章,来源筛选与搜索叠加过滤),
//! 右侧为所选会话的消息流预览;「迁移到 …」按钮组按目标分发转换——
//! Codex / Claude 目标给出可直接复制到终端的续聊命令,
//! ZCode 目标(桌面应用)给出打开应用查看任务的指引。

use std::path::PathBuf;

use eframe::egui;
use egui::NumExt as _;
use hub_core::{
    read_codex_session, read_session as read_claude_session, read_zcode_session,
    scan_codex_sessions, scan_sessions as scan_claude_sessions, scan_zcode_sessions,
    write_claude_session, write_session as write_codex_session, write_zcode_session, Role,
    SessionSummary, Tool, UnifiedPart, UnifiedSession,
};

// ---- 配色(集中定义,便于统一调整)----
const COLOR_USER_ROLE: egui::Color32 = egui::Color32::from_rgb(37, 99, 200);
const COLOR_USER_BUBBLE: egui::Color32 = egui::Color32::from_rgb(238, 244, 253);
const COLOR_ASSISTANT_ROLE: egui::Color32 = egui::Color32::from_rgb(34, 139, 92);
const COLOR_ASSISTANT_BUBBLE: egui::Color32 = egui::Color32::from_rgb(240, 241, 243);
const COLOR_OK_BUBBLE: egui::Color32 = egui::Color32::from_rgb(232, 245, 233);
const COLOR_OK_TEXT: egui::Color32 = egui::Color32::from_rgb(31, 112, 55);
const COLOR_WARN_BUBBLE: egui::Color32 = egui::Color32::from_rgb(253, 246, 227);
const COLOR_WARN_TEXT: egui::Color32 = egui::Color32::from_rgb(146, 106, 12);
const COLOR_ERR_TEXT: egui::Color32 = egui::Color32::from_rgb(190, 45, 45);
/// 未选中会话卡片的底色(需要肉眼可辨,拉开条目间距感)
const COLOR_CARD_BG: egui::Color32 = egui::Color32::from_rgb(242, 243, 246);
// 来源徽章配色:暖橙(Claude)/ 墨绿(Codex)/ 靛蓝(ZCode),统一白字保证可读。
const COLOR_BADGE_CLAUDE: egui::Color32 = egui::Color32::from_rgb(194, 94, 58);
const COLOR_BADGE_CODEX: egui::Color32 = egui::Color32::from_rgb(31, 111, 84);
const COLOR_BADGE_ZCODE: egui::Color32 = egui::Color32::from_rgb(59, 91, 219);
const COLOR_BADGE_TEXT: egui::Color32 = egui::Color32::WHITE;
/// 来源徽章尺寸:固定宽度让各卡片的徽章右缘对齐。
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

/// 列表项:会话摘要 + 来源工具(决定徽章配色、详情读取入口与迁移目标分发)。
struct SessionEntry {
    tool: Tool,
    summary: SessionSummary,
}

/// 顶部来源筛选项,与搜索叠加(AND)过滤列表。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceFilter {
    /// 三家全部显示。
    All,
    /// 只看某一家。
    Source(Tool),
}

impl SourceFilter {
    /// 该来源在当前筛选下是否可见。
    fn matches(self, tool: Tool) -> bool {
        match self {
            Self::All => true,
            Self::Source(t) => t == tool,
        }
    }
}

/// 迁移成功后的展示状态(错误以 Display 字符串保存)。
///
/// Codex / Claude 目标给出终端续聊命令与产物文件路径;
/// ZCode 目标为桌面应用,没有终端命令,改为打开指引文案与 session_id。
struct MigrationOk {
    /// 迁移目标工具,决定成功面板的展示形态。
    target: Tool,
    /// 会话标题(ZCode 提示文案引用)。
    title: String,
    session_id: String,
    /// 终端续聊命令;ZCode 无命令时为 None。
    resume_command: Option<String>,
    /// 产物文件路径;ZCode 写入 SQLite 双库无单文件产物,为 None。
    file_path: Option<String>,
    parse_warnings: usize,
}

struct HubApp {
    claude_root: PathBuf,
    codex_root: PathBuf,
    /// ZCode 会话正文库(读取与写入共用)。
    zcode_cli_db: PathBuf,
    /// ZCode 桌面任务列表库(迁移写入用)。
    zcode_tasks_db: PathBuf,
    entries: Vec<SessionEntry>,
    search_query: String,
    source_filter: SourceFilter,
    selected: Option<usize>,
    session: Option<UnifiedSession>,
    detail_error: Option<String>,
    migration: Option<Result<MigrationOk, String>>,
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
        };
        app.rescan();
        app
    }

    /// 重新扫描三家来源并合并列表。
    ///
    /// 某一家目录/库不存在或读取失败时静默跳过该家(常见于未安装该工具),
    /// 不影响其余家;合并后统一按 last_active 倒序——
    /// 稳定排序保留各家内部的 session_id tie-break,
    /// 跨家同刻按扫描顺序(Claude → Codex → ZCode)。
    fn rescan(&mut self) {
        self.selected = None;
        self.session = None;
        self.detail_error = None;
        self.migration = None;

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

    /// 选中会话并读取详情;读取入口按来源分发。
    fn select(&mut self, index: usize) {
        self.selected = Some(index);
        self.migration = None;
        let entry = &self.entries[index];
        let result = match entry.tool {
            // Claude / Codex 会话为独立 JSONL 文件,直接按路径读取
            Tool::ClaudeCode => read_claude_session(&entry.summary.source_path),
            Tool::Codex => read_codex_session(&entry.summary.source_path),
            // ZCode 会话正文在 SQLite 库内,source_path 只是库路径,需按 id 读取
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

    /// 执行迁移:按目标工具分发到对应写入器。
    /// 写入函数自带护栏(幂等、原子落盘 / 事务 + 自动备份),UI 无需额外防护。
    fn do_migrate(&mut self, target: Tool) {
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
                    // ZCode 为桌面应用:无终端命令,成功面板改为打开指引
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
        egui::Panel::left("sessions")
            .default_size(400.0)
            .show(ui, |ui| {
                self.show_sidebar(ui);
            });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.show_detail(ui);
        });
    }
}

impl HubApp {
    // ---------- 左侧:统一会话列表 ----------

    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("全部会话").heading());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("重新扫描").clicked() {
                    self.rescan();
                }
            });
        });
        // 搜索框:按标题或项目路径即时过滤
        ui.add(
            egui::TextEdit::singleline(&mut self.search_query)
                .hint_text("搜索标题或路径…")
                .desired_width(ui.available_width()),
        );
        // 来源筛选:一行四项互斥,与搜索叠加(AND)过滤
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("来源:").small().weak());
            ui.selectable_value(&mut self.source_filter, SourceFilter::All, "全部");
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
        // 不区分大小写的子串匹配,空输入显示全部;来源筛选与搜索同时生效
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
        // 选中项被过滤掉时清空选中,右侧回到未选择状态
        if let Some(sel) = self.selected {
            if !visible.contains(&sel) {
                self.selected = None;
                self.session = None;
                self.detail_error = None;
                self.migration = None;
            }
        }
        let filtering = self.source_filter != SourceFilter::All || !query.is_empty();
        let subtitle = if filtering {
            format!(
                "共 {} / {} 个会话,按最近活跃排序",
                visible.len(),
                self.entries.len()
            )
        } else {
            format!("共 {} 个会话,按最近活跃排序", self.entries.len())
        };
        ui.label(egui::RichText::new(subtitle).small().weak());
        ui.add_space(4.0);

        if self.entries.is_empty() {
            // 三家都未发现会话(某家目录/库不存在属正常情况,已静默跳过)
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("未发现任何会话").weak().size(15.0));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("支持自动读取 Claude Code、Codex、ZCode 的本地会话")
                        .small()
                        .weak(),
                );
            });
            return;
        }
        if visible.is_empty() {
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("没有符合当前条件的会话").weak());
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
                        // 首行:标题占满除徽章外的宽度(超长截断),右上角为来源徽章
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
                                egui::RichText::new(format!(
                                    "{} · {} 条消息",
                                    friendly_time(&summary.last_active),
                                    summary.message_count
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

    // ---------- 右侧:详情 ----------

    fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.selected else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("在左侧选择一个会话进行预览")
                        .weak()
                        .size(16.0),
                );
            });
            return;
        };
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let summary = &entry.summary;
        let source = entry.tool;

        // 头部信息:来源徽章 + 标题
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
                egui::RichText::new(format!("{} 条消息", summary.message_count))
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
            ui.colored_label(COLOR_ERR_TEXT, format!("读取会话失败:{err}"));
            return;
        }

        // 先取需要的数据,尽早结束不可变借用(后面按钮区需要 &mut self)
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
                    egui::RichText::new(format!(
                        "源中有 {} 行无法解析,迁移时将跳过(不影响其余内容)",
                        parse_warnings
                    ))
                    .small()
                    .color(COLOR_WARN_TEXT),
                );
            });
            ui.add_space(4.0);
        }

        // 迁移操作区:按钮组按目标分发,排除当前来源(同家迁移无意义,不显示按钮)
        ui.horizontal(|ui| {
            for target in [Tool::Codex, Tool::ClaudeCode, Tool::ZCode] {
                if target == source {
                    continue;
                }
                let button =
                    egui::Button::new(egui::RichText::new(migrate_target_label(target)).strong())
                        .min_size(egui::vec2(150.0, 30.0));
                let response = ui.add_enabled(can_migrate, button);
                let response = if can_migrate {
                    response
                } else {
                    response.on_disabled_hover_text("该会话没有可迁移的内容")
                };
                if response.clicked() {
                    self.do_migrate(target);
                }
            }
        });
        ui.add_space(4.0);
        self.show_migration_result(ui);

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        // 消息流预览(此处重新借用,不再与按钮区的 &mut self 冲突)
        let Some(session) = &self.session else {
            return;
        };
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for message in &session.messages {
                    let (role_name, role_color, bubble) = match message.role {
                        Role::User => ("用户", COLOR_USER_ROLE, COLOR_USER_BUBBLE),
                        Role::Assistant => ("助手", COLOR_ASSISTANT_ROLE, COLOR_ASSISTANT_BUBBLE),
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

    fn show_migration_result(&mut self, ui: &mut egui::Ui) {
        let Some(result) = &self.migration else {
            return;
        };
        match result {
            Ok(ok) => notice_panel(ui, COLOR_OK_BUBBLE, COLOR_OK_TEXT, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new("迁移成功")
                        .strong()
                        .color(COLOR_OK_TEXT),
                );
                if ok.parse_warnings > 0 {
                    ui.label(
                        egui::RichText::new(format!(
                            "已跳过 {} 行无法解析的源内容",
                            ok.parse_warnings
                        ))
                        .small()
                        .color(COLOR_OK_TEXT),
                    );
                }
                if ok.target != Tool::ZCode {
                    // Codex / Claude 目标:在终端执行命令继续会话
                    if let Some(command) = ok.resume_command.as_deref() {
                        ui.label(
                            egui::RichText::new("在终端执行以下命令,继续这个会话:")
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
                        if ui.button("复制命令").clicked() {
                            ui.ctx().copy_text(command.to_string());
                        }
                    }
                    if let Some(path) = ok.file_path.as_deref() {
                        ui.label(
                            egui::RichText::new(format!("产物文件:{path}"))
                                .small()
                                .weak()
                                .monospace(),
                        );
                    }
                } else {
                    // ZCode 目标:桌面应用无终端命令,给出打开指引与 session_id
                    ui.label(
                        egui::RichText::new(format!(
                            "迁移完成,打开 ZCode 应用即可在任务列表中看到(会话标题:{})",
                            ok.title
                        ))
                        .small()
                        .color(COLOR_OK_TEXT),
                    );
                    ui.label(
                        egui::RichText::new(format!("session id:{}", ok.session_id))
                            .small()
                            .weak()
                            .monospace(),
                    );
                }
            }),
            Err(err) => {
                ui.colored_label(COLOR_ERR_TEXT, format!("迁移失败:{err}"));
            }
        }
    }
}

// ---------- 小组件 ----------

/// 提示条面板(成功/警告等)。
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

/// 卡片 / 详情头部的来源徽章:固定尺寸着色小标签,白字居中。
/// 直接用 painter 绘制,宽度确定,便于标题按「剩余宽度截断」排布。
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

/// 迁移按钮文案(按目标工具)。
fn migrate_target_label(tool: Tool) -> &'static str {
    match tool {
        Tool::ClaudeCode => "迁移到 Claude  →",
        Tool::Codex => "迁移到 Codex  →",
        Tool::ZCode => "迁移到 ZCode  →",
    }
}

/// RFC3339 时间戳显示为 "MM-DD HH:MM"(解析失败则原样返回)。
fn friendly_time(rfc3339: &str) -> String {
    if rfc3339.len() >= 16 {
        rfc3339[5..16].replace('T', " ")
    } else {
        rfc3339.to_string()
    }
}

// ---------- 字体 ----------

/// 加载 CJK 字体,保证中英文在同一基线。
///
/// 关键点:把 CJK 字体插入 Proportional 家族**最前面**,拉丁字符同样由它渲染——
/// 单一字体内部的中西文 metrics 天然对齐;若拉丁走内置字体、中文走回退字体,
/// 两套字体的行高差异会让同一行文字高低错落。
/// Monospace 家族保持内置等宽字体在前,CJK 追加在末尾(等宽内容仍对齐,中文可回落)。
///
/// 候选:已知含简体中文的系统字体(按优先级),全部缺失时扫描用户字体目录。
/// 不做用户目录扫描优先,因为用户目录常见纯拉丁等宽字体(如 JetBrains Mono),
/// 先命中它会导致中文显示为方框。
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
        // Proportional:插入最前,中西文同字体 → 同基线
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, name.clone());
        // Monospace:等宽字体保持在前,中文末尾回落
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(name.clone());
        break;
    }
    ctx.set_fonts(fonts);
}
