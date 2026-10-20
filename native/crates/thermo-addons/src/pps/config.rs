//! PPS 解锁的配置分区（`config.json` 的 `pps` 段）
//!
//! 契约见 `ThermoCharge-PPS移植方案.md` §5。
//!
//! # 为什么单独一个分区
//!
//! 与 `limit`（充电上限）平级、**互不依赖**：PPS 的配置写坏只让 PPS 停用，
//! 充电上限与温控都不受影响（见 `registry::build_all` 的分区独立解析）。

use serde::{Deserialize, Serialize};

use crate::addon::AddonSpec;

/// 配置分区名（`config.json` 的键）
pub const SECTION: &str = "pps";

/// 界面显示的满血功率下限（W）
pub const POWER_MIN_W: u16 = 30;
/// 界面显示的满血功率上限（W）
pub const POWER_MAX_W: u16 = 120;

/// 广播门控电压下限的合法范围（μV）：0 ~ 20 V
pub const MIN_VBUS_MAX_UV: i64 = 20_000_000;

/// 巡检间隔下限（秒）
pub const VERIFY_MIN_S: u32 = 5;
/// 巡检间隔上限（秒）
pub const VERIFY_MAX_S: u32 = 300;

/// 解锁模式
///
/// FreePPS 的 `free` 软开关只有"开/关"两态，这里保留两态语义，
/// 但把"什么时候写 1"拆成两种策略 —— 因为 `lock` 有副作用
/// （可能让小米原装头失去 MIPPS 私有协议路径，见 [`WARN_MIPPS`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PpsMode {
    /// **始终锁定**：不管插什么头都写 `pd_verifed=1`
    ///
    /// 对应 FreePPS 的默认行为。效果最彻底，但公版头之外的头
    /// （含小米原装头）也会被解锁。
    #[default]
    Lock,
    /// **自动**：只在 `real_type` 已是 PPS 类协议时才写 `1`
    ///
    /// 更保守：不影响原装头的私有协议路径，但对"原本没走 PPS 的头"
    /// 不起作用。
    Auto,
}

/// 宽松反序列化：大小写不敏感，未知值回落默认
///
/// # 为什么不用 `#[serde(rename_all)]` 直接派生
///
/// `rename_all = "lowercase"` 只接受**全小写**的 `"auto"`，
/// 而用户在配置文件里手写 `"AUTO"` / `"Auto"` 是很自然的事 ——
/// 那样会解析失败 ⇒ 按 fail-closed 规则整个功能被停用。
///
/// 一个大小写错误就让功能消失，代价太大。这里改成：
/// 认得出的值就解析，认不出的**回落默认并继续工作**。
///
/// ⚠️ 真正的 fail-closed 只留给"会写坏硬件"的取值
///    （如越界功率，见 [`PpsConfig::validate`]）。
impl<'de> Deserialize<'de> for PpsMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

impl PpsMode {
    /// 配置值 → 枚举（大小写不敏感，未知值回落默认）
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            _ => Self::Lock,
        }
    }

    /// 供界面显示的中文名
    pub fn label(self) -> &'static str {
        match self {
            Self::Lock => "锁定",
            Self::Auto => "自动",
        }
    }
}

/// **MIPPS 冲突警示**（界面与 `addon show` 共用同一份文案）
///
/// # 为什么必须警示
///
/// 小米原装充电头走的是**私有协议（MIPPS）**，`pd_verifed` 被强制置 1 后，
/// 驱动可能改走标准 PD/PPS 路径 —— 结果**功率反而下降**（原装头能给的
/// 私有档位拿不到了）。
///
/// FreePPS 的实测结论：这是真实风险，不是理论担忧。因此：
/// - 默认**不启用**（`enabled: false`）
/// - 界面上必须显示这段警示
/// - 提供 `auto` 模式作为更保守的选择
pub const WARN_MIPPS: &str =
    "锁定 PPS 可能让小米原装头失去私有快充协议（MIPPS），导致功率反而下降。\
     用原装头时若发现变慢，请关闭本功能或改用「自动」模式";

/// **需要重新插拔**的说明
pub const NOTE_REPLUG: &str = "修改后需插拔一次充电器才会生效（协议握手在插电时进行）";

/// PPS 解锁配置
///
/// 字段缺失时一律取 [`Default`]（升级/降级不因缺键而拒绝启动）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PpsConfig {
    /// 总开关（对应 `addon on/off pps`）
    ///
    /// **出厂默认 `false`**：本功能会改变充电协议协商，必须由用户主动开启。
    pub enabled: bool,
    /// 解锁模式
    pub mode: PpsMode,
    /// 是否伪造"金标"快充动画（仅 qcom 平台，M2 阶段实现）
    pub forge_broadcast: bool,
    /// 满血功率显示值（W）—— `apdo_max >= 90` 时用于广播
    pub power_display_w: u16,
    /// 广播门控电压（μV）：低于此值不发广播
    ///
    /// 默认 12 V：PPS 的典型高压档才有意义，5 V 档发金标会误导用户。
    pub min_vbus_uv: i64,
    /// 充电中巡检间隔（秒）
    ///
    /// 比充电上限宽（默认 30 s）：PPS 的状态变化不像 SOC 那样频繁，
    /// 且每次巡检都是一次 sysfs 读取（打断 CPU 深睡）。
    pub verify_s: u32,
}

impl Default for PpsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: PpsMode::Lock,
            forge_broadcast: true,
            power_display_w: 100,
            min_vbus_uv: 12_000_000,
            verify_s: 30,
        }
    }
}

impl PpsConfig {
    /// 从 `config.json` 的 `pps` 分区解析（**独立容错**）
    ///
    /// | 情况 | 行为 |
    /// |---|---|
    /// | 分区缺失 / 为 `null` | 取默认值（关闭），**不算错误** |
    /// | 字段缺失 | 取该字段默认值 |
    /// | 类型错误 | 返回 `Err` —— 调用方应停用该功能（fail-closed） |
    pub fn from_section_opt(section: Option<&serde_json::Value>) -> Result<Self, String> {
        match section {
            None | Some(serde_json::Value::Null) => Ok(Self::default()),
            Some(v) => serde_json::from_value::<Self>(v.clone())
                .map_err(|e| format!("`{SECTION}` 分区解析失败：{e}")),
        }
    }

    /// 校验取值是否合法（fail-closed：不合法则拒绝应用）
    pub fn validate(&self) -> Result<(), String> {
        if !(POWER_MIN_W..=POWER_MAX_W).contains(&self.power_display_w) {
            return Err(format!(
                "满血显示功率 {}W 超出范围（{POWER_MIN_W}~{POWER_MAX_W}W）",
                self.power_display_w
            ));
        }
        if !(0..=MIN_VBUS_MAX_UV).contains(&self.min_vbus_uv) {
            return Err(format!(
                "广播门控电压 {}μV 超出范围（0~{MIN_VBUS_MAX_UV}μV）",
                self.min_vbus_uv
            ));
        }
        if !(VERIFY_MIN_S..=VERIFY_MAX_S).contains(&self.verify_s) {
            return Err(format!(
                "巡检间隔 {}s 超出范围（{VERIFY_MIN_S}~{VERIFY_MAX_S}s）",
                self.verify_s
            ));
        }
        Ok(())
    }

    /// 当前生效的巡检间隔
    pub fn interval_s(&self) -> u32 {
        self.verify_s.clamp(VERIFY_MIN_S, VERIFY_MAX_S)
    }
}

/// 功能自我介绍（界面与 CLI 共用）
pub fn spec(config: &PpsConfig) -> AddonSpec {
    AddonSpec {
        id: "pps",
        label: "PPS 解锁",
        summary: "绕过 PD 认证，让第三方充电头也能触发快充；可选伪造金标动画",
        // ⚠️ 不指名任何第三方模块（品牌洁癖，见 docs/08）
        origin: "节点机制由真机实测确证：写 qcom-battery/pd_verifed，读 real_type 与 adapter_svid 判断协议",
        default_interval_s: config.interval_s(),
        requires: Some(WARN_MIPPS),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    #[test]
    fn 缺失分区取默认且关闭() {
        let c = PpsConfig::from_section_opt(None).unwrap();
        assert!(!c.enabled, "出厂必须关闭");
        assert_eq!(c.mode, PpsMode::Lock);
        assert_eq!(c.power_display_w, 100);
        assert_eq!(c.min_vbus_uv, 12_000_000);
        assert_eq!(c.verify_s, 30);
    }

    #[test]
    fn 类型错误返回错误() {
        let bad = json!({ "enabled": "是的" });
        assert!(PpsConfig::from_section_opt(Some(&bad)).is_err());
    }

    #[test]
    fn 字段缺失取默认() {
        let c = PpsConfig::from_section_opt(Some(&json!({ "enabled": true }))).unwrap();
        assert!(c.enabled);
        assert_eq!(c.verify_s, 30, "未给的字段走默认");
    }

    #[test]
    fn 模式解析宽松() {
        let c = PpsConfig::from_section_opt(Some(&json!({ "mode": "AUTO" }))).unwrap();
        assert_eq!(c.mode, PpsMode::Auto);
        // 拼错的值不该让功能停用
        let c2 = PpsConfig::from_section_opt(Some(&json!({ "mode": "啥" }))).unwrap();
        assert_eq!(c2.mode, PpsMode::Lock, "未知值回落默认");
    }

    #[test]
    fn 越界配置被拒绝() {
        let c = PpsConfig {
            power_display_w: 500,
            ..PpsConfig::default()
        };
        assert!(c.validate().is_err());

        let c = PpsConfig {
            min_vbus_uv: 99_000_000,
            ..PpsConfig::default()
        };
        assert!(c.validate().is_err());

        let c = PpsConfig {
            verify_s: 1,
            ..PpsConfig::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn 默认配置合法() {
        assert!(PpsConfig::default().validate().is_ok());
    }

    #[test]
    fn 区间被钳位() {
        let c = PpsConfig {
            verify_s: 9999,
            ..PpsConfig::default()
        };
        assert_eq!(c.interval_s(), VERIFY_MAX_S);
    }
}
