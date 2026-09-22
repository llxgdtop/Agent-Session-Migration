//! 统一错误类型。

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum HubError {
    /// 扫描根不存在 / 单个源文件不存在。
    SourceNotFound(PathBuf),
    /// 会话无 user/assistant 消息,或 map 后事件为空。
    EmptySession(PathBuf),
    /// 目标 rollout 文件已存在。
    TargetExists(PathBuf),
    /// 目标目录创建失败/无权限。
    NoWritableTarget(PathBuf),
    /// 目标 ZCode 库的 schema_migration 版本不在已验证支持范围内,
    /// 拒绝写入以免破坏新版本表结构。内容为发现的迁移 id。
    SchemaUnsupported(String),
    /// 调用方传入的会话数据不满足写入前提(如缺少项目目录)。
    InvalidInput(String),
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

/// SQLite 错误并入 Io 变体:底层错误经 io::Error::other 保留完整 source 链,
/// 供上层诊断(表缺失、约束冲突等)。
impl From<rusqlite::Error> for HubError {
    fn from(e: rusqlite::Error) -> Self {
        HubError::Io(std::io::Error::other(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// ZCode 相关新变体:Display 文案可读,且 rusqlite 错误可自动转换并入 Io。
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

        // rusqlite 错误经 From 并入 Io,底层原因可从 source 链取回
        let rusqlite_err = rusqlite::Error::InvalidParameterName("缺参数名".to_string());
        let hub: HubError = rusqlite_err.into();
        assert!(matches!(hub, HubError::Io(_)));
        assert!(hub.to_string().contains("缺参数名"));
        assert!(std::error::Error::source(&hub).is_some());
    }
}
