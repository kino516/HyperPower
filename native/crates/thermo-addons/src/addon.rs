//! 附加功能的统一接口（插槽）
//!
//! 设计见 `docs/09-设计-附加功能架构.md` §4。
//!
//! # 隔离纪律
//!
//! - 本接口**不出现任何温控概念**（没有档位、没有温度、没有 `Decision`）
//! - 附加功能想要什么数据，自己从节点读
//! - 禁止 panic：workspace lint 已 `deny` `unwrap_used` / `expect_used` / `panic`

use std::fmt;

use serde::Serialize;
use thermo_ipc::ErrorCode;

/// 附加功能的静态描述（"我是谁"）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddonSpec {
    /// 唯一标识：同时用作**配置分区名**与**命令参数名**（小写下划线）
    pub id: &'static str,
    /// 显示名（中文，给用户看）
    pub label: &'static str,
    /// 一句话说明
    pub summary: &'static str,
    /// 来源说明（含许可证相关信息）
    pub origin: &'static str,
    /// 默认巡检间隔（秒）
    pub default_interval_s: u32,
}

/// 能力探测结果
///
/// ⚠️ "本机不支持"**不是错误** —— 该功能停用即可，其余功能与温控照常。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// 本机支持
    Supported {
        /// 命中详情（例如命中的节点路径）
        detail: String,
    },
    /// 本机不支持
    Unsupported {
        /// 原因（人话，用于提示与事件记录）
        reason: String,
    },
}

impl ProbeOutcome {
    /// 是否支持
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported { .. })
    }

    /// 详情 / 原因文本（两者取其一）
    pub fn message(&self) -> &str {
        match self {
            Self::Supported { detail } => detail,
            Self::Unsupported { reason } => reason,
        }
    }
}

/// 附加功能的一次错误
///
/// 复用契约层既有的 15 个错误码（不新增退出码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonError {
    /// 错误码（决定 CLI 退出码）
    pub code: ErrorCode,
    /// 人话说明
    pub msg: String,
}

impl AddonError {
    /// 构造
    pub fn new(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
        }
    }

    /// 节点不存在 / 不可用
    pub fn node_missing(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::NoNode, msg)
    }

    /// 条件不满足（写失败、回读不符等）
    pub fn bad_cond(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadCond, msg)
    }

    /// 值越界
    pub fn out_of_range(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Range, msg)
    }
}

impl fmt::Display for AddonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.msg)
    }
}

/// 一拍的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    /// 本次是否写了节点
    pub wrote: bool,
    /// 状态是否发生变化（用于决定要不要记事件）
    pub changed: bool,
    /// 出错信息；`None` = 正常
    pub error: Option<AddonError>,
    /// 建议的下一拍间隔（秒）—— 由功能自己决定
    pub next_interval_s: u32,
}

impl TickOutcome {
    /// 什么都没做（含"未启用"与"已一致，无需写回"）
    pub fn idle(next_interval_s: u32) -> Self {
        Self {
            wrote: false,
            changed: false,
            error: None,
            next_interval_s,
        }
    }

    /// 成功写入了节点
    pub fn wrote(next_interval_s: u32) -> Self {
        Self {
            wrote: true,
            changed: true,
            error: None,
            next_interval_s,
        }
    }

    /// 失败（宿主会累计连续失败次数，达阈值触发熔断）
    pub fn failed(error: AddonError, next_interval_s: u32) -> Self {
        Self {
            wrote: false,
            changed: false,
            error: Some(error),
            next_interval_s,
        }
    }

    /// 是否正常
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// 对外状态快照（供 `limit show` / `addon list` 输出）
///
/// ⚠️ 刻意**不塞进温控的 `status` 响应** —— 那是冻结契约，且属于温控。
#[derive(Debug, Clone, Serialize)]
pub struct AddonStatus {
    /// 功能标识
    pub id: String,
    /// 显示名
    pub label: String,
    /// 一句话说明
    pub summary: String,
    /// 来源说明
    pub origin: String,
    /// 是否已启用（来自配置）
    pub enabled: bool,
    /// 是否已确认本机支持（来自探测）
    pub supported: bool,
    /// 是否已熔断（连续失败被自动停用）
    pub tripped: bool,
    /// 当前巡检间隔（秒）
    pub interval_s: u32,
    /// 探测详情
    pub probe: String,
    /// 各自的细节（由功能自定义，便于前端展示）
    pub detail: serde_json::Value,
}

/// 附加功能（插槽）
///
/// 实现这 5 个方法即可接入；**不要求、也不允许**触碰温控的任何东西。
pub trait Addon: fmt::Debug {
    /// 自我介绍
    fn spec(&self) -> AddonSpec;

    /// 探测本机是否支持（**只读，零副作用**）
    fn probe(&mut self, io: &mut dyn crate::io::AddonIo) -> ProbeOutcome;

    /// 跑一拍：看配置 + 看现状 → 决定要不要写节点
    ///
    /// `now_s` 为宿主提供的单调秒数（功能可用它做节流，但不必自己计时）。
    fn tick(&mut self, io: &mut dyn crate::io::AddonIo, now_s: i64) -> TickOutcome;

    /// 收尾：功能关闭 / 进程退出 / 交回官方时调用
    ///
    /// ⚠️ 对"停止干预即零写入"的功能（如充电上限），这里应当是**空操作**。
    fn release(&mut self, io: &mut dyn crate::io::AddonIo) -> TickOutcome;

    /// 对外状态快照
    fn status(&self) -> AddonStatus;

    /// 是否已启用（供宿主决定要不要跑）
    fn enabled(&self) -> bool;

    /// 是否已确认本机支持（供宿主决定要不要跑）
    fn supported(&self) -> bool;

    /// 取回具体类型以便推送配置变更（宿主用）
    ///
    /// 例：`slot.addon_mut().as_any_mut().downcast_mut::<SocLimit>()`
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}
