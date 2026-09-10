//! Agent Session Hub — egui 壳(hub-app)。
//!
//! MVP 形态(§6 ⑦):左侧 Claude 会话列表(scan_sessions)、右侧角色消息流预览、
//! "迁移到 Codex" 按钮(write_session)、成功后显示可复制的 resume 命令与 parse_warnings 提示。
//! 简洁可用即可,不追求视觉(BR-2/8/13)。

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
            ui.label("在左侧选择一个会话以预览(BR-2)");
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

        // BR-10:坏行提示
        if session.parse_warnings > 0 {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!(
                    "解析警告:源中有 {} 行无法解析,迁移时将跳过",
                    session.parse_warnings
                ),
            );
        }

        // 迁移按钮(BR-11:空会话禁用)
        let can_migrate = session.messages.iter().any(|m| !m.parts.is_empty());
        let response = ui.add_enabled(can_migrate, egui::Button::new("迁移到 Codex"));
        let response = if can_migrate {
            response
        } else {
            response.on_disabled_hover_text("空会话,无可迁移消息(BR-11)")
        };
        let migrate_clicked = response.clicked();
        ui.separator();

        // BR-13:M1 预览 = 仅角色消息流
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

        // 迁移结果(BR-8/BR-14)
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

/// best-effort CJK 字体:仅扫描 $HOME 内的字体目录(macOS:~/Library/Fonts;Linux:~/.fonts)。
///
/// egui 内置字体不含 CJK,若 $HOME 无可用字体,中文将显示为方框(已知限制,见 README);
/// 读取 $HOME 之外的系统字体路径属自治边界 #9,待 owner 决策。
fn install_home_fonts(ctx: &egui::Context) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    let candidate_dirs = [home.join("Library/Fonts"), home.join(".fonts")];

    let mut fonts = egui::FontDefinitions::default();
    'outer: for dir in candidate_dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_font = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e, "ttf" | "ttc" | "otf"))
                .unwrap_or(false);
            if !is_font {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let name = format!(
                "home-font-{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            fonts
                .font_data
                .insert(name.clone(), egui::FontData::from_owned(bytes).into());
            // 追加到家族末尾作为回退字体
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
            break 'outer; // 只需一个可用字体
        }
    }
    ctx.set_fonts(fonts);
}
