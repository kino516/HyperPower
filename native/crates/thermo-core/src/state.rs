//! 决策状态（跨拍保持，由 I/O 层负责持久化）

use serde::{Deserialize, Serialize};

/// 控制阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 未介入（交回官方）
    #[default]
    Idle,
    /// 正常放行（未达收紧温度）
    Normal,
    /// 降速中
    Tightening,
    /// 硬保护（两层收紧）
    HardProtect,
}

/// 跨拍状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// 当前阶段
    pub phase: Phase,
    /// 当前档位
    pub gear: u8,
    /// 当前电流（μA）
    pub curr_ua: i32,
    /// 收紧期峰值温度（℃）；仅在收紧期内有值
    pub peak_c: Option<f32>,
    /// 超限已持续的拍数
    pub over_temp_ticks: u32,
    /// 连续读取失败次数
    pub read_failures: u32,
    /// 连续写入失败次数
    pub write_failures: u32,
    /// 上一拍是否处于收紧状态（用于迟滞）
    pub tightening: bool,
    /// 功率闭环当前算出的电流上限（μA）
    ///
    /// # 为什么需要跨拍记住它
    ///
    /// 功率闭环是**比例校正**：每拍基于上一拍的设定值按实测功率误差调整。
    /// 没有这个记忆，每拍都会从零开始推算，误差无法收敛。
    ///
    /// `None` = 本会话尚未限制过功率（首拍会用前馈公式起步）。
    #[serde(default)]
    pub power_cap_ua: Option<i32>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            gear: 0,
            curr_ua: 0,
            peak_c: None,
            over_temp_ticks: 0,
            read_failures: 0,
            write_failures: 0,
            tightening: false,
            power_cap_ua: None,
        }
    }
}

impl State {
    /// 重置为未介入状态
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}
