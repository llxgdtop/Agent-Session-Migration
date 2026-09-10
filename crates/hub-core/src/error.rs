//! 统一错误类型(契约见 MVP-DEVELOPMENT.md §2.1/§2.2)。

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum HubError {
    /// 扫描根不存在 / 单个源文件不存在。
    SourceNotFound(PathBuf),
    /// 会话无 user/assistant 消息,或 map 后事件为空(BR-11/BR-21 极端)。
    EmptySession(PathBuf),
    /// 目标 rollout 文件已存在(BR-9,幂等键 = 目标文件路径)。
    TargetExists(PathBuf),
    /// 目标目录创建失败/无权限。
    NoWritableTarget(PathBuf),
    /// 其他 IO 错误。
    Io(std::io::Error),
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubError::SourceNotFound(p) => write!(f, "源不存在: {}", p.display()),
            HubError::EmptySession(p) => write!(f, "空会话,无可迁移消息: {}", p.display()),
            HubError::TargetExists(p) => write!(f, "目标文件已存在,拒绝覆盖: {}", p.display()),
            HubError::NoWritableTarget(p) => write!(f, "目标目录不可写: {}", p.display()),
            HubError::Io(e) => write!(f, "IO 错误: {e}"),
        }
    }
}

impl std::error::Error for HubError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HubError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for HubError {
    fn from(e: std::io::Error) -> Self {
        HubError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TC-IR-02(§6 commit ① 中列出;§7 表无独立行,此处覆盖 error.rs):
    /// HubError 各变体可展示、携带路径,Io 透出底层 source。
    #[test]
    fn tc_ir_02_hub_error_display_and_source() {
        assert!(HubError::SourceNotFound(PathBuf::from("/no/such/dir"))
            .to_string()
            .contains("/no/such/dir"));
        assert!(HubError::EmptySession(PathBuf::from("/a/b.jsonl"))
            .to_string()
            .contains("/a/b.jsonl"));
        assert!(HubError::TargetExists(PathBuf::from("/t/rollout-x.jsonl"))
            .to_string()
            .contains("/t/rollout-x.jsonl"));
        assert!(HubError::NoWritableTarget(PathBuf::from("/t/2026"))
            .to_string()
            .contains("/t/2026"));

        let e: HubError = std::io::Error::other("boom").into();
        assert!(e.to_string().contains("boom"));
        assert!(std::error::Error::source(&e).is_some());
    }
}
