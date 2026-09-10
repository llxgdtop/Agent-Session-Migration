//! Launcher 层:目标工具 resume 命令生成。MVP 仅实现 Codex。

/// BR-8/BR-14:迁移成功后交付的命令。
///
/// `cd <project_dir> && codex resume <session_id>`
/// (交互式形态;M1 验收时由 owner 人工复验,不可用则回退 `codex exec resume`,见 BR-14。)
pub fn codex_resume_command(session_id: &str, project_dir: &str) -> String {
    format!("cd {project_dir} && codex resume {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- TC-LAUNCH-01 ----------
    #[test]
    fn tc_launch_01_resume_command_format() {
        assert_eq!(
            codex_resume_command("8a0d022a-6659-4097-bb6b-cff0fd6e7ed1", "/Users/x/proj"),
            "cd /Users/x/proj && codex resume 8a0d022a-6659-4097-bb6b-cff0fd6e7ed1"
        );
    }
}
