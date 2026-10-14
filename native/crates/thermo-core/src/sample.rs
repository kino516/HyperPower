//! 采样输入与充电状态

use serde::{Deserialize, Serialize};

/// 充电状态（源自 `power_supply` 的 `status` 属性）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargingStatus {
    /// 充电中 —— 全速运行
    Charging,
    /// 已充满 —— 全速运行
    Full,
    /// 放电中 —— 待机
    Discharging,
    /// 已连接但未充电 —— 待机
    NotCharging,
    /// 未知 —— 按待机处理（fail-closed）
    Unknown,
}

impl ChargingStatus {
    /// 是否需要全速运行（充电判定）
    pub fn is_active(self) -> bool {
        matches!(self, Self::Charging | Self::Full)
    }

    /// 从 `sysfs` 的字符串解析；未知值一律归为 `Unknown`
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "Charging" => Self::Charging,
            "Full" => Self::Full,
            "Discharging" => Self::Discharging,
            "Not charging" => Self::NotCharging,
            _ => Self::Unknown,
        }
    }
}

/// 一次采样
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// 电池温度（℃）
    pub temp_c: f32,
    /// 机身/虚拟温度（℃），可能缺失
    pub virt_c: Option<f32>,
    /// 电池电流的**大小**（μA，非负）
    ///
    /// # 为什么约定为非负
    ///
    /// 内核的符号约定**因方向而异**，且与直觉相反 —— 真机实测
    /// （K90 Pro Max / Android 17）：
    ///
    /// | 状态 | `current_now` |
    /// |---|---|
    /// | **充电中** | **负数**（如 `-9599000`） |
    /// | 放电中 | 正数 |
    ///
    /// 若把原始带符号值透传进来，每个消费方都得自己记得取绝对值 ——
    /// 漏一处就会算错（`detect.rs` 曾因此报"明明在充电却未实测"）。
    /// 因此采样层**统一规范化**为非负值，充放电方向改由 [`Self::status`] 表达。
    pub current_ua: i32,
    /// 电池电压（μV）
    pub voltage_uv: i32,
    /// 电量（%）
    pub soc: u8,
    /// 充电状态
    pub status: ChargingStatus,
}

impl Sample {
    /// 瞬时功率（W）
    ///
    /// 由电流与电压推算；电流取绝对值（充放电均可表达）。
    pub fn power_w(&self) -> f32 {
        let cur = f64::from(self.current_ua.abs());
        let vol = f64::from(self.voltage_uv.abs());
        ((cur * vol) / 1e12) as f32
    }
}
