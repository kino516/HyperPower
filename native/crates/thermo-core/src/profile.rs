//! 设备能力边界（来自检测得到的设备档案）
//!
//! # 设计要点
//!
//! - 本模块只承载**控制所需的能力边界**（客观事实），不含展示信息
//! - 与控制配置（`Config`，主观选择）严格分离：
//!   `Config` 是"想要什么"，`Capabilities` 是"最多能给什么"
//! - 所有控制参数都必须落在边界内 —— 见 [`Config::validate_against`]
//!
//! 完整档案结构见 `docs/07-契约-检测与设备档案.md`。

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// 档位表中的一行
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GearRow {
    /// 档位编号
    pub gear: u8,
    /// 该档位的电流上限（μA）
    pub current_limit_ua: i32,
}

/// 档位能力
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GearCapability {
    pub min: u8,
    pub max: u8,
    /// 最松档位（不限速）
    pub open: u8,
    /// 最紧档位（安全值）
    pub safe: u8,
    /// 档位 → 电流上限（设备树直读，可能不完整）
    #[serde(default)]
    pub table: Vec<GearRow>,
}

impl GearCapability {
    /// 取某档的电流上限（μA）；表中缺失时返回 `None`
    pub fn current_limit_ua(&self, gear: u8) -> Option<i32> {
        self.table
            .iter()
            .find(|r| r.gear == gear)
            .map(|r| r.current_limit_ua)
    }

    /// "放开"档位对应的电流上限（即不限速时的理论上限）
    pub fn open_current_ua(&self) -> Option<i32> {
        self.current_limit_ua(self.open)
    }
}

/// 电流能力
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurrentCapability {
    pub min_ua: i32,
    /// 实测上界
    pub max_ua: i32,
    /// 安全值（收紧时使用）
    pub safe_ua: i32,
}

/// 功率能力（**控制的核心约束**）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerCapability {
    /// 整机最大充电功率（W）；未实测时为 `None`
    pub max_w: Option<u16>,
    /// 功率最大的档位
    pub max_power_gear: Option<u8>,
    /// 充电协议（如 `PD_PPS`）
    pub protocol: Option<String>,
}

/// 温度能力
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThermalCapability {
    /// 硬件保护温度（℃）—— 用户阈值不得超过它
    pub hardware_max_c: f32,
    /// 模块硬上限（℃）—— 不可绕过
    pub abs_max_c: f32,
}

/// SOC 上限能力
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SocLimitCapability {
    pub supported: bool,
    pub min: u8,
    pub max: u8,
}

/// 设备能力边界（控制的一切依据）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub gear: GearCapability,
    pub current: CurrentCapability,
    pub power: PowerCapability,
    pub thermal: ThermalCapability,
    #[serde(default)]
    pub soc_limit: Option<SocLimitCapability>,
    /// 是否为已验证机型（未验证时需用户确认后才允许控制）
    #[serde(default)]
    pub verified: bool,
    /// 本机是否**跑过检测**
    ///
    /// # ⚠️ 这是"要不要控制"的总闸门
    ///
    /// `false` ⇒ 决策层**零写入**（详见 `decision::evaluate_with` 的门禁）。
    ///
    /// 用户明确要求："在使用模块之前未进行测试的，不要进行任何控制行为"。
    /// 因为未检测时的边界是**兜底值**，用它控制真实硬件必然出错：
    ///
    /// | 兜底值 | 后果 |
    /// |---|---|
    /// | 18.4A（K90PM 实测值） | 67W 机型**超限** |
    /// | 5A（保守猜测） | 用户什么都没设却被**限到约 22W** |
    #[serde(default)]
    pub detected: bool,
}

impl Default for Capabilities {
    /// **未知边界**（无任何档案时使用）
    ///
    /// # ⚠️ 为什么这里必须是"不限制"而不是"保守小值"
    ///
    /// 初版这里写的是 K90 Pro Max 的实测值（18.4 A）。改成"保守值 5 A"
    /// 之后真机立刻暴露了问题：
    ///
    /// | 环节 | 结果 |
    /// |---|---|
    /// | 用户装完模块，**尚未跑检测** | 档案 = 本默认值（5 A） |
    /// | 插上充电器，模块"正常放行" | 写 `curr_max` = 5 A |
    /// | 实际后果 | **充电被限到约 22 W**，用户什么都没设却变慢了 |
    ///
    /// 也就是说：`curr_max` 不只是"上限"，它还是**正常放行时写入节点的值**。
    /// 给它一个保守小值，等于"未检测就主动限速"。
    ///
    /// 正确的语义是：**不知道本机上限时就不去限制**（交给驱动自身的保护），
    /// 而不是用一个猜的值去限制用户。
    ///
    /// 因此这里取**物理上限**（30 A）—— 任何机型都不会超，
    /// 驱动会把它钳到自己的最大值，等价于"不干预"。
    ///
    /// 真正的机型边界由用户跑一次检测得到（见 `docs/13`）。
    fn default() -> Self {
        Self {
            gear: GearCapability {
                min: 0,
                max: 15,
                open: 0,
                safe: 15,
                table: Vec::new(),
            },
            current: CurrentCapability {
                min_ua: 0,
                // 物理上限 —— 语义是"不限制"，不是"限制到 30A"
                max_ua: crate::config::CURR_MAX_UA,
                safe_ua: 700_000,
            },
            power: PowerCapability {
                max_w: None,
                max_power_gear: None,
                protocol: None,
            },
            thermal: ThermalCapability {
                hardware_max_c: 45.0,
                abs_max_c: 45.0,
            },
            soc_limit: None,
            verified: false,
            // 兜底边界 ≠ 本机实测 ⇒ 默认**未检测**（不允许控制）
            detected: false,
        }
    }
}

impl Capabilities {
    /// 是否为未验证机型（需用户确认）
    pub fn is_unverified(&self) -> bool {
        !self.verified
    }

    /// 目标功率是否在能力范围内
    ///
    /// `max_w == None`（未实测）时返回 `Ok(true)` —— 由调用方决定是否额外提示。
    pub fn power_within(&self, target_w: u16) -> bool {
        self.power.max_w.is_none_or(|max| target_w <= max)
    }

    /// 电流是否在能力范围内
    pub fn current_within(&self, curr_ua: i32) -> bool {
        curr_ua >= self.current.min_ua && curr_ua <= self.current.max_ua
    }

    /// 档位是否在能力范围内
    pub fn gear_within(&self, gear: u8) -> bool {
        gear >= self.gear.min && gear <= self.gear.max
    }

    /// 温度阈值是否合法
    pub fn temp_within(&self, temp_c: f32) -> bool {
        temp_c <= self.thermal.hardware_max_c
    }
}

/// 边界校验错误的具体原因（用于给出人话提示）
#[derive(Debug, Clone, PartialEq)]
pub enum BoundaryViolation {
    /// 目标功率超过设备最大功率
    Power { target: u16, max: u16 },
    /// 电流超过设备上限
    Current { target: i32, max: i32 },
    /// 档位超出设备范围
    Gear { target: u8, max: u8 },
    /// 温度阈值超过硬件保护温度
    Temp { target: f32, max: f32 },
    /// SOC 上限超出支持范围
    Soc { target: u8, min: u8, max: u8 },
}

impl std::fmt::Display for BoundaryViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Power { target, max } => {
                write!(f, "目标功率 {target}W 超过设备最大功率 {max}W")
            }
            Self::Current { target, max } => {
                write!(f, "电流 {target} μA 超过设备上限 {max} μA")
            }
            Self::Gear { target, max } => {
                write!(f, "档位 {target} 超出设备范围（最大 {max}）")
            }
            Self::Temp { target, max } => {
                write!(f, "温度阈值 {target}℃ 超过硬件保护温度 {max}℃")
            }
            Self::Soc { target, min, max } => {
                write!(f, "SOC 上限 {target}% 超出支持范围 {min}~{max}%")
            }
        }
    }
}

impl From<BoundaryViolation> for CoreError {
    fn from(v: BoundaryViolation) -> Self {
        CoreError::OutOfBounds(v.to_string())
    }
}
