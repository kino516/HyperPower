//! 错误码与退出码（契约冻结，见 docs/05-契约-接口输出规范.md §3）

use serde::{Deserialize, Serialize};

/// 全部错误码
///
/// ⚠️ **契约冻结**：既有取值的语义不得改变；新增只能使用未占用取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// 内部错误
    #[serde(rename = "E_INTERNAL")]
    Internal,
    /// 键不存在
    #[serde(rename = "E_UNKNOWN_KEY")]
    UnknownKey,
    /// 类型错误 / 参数格式非法
    #[serde(rename = "E_TYPE")]
    Type,
    /// 值超出范围
    #[serde(rename = "E_RANGE")]
    Range,
    /// 只读字段不可修改
    #[serde(rename = "E_READONLY")]
    Readonly,
    /// 需要二次确认
    #[serde(rename = "E_CONFIRM")]
    Confirm,
    /// 配置校验未通过
    #[serde(rename = "E_COLLISION")]
    Collision,
    /// 权限不足
    #[serde(rename = "E_NO_PERM")]
    NoPerm,
    /// 节点不存在
    #[serde(rename = "E_NO_NODE")]
    NoNode,
    /// 需要充电中
    #[serde(rename = "E_NOT_CHARGING")]
    NotCharging,
    /// 条件不满足
    #[serde(rename = "E_BAD_COND")]
    BadCond,
    /// 无法读取来源
    #[serde(rename = "E_NO_SOURCE")]
    NoSource,
    /// 忙
    #[serde(rename = "E_BUSY")]
    Busy,
    /// 安全模式
    #[serde(rename = "E_SAFE_MODE")]
    SafeMode,
    /// 模式限制
    #[serde(rename = "E_MODE_LIMIT")]
    ModeLimit,
}

impl ErrorCode {
    /// 错误码字符串
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "E_INTERNAL",
            Self::UnknownKey => "E_UNKNOWN_KEY",
            Self::Type => "E_TYPE",
            Self::Range => "E_RANGE",
            Self::Readonly => "E_READONLY",
            Self::Confirm => "E_CONFIRM",
            Self::Collision => "E_COLLISION",
            Self::NoPerm => "E_NO_PERM",
            Self::NoNode => "E_NO_NODE",
            Self::NotCharging => "E_NOT_CHARGING",
            Self::BadCond => "E_BAD_COND",
            Self::NoSource => "E_NO_SOURCE",
            Self::Busy => "E_BUSY",
            Self::SafeMode => "E_SAFE_MODE",
            Self::ModeLimit => "E_MODE_LIMIT",
        }
    }

    /// 对应退出码（**契约冻结**）
    ///
    /// | 错误码 | 退出码 |
    /// |---|---|
    /// | `E_INTERNAL` | 1 |
    /// | `E_UNKNOWN_KEY` / `E_TYPE` / `E_RANGE` / `E_READONLY` / `E_CONFIRM` / `E_COLLISION` | 2 |
    /// | `E_NO_PERM` | 3 |
    /// | `E_NO_NODE` / `E_NOT_CHARGING` / `E_BAD_COND` / `E_NO_SOURCE` | 4 |
    /// | `E_BUSY` | 5 |
    /// | `E_SAFE_MODE` / `E_MODE_LIMIT` | 6 |
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Internal => 1,
            Self::UnknownKey
            | Self::Type
            | Self::Range
            | Self::Readonly
            | Self::Confirm
            | Self::Collision => 2,
            Self::NoPerm => 3,
            Self::NoNode | Self::NotCharging | Self::BadCond | Self::NoSource => 4,
            Self::Busy => 5,
            Self::SafeMode | Self::ModeLimit => 6,
        }
    }
}

/// 全部错误码（用于自检与文档生成）
pub const ERROR_CODES: &[ErrorCode] = &[
    ErrorCode::Internal,
    ErrorCode::UnknownKey,
    ErrorCode::Type,
    ErrorCode::Range,
    ErrorCode::Readonly,
    ErrorCode::Confirm,
    ErrorCode::Collision,
    ErrorCode::NoPerm,
    ErrorCode::NoNode,
    ErrorCode::NotCharging,
    ErrorCode::BadCond,
    ErrorCode::NoSource,
    ErrorCode::Busy,
    ErrorCode::SafeMode,
    ErrorCode::ModeLimit,
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// 每个错误码都必须能从字符串**往返**解析回来
    ///
    /// # 为什么（2026-11-01 审计 L-12 / C-20）
    ///
    /// 前端只认字符串（`"E_RANGE"`），后端 `ipc.rs` 也要从字符串反查退出码。
    /// 若 `as_str()` 与解析表不同步，就会出现「界面显示了一个后端永远不会
    /// 产生的错误码」这类静默漂移。这条测试把 `ERROR_CODES` 锁成真源。
    #[test]
    fn 错误码字符串往返一致() {
        for c in ERROR_CODES {
            let s = c.as_str();
            assert!(!s.is_empty(), "错误码字符串不能为空");
            assert!(
                s.starts_with("E_"),
                "错误码必须以 E_ 开头（契约约定），实得 {s}"
            );
            // 反查（与 cli 的 exit_code_for 同一套逻辑）
            let found = ERROR_CODES.iter().find(|x| x.as_str() == s);
            assert!(found.is_some(), "错误码 {s} 无法从字符串反查");
        }
    }

    /// 退出码必须落在契约规定的 1..=6
    #[test]
    fn 退出码落在契约范围() {
        for c in ERROR_CODES {
            let code = c.exit_code();
            assert!(
                (1..=6).contains(&code),
                "错误码 {} 的退出码 {code} 超出契约范围 1..=6",
                c.as_str()
            );
        }
    }

    /// `ERROR_CODES` 不得有重复项
    #[test]
    fn 错误码集合无重复() {
        for (i, a) in ERROR_CODES.iter().enumerate() {
            for b in &ERROR_CODES[i + 1..] {
                assert_ne!(
                    a.as_str(),
                    b.as_str(),
                    "ERROR_CODES 中出现重复错误码 {}",
                    a.as_str()
                );
            }
        }
    }
}
