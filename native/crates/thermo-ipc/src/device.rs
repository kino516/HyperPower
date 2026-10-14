//! 设备档案（检测结果）
//!
//! 契约见 `docs/07-契约-检测与设备档案.md`。
//!
//! # 三层数据模型
//!
//! | 层 | 位置 | 生命周期 |
//! |---|---|---|
//! | 出厂档案 | `module/profiles/*.json` | 随模块分发，只读 |
//! | 本机档案 | `/data/adb/thermo_charge/device.json` | 检测后持久保存 |
//! | 实时数据 | 内存 | 每拍刷新，**永不落盘** |

use serde::{Deserialize, Serialize};

/// 档案 schema 版本（升级时递增，过新则拒绝加载）
pub const PROFILE_SCHEMA: u32 = 1;

/// 档案类型标记
pub const PROFILE_KIND: &str = "device_profile";

/// 完整设备档案
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceProfile {
    /// schema 版本
    pub schema: u32,
    /// 类型标记（固定 `device_profile`）
    pub kind: String,
    /// 生成时间（Unix 秒）
    pub generated_at: i64,
    /// 生成工具与版本（如 `thermo 0.1.0`）
    pub generator: String,

    /// 是否经过人工核对
    #[serde(default)]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<String>,

    /// 身份信息
    pub identity: Identity,
    /// 电池信息
    #[serde(default)]
    pub battery: BatteryInfo,
    /// 节点映射
    pub nodes: NodePaths,
    /// 能力边界
    pub capabilities: Capabilities,
    /// 探测元信息
    #[serde(default)]
    pub probe_meta: ProbeMeta,
}

/// 机型身份信息
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Identity {
    /// 设备代号（`ro.product.device`，如 `myron`）
    pub device: String,
    /// 机型编号（`ro.product.model`，如 `25102RKBEC`）
    pub model: String,
    /// 平台代号（`ro.board.platform`，如 `canoe`）
    #[serde(default)]
    pub platform: String,
    /// 市场名（`ro.product.marketname`）
    #[serde(default)]
    pub marketname: String,
    /// Android API 级别
    #[serde(default)]
    pub android_api: u32,
    /// Android 版本字符串
    #[serde(default)]
    pub android_release: String,
    /// 内核版本
    #[serde(default)]
    pub kernel: String,
    /// 内存页大小（字节）—— 与 16 KB 对齐要求相关
    #[serde(default)]
    pub page_size: u32,
}

impl Identity {
    /// 是否与另一台设备的身份匹配
    ///
    /// 匹配依据：`device` / `model` / `platform` **任一命中**即为匹配
    /// （契约 §1.1 规则 3）。
    pub fn matches(&self, other: &Self) -> bool {
        let any_eq = |a: &str, b: &str| !a.is_empty() && !b.is_empty() && a == b;
        any_eq(&self.device, &other.device)
            || any_eq(&self.model, &other.model)
            || any_eq(&self.platform, &other.platform)
    }

    /// 供展示的简短描述
    pub fn display(&self) -> String {
        if !self.marketname.is_empty() {
            format!("{} ({})", self.marketname, self.device)
        } else if !self.device.is_empty() {
            self.device.clone()
        } else {
            "未知机型".to_string()
        }
    }
}

/// 电池信息
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BatteryInfo {
    /// 电池型号
    #[serde(default)]
    pub model_name: String,
    /// 电池技术（如 `Li-poly`）
    #[serde(default)]
    pub technology: String,
    /// 设计容量（μAh）
    #[serde(default)]
    pub charge_full_design_uah: i64,
    /// 当前满电容量（μAh）—— 用于估算健康度
    #[serde(default)]
    pub charge_full_uah: i64,
    /// 循环次数
    #[serde(default)]
    pub cycle_count: i32,
}

impl BatteryInfo {
    /// 电池健康度（%）：当前满电容量 / 设计容量
    ///
    /// 设计容量缺失或为 0 时返回 `None`。
    pub fn health_percent(&self) -> Option<f32> {
        if self.charge_full_design_uah <= 0 || self.charge_full_uah <= 0 {
            return None;
        }
        let ratio = self.charge_full_uah as f64 / self.charge_full_design_uah as f64;
        Some((ratio * 100.0) as f32)
    }
}

/// 节点路径映射
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct NodePaths {
    /// 限流档位
    pub ctrl: String,
    /// 限流电流
    pub curr: String,
    /// 第二路电流
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curr2: Option<String>,
    /// 移除标记
    pub remove: String,
    /// 电量上限控制字（写）
    pub soc_ctrl: String,
    /// 电量上限生效值（只读）
    pub soc_read: String,
    /// 电池温度
    pub battery_temp: String,
    /// 电池电流
    pub battery_current: String,
    /// 电池电压
    pub battery_voltage: String,
    /// 电池状态
    pub battery_status: String,
    /// 电池电量
    pub battery_capacity: String,
}

/// 能力边界（与 `thermo-core::profile::Capabilities` 对应）
///
/// ⚠️ 这是 IPC 侧的镜像定义：`thermo-core` 不能依赖 serde 之外的运行时库，
/// 而 IPC 需要独立的序列化结构。两者字段保持一致，由契约测试守护。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub gear: GearCapability,
    pub current: CurrentCapability,
    pub power: PowerCapability,
    pub thermal: ThermalCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soc_limit: Option<SocLimitCapability>,
    #[serde(default)]
    pub verified: bool,
    /// 各能力值的**来源**（可信度标记）
    #[serde(default)]
    pub sources: Sources,
    /// 本机是否**跑过检测**
    ///
    /// # ⚠️ 这是"要不要控制"的总闸门
    ///
    /// `false` 表示这些边界**不是从本机测出来的**（用户还没跑检测）。
    /// 此时决策层**不做任何控制、不写任何节点**：
    ///
    /// | `detected` | 行为 |
    /// |---|---|
    /// | `false` | **零写入** —— 完全交给系统原有的充电逻辑 |
    /// | `true` | 按本机边界控制 |
    ///
    /// # 为什么必须这样
    ///
    /// 初版让"未检测"也参与控制，用兜底值放行，真机后果：
    ///
    /// | 兜底值 | 后果 |
    /// |---|---|
    /// | `18_400_000`（K90PM 实测值） | 67W 机型会被**超限** |
    /// | `5_000_000`（保守猜测） | 用户什么都没设，充电被**限到约 22W** |
    ///
    /// 两者都错 —— 因为**猜的边界不该用来控制真实硬件**。
    /// 用户明确要求："在使用模块之前未进行测试的，不要进行任何控制行为"。
    #[serde(default)]
    pub detected: bool,
}

/// 能力值的来源（可信度递减）
///
/// # 为什么需要它
///
/// 不同来源的可信度**差别很大**。用户与界面必须能分清：
///
/// | 来源 | 可信度 | 说明 |
/// |---|---|---|
/// | [`Node`](Self::Node) | ★★★ | 从只读节点直接读到（如 `charger_common/power_max`） |
/// | [`BatteryModel`](Self::BatteryModel) | ★★★ | 从电池型号解析（如 `P11U_7560mah_100w`） |
/// | [`Devicetree`](Self::Devicetree) | ★★ | 设备树推断（语义可能有偏差） |
/// | [`Profile`](Self::Profile) | ★★ | 来自档案文件（可能是他人实测，非本机） |
/// | [`Default`](Self::Default) | ★ | 硬编码兜底（对未知机型只是假设） |
/// | [`Unknown`](Self::Unknown) | —— | 没探到 |
///
/// 本项目的口径：**前两档是"实测/直读"，可作为控制依据**；
/// 后三档是降级链上的兜底，界面应显著标注。
///
/// 这一设计借鉴自老项目 `ThermoCharge` 的 `*_source` 标记 ——
/// 那边用 6 档（`node`/`measured`/`probe`/`profile`/`dt`/`default`）
/// 区分"这个数是实测的还是猜的"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    /// 从只读节点直接读到
    Node,
    /// 从电池型号解析
    BatteryModel,
    /// 从设备树推断
    Devicetree,
    /// 来自机型档案
    Profile,
    /// 硬编码兜底
    Default,
    /// 未探到
    #[default]
    Unknown,
}

impl ValueSource {
    /// 是否来自**本机**（而非他人的档案或硬编码猜测）
    ///
    /// # 用途：决定要不要提醒用户
    ///
    /// | 来源 | 本机？ | 说明 |
    /// |---|---|---|
    /// | `Node` | ✅ | 本机节点直读 |
    /// | `BatteryModel` | ✅ | 本机电池型号解析 |
    /// | `Devicetree` | ✅ | 本机设备树直读 |
    /// | `Profile` | ❌ | **可能来自他人实测**（档案是分发的） |
    /// | `Default` | ❌ | 硬编码猜测 |
    /// | `Unknown` | ❌ | 没探到 |
    ///
    /// ⚠️ 设备树**算本机数据**，不算"推断"：
    /// 档位表与硬件保护温度本来就只能从设备树读（没有等价节点），
    /// 而且实测验证过（K90PM 的 15 档电流表与 48℃ 都与节点值一致）。
    /// 把它算作不可信会让用户误以为数据有问题。
    pub fn is_local(self) -> bool {
        matches!(self, Self::Node | Self::BatteryModel | Self::Devicetree)
    }

    /// 是否属于**权威**级别（用于"哪个来源更优先"的取舍）
    ///
    /// ⚠️ 这与 [`is_local`](Self::is_local) 是**两个概念**，不要混用：
    ///
    /// | 场景 | 该用哪个 |
    /// |---|---|---|
    /// | 决定要不要提醒用户"数据可能不准" | `is_local` |
    /// | 多个来源取值时挑更可信的 | `is_authoritative` |
    ///
    /// 设备树在**取值优先级**上低于节点直读（语义可能有偏差），
    /// 但在**本机性**上与节点同级。
    pub fn is_authoritative(self) -> bool {
        matches!(self, Self::Node | Self::BatteryModel)
    }

    /// 兼容旧名（等价于 [`is_authoritative`](Self::is_authoritative)）
    pub fn is_trusted(self) -> bool {
        self.is_authoritative()
    }

    /// 供界面展示的短标签
    pub fn label(self) -> &'static str {
        match self {
            Self::Node => "节点直读",
            Self::BatteryModel => "电池型号",
            Self::Devicetree => "设备树",
            Self::Profile => "机型档案",
            Self::Default => "默认值",
            Self::Unknown => "未探到",
        }
    }
}

/// 各能力值的来源集合
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Sources {
    /// 档位上限
    #[serde(default)]
    pub gear_max: ValueSource,
    /// 电流上限
    #[serde(default)]
    pub current_max: ValueSource,
    /// 标称功率
    #[serde(default)]
    pub power_max: ValueSource,
    /// 硬件保护温度
    #[serde(default)]
    pub hardware_max_temp: ValueSource,
    /// 档位表
    #[serde(default)]
    pub gear_table: ValueSource,
}

/// 档位能力
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GearCapability {
    pub min: u8,
    pub max: u8,
    /// 最松档位
    pub open: u8,
    /// 最紧档位（安全值）
    pub safe: u8,
    /// 档位 → 电流上限（设备树直读，可能不完整）
    #[serde(default)]
    pub table: Vec<GearRow>,
}

impl GearCapability {
    /// 取某档的电流上限（μA）
    pub fn current_limit_ua(&self, gear: u8) -> Option<i32> {
        self.table
            .iter()
            .find(|r| r.gear == gear)
            .map(|r| r.current_limit_ua)
    }
}

/// 档位表的一行
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GearRow {
    pub gear: u8,
    /// μA
    pub current_limit_ua: i32,
}

/// 电流能力
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurrentCapability {
    pub min_ua: i32,
    /// 实测上界
    pub max_ua: i32,
    /// 安全值
    pub safe_ua: i32,
}

/// 功率能力
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerCapability {
    /// 整机最大充电功率（W）；未实测时为 `null`
    #[serde(default)]
    pub max_w: Option<u16>,
    /// 功率最大的档位
    #[serde(default)]
    pub max_power_gear: Option<u8>,
    /// 充电协议
    #[serde(default)]
    pub protocol: Option<String>,
}

/// 温度能力
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThermalCapability {
    /// 硬件保护温度（℃）
    pub hardware_max_c: f32,
    /// 模块硬上限（℃）
    pub abs_max_c: f32,
}

/// SOC 上限能力
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SocLimitCapability {
    pub supported: bool,
    pub min: u8,
    pub max: u8,
}

/// 探测元信息
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ProbeMeta {
    /// 采用的探测手段
    #[serde(default)]
    pub method: String,
    /// 探测时间（Unix 秒）
    #[serde(default)]
    pub probed_at: i64,
    /// 哪些项需要充电时才准确
    #[serde(default)]
    pub needs_charge_for: Vec<String>,
    /// 备注
    #[serde(default)]
    pub notes: Vec<String>,
    /// 本次检测未成功的项
    #[serde(default)]
    pub failed_items: Vec<String>,
}

impl DeviceProfile {
    /// 构造空档案骨架（各探测步骤逐步填充）
    pub fn new_skeleton(generator: impl Into<String>, now: i64) -> Self {
        Self {
            schema: PROFILE_SCHEMA,
            kind: PROFILE_KIND.to_string(),
            generated_at: now,
            generator: generator.into(),
            verified: false,
            verified_date: None,
            verified_by: None,
            identity: Identity::default(),
            battery: BatteryInfo::default(),
            nodes: NodePaths::default(),
            capabilities: Capabilities {
                gear: GearCapability {
                    min: 0,
                    // ⚠️ 骨架值是**物理护栏**，不是机型数据
                    //
                    // 它只用于"检测还没跑完"的中间态，且此时
                    // `detected = false` ⇒ 决策层零写入（见字段文档）。
                    // 真正生效的值一律来自检测（`gear.max = 档位表行数`）。
                    //
                    // 15 的来源：本机实测可写 0~15。换机型后**不会**用到这里
                    // —— 检测要么覆盖它，要么整个档案判为未检测。
                    max: 15,
                    open: 0,
                    safe: 15,
                    table: Vec::new(),
                },
                current: CurrentCapability {
                    min_ua: 0,
                    // ⚠️ 物理上限（30A）—— 语义是"**不限制**"，不是"限制到 30A"
                    //
                    // 与 `thermo-core::Capabilities::default()` 保持一致。
                    // 这个值不只是"上限"，还是**正常放行时写入节点的值** ——
                    // 给保守小值会导致"未检测就限速"（真机踩过：5A ⇒ 约 22W）。
                    max_ua: 30_000_000,
                    safe_ua: 700_000,
                },
                power: PowerCapability {
                    max_w: None,
                    max_power_gear: None,
                    protocol: None,
                },
                thermal: ThermalCapability {
                    // ⚠️ 这里是**兜底**，检测一定会覆盖它
                    //
                    // 检测的降级链见 `detect.rs` §⑥：
                    // 设备树 → 官方热控表 → 本值。
                    // 前两级都拿不到时说明该机型既不标准也不是小米，
                    // 此时用 45℃ 这个"偏保守"的值，并标记来源为 `Default`。
                    hardware_max_c: 45.0,
                    abs_max_c: 45.0,
                },
                soc_limit: None,
                verified: false,
                sources: Sources::default(),
                // 兜底档案 ≠ 本机实测 ⇒ 默认**未检测**（不允许控制）
                detected: false,
            },
            probe_meta: ProbeMeta::default(),
        }
    }

    /// 校验档案基本合法性
    pub fn validate(&self) -> Result<(), String> {
        if self.schema > PROFILE_SCHEMA {
            return Err(format!(
                "档案 schema {} 过新（本版最高支持 {PROFILE_SCHEMA}）",
                self.schema
            ));
        }
        if self.kind != PROFILE_KIND {
            return Err(format!(
                "档案类型不匹配：期望 {PROFILE_KIND}，实得 {}",
                self.kind
            ));
        }
        if self.identity.device.is_empty() && self.identity.model.is_empty() {
            return Err("档案缺少机型标识（device 与 model 均为空）".to_string());
        }
        if self.capabilities.gear.max < self.capabilities.gear.min {
            return Err("档位范围非法（max < min）".to_string());
        }
        if self.capabilities.current.max_ua <= 0 {
            return Err("电流上限必须为正".to_string());
        }
        Ok(())
    }

    /// 导出文件名（`<model>_<device>_<日期>.json`）
    pub fn export_filename(&self, date: &str) -> String {
        let model = if self.identity.model.is_empty() {
            "unknown"
        } else {
            &self.identity.model
        };
        let device = if self.identity.device.is_empty() {
            "unknown"
        } else {
            &self.identity.device
        };
        format!("{model}_{device}_{date}.json")
    }
}

/// 档案来源（用于诊断"这份档案是哪来的"）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    /// 本机实测（`device.json`）
    Local,
    /// 出厂档案（按机型匹配）
    Factory,
    /// 通用保守默认
    Generic,
}

impl ProfileSource {
    /// 是否为精确匹配（本机或出厂）
    pub fn is_exact(self) -> bool {
        matches!(self, Self::Local | Self::Factory)
    }

    /// 展示用名称
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "本机实测",
            Self::Factory => "出厂档案",
            Self::Generic => "通用默认",
        }
    }
}

/// 生效档案 + 来源信息
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveProfile {
    pub profile: DeviceProfile,
    pub source: ProfileSource,
}

impl EffectiveProfile {
    /// 是否未验证（需用户确认后才允许控制）
    pub fn is_unverified(&self) -> bool {
        !self.profile.verified || !self.source.is_exact()
    }
}
