//! 响应数据形态（契约见 docs/05-契约-接口输出规范.md §5）

use serde::{Deserialize, Serialize};

/// 接口版本标识（当前 1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiVersion(pub u32);

impl Default for ApiVersion {
    fn default() -> Self {
        Self(1)
    }
}

/// 状态快照（`status` 的 `--plain` 与 JSON 共同来源）
///
/// ⚠️ 契约要点：
/// - `peak_c` 为**省略式字段**：仅在收紧期内出现，出现时必为数值
/// - `cfg_ver` **仅在 `--plain` 输出**，不出现在 JSON 中
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub ts: i64,
    pub state: String,
    pub daemon: String,
    pub battery: BatteryPart,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<ControlPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal: Option<ThermalPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionPart>,
    pub uptime_s: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryPart {
    pub temp_c: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soc: Option<u8>,
    pub power_w: f32,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlPart {
    pub gear_now: u8,
    pub curr_now: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power_limit_w: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duty_pct: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalPart {
    /// 收紧期峰值温度；**仅收紧期内出现**
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_c: Option<f32>,
    pub basis: String,
    pub tighten_temp: f32,
    pub abs_max: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPart {
    pub duration_s: u64,
}
