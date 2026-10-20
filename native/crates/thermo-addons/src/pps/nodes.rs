//! PPS 相关节点的候选表（**路径因机型/平台而异**）
//!
//! # 这份表是怎么定出来的
//!
//! 方案原文给的多是小米封装层（`xm_power`）路径，但 2026-10-20 在
//! Redmi K70 Pro 上实测发现：**该机型根本没有 `xm_power`**，
//! 所有 PPS 节点都在 `qcom-battery` 下。
//!
//! | 节点 | K70 Pro 实际位置 | 状态 |
//! |---|---|---|
//! | `pd_verifed` | `qcom-battery/pd_verifed` | ✅ 可写（`-rw-rw-r-- root:root`） |
//! | `real_type` | `qcom-battery/real_type` | ✅ 只读 |
//! | `adapter_svid` | `qcom-battery/adapter_svid` | ✅ 只读（公版 PPS 头 = `0000`） |
//! | `apdo_max` | `qcom-battery/apdo_max` | ✅ 只读 |
//! | USB 电压 | `power_supply/usb/voltage_now` | ✅ 只读 |
//!
//! ⇒ 因此候选顺序统一为 **`qcom-battery` → `xm_power` → 标准 `power_supply`**：
//! 先试语义最明确、覆盖面最广的上游/高通接口，再退小米封装层。
//!
//! ⚠️ 每项都**必须**运行期探测，不要写死单一路径 —— 与 `soc_limit`
//!    同样的纪律（硬编码路径会让部分机型直接失效）。

/// **PD 认证位**（可写）—— 本功能唯一会写的节点
///
/// | 平台 | 路径 |
/// |---|---|
/// | qcom | `qcom-battery/pd_verifed` |
/// | mtk | `Charging_Adapter/pd_adapter/usbpd_verifed` |
///
/// 写 `1` = 解锁（跳过 PD 认证），写 `0` = 还原官方状态。
///
/// ⚠️ 实测（K70 Pro）：写入**诚实** —— 写 1 读回 1、写 0 读回 0、
///    非法值（`2` / `abc`）被驱动明确拒绝。因此可以放心用回读校验，
///    不像 `charge_control_end_threshold` 那样会"表面成功实际被吞"。
pub const PD_VERIFIED: &[&str] = &[
    // qcom（K70 Pro 命中）
    "/sys/class/qcom-battery/pd_verifed",
    // mtk
    "/sys/class/Charging_Adapter/pd_adapter/usbpd_verifed",
];

/// 当前充电协议类型（只读）
///
/// 取值形如 `PD_PPS` / `PD` / `USB` / `Unknown`。
/// `auto` 模式据此判断"本来就走 PPS 才解锁"。
pub const REAL_TYPE: &[&str] = &[
    "/sys/class/qcom-battery/real_type",
    "/sys/class/qcom-battery/usb_real_type",
    "/sys/class/xm_power/charger/charger_common/real_type",
    "/sys/class/power_supply/usb/real_type",
];

/// PD 适配器的 SVID（只读，十六进制字符串）
///
/// # 为什么它是广播门控的关键
///
/// | 值 | 含义 |
/// |---|---|
/// | `0000` | **公版 PPS 头** |
/// | 非 `0000` | 厂商私有（小米原装头走 MIPPS） |
///
/// 只对公版头发"金标"才不会误导用户 —— 原装头本来就有自己的动画。
pub const ADAPTER_SVID: &[&str] = &[
    "/sys/class/qcom-battery/adapter_svid",
    "/sys/class/xm_power/typec/strategy_pd_auth/adapter_svid",
    "/sys/class/power_supply/usb/adapter_svid",
];

/// 充电器能提供的最高功率（W，只读）
///
/// 广播时决定要不要显示"满血"：`>= 90` 才值得喊 100W。
pub const APDO_MAX: &[&str] = &[
    "/sys/class/qcom-battery/apdo_max",
    "/sys/class/xm_power/typec/apdo_max",
    "/sys/class/qcom-battery/power_max",
];

/// USB 侧电压（μV，只读）—— 广播门控用
///
/// ⚠️ 用 **usb** 侧而非 battery 侧：门控关心的是"充电器此刻给的是几伏"，
///    battery 侧是电芯电压（约 4 V），两者差一个数量级。
pub const USB_VOLTAGE_NOW: &[&str] = &["/sys/class/power_supply/usb/voltage_now"];

/// 电池状态（只读）—— 会话状态机用
pub const BATTERY_STATUS: &[&str] = &["/sys/class/power_supply/battery/status"];

/// 探测到的平台
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// 高通（`qcom-battery` 或小米封装层）
    Qcom,
    /// 联发科（`Charging_Adapter`）
    Mtk,
}

impl Platform {
    /// 供界面显示的短标签
    pub fn label(self) -> &'static str {
        match self {
            Self::Qcom => "qcom",
            Self::Mtk => "mtk",
        }
    }

    /// 由命中的 `pd_verifed` 路径反推
    ///
    /// ⚠️ 按**路径**判定而非"目录里有某个文件夹"：将来若某机型把
    ///    节点搬到别的目录，这里依然正确。
    fn of(path: &str) -> Self {
        if path.contains("/Charging_Adapter/") {
            Self::Mtk
        } else {
            Self::Qcom
        }
    }
}

/// 探测结果：本机可用的节点集合
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nodes {
    /// 平台
    pub platform: Platform,
    /// **可写**的认证位（本功能的核心）
    pub pd_verified: String,
    /// 协议类型（只读；找不到则 `None`）
    pub real_type: Option<String>,
    /// 适配器 SVID（只读）
    pub adapter_svid: Option<String>,
    /// 最高功率（只读）
    pub apdo_max: Option<String>,
    /// USB 电压（只读）
    pub usb_voltage: Option<String>,
    /// 电池状态（只读）
    pub battery_status: Option<String>,
}

impl Nodes {
    /// 在候选表里找第一个**存在**的路径
    fn first_existing(io: &mut dyn crate::io::AddonIo, cands: &[&str]) -> Option<String> {
        cands
            .iter()
            .find(|p| io.exists(p))
            .map(|p| (*p).to_string())
    }

    /// 探测本机节点（**只读**：`exists()` 只查元信息，不读内容）
    ///
    /// 返回 `None` = 本机没有可写的认证位（既非 qcom 也非 mtk）⇒ 功能不支持。
    pub fn detect(io: &mut dyn crate::io::AddonIo) -> Option<Self> {
        // 核心：必须有一个**可写**的认证位，否则整个功能无从谈起
        let pd_verified = PD_VERIFIED
            .iter()
            .find(|p| io.writable(p))
            .map(|p| (*p).to_string())?;

        Some(Self {
            platform: Platform::of(&pd_verified),
            pd_verified,
            real_type: Self::first_existing(io, REAL_TYPE),
            adapter_svid: Self::first_existing(io, ADAPTER_SVID),
            apdo_max: Self::first_existing(io, APDO_MAX),
            usb_voltage: Self::first_existing(io, USB_VOLTAGE_NOW),
            battery_status: Self::first_existing(io, BATTERY_STATUS),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn 平台按路径判定() {
        assert_eq!(
            Platform::of("/sys/class/qcom-battery/pd_verifed"),
            Platform::Qcom
        );
        assert_eq!(
            Platform::of("/sys/class/xm_power/charger/pd_verifed"),
            Platform::Qcom,
            "小米封装层也算 qcom 平台"
        );
        assert_eq!(
            Platform::of("/sys/class/Charging_Adapter/pd_adapter/usbpd_verifed"),
            Platform::Mtk
        );
    }

    #[test]
    fn 候选表都以绝对路径开头() {
        for table in [
            PD_VERIFIED,
            REAL_TYPE,
            ADAPTER_SVID,
            APDO_MAX,
            USB_VOLTAGE_NOW,
            BATTERY_STATUS,
        ] {
            for p in table {
                assert!(p.starts_with("/sys/"), "必须是绝对路径：{p}");
            }
        }
    }

    /// 防回归：`pd_verifed` 的候选必须覆盖 qcom 与 mtk 两个平台
    #[test]
    fn 认证位覆盖两个平台() {
        let joined = PD_VERIFIED.join(" ");
        assert!(joined.contains("qcom-battery"), "缺 qcom 候选");
        assert!(joined.contains("Charging_Adapter"), "缺 mtk 候选");
    }
}
