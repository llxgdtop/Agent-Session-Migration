//! Launcher 层:目标工具 resume 命令生成与一键打开。
//!
//! resume 命令部分当前支持 Codex 与 Claude Code;
//! 一键打开部分(`open_in_terminal` / `open_zcode_app`)仅在 macOS 上生效,
//! 且只应由 UI 在用户点击按钮时调用(本模块的测试只验证参数构造,不真正启动)。

/// 迁移成功后交付的 Codex 命令。
///
/// `cd '<project_dir>' && codex resume <session_id>`
///
/// 安全(代码审查修复):project_dir 来自源会话文件的 cwd 字段(外部可控文本),
/// 而本命令会被用户复制到终端执行,不得裸拼。规则见 [`cd_prefix`]。
pub fn codex_resume_command(session_id: &str, project_dir: &str) -> String {
    format!("{}codex resume {session_id}", cd_prefix(project_dir))
}

/// 迁移成功后交付的 Claude Code 命令。
///
/// `cd '<project_dir>' && claude --resume <session_id>`
///
/// 引号防护规则与 [`codex_resume_command`] 完全一致(共用 [`cd_prefix`])。
pub fn claude_resume_command(session_id: &str, project_dir: &str) -> String {
    format!("{}claude --resume {session_id}", cd_prefix(project_dir))
}

/// cd 段生成(两个 resume 命令共用):不含单引号时以单引号包裹
/// (shell 单引号内无任何展开);含单引号或为空时放弃 cd 段
/// (session_id 为本工具生成的 uuid v4,天然安全)。
fn cd_prefix(project_dir: &str) -> String {
    if project_dir.is_empty() || project_dir.contains('\'') {
        String::new()
    } else {
        format!("cd '{project_dir}' && ")
    }
}

// ---------- 一键打开(实际启动仅在用户点击界面按钮时发生)----------

/// 在终端中启动命令(macOS 用 Terminal.app 执行)。
///
/// 通过 AppleScript 让 Terminal 新建窗口以 shell 执行 `command`,并把 Terminal
/// 带到前台;osascript 在命令发起成功后即返回,本函数不等待命令结束。
/// 非 macOS 平台返回 `Unsupported` 错误。
pub fn open_in_terminal(command: &str) -> std::io::Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "当前平台不支持在终端中打开(仅支持 macOS)",
        ));
    }
    run_and_check("osascript", &osascript_args_for_terminal(command))
}

/// 打开 ZCode 桌面应用。
///
/// `open -a ZCode`:按应用名启动;找不到应用时 open 自身报错,原样透传。
/// 非 macOS 平台返回 `Unsupported` 错误。
pub fn open_zcode_app() -> std::io::Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "当前平台不支持打开 ZCode(仅支持 macOS)",
        ));
    }
    run_and_check("open", &open_app_args("ZCode"))
}

/// 构造让 Terminal 执行命令的 osascript 参数(纯函数,便于单测)。
///
/// 两条 AppleScript 语句:`do script` 在新 Terminal 窗口以 shell 执行命令,
/// `activate` 把 Terminal 带到前台。command 经参数数组传给 osascript(不经 shell,
/// 不会被 shell 再解释),只需做 AppleScript 字符串一层的转义,见
/// [`escape_applescript_string`]。
fn osascript_args_for_terminal(command: &str) -> Vec<String> {
    vec![
        "-e".to_string(),
        format!(
            "tell application \"Terminal\" to do script \"{}\"",
            escape_applescript_string(command)
        ),
        "-e".to_string(),
        "tell application \"Terminal\" to activate".to_string(),
    ]
}

/// 构造 `open -a <应用名>` 的参数(纯函数,便于单测)。
fn open_app_args(app_name: &str) -> Vec<String> {
    vec!["-a".to_string(), app_name.to_string()]
}

/// AppleScript 字符串字面量转义:反斜杠与双引号前加反斜杠。
/// 其余字符(含单引号、换行、非 ASCII)在 AppleScript 字符串中均按字面量传递。
fn escape_applescript_string(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// 执行外部命令并检查结果:非零退出时把退出码与 stderr 拼进错误信息返回。
///
/// osascript / open 都只负责"发起"动作,发起成功后立刻退出,因此这里同步等待
/// 它们退出并不会挂住被启动的程序,反而能拿到失败原因(如应用不存在)。
fn run_and_check(program: &str, args: &[String]) -> std::io::Result<()> {
    let output = std::process::Command::new(program).args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    let status = match output.status.code() {
        Some(code) => format!("退出码 {code}"),
        None => "被信号终止".to_string(),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    Err(std::io::Error::other(format!(
        "`{program}` 执行失败({status}):{detail}"
    )))
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

    // ---------- TC-LAUNCH-03 ----------
    #[test]
    fn tc_launch_03_claude_resume_command_format() {
        assert_eq!(
            claude_resume_command("8a0d022a-6659-4097-bb6b-cff0fd6e7ed1", "/Users/x/proj"),
            "cd '/Users/x/proj' && claude --resume 8a0d022a-6659-4097-bb6b-cff0fd6e7ed1"
        );
        // 引号注入防护与 codex 版本同规则:含单引号放弃 cd 段
        assert_eq!(
            claude_resume_command("8a0d022a-6659-4097-bb6b-cff0fd6e7ed1", "/tmp/x'; rm -rf ~"),
            "claude --resume 8a0d022a-6659-4097-bb6b-cff0fd6e7ed1"
        );
        // 单引号内的常规元字符仅作字面量
        assert_eq!(
            claude_resume_command("sid", "/tmp/a;b&c$d`e"),
            "cd '/tmp/a;b&c$d`e' && claude --resume sid"
        );
        // 空路径:只给 resume 命令
        assert_eq!(claude_resume_command("sid", ""), "claude --resume sid");
    }

    // ---------- TC-LAUNCH-04:一键打开的 osascript 参数构造 ----------
    // 结构:do script 语句承载命令,activate 语句把 Terminal 带到前台。
    // 只验证参数构造,不真正启动 Terminal(避免测试弹窗口)。
    #[test]
    fn tc_launch_04_terminal_osascript_args() {
        let args = osascript_args_for_terminal("cd '/Users/x/proj' && codex resume abc");
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "-e");
        assert_eq!(
            args[1],
            "tell application \"Terminal\" to do script \"cd '/Users/x/proj' && codex resume abc\""
        );
        assert_eq!(args[2], "-e");
        assert_eq!(args[3], "tell application \"Terminal\" to activate");
    }

    // TC-LAUNCH-05:AppleScript 字符串转义——命令中的双引号与反斜杠必须转义,
    // 否则字符串在引号处提前截断、反斜杠会被 AppleScript 吃掉;其余字符原样透传。
    #[test]
    fn tc_launch_05_applescript_escaping() {
        assert_eq!(escape_applescript_string("plain text"), "plain text");
        assert_eq!(escape_applescript_string("echo \"hi\""), "echo \\\"hi\\\"");
        assert_eq!(escape_applescript_string("a\\b"), "a\\\\b");
        // 非 ASCII 与单引号不做任何处理
        assert_eq!(escape_applescript_string("中文'引号'"), "中文'引号'");
        // 组合场景:转义后的命令仍是完整的一条 do script 字符串
        let args = osascript_args_for_terminal("echo \"ok\" && cd 'C:\\work'");
        assert_eq!(
            args[1],
            "tell application \"Terminal\" to do script \"echo \\\"ok\\\" && cd 'C:\\\\work'\""
        );
    }

    // TC-LAUNCH-06:open 命令参数构造(-a 按应用名启动)。
    #[test]
    fn tc_launch_06_open_app_args() {
        assert_eq!(open_app_args("ZCode"), vec!["-a", "ZCode"]);
    }
}
