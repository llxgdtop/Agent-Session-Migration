//! Unified error type.

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum HubError {
    /// Scan root does not exist / a single source file does not exist.
    SourceNotFound(PathBuf),
    /// Session has no user/assistant messages, or the mapped event list is empty.
    EmptySession(PathBuf),
    /// Target rollout file already exists.
    TargetExists(PathBuf),
    /// Target directory could not be created / is not writable.
    NoWritableTarget(PathBuf),
    /// The target ZCode database's schema_migration version is outside the
    /// verified-supported range; refuse to write so a newer table layout is
    /// never corrupted. Carries the discovered migration id.
    SchemaUnsupported(String),
    /// Session data supplied by the caller violates a write precondition
    /// (e.g. missing project directory).
    InvalidInput(String),
    /// Other IO errors.
    Io(std::io::Error),
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HubError::SourceNotFound(p) => write!(f, "源不存在: {}", p.display()),
            HubError::EmptySession(p) => write!(f, "空会话,无可迁移消息: {}", p.display()),
            HubError::TargetExists(p) => write!(f, "目标文件已存在,拒绝覆盖: {}", p.display()),
            HubError::NoWritableTarget(p) => write!(f, "目标目录不可写: {}", p.display()),
            HubError::SchemaUnsupported(id) => {
                write!(f, "ZCode 数据版本不受支持: {id},请更新应用")
            }
            HubError::InvalidInput(reason) => write!(f, "无法迁移:{reason}"),
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

/// Fold SQLite errors into the Io variant: the underlying error is preserved
/// via io::Error::other with a full source chain, so upper layers can
/// diagnose it (missing table, constraint violation, etc.).
impl From<rusqlite::Error> for HubError {
    fn from(e: rusqlite::Error) -> Self {
        HubError::Io(std::io::Error::other(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every HubError variant displays and carries its path; Io exposes the
    /// underlying source.
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

    /// The newer ZCode variants: readable Display text, and rusqlite errors
    /// convert automatically into Io.
    #[test]
    fn tc_err_03_zcode_variants_display_and_rusqlite_from() {
        let schema = HubError::SchemaUnsupported("0099_future_schema".to_string());
        let text = schema.to_string();
        assert!(text.contains("ZCode 数据版本不受支持"));
        assert!(text.contains("0099_future_schema"));
        assert!(text.contains("请更新应用"));

        let input = HubError::InvalidInput("源会话缺少项目目录".to_string());
        let text = input.to_string();
        assert!(text.contains("无法迁移"));
        assert!(text.contains("源会话缺少项目目录"));

        // A rusqlite error folds into Io via From; the underlying reason stays
        // reachable through the source chain
        let rusqlite_err = rusqlite::Error::InvalidParameterName("缺参数名".to_string());
        let hub: HubError = rusqlite_err.into();
        assert!(matches!(hub, HubError::Io(_)));
        assert!(hub.to_string().contains("缺参数名"));
        assert!(std::error::Error::source(&hub).is_some());
    }
}
