//! Agent Session Hub 桌面应用。
//!
//! 界面:左侧为 Claude Code 会话列表(自动扫描,按最近活跃排序),
//! 右侧为所选会话的消息流预览;点击"迁移到 Codex"完成转换,
//! 并给出可直接复制到终端的续聊命令。

use std::path::PathBuf;

use eframe::egui;
use hub_core::{
    read_session, scan_sessions, write_session, Role, SessionSummary, UnifiedPart, UnifiedSession,
};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_title("Agent Session Hub"),
        ..Default::default()
    };
    eframe::run_native(
        "Agent Session Hub",
        options,
        Box::new(|cc| {
            install_home_fonts(&cc.egui_ctx);
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
            .default_size(360.0)
            .show(ui, |ui| {
                ui.heading("Claude 会话");
                if ui.button("重新扫描").clicked() {
                    self.rescan();
                }
                if let Some(err) = &self.scan_error {
                    ui.colored_label(
                        egui::Color32::RED,
                        format!("扫描失败:{err}(源目录:{})", self.claude_root.display()),
                    );
                }
                ui.separator();
                let mut clicked: Option<usize> = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, summary) in self.summaries.iter().enumerate() {
                        let label = format!(
                            "{}\n{} · {} 条消息\n{}",
                            summary.title,
                            summary.last_active,
                            summary.message_count,
                            summary.project_dir
                        );
                        if ui
                            .selectable_label(self.selected == Some(i), label)
                            .clicked()
                        {
                            clicked = Some(i);
                        }
                    }
                });
                if let Some(i) = clicked {
                    self.select(i);
                }
            });

        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.show_detail(ui);
        });
    }
}

impl HubApp {
    fn show_detail(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.selected else {
            ui.label("在左侧选择一个会话以预览");
            return;
        };
        let Some(summary) = self.summaries.get(index) else {
            return;
        };
        ui.heading(&summary.title);
        ui.label(format!("项目目录:{}", summary.project_dir));
        ui.label(format!("最后活跃:{}", summary.last_active));
        ui.label(format!("会话 ID:{}", summary.session_id));
        ui.separator();

        if let Some(err) = &self.detail_error {
            ui.colored_label(egui::Color32::RED, format!("读取会话失败:{err}"));
            return;
        }
        let Some(session) = &self.session else {
            return;
        };

        // 坏行提示
        if session.parse_warnings > 0 {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "解析警告:源中有 {} 行无法解析,迁移时将跳过",
                    session.parse_warnings
                ),
            );
        }

        // 迁移按钮
        let can_migrate = session.messages.iter().any(|m| !m.parts.is_empty());
        let response = ui.add_enabled(can_migrate, egui::Button::new("迁移到 Codex"));
        let response = if can_migrate {
            response
        } else {
            response.on_disabled_hover_text("空会话,无可迁移消息")
        };
        let migrate_clicked = response.clicked();
        ui.separator();

        // 预览:角色消息流
        egui::ScrollArea::vertical().show(ui, |ui| {
            for message in &session.messages {
                let (role_name, color) = match message.role {
                    Role::User => ("用户", egui::Color32::LIGHT_BLUE),
                    Role::Assistant => ("助手", egui::Color32::LIGHT_GREEN),
                };
                ui.colored_label(color, format!("── {role_name} ──"));
                for part in &message.parts {
                    match part {
                        UnifiedPart::Text(text) => {
                            ui.label(text);
                        }
                        UnifiedPart::Reasoning(text) => {
                            ui.weak(format!("> 内部推理:{text}"));
                        }
                        UnifiedPart::ToolCall { tool, input_json } => {
                            ui.monospace(format!("[调用工具 {tool}] {input_json}"));
                        }
                        UnifiedPart::ToolResult {
                            tool,
                            content,
                            is_error,
                        } => {
                            ui.monospace(format!("[工具结果 {tool} isError={is_error}] {content}"));
                        }
                    }
                }
                ui.add_space(6.0);
            }
        });

        if migrate_clicked {
            self.do_migrate();
        }

        // 迁移结果
        match &self.migration {
            Some(Ok(ok)) => {
                ui.separator();
                ui.heading("迁移成功");
                if ok.parse_warnings > 0 {
                    ui.label(format!(
                        "提示:已跳过 {} 行无法解析的源内容",
                        ok.parse_warnings
                    ));
                }
                ui.label("产物文件:");
                ui.monospace(&ok.file_path);
                ui.label("在终端执行以下命令继续会话:");
                ui.monospace(&ok.resume_command);
                if ui.button("复制命令").clicked() {
                    ui.ctx().copy_text(ok.resume_command.clone());
                }
            }
            Some(Err(err)) => {
                ui.separator();
                ui.colored_label(egui::Color32::RED, format!("迁移失败:{err}"));
            }
            None => {}
        }
    }
}

/// 加载 CJK 回退字体(界面为中文,egui 内置字体不含 CJK)。
///
/// 策略:优先挂载"已知含简体中文"的系统字体(按优先级逐个尝试,读不到就下一个);
/// 全部未命中时才退回扫描用户字体目录。不做目录扫描优先,是因为用户目录里
/// 常见的是纯拉丁等宽字体(如 JetBrains Mono),先命中它会导致中文显示为方框。
fn install_home_fonts(ctx: &egui::Context) {
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

    let mut fallback_scan_dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        fallback_scan_dirs.extend([home.join("Library/Fonts"), home.join(".fonts")]);
    }

    // 已知白名单(按优先级)→ 用户目录扫描(按文件名序),取第一个能读到的
    let mut candidates: Vec<PathBuf> = known_cjk_fonts.iter().map(PathBuf::from).collect();
    for dir in &fallback_scan_dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            candidates.extend(entries.flatten().map(|e| e.path()).filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| matches!(e, "ttf" | "ttc" | "otf"))
                    .unwrap_or(false)
            }));
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
        // 追加到家族末尾作为回退字体:拉丁字符仍用内置字体,缺字才落到 CJK 字体
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push(name.clone());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(name.clone());
        break;
    }
    ctx.set_fonts(fonts);
}
