//! Agent Session Hub 桌面应用。
//!
//! 界面:左侧为 Claude Code 会话列表(自动扫描,按最近活跃排序,卡片式),
//! 右侧为所选会话的消息流预览;点击"迁移到 Codex"完成转换,
//! 并给出可直接复制到终端的续聊命令。

use std::path::PathBuf;

use eframe::egui;
use hub_core::{
    read_session, scan_sessions, write_session, Role, SessionSummary, UnifiedPart, UnifiedSession,
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

/// 迁移成功后的展示状态(错误以 Display 字符串保存)。
struct MigrationOk {
    resume_command: String,
    file_path: String,
    parse_warnings: usize,
}

struct HubApp {
    claude_root: PathBuf,
    codex_root: PathBuf,
    summaries: Vec<SessionSummary>,
    scan_error: Option<String>,
    search_query: String,
    selected: Option<usize>,
    session: Option<UnifiedSession>,
    detail_error: Option<String>,
    migration: Option<Result<MigrationOk, String>>,
}

impl HubApp {
    fn new() -> Self {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let mut app = HubApp {
            claude_root: home.join(".claude").join("projects"),
            codex_root: home.join(".codex").join("sessions"),
            summaries: Vec::new(),
            scan_error: None,
            search_query: String::new(),
            selected: None,
            session: None,
            detail_error: None,
            migration: None,
        };
        app.rescan();
        app
    }

    fn rescan(&mut self) {
        self.selected = None;
        self.session = None;
        self.detail_error = None;
        self.migration = None;
        match scan_sessions(&self.claude_root) {
            Ok(summaries) => {
                self.summaries = summaries;
                self.scan_error = None;
            }
            Err(e) => {
                self.summaries.clear();
                self.scan_error = Some(e.to_string());
            }
        }
    }

    fn select(&mut self, index: usize) {
        self.selected = Some(index);
        self.migration = None;
        let path = self.summaries[index].source_path.clone();
        match read_session(&path) {
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

    fn do_migrate(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        let warnings = session.parse_warnings;
        self.migration = Some(match write_session(session, &self.codex_root) {
            Ok(out) => Ok(MigrationOk {
                resume_command: out.resume_command,
                file_path: out.file_path.display().to_string(),
                parse_warnings: warnings,
            }),
            Err(e) => Err(e.to_string()),
        });
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
    // ---------- 左侧:会话列表 ----------

    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Claude 会话").heading());
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
        // 不区分大小写的子串匹配,空输入显示全部
        let query = self.search_query.trim().to_lowercase();
        let visible: Vec<usize> = self
            .summaries
            .iter()
            .enumerate()
            .filter(|(_, summary)| {
                query.is_empty()
                    || summary.title.to_lowercase().contains(&query)
                    || summary.project_dir.to_lowercase().contains(&query)
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
        let subtitle = if query.is_empty() {
            format!("共 {} 个会话,按最近活跃排序", self.summaries.len())
        } else {
            format!(
                "共 {} / {} 个会话,按最近活跃排序",
                visible.len(),
                self.summaries.len()
            )
        };
        ui.label(egui::RichText::new(subtitle).small().weak());
        ui.add_space(4.0);

        if let Some(err) = &self.scan_error {
            let (rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 44.0), egui::Sense::hover());
            ui.painter().rect_filled(
                rect,
                egui::CornerRadius::same(6),
                egui::Color32::from_rgb(253, 235, 235),
            );
            ui.put(
                rect.shrink2(egui::vec2(8.0, 4.0)),
                egui::Label::new(
                    egui::RichText::new(format!("扫描失败:{err}"))
                        .small()
                        .color(COLOR_ERR_TEXT),
                )
                .truncate(),
            );
        }

        let mut clicked: Option<usize> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for &i in &visible {
                    let summary = &self.summaries[i];
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
                        ui.add(
                            egui::Label::new(egui::RichText::new(&summary.title).strong())
                                .truncate(),
                        );
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
        let Some(summary) = self.summaries.get(index) else {
            return;
        };

        // 头部信息
        ui.label(egui::RichText::new(&summary.title).heading().strong());
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

        // 迁移操作区
        ui.horizontal(|ui| {
            let button = egui::Button::new(egui::RichText::new("迁移到 Codex  →").strong())
                .min_size(egui::vec2(170.0, 30.0));
            let response = ui.add_enabled(can_migrate, button);
            let response = if can_migrate {
                response
            } else {
                response.on_disabled_hover_text("该会话没有可迁移的内容")
            };
            if response.clicked() {
                self.do_migrate();
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
        match &self.migration {
            Some(Ok(ok)) => {
                notice_panel(ui, COLOR_OK_BUBBLE, COLOR_OK_TEXT, |ui| {
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
                                egui::Label::new(
                                    egui::RichText::new(&ok.resume_command).monospace(),
                                )
                                .truncate(),
                            );
                        });
                    if ui.button("复制命令").clicked() {
                        ui.ctx().copy_text(ok.resume_command.clone());
                    }
                    ui.label(
                        egui::RichText::new(format!("产物文件:{}", ok.file_path))
                            .small()
                            .weak()
                            .monospace(),
                    );
                });
            }
            Some(Err(err)) => {
                ui.colored_label(COLOR_ERR_TEXT, format!("迁移失败:{err}"));
            }
            None => {}
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
