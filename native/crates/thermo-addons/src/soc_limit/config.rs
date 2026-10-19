//! 充电上限功能的配置（`config.json` 的 `limit` 分区）
//!
//! 契约见 `docs/04-契约-配置说明书.md` §8。
//!
//! # 与温控配置的关系
//!
//! **同一个文件、各解析各的**（设计 §6）：温控内核的结构里**根本没有** `limit` 字段，
//! 因此它连"充电上限有配置"这件事都看不见；反过来本功能也只读自己的分区。

use serde::{Deserialize, Serialize};

/// 配置分区名（`config.json` 里的键）
pub const SECTION: &str = "limit";

// ============================================================
//  SOC 值域
// ============================================================
//
// # ⚠️ 三层范围，不要混用
//
// | 层 | 来源 | 用途 |
// |---|---|---|
// | **物理范围** | 常量 `SOC_PHYSICAL_*` | 拒绝荒谬输入（如 5% / 200%） |
// | **兜底范围** | 常量 `SOC_FALLBACK_*` | 检测尚未跑过时的保守取值 |
// | **本机范围** | 检测档案 `capabilities.soc_limit.{min,max}` | **实际校验依据** |
//
// 初版只有一个硬编码的 `60 ~ 100`（本机 K90PM 的值），换机型就会
// 用错范围。现在实际校验走**本机范围**（由宿主注入，见
// [`SocLimit::set_range`](super::SocLimit::set_range)）。

/// 物理下限（%）：低于此值对"充电上限"没有意义
pub const SOC_PHYSICAL_MIN: u8 = 20;
/// 物理上限（%）
pub const SOC_PHYSICAL_MAX: u8 = 100;

/// 兜底下限（%）—— 检测未跑过时使用
///
/// # 55 不是随便定的（2026-10-19 真机证据）
///
/// K70 Pro 的驱动在拒绝越界写入时，dmesg 会明确打出允许区间：
///
/// ```text
/// BATTERY_CHG: battery_psy_set_charge_end_threshold:
///              Charge control end_threshold should be within [55 100]
/// ```
///
/// 也就是说标准 `power_supply` 系的**下限是 55**，不是老机型
/// （小米 `smart_chg`）那套的 60。
///
/// | 值 | 后果 |
/// |---|---|
/// | 写 50 | 被驱动拒绝（`write: Invalid argument`） |
/// | 写 0 | 同样被拒绝 —— **0 不是"不限制"，而是非法值** |
///
/// ⚠️ 取 55 而不是 60：给到 60 会把"55~59"这段本来能用的区间白丢掉，
///    用户想设 58% 会被界面直接挡住。宁可放开，越界时驱动会如实报错。
/// ⚠️ 不同机型下限可能不同（有的 50、有的 60）。这里只是**检测未跑过
///    时的兜底**；检测跑过之后用档案里的值。
pub const SOC_FALLBACK_MIN: u8 = 55;
/// 兜底上限（%）
pub const SOC_FALLBACK_MAX: u8 = 100;

/// 系统自带智能断充的默认上限（%）
///
/// 模块功能关闭时，系统就按这个值工作；开启后若不指定，也默认用它。
pub const SYS_DEFAULT_SOC: u8 = 80;

/// 校验间隔下限（秒）
pub const VERIFY_MIN_S: u32 = 5;
/// 校验间隔上限（秒）
pub const VERIFY_MAX_S: u32 = 300;

/// 充电上限功能的配置
///
/// 字段缺失时一律取 [`Default`]（升级/降级不因缺键而拒绝启动）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SocLimitConfig {
    /// 是否启用该功能
    ///
    /// **出厂默认 `false`**：模块对该节点**零写入**，系统自带的智能断充照常工作。
    pub enabled: bool,
    /// 目标上限（%）
    pub soc: u8,
    /// 校验间隔（秒）：对抗系统"事件驱动式"改写
    pub verify_s: u32,
    /// 温控"交回官方"时是否保留本上限
    pub keep_on_restore: bool,
    /// 检测到**其它也在写同一节点的充电控制模块**时是否提示用户
    ///
    /// # 为什么需要这个提示
    ///
    /// `smart_chg` 是一个**全局节点** —— 任何写它的程序都在改同一个开关。
    /// 两个程序同时启用时，谁都可能在后，于是**互相覆盖**：
    /// 用户会看到"设置过一会儿自己变回去了"这种莫名现象，
    /// 却不知道是有两个程序在抢。
    ///
    /// 因此检测到就明确提示，让用户**二选一**。
    ///
    /// ⚠️ 字段名保持中性（`conflict` 而不是指名道姓）—— 会写这个节点的
    ///    程序不止一个，写死某个名字将来必然过时。
    ///
    /// ⚠️ 缺省值由**容器级** `#[serde(default)]` 提供（走 `Default` = `true`），
    ///    这里不要再写字段级 `default` —— 那会覆盖成 `false`，
    ///    于是"老配置缺这个键"变成"关闭提示"，与 `Default` 不一致。
    pub warn_on_conflict: bool,
}

impl Default for SocLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            soc: SYS_DEFAULT_SOC,
            verify_s: 15,
            keep_on_restore: true,
            warn_on_conflict: true,
        }
    }
}

impl SocLimitConfig {
    /// 从 `config.json` 的 `limit` 分区解析（**独立容错**）
    ///
    /// # 规则
    ///
    /// | 情况 | 行为 |
    /// |---|---|
    /// | 分区缺失 / 为 `null` | 取默认值（即功能关闭），**不算错误** |
    /// | 字段缺失 | 取该字段默认值 |
    /// | 类型错误 | 返回 `Err` —— 调用方应**停用**该功能（fail-closed），不要拿坏配置去写节点 |
    pub fn from_section_opt(section: Option<&serde_json::Value>) -> Result<Self, String> {
        match section {
            None | Some(serde_json::Value::Null) => Ok(Self::default()),
            Some(v) => serde_json::from_value::<Self>(v.clone())
                .map_err(|e| format!("`{SECTION}` 分区解析失败：{e}")),
        }
    }

    /// 校验取值是否合法（fail-closed：不合法则拒绝应用）
    ///
    /// 用**兜底范围**（检测未跑过时的保守取值）。
    /// 已知本机范围时请用 [`validate_within`](Self::validate_within)。
    pub fn validate(&self) -> Result<(), String> {
        self.validate_within(SOC_FALLBACK_MIN, SOC_FALLBACK_MAX)
    }

    /// 用**指定的本机范围**校验
    ///
    /// `range` 来自检测档案的 `capabilities.soc_limit.{min,max}` ——
    /// 不同机型驱动的支持范围不同，因此必须按检测值校验。
    pub fn validate_within(&self, min: u8, max: u8) -> Result<(), String> {
        // 先做物理护栏（防注入的 range 本身荒谬）
        let (min, max) = (
            min.clamp(SOC_PHYSICAL_MIN, SOC_PHYSICAL_MAX),
            max.clamp(SOC_PHYSICAL_MIN, SOC_PHYSICAL_MAX),
        );
        // range 反了就退回兜底，而不是拒绝一切
        let (min, max) = if min <= max {
            (min, max)
        } else {
            (SOC_FALLBACK_MIN, SOC_FALLBACK_MAX)
        };

        if !(min..=max).contains(&self.soc) {
            return Err(format!(
                "上限 {}% 超出本机支持范围（{min}~{max}%）",
                self.soc
            ));
        }
        if !(VERIFY_MIN_S..=VERIFY_MAX_S).contains(&self.verify_s) {
            return Err(format!(
                "校验间隔 {}s 超出支持范围（{VERIFY_MIN_S}~{VERIFY_MAX_S}s）",
                self.verify_s
            ));
        }
        Ok(())
    }

    /// 每拍间隔（秒）：启用时按 `verify_s`，未启用时放慢（只需响应配置变更）
    pub fn interval_s(&self) -> u32 {
        if self.enabled {
            self.verify_s
        } else {
            IDLE_INTERVAL_S
        }
    }
}

/// 未启用时的巡检间隔（秒）
///
/// 不做事不代表不用醒来 —— 用户可能随时改配置（或经 UDS 通知），
/// 放慢到 60 秒看一眼即可，开销可忽略。
pub const IDLE_INTERVAL_S: u32 = 60;
