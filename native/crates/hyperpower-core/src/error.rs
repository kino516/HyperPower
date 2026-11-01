//! 内核错误类型

use thiserror::Error;

/// 决策内核错误
#[derive(Debug, Error, PartialEq)]
pub enum CoreError {
    #[error("配置非法：{0}")]
    InvalidConfig(String),

    #[error("采样数据不可用：{0}")]
    BadSample(String),

    /// 超出设备能力边界（检测得出的客观上限）
    ///
    /// ⚠️ 与 `InvalidConfig` 的区别：后者是"配置自身不合法"（如范围写反），
    /// 前者是"配置合法但超出本机能力"（如目标功率超过整机最大功率）。
    #[error("超出设备能力：{0}")]
    OutOfBounds(String),
}
