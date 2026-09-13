//! Launcher 层:目标工具 resume 命令生成。当前支持 Codex。

/// /:迁移成功后交付的命令。
///
/// `cd '<project_dir>' && codex resume <session_id>`
///
/// 安全(代码审查修复):project_dir 来自源会话文件的 cwd 字段(外部可控文本),
/// 而本命令会被用户复制到终端执行,不得裸拼。规则:不含单引号时以单引号包裹
/// (shell 单引号内无任何展开);含单引号或为空时放弃 cd 段(session_id 为本工具
/// 生成的 uuid v4,天然安全)。
pub fn codex_resume_command(session_id: &str, project_dir: &str) -> String {
    let cd = if project_dir.is_empty() || project_dir.contains('\'') {
        String::new()
    } else {
        format!("cd '{project_dir}' && ")
    };
    format!("{cd}codex resume {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- TC-LAUNCH-01 ----------
    #[test]
    fn tc_launch_01_resume_command_format() {
        assert_eq!(
            codex_resume_command("8a0d022a-6659-4097-bb6b-cff0fd6e7ed1", "/Users/x/proj"),
            "cd '/Users/x/proj' && codex resume 8a0d022a-6659-4097-bb6b-cff0fd6e7ed1"
        );
    }

    /// 注入防护(代码审查补充):含单引号的 project_dir 放弃 cd 段;
    /// 路径中的 `$`/`;`/反引号等元字符在单引号内不再具有 shell 语义。
    #[test]
    fn tc_launch_02_quoting_blocks_injection() {
        assert_eq!(
            codex_resume_command("8a0d022a-6659-4097-bb6b-cff0fd6e7ed1", "/tmp/x'; rm -rf ~"),
            "codex resume 8a0d022a-6659-4097-bb6b-cff0fd6e7ed1"
        );
        // 单引号内的常规元字符仅作字面量
        assert_eq!(
            codex_resume_command("sid", "/tmp/a;b&c$d`e"),
            "cd '/tmp/a;b&c$d`e' && codex resume sid"
        );
        // 空路径:只给 resume 命令
        assert_eq!(codex_resume_command("sid", ""), "codex resume sid");
    }
}
