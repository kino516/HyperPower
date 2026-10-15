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

impl Phase {
    /// 中文名 —— 用于**给用户看**的地方（事件消息、界面文案）
    ///
    /// # 为什么要有它（2026-10-16 用户反馈）
    ///
    /// 事件消息原先写的是 `format!("{:?} → {:?}", prev, now)` ——
    /// 那是 Rust 的 **Debug 格式**，于是用户在「最近事件」里看到的是：
    ///
    /// ```text
    /// Normal → Tightening
    /// ```
    ///
    /// 用户原话：「最近事件内的英文显示全部转换成中文」。
    ///
    /// 项目里其它事件消息**本来就是中文**（"已交回官方控制"、"充电会话已归档"、
    /// "触发硬保护"），只有这一条漏了 —— 因为枚举没有中文名可用，
    /// 就顺手用了 Debug。现在补上，两处都对齐。
    ///
    /// ⚠️ **不要**改 `{:?}` 的输出本身（`Debug` 是给开发者的，日志与
    ///    序列化都依赖它保持英文稳定）。要中文就调这个函数。
    ///
    /// ⚠️ 措辞必须与前端 `format.ts::stateLabel` / `LogsPage::phaseLabel`
    ///    **逐字一致** —— 同一个状态在两处显示不同说法会让用户以为
    ///    是两个东西。前端用的是「正在限速」，这里就跟着用。
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "待机",
            Self::Normal => "正常放行",
            Self::Tightening => "正在限速",
            Self::HardProtect => "硬保护",
        }
    }
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
