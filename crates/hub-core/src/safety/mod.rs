//! 写入前护栏:迁移写入前的目标工具进程检测。
//!
//! 迁移会直接写目标工具的会话文件/会话库;若目标工具正在运行,双方同时写入
//! 可能互相冲突(ZCode 为 SQLite 双库,同时写入还有数据损坏风险)。
//! UI 在用户点击迁移按钮时调用 [`is_tool_running`],命中则先警示、
//! 由用户确认「仍要迁移」后再放行。本模块只做只读探测,不产生任何副作用。

use crate::ir::Tool;

/// 检测目标工具进程是否正在运行(macOS / Linux 通用)。
///
/// 每工具配置一组候选精确进程名(`pgrep -x`,ZCode 附加 `-i` 忽略大小写),
/// 任一命中即视为运行中;候选名同时经 `ps` 兜底核对(见 [`is_process_running`])。
/// 探测工具缺失或执行失败时返回 false(保守放行):护栏缺失不应阻塞用户,
/// 冲突风险另由写入器的原子落盘 / 事务备份兜底。
pub fn is_tool_running(tool: Tool) -> bool {
    let (names, ignore_case) = tool_process_config(tool);
    names
        .iter()
        .any(|name| is_process_running(name, ignore_case))
}

/// 每工具的进程检测配置:(精确进程名列表, 匹配是否忽略大小写)。
///
/// - Claude Code CLI 进程名为 `claude`;
/// - Codex CLI 进程名为 `codex`;
/// - ZCode 桌面应用进程名为 `ZCode`,不同版本/平台大小写不定,
///   忽略大小写匹配 `zcode` 更稳。
fn tool_process_config(tool: Tool) -> (&'static [&'static str], bool) {
    match tool {
        Tool::ClaudeCode => (&["claude"], false),
        Tool::Codex => (&["codex"], false),
        Tool::ZCode => (&["zcode"], true),
    }
}

/// 单个精确进程名的两层探测:`pgrep -x` 为主,`ps` 进程名全量扫描兜底。
///
/// 不用 `pgrep -f`(匹配完整命令行):容易被无关进程——甚至我们自己进程的
/// 调用参数——误命中;精确进程名最稳。
///
/// 为什么要 ps 兜底:实测部分 macOS 上 `pgrep` 存在进程盲区(如无法看到
/// Claude Code CLI 主进程),而 `ps` 走 kinfo 通路不受影响。两层取"或":
/// - 任一层命中 → 运行中(宁可误报让用户确认,不可漏报放任冲突);
/// - 两层都未命中 / 工具执行失败 → 未运行(保守放行)。
fn is_process_running(name: &str, ignore_case: bool) -> bool {
    pgrep_exact(name, ignore_case) || ps_comm_has(name, ignore_case)
}

/// `pgrep -x`(可选 `-i`)按精确进程名探测:退出码 0 视为命中。
/// pgrep 自身会被排除,不会自匹配。
fn pgrep_exact(name: &str, ignore_case: bool) -> bool {
    let mut command = std::process::Command::new("pgrep");
    command.arg("-x");
    if ignore_case {
        command.arg("-i");
    }
    command.arg(name);
    // 退出码 0=命中,1=未命中;其他失败(pgrep 缺失、参数错误)一律视为未命中
    command
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// `ps -axo comm=` 进程名全量扫描(macOS/Linux 均支持)。
///
/// macOS 输出的是可执行文件绝对路径(如 `/Applications/ZCode.app/.../ZCode`),
/// Linux 输出裸进程名,统一取最后的路径段再做精确比较(可选忽略大小写)。
fn ps_comm_has(name: &str, ignore_case: bool) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "comm="])
        .output()
    else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| {
        let process_name = line.trim().rsplit('/').next().unwrap_or_default();
        if ignore_case {
            process_name.eq_ignore_ascii_case(name)
        } else {
            process_name == name
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TC-SAFETY-01:肯定存在的系统能被命中——macOS 的 1 号进程为 launchd;
    /// Linux 上兜底 systemd / init / 内核线程 kthreadd,任一存在即通过。
    /// 走完整两层探测(部分 macOS 的 pgrep 看不到 launchd,恰好同时验证 ps 兜底)。
    #[test]
    fn tc_safety_01_detects_existing_process() {
        #[cfg(target_os = "macos")]
        let candidates = ["launchd"];
        #[cfg(not(target_os = "macos"))]
        let candidates = ["systemd", "init", "kthreadd"];
        assert!(
            candidates
                .iter()
                .any(|&name| is_process_running(name, false)),
            "系统常驻进程未被命中,检测通路异常"
        );
    }

    /// TC-SAFETY-02:不存在的进程不命中。用 uuid 随机长名,杜绝与环境里
    /// 真实进程撞名的可能;大小写敏感与不敏感两条路径都要验证。
    #[test]
    fn tc_safety_02_misses_nonexistent_process() {
        let name = format!("hub-no-proc-{}", uuid::Uuid::new_v4());
        assert!(!is_process_running(&name, false));
        assert!(!is_process_running(&name, true));
    }

    /// TC-SAFETY-03:三家工具的检测配置非空,`is_tool_running` 全通路可正常执行。
    /// 不断言返回值——本机是否真在运行这些工具取决于环境(如开发者自己开着 claude)。
    #[test]
    fn tc_safety_03_tool_configs_runnable() {
        for tool in [Tool::ClaudeCode, Tool::Codex, Tool::ZCode] {
            let (names, _) = tool_process_config(tool);
            assert!(!names.is_empty(), "{tool:?} 的进程名配置不应为空");
            let _ = is_tool_running(tool);
        }
    }
}
