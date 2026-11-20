//! 附加功能 ①：充电上限（`soc_limit`）
//!
//! 设定"充到指定电量即停止"。
//!
//! # 机制是怎么来的
//!
//! 全部由**真机实测确证**（见 [`logic`] 的模块文档与那几张对照表）：
//! 写入节点是 `smart_chg`、读取节点是 `soc_limit`、控制字的使能位是 `0x11`。
//! 这些都是在这台设备上一条一条试出来的，不是从别处抄的。
//!
//! # 它碰什么、不碰什么
//!
//! | 维度 | 值 |
//! |---|---|
//! | 写节点 | `smart_chg`（控制字） |
//! | 读节点 | 同目录 `soc_limit`（生效值） |
//! | 额外写 | 电量显示 `fake_soc`（**仅小米控制字方言** + `limit.sync_display`；标准接口不写） |
//! | **不碰** | 档位 / 电流 / 温度阈值 / 任何温控节点 |
//! | 周期 | 启用时 15 秒；未启用时 60 秒（只为响应配置变更） |
//! | 失败后果 | 最多"没限制住"，不会伤害设备 |
//!
//! # ⚠️ 2026-11-19 真机反馈修正：标准接口按「系统电量」截止
//!
//! 群友反馈（K70 Pro 一类标准 `power_supply` 机型）：
//!
//! | 操作 | 现象 |
//! |---|---|
//! | 设上限 90、同步开 | 图标停在 85 就不充了 |
//! | 设 100 | 还是不充 |
//! | 关掉上限 | 图标跳到 100，又能继续充 |
//!
//! 三个现象只能由同一个解释串起来：`charge_control_end_threshold`
//! 是拿**系统电量**（`battery/capacity`，即电池图标那个数）比较的，
//! **不是** `fg1_rsoc`。同步把图标改写成 `rsoc`（85）后：
//!
//! 1. 系统电量到 90 时驱动已经停充，图标却显示 85 ⇒「85 就停」；
//! 2. 设 100 时系统电量已经是 100 ⇒ 驱动直接判定到点，不充；
//! 3. 关掉上限（`en=0`）后驱动不再执行阈值 ⇒ 恢复充电，图标也回到 100。
//!
//! ⇒ 标准接口下**禁止**改写系统显示；目标 100% 也**不能写 100**，
//!    而是释放使能位（见 [`SocLimit::is_unlimited_target`]）。

pub mod config;
pub mod logic;

use serde_json::json;

use crate::addon::{Addon, AddonError, AddonSpec, AddonStatus, ProbeOutcome, TickOutcome};
use crate::io::AddonIo;
use config::IDLE_INTERVAL_S;
pub use config::{FullChargeState, SocLimitConfig};

/// `smart_chg` 的候选路径（**顺序即优先级**）
///
/// ⚠️ 路径**因机型而异**，必须运行期探测 —— 硬编码单一路径会让部分机型的
/// 充电上限功能直接失效。这与温控侧的候选表**刻意分开维护**（互不依赖）。
pub const SMART_CHG_CANDIDATES: &[&str] = &[
    // 小米 HyperOS 主路径（K90 Pro Max / Android 17 命中）
    "/sys/class/xm_power/charger/smart_charge/smart_chg",
    // 面向其他机型的兼容回退
    "/sys/class/charger/smart_charge/smart_chg",
    // ⚠️ **刻意不收录** `/sys/class/qcom-battery/smart_chg`
    //
    // 实测 2026-10-19：它「可写」（写入后节点值变成 16），但
    // **写进去不生效** —— `battery/status` 仍是 Charging，而且它旁边
    // 没有 `soc_limit` 可回读，`logic.rs` 的注释也写着"读值在 16/17/80
    // 漂移，不能用它判断生效"。
    //
    // ⇒ 一个"能写、改不了行为、也无法验证"的节点，收录它只会让
    //   `probe` 谎报"支持"，然后每拍失败、最终熔断。不如不收录：
    //   本机真诚地报「不支持」，用户就知道该去找别的方案。
    // 标准 Linux `power_supply` 接口：**纯百分比**（`0` 表示不限制）
    //
    // ⚠️ 语义与小米的 `smart_chg` 不同：那个是**控制字编码**
    //    （位域打包，见 `logic::encode`），这个是直白的 `0~100`。
    //    写值格式由 [`Dialect`] 决定，不要混用。
    "/sys/class/power_supply/battery/charge_control_end_threshold",
    // MediaTek / 小米 17T（chagall，MT6899）：系统「智能断充」的目标百分比节点。
    // 语义是纯百分比（用户实测读值为 80，即目标 80%），写在 `usb/device` 下。
    //
    // ⚠️ 与标准 `charge_control_end_threshold` 的**关键差异**：本节点没有
    //    `charge_control_en` 那类使能位。percent 方言的"停用/卸载"路径只写
    //    使能位回到 0（见 `tick` 的 `!enabled` 分支与 `release`），对本节点
    //    ⇒ **零写入**，上次写的目标值可能留在驱动里（R12「关闭 ≠ 卸载」）。
    //
    // 🔴 待真机确认（2026-11-20，无机器）：
    //    ① `ctl limit off` / 卸载后本节点是否残留？（残留则需在 `release`
    //       里还原"探测时读到的原值"，不要猜 100）
    //    ② 写入是否**真的改变充电行为**？若像 `qcom-battery/smart_chg` 那样
    //       "能写但不生效"，probe 会谎报 supported，3 次后熔断。
    //    两条都确认前，不要把这套逻辑当"已验证"写进文档。
    "/sys/class/power_supply/usb/device/sc_tuisoc",
];

/// **使能节点**候选（标准百分比方言专用）
///
/// # 为什么必须有它（2026-10-19 真机查明，K70 Pro）
///
/// `charge_control_end_threshold` 在高通这套驱动上**不是自足的**：
///
/// | `charge_control_en` | 写 `threshold=95` | `write()` 返回值 |
/// |---|---|---|
/// | `0` | 读回 **0**（被吞掉） | **成功**（谎报） |
/// | `1` | 读回 **95** ✓ | 成功 |
///
/// 关键在最后一行：**写入失败时系统调用仍返回成功**。于是模块的回读
/// 校验看到"期望 95、实得 0"，连续三次后判定成"有另一个程序在抢这个
/// 节点"并熔断 —— 而真相只是少开了一个使能位。
///
/// ⚠️ 小米的 `smart_chg` 控制字自带 `0x11` 使能位，**不需要**这个节点
///    （所以候选命中后是否使用，由 [`Dialect`] 决定）。
pub const SOC_ENABLE_CANDIDATES: &[&str] = &[
    "/sys/class/qcom-battery/charge_control_en",
    "/sys/class/power_supply/battery/charge_control_en",
];

/// **电量计口径** SOC 节点候选（%）
///
/// 用于满充计数与界面诊断。
///
/// ⚠️ 2026-11-19 真机反馈修正：标准 `power_supply` 的
///    `charge_control_end_threshold` 按**系统电量**（`capacity`）截止，
///    不按这个值。只有小米控制字方言才可能按它比较。
const RSOC_CANDIDATES: &[&str] = &[
    "/sys/class/qcom-battery/fg1_rsoc",
    "/sys/class/power_supply/battery/fg1_rsoc",
    "/sys/class/power_supply/battery/rsoc",
];

/// **系统电量覆盖**节点候选 —— 可写，用来把显示校正成驱动真实值
///
/// # 🔴 为什么需要它（2026-10-20 用户明确要求）
///
/// 本机上两个 SOC 严重不一致，且系统显示**偏高**：
///
/// | 来源 | 节点 | 实测值 |
/// |---|---|---|
/// | 电池图标 | `battery/capacity` | **90~99%** |
/// | 驱动真实 | `qcom-battery/fg1_rsoc` | **78~81%** |
///
/// 后果是用户看到的两件事：
///
/// 1. 设 95%，驱动按 `rsoc` 充到 95 才停 —— 那时图标早就 100% 了
/// 2. 图标本身的电量就是虚高的，充电过程中的读数不可信
///
/// 用户的要求：**「系统显示按驱动读取的电量调整」** ——
/// 把图标校正成驱动真实值，两边永远同一口径。
///
/// ⇒ 每拍把 `fake_soc` 写成当前 `rsoc`；禁用/卸载时恢复原值。
///
/// ⚠️ `capacity` 本身只读（`-r--r--r--`），`fake_soc` 是驱动提供的
///    唯一覆盖入口（实测：写 85 → 图标 85；恢复 -22 → 回到真实值）。
/// ⚠️ **必须记录原值**并在禁用时恢复 —— 否则模块卸载后图标会一直
///    停在最后写入的假电量上，那是很糟的用户体验。
///
/// ⚠️ 2026-11-19：只在**小米控制字方言**下使用。标准接口按系统电量
///    截止，改写图标会造成「设 90、显示 85 就停」的假象
///    （见 [`SocLimit::sync_supported`]）。
const FAKE_SOC_CANDIDATES: &[&str] = &[
    "/sys/class/qcom-battery/fake_soc",
    "/sys/class/power_supply/battery/fake_soc",
];

/// **系统电量**节点候选（%）—— 定期满充的充入量累计用
///
/// 优先用驱动真实值（[`RSOC_CANDIDATES`]），读不到时退回系统口径。
/// 两个都是 0~100 的百分比，`logic::parse_active` 可以直接解析。
const CAPACITY_CANDIDATES: &[&str] = &[
    "/sys/class/power_supply/battery/capacity",
    "/sys/class/power_supply/bms/capacity",
];

/// **充电状态**节点候选 —— 判断"已经充满"
///
/// 满充完成的判据有两路：
/// 1. 读到的 SOC 达到 100%（**主判据**）
/// 2. `status` 变成 `Full`（有些机型满电后 `capacity` 停在 99）
///
/// 两路都是只读，不写任何节点。
const STATUS_CANDIDATES: &[&str] = &[
    "/sys/class/power_supply/battery/status",
    "/sys/class/power_supply/bms/status",
];

/// 控制节点的**写入方言**
///
/// # 为什么需要区分
///
/// 同样是"限制充到百分之几"，两种框架要写的东西完全不同：
///
/// | 框架 | 节点 | 要写入的内容 |
/// |---|---|---|
/// | 小米 `smart_chg` | `xm_power/charger/smart_charge/smart_chg` | **控制字** `0x50` 这类位域打包值 |
/// | 标准 `power_supply` | `battery/charge_control_end_threshold` | **纯百分比** `80` |
///
/// 把 `0x50` 写进标准节点 ⇒ 它会当成 `80`（恰好巧合），
/// 而把 `80` 写进小米节点 ⇒ 位域全错。所以必须按命中节点区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// 小米控制字（`logic::encode` 打包）
    ControlWord,
    /// 标准百分比（纯十进制）
    Percent,
}

impl Dialect {
    /// 由控制节点路径判定方言
    ///
    /// ⚠️ 用**节点名**判定而不是"目录里有 `xm_power` 就…"：
    ///    将来若某机型把标准节点放到别的目录，这里依然正确。
    fn of(ctrl: &str) -> Self {
        if ctrl.ends_with("/charge_control_end_threshold") || ctrl.ends_with("/sc_tuisoc") {
            Self::Percent
        } else {
            Self::ControlWord
        }
    }
}

/// **使用前提**文案（界面与 `limit show` 共用同一份）
///
/// 用户明确要求（2026-10-16）：「加上提示：此模块需打开电池健康保护功能」。
///
/// ## 为什么这不是"多余的提醒"
///
/// `smart_charge/*` 这一组节点属于系统「电池保护」功能的实现 ——
/// 系统设置里的「智能断充」（默认 80%）改的就是同一个节点。
/// 开关关掉时驱动是否仍执行 SOC 限制**不可依赖**，因此这是**真实前提**，
/// 不是免责声明。
///
/// ## 措辞为什么把两个名字都写上
///
/// 同一个开关在不同 HyperOS 版本 / 机型上叫法不同：
///
/// | 叫法 | 出现位置 |
/// |---|---|
/// | **电池健康保护** | 部分新版设置项名称 |
/// | **电池保护** | 设置路径与系统组件名（`ChargeProtectFragment`） |
///
/// 只写一个名字，另一个版本的用户会在设置里**找不到**，然后以为
/// 提示写错了。写上两个 + 路径，任何版本都能对上。
pub const REQUIRES_BATTERY_PROTECT: &str =
    "需先在系统设置中打开「电池健康保护」（设置 → 省电与电池 → 电池保护）";

/// **标准 `power_supply` 框架**的使用前提文案
///
/// # 为什么不能沿用上面那句（2026-10-19）
///
/// 上面那句的前提是「我们和系统**写同一个节点**」—— 系统的「电池保护」
/// 开关关掉后，那个节点的行为就不可依赖。这个推理对小米的
/// `smart_charge/smart_chg` 成立。
///
/// 但标准框架的 `charge_control_end_threshold` 是**上游内核的通用接口**，
/// 归 `power_supply` 子系统管，**不受**「电池健康保护」开关控制。
/// 对这类机型继续喊"请先打开电池健康保护"，后果是：
///
/// | 用户照做 | 结果 |
/// |---|---|
/// | 去设置里找那个开关 | 找到也可能本来就没关 |
/// | 找不到（机型没这个开关） | 以为缺了前提，**白白放弃该功能** |
///
/// ⇒ 改成如实说明"本机走标准接口"，并保留**唯一真实存在的注意点**：
///   未插充电器时看不到效果（驱动只记录不执行，小米与标准框架都如此）。
///
/// ⚠️ 不写"无需任何前提"：那等于替用户断言他没遇到过的问题不存在。
///    只说能确证的事。
///
/// ⚠️ 2026-11-20 追加：percent 方言现在**不再只有上游标准接口** ——
///    MediaTek 的 `sc_tuisoc` 也走 percent，但它是厂商节点，很可能恰恰
///    受系统「电池保护 / 智能断充」开关控制。所以文案不能再断言
///    "与开关无关"，改成条件式：先按标准接口说明，再给一句"不生效时查开关"。
pub const REQUIRES_STANDARD_PSY: &str = "本机走 power_supply 百分比接口（上游标准接口或厂商同类节点），\
                                         无需额外设置（未插充电器时看不到效果是正常的）；\
                                         若设置后不生效，请确认系统的「电池保护 / 智能断充」开关已打开";

/// **系统界面不同步**的说明（界面用）
///
/// # 为什么需要说明这件事
///
/// 系统的「电池保护」设置页有一套**自己的界面文案**，写的是它自己记的
/// 那个百分比（默认 80%）。那个文案的显示逻辑在**系统应用里**，
/// 而本模块是**纯 root + sysfs，没有 hook 层**，改不了系统界面。
///
/// ⇒ 用户设了 90%，进系统设置一看仍写着 80%。
///
/// 不说清楚的话，用户会判断成"没生效" —— 而实际上限制**已经生效**
/// （写的是同一个节点，驱动按他设的值执行）。
/// 这是"看起来失败其实成功了"的典型，必须在界面上说透。
///
/// ⚠️ 文案里**不要提任何具体做法**（比如"改界面文案"），
///    那既不是我们能做的事，也容易让用户误以为可以做到。
pub const NOTE_UI_NOT_SYNCED: &str =
    "系统设置里显示的百分比不会跟着变（那是系统自己的文案）。以本页的「已生效」为准。";

/// 由控制节点推出"生效值"节点（同目录的 `soc_limit`）
fn read_node_for(ctrl: &str, dialect: Dialect) -> String {
    // 标准接口：控制节点**自己**就是生效值（读它就能回读校验）
    if dialect == Dialect::Percent {
        return ctrl.to_string();
    }
    // 小米控制字：控制节点是打包值，生效值在同目录的 `soc_limit`
    match ctrl.rfind('/') {
        Some(idx) => format!("{}/soc_limit", &ctrl[..idx]),
        None => "soc_limit".to_string(),
    }
}

/// 充电上限功能
#[derive(Debug, Clone)]
pub struct SocLimit {
    /// 配置（来自 `config.json` 的 `limit` 分区）
    config: SocLimitConfig,
    /// 命中的控制节点路径（探测后填充）
    node_ctrl: Option<String>,
    /// 命中节点的写入方言（probe 时确定）
    dialect: Dialect,
    /// 对应的生效值节点路径
    node_read: Option<String>,
    /// **使能节点**路径（可选，`None` = 本机不需要）
    ///
    /// # 为什么需要它（2026-10-19 真机查明）
    ///
    /// K70 Pro 的 `charge_control_end_threshold` **不是自足的**：
    ///
    /// | `charge_control_en` | 写 `threshold=95` 后读回 |
    /// |---|---|
    /// | `0` | **0** ← 写入被驱动吞掉，且**不报错** |
    /// | `1` | **95** ✓ 正常保存 |
    ///
    /// 也就是说：不开使能位时，写入看起来成功、读回永远是 0 ——
    /// 模块据此判定"回读不符"，连续三次后误判成"有别的程序在抢"
    /// 并熔断，界面上就成了"充电上限不可用"。
    ///
    /// ⚠️ 只对**标准百分比**方言有效。小米的 `smart_chg` 控制字
    ///    自带 `0x11` 使能位，不需要额外节点。
    node_enable: Option<String>,
    /// 是否已经开过使能位（避免每拍重复写）
    enable_engaged: bool,
    /// 探测结果
    support: Option<ProbeOutcome>,
    /// 最近一次读到的生效上限
    last_seen: Option<u8>,
    /// 最近一次写入的上限
    last_written: Option<u8>,
    /// 累计写入次数
    write_count: u64,
    /// **本机支持的上限区间**（%），由宿主从检测档案注入
    ///
    /// `None` = 尚未注入（检测未跑过），此时用兜底区间
    /// `SOC_FALLBACK_MIN ~ SOC_FALLBACK_MAX`。
    ///
    /// ⚠️ 这个区间**因机型而异**：初版把它写死成 `60 ~ 100`
    /// （本机 K90PM 的值），换机型就会用错范围。
    range: Option<(u8, u8)>,
    /// **连续**"写下去又被改回"的次数 —— 用于识别**写入方冲突**
    ///
    /// # 为什么这个数能识别冲突
    ///
    /// `smart_chg` 是**全局节点**：任何写它的程序都在改同一个开关。
    /// 两个程序同时启用时，谁都可能在后 —— 于是出现
    /// 「我们写 85 → 立刻被改回 80 → 下拍再写 85 → 又被改回」的循环。
    ///
    /// 单次回读不符**不能**说明有冲突（可能只是驱动延迟一拍才生效），
    /// 所以只在**连续**多次不符时才提示 —— 见 [`Self::conflict_hint`]。
    ///
    /// 写入成功即清零。
    verify_fail_streak: u32,
    /// 本拍**实际写入的阈值**
    ///
    /// 恒等于 `config.soc` —— 标准接口按系统电量截止，小米控制字
    /// 也按控制字里的百分比，两边都不需要换算。
    last_target: Option<u8>,
    /// **驱动真实电量**节点（`fg1_rsoc`，%）
    node_rsoc: Option<String>,
    /// **系统电量覆盖**节点（`fake_soc`）
    ///
    /// 写它 = 让电池图标显示指定值。见 [`FAKE_SOC_CANDIDATES`]。
    node_fake_soc: Option<String>,
    /// `fake_soc` 的**原始值**（禁用/清理时恢复）
    ///
    /// ⚠️ 原值通常是负数（本机 `-22`，表示"不覆盖"）。
    ///    必须原样存下、原样写回 —— 不能写 0 或 100。
    fake_soc_orig: Option<String>,
    /// 上一次写入 `fake_soc` 的值（相同则跳过，减少无谓写入）
    fake_soc_last: Option<u8>,
    /// 上一次写 `fake_soc` **失败了**（同步开着但改不动显示）
    ///
    /// # 为什么必须有这个标记
    ///
    /// 同步是**用户可见开关**：打开后图标没变，用户只会认为"功能坏了"。
    /// 写失败的真实原因（缺少 SELinux 放行 / 节点只读 / 被占用）必须能
    /// 传到界面上，而不是被 `is_ok()` 静默吞掉（R17「不静默」）。
    ///
    /// ⚠️ 只在**状态翻转**时打日志，否则每拍一次 warn 会刷屏。
    sync_error: bool,
    /// 跟随 SOC 的偏差（`capacity - rsoc`），诊断用
    follow_offset: Option<i16>,
    /// 跟随 SOC 最近写入的阈值（**capacity 口径**）
    follow_threshold: Option<u8>,
    /// 跟随 SOC 时目标在系统满电下不可达（`capacity` 已封顶、`rsoc` 仍低于目标）
    follow_unreachable: bool,
    /// 跟随 SOC 刚写了 `en=0`，等待下一拍恢复 `en=1`
    follow_resume: bool,
    /// 跟随 SOC 的刷新计数
    ///
    /// `fake_soc` 会把 `battery/capacity` 也覆盖成 SOC，导致读不到
    /// 「驱动实际比较的原始容量」。每 4 拍（约 60 秒）临时恢复一次
    /// `fake_soc` 原值读原始容量，再写回 SOC；其余拍用缓存的偏差。
    follow_tick: u32,
    /// **定期满充**的运行状态（首次巡检时从宿主加载）
    ///
    /// 与配置分开：这是记账（充了多少、上次何时满充），不是用户意图。
    /// 见 [`FullChargeState`] 与 [`config::STATE_ID`]。
    state: FullChargeState,
    /// 状态是否已从宿主加载（每个进程只加载一次）
    state_loaded: bool,
    /// 状态是否有未落盘的改动
    state_dirty: bool,
    /// 下一次立即落盘（触发/完成满充等关键变化不等节流）
    state_force_save: bool,
    /// 状态落盘失败是否已告警（避免每拍刷日志）
    state_save_warned: bool,
    /// 采样节点：系统电量 `capacity`（[`node_rsoc`](Self::node_rsoc) 不存在时用）
    node_soc: Option<String>,
    /// 充电状态节点（`Full` 判据）
    node_status: Option<String>,
}

impl SocLimit {
    /// 用给定配置构造（尚未探测）
    pub fn new(config: SocLimitConfig) -> Self {
        Self {
            config,
            node_ctrl: None,
            dialect: Dialect::ControlWord,
            node_read: None,
            node_enable: None,
            enable_engaged: false,
            support: None,
            last_seen: None,
            last_written: None,
            write_count: 0,
            range: None,
            verify_fail_streak: 0,
            last_target: None,
            node_rsoc: None,
            node_fake_soc: None,
            fake_soc_orig: None,
            fake_soc_last: None,
            sync_error: false,
            follow_offset: None,
            follow_threshold: None,
            follow_unreachable: false,
            follow_resume: false,
            follow_tick: 0,
            state: FullChargeState::default(),
            state_loaded: false,
            state_dirty: false,
            state_force_save: false,
            state_save_warned: false,
            node_soc: None,
            node_status: None,
        }
    }

    /// 注入**本机支持的上限区间**（%）
    ///
    /// 由宿主在加载档案后调用（见 `addon_host::reload_config`）。
    /// 不注入时用兜底区间 —— 那种情况只在检测尚未跑过时出现。
    pub fn set_range(&mut self, min: u8, max: u8) {
        self.range = Some((min, max));
    }

    /// 当前生效的区间（本机 or 兜底）
    fn effective_range(&self) -> (u8, u8) {
        self.range
            .unwrap_or((config::SOC_FALLBACK_MIN, config::SOC_FALLBACK_MAX))
    }

    /// 本机是否支持把系统电量显示改成驱动 SOC
    ///
    /// # 为什么标准接口不支持（2026-11-19 真机反馈）
    ///
    /// 标准 `power_supply` 的 `charge_control_end_threshold` 由驱动按
    /// **系统电量**（`battery/capacity`，即电池图标那个数）比较，
    /// 不是按 `fg1_rsoc`：
    ///
    /// | 操作 | 现象 |
    /// |---|---|
    /// | 设 90、同步开 | 图标被改成 `rsoc`（85）⇒ 看起来「85 就停」 |
    /// | 设 100 | 系统电量已 100 ⇒ 驱动直接不充 |
    /// | 关掉上限 | 图标恢复系统值 100，又能继续充 |
    ///
    /// ⇒ 标准接口下**不能**改写图标：显示必须与截止口径同一个数。
    fn sync_supported(&self) -> bool {
        self.node_ctrl.is_some() && (self.dialect == Dialect::ControlWord || self.follow_mode())
    }

    /// 本机是否**具备**同步条件（方言支持 + 能读 `rsoc` + 能写 `fake_soc`）
    fn sync_available(&self) -> bool {
        self.sync_supported() && self.node_rsoc.is_some() && self.node_fake_soc.is_some()
    }

    /// 跟随 SOC 的节点是否齐全（标准接口闭环所需）
    ///
    /// 需要：控制节点、`en`、`capacity`、`fg1_rsoc`、`fake_soc`、`status`。
    /// 少任何一个都无法「按 SOC 停 + 同步显示」，此时退回按系统电量。
    fn follow_supported(&self) -> bool {
        self.node_ctrl.is_some()
            && self.dialect == Dialect::Percent
            && self.node_enable.is_some()
            && self.node_rsoc.is_some()
            && self.node_fake_soc.is_some()
            && self.fake_soc_orig.is_some()
            && self.node_soc.is_some()
            && self.node_status.is_some()
    }

    /// 本拍是否走「跟随 SOC」闭环
    fn follow_mode(&self) -> bool {
        self.config.follow_soc && self.follow_supported()
    }

    /// 同步不可用的原因（界面直接展示；可用时为 `None`）
    fn sync_reason(&self) -> Option<&'static str> {
        if !self.supported() {
            return None;
        }
        if self.dialect == Dialect::Percent && !self.follow_mode() {
            if self.config.follow_soc {
                return Some(
                    "本机缺少跟随 SOC 所需的节点（capacity / fg1_rsoc / fake_soc / en / status），\
                     无法开启「按 SOC 限制」。",
                );
            }
            return Some(
                "本机走百分比接口：在上游标准充电接口上驱动按系统电量（电池图标）截止，\
                 SOC 只是电量计读数。把图标改成 SOC 会让显示与截止口径不一致\
                 （设 90 却像在 85 停下）；如需按 SOC 限制，请开启「按 SOC 限制」。",
            );
        }
        if self.node_rsoc.is_none() || self.node_fake_soc.is_none() {
            return Some("本机缺少电量计节点（fg1_rsoc）或可写的显示节点（fake_soc），无法同步。");
        }
        None
    }

    /// 目标 100% 在标准接口上等价于「不限制」
    ///
    /// 标准接口按系统电量截止；系统电量到 100 时驱动就停充，哪怕
    /// 电池实际还没充满（真机反馈：设 100 也不充，关掉上限反而继续充）。
    /// ⇒ 不写 100，而是把使能位关回 0，交回系统自己的充电逻辑。
    fn is_unlimited_target(&self, target: u8) -> bool {
        self.dialect == Dialect::Percent && target >= config::FULL_SOC
    }

    /// 把**系统显示电量校正成驱动真实电量**（写 `fake_soc`）
    ///
    /// ⚠️ 仅小米控制字方言可用；标准接口见 [`Self::sync_supported`]。
    ///
    /// # 用户要求（2026-10-20）
    ///
    /// 「充电上限设置成 95，写入驱动 95，**系统显示 95**；
    ///   系统显示不准，按驱动读取的电量调整」
    ///
    /// # 真机数据
    ///
    /// | 来源 | 值 |
    /// |---|---|
    /// | 电池图标 `capacity` | **90~99%**（虚高） |
    /// | 驱动真实 `fg1_rsoc` | **78~81%** |
    ///
    /// 虚高的后果：设 95% 时驱动按 `rsoc=95` 停，图标那时早已 100%。
    ///
    /// ⇒ 每拍把 `fake_soc` 写成当前 `rsoc`，图标就与驱动同口径：
    ///
    /// ```text
    /// fake_soc = rsoc   ⇒   capacity 显示 rsoc
    /// ```
    ///
    /// 于是"设 95 → 驱动 95 → 显示 95"三者统一。
    ///
    /// # 2026-11-18：变成**可开关**的
    ///
    /// 用户要求「加入 SOC 电量显示与同步功能」 ⇒ 同步不再是无条件的：
    /// 由 `config.json` 的 `limit.sync_display` 控制（出厂默认 `true`，
    /// 与既有行为一致）。关闭时走 [`Self::restore_display`] 恢复原值，
    /// 不是简单跳过 —— 否则图标会停在最后一次同步的过期值上。
    ///
    /// # 安全约束
    ///
    /// - 只在**已启用且 `limit.sync_display` 开启**时才写，关闭时恢复原值
    /// - 值没变就跳过，避免每拍无谓写入
    /// - 读不到 `rsoc` 时**不动** `fake_soc`（宁可显示不准，也不要写错）
    fn sync_display_to_driver(&mut self, io: &mut dyn AddonIo) {
        let (Some(rsoc_path), Some(fake_path)) =
            (self.node_rsoc.clone(), self.node_fake_soc.clone())
        else {
            return;
        };
        let Some(rsoc) = io
            .read(&rsoc_path)
            .ok()
            .and_then(|r| logic::parse_active(&r))
        else {
            return;
        };
        // 值没变就不重复写
        if self.fake_soc_last == Some(rsoc) {
            return;
        }
        match io.write(&fake_path, &rsoc.to_string()) {
            Ok(()) => {
                self.fake_soc_last = Some(rsoc);
                self.sync_error = false;
            }
            Err(e) => {
                // ⚠️ 只在状态翻转时告警 —— 充电中每拍都会重试，
                //    每拍一条 warn 会把日志刷满（也掩盖真正的问题）。
                if !self.sync_error {
                    log::warn!(
                        "[soc_limit] 写入电量显示节点 {fake_path} 失败：{e}\
                         （电量同步暂不生效；检查 SELinux 放行与节点权限）"
                    );
                    self.sync_error = true;
                }
            }
        }
    }

    /// 恢复 `fake_soc` 的原始值（禁用 / 交回官方 / 退出时调用）
    ///
    /// ⚠️ 不恢复的后果：模块停用后电池图标会一直显示模块最后写入的
    ///    假电量，与实际严重脱节 —— 必须还原成驱动自己的行为。
    fn restore_display(&mut self, io: &mut dyn AddonIo) {
        let (Some(fake_path), Some(orig)) =
            (self.node_fake_soc.clone(), self.fake_soc_orig.clone())
        else {
            return;
        };
        if self.fake_soc_last.is_none() {
            return;
        }
        if io.write(&fake_path, &orig).is_ok() {
            self.fake_soc_last = None;
            self.sync_error = false;
            log::info!("[soc_limit] 已恢复系统电量显示（fake_soc={orig}）");
        }
    }

    /// 读**原始** `capacity`
    ///
    /// `fake_soc` 会把 `battery/capacity` 也覆盖成 SOC，而驱动截止用的是
    /// 原始容量。⇒ 先写回 `fake_soc` 原值、读容量、再写回 SOC。
    /// 只在跟随模式每 4 拍做一次（显示会闪一下，窗口是毫秒级）。
    fn read_raw_capacity(
        &mut self,
        io: &mut dyn AddonIo,
        rsoc: u8,
        cap_path: &str,
        fake_path: &str,
    ) -> Result<u8, String> {
        let orig = self
            .fake_soc_orig
            .clone()
            .ok_or_else(|| "缺少 fake_soc 原值，无法读原始 capacity".to_string())?;
        let restore_needed = self.fake_soc_last.is_some();
        if restore_needed {
            io.write(fake_path, &orig)
                .map_err(|e| format!("临时恢复 {fake_path} 失败：{e}"))?;
        }
        let raw = io.read(cap_path).ok().and_then(|r| logic::parse_active(&r));
        if restore_needed {
            io.write(fake_path, &rsoc.to_string())
                .map_err(|e| format!("写回 {fake_path}={rsoc} 失败：{e}"))?;
            self.fake_soc_last = Some(rsoc);
        }
        raw.ok_or_else(|| "读不到原始 capacity".to_string())
    }

    /// **跟随 SOC** 的一拍（标准接口闭环，2026-11-20）
    ///
    /// # 为什么不能只写一个百分比
    ///
    /// 驱动的 `charge_control_end_threshold` 只认 `capacity`。要让停充点
    /// 落在用户设定的 `rsoc` 上，必须用实时偏差平移阈值：
    ///
    /// ```text
    /// threshold = clamp(target + (capacity - rsoc), 本机下限, 100)
    /// ```
    ///
    /// 驱动在 `capacity` 到 `threshold` 时停，此时 `rsoc ≈ target`；
    /// 图标由 ①-0 同步成 `rsoc`，三者对齐。
    ///
    /// # 停充后的恢复
    ///
    /// 驱动停住后不会因为「阈值被抬高」自动恢复。检测到
    /// `status = Full / Not charging` 且 `rsoc` 低于 `目标 - 回差` 时，
    /// 先写一拍 `en=0` 重置，下一拍再写 `en=1 + 新阈值`。
    ///
    /// # 物理上限
    ///
    /// `capacity` 封顶 100。若系统电量已满、`rsoc` 仍低于目标（电量计
    /// 校准偏差），目标**到不了**：置 `follow_unreachable` 并释放 `en`，
    /// 不再空转。
    fn tick_follow(&mut self, io: &mut dyn AddonIo, target: u8, interval: u32) -> TickOutcome {
        let (Some(cap_path), Some(rsoc_path), Some(en_path), Some(ctrl), Some(fake_path)) = (
            self.node_soc.clone(),
            self.node_rsoc.clone(),
            self.node_enable.clone(),
            self.node_ctrl.clone(),
            self.node_fake_soc.clone(),
        ) else {
            return TickOutcome::failed(
                AddonError::node_missing(
                    "跟随 SOC 缺少 capacity / fg1_rsoc / fake_soc / en / ctrl 节点",
                ),
                interval,
            );
        };
        let Some(rsoc) = io
            .read(&rsoc_path)
            .ok()
            .and_then(|r| logic::parse_active(&r))
        else {
            return TickOutcome::failed(
                AddonError::bad_cond("跟随 SOC 需要读到 fg1_rsoc"),
                interval,
            );
        };

        // fake_soc 会覆盖 capacity，必须周期性恢复原值读原始容量。
        // 接近目标时每拍刷新（偏差在尾段会跳 1~2%），其余拍 4 拍一次；
        // 中间拍用缓存偏差估算（capacity ≈ rsoc + offset）。
        self.follow_tick = self.follow_tick.wrapping_add(1);
        let near_target = rsoc.saturating_add(5) >= target;
        let refresh = self.follow_offset.is_none() || near_target || self.follow_tick % 4 == 0;
        let cap = if refresh {
            match self.read_raw_capacity(io, rsoc, &cap_path, &fake_path) {
                Ok(c) => c,
                Err(e) => return TickOutcome::failed(AddonError::bad_cond(e), interval),
            }
        } else {
            let off = self.follow_offset.unwrap_or(0);
            (i16::from(rsoc) + off).clamp(0, 100) as u8
        };

        let offset = i16::from(cap) - i16::from(rsoc);
        self.follow_offset = Some(offset);
        let (min, max) = self.effective_range();
        let shifted = i16::from(target) + offset;
        let threshold = shifted.clamp(i16::from(min), i16::from(max)) as u8;
        self.follow_threshold = Some(threshold);
        self.last_target = Some(target);

        let en_on = io
            .read(&en_path)
            .ok()
            .map(|v| v.trim() != "0")
            .unwrap_or(false);
        let status = self
            .node_status
            .clone()
            .and_then(|p| io.read(&p).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let stopped = matches!(status.as_str(), "Full" | "Not charging");
        let need_charge = rsoc < target;
        let below_hyst = rsoc.saturating_add(config::FOLLOW_HYSTERESIS) < target;

        // ① 系统电量已满、目标仍高于 rsoc，且平移阈值已顶到上限：
        //    物理上到不了（电量计校准限制）⇒ 释放 en，不再空转。
        if cap >= max && need_charge && shifted > i16::from(max) {
            self.follow_unreachable = true;
            self.follow_resume = false;
            if en_on {
                let _ = io.write(&en_path, "0");
                self.enable_engaged = false;
                self.last_written = None;
                return TickOutcome::wrote(interval);
            }
            self.enable_engaged = false;
            return TickOutcome::idle(interval);
        }
        self.follow_unreachable = false;

        // ② 已停、en 还开着、rsoc 已低于「目标 - 回差」：释放一拍恢复充电
        if need_charge && stopped && en_on && below_hyst && io.write(&en_path, "0").is_ok() {
            self.enable_engaged = false;
            self.follow_resume = true;
            self.last_written = None;
            log::info!("[soc_limit] 跟随 SOC：rsoc {rsoc}% 低于目标 {target}%，释放 en 以恢复充电");
            return TickOutcome::wrote(interval);
        }

        // ③ 已停在回差区间内（目标-回差 <= rsoc < 目标）：保持，不反复开关
        if need_charge && stopped && !below_hyst && en_on {
            self.follow_resume = false;
            return TickOutcome::idle(interval);
        }
        // ④ 已到目标且已停、en/阈值都正确：保持
        if !need_charge && stopped && en_on && self.last_seen == Some(threshold) {
            self.follow_resume = false;
            return TickOutcome::idle(interval);
        }

        // ⑤ 其余：en=1 + 平移阈值（低于目标时 threshold > capacity，
        //    驱动会继续充；到目标时 capacity >= threshold，驱动停）
        let mut wrote = false;
        let en_was_off = !en_on;
        if en_was_off {
            if let Err(e) = io.write(&en_path, "1") {
                return TickOutcome::failed(
                    AddonError::bad_cond(format!("打开 {en_path} 失败：{e}")),
                    interval,
                );
            }
            self.enable_engaged = true;
            self.follow_resume = false;
            wrote = true;
        }
        if en_was_off || self.last_seen != Some(threshold) {
            if let Err(e) = io.write(&ctrl, &threshold.to_string()) {
                return TickOutcome::failed(
                    AddonError::bad_cond(format!("写入 {ctrl} 失败：{e}")),
                    interval,
                );
            }
            self.last_written = Some(threshold);
            self.write_count = self.write_count.saturating_add(1);
            wrote = true;
        }
        let seen = io.read(&ctrl).ok().and_then(|r| logic::parse_active(&r));
        self.last_seen = seen;
        if seen != Some(threshold) {
            self.verify_fail_streak = self.verify_fail_streak.saturating_add(1);
            return TickOutcome {
                wrote: true,
                changed: true,
                error: Some(AddonError::bad_cond(format!(
                    "跟随 SOC 写入后回读不符：期望阈值 {threshold}%，实得 {}",
                    seen.map_or_else(|| "读不到".to_string(), |v| format!("{v}%"))
                ))),
                next_interval_s: interval,
            };
        }
        self.verify_fail_streak = 0;
        if wrote {
            TickOutcome::wrote(interval)
        } else {
            TickOutcome::idle(interval)
        }
    }

    /// **写入方冲突**的提示后缀（不含则返回空串）
    ///
    /// # 判据
    ///
    /// `smart_chg` 是**全局节点** —— 任何写它的程序都在改同一个开关。
    /// 两个程序同时启用时会出现「我们写 85 → 立刻被改回 80 → 下拍再写 85
    /// → 又被改回」的循环，表现为**连续多次**回读不符。
    ///
    /// ⚠️ 门槛设在 **2 次**而不是 1 次：
    ///    单次回读不符未必是冲突 —— 驱动可能延迟一拍才把控制字应用到
    ///    `soc_limit` 上。凭一次不符就断言"有别的程序在抢"会误报，
    ///    反而让用户去卸载一个根本没冲突的模块。
    ///
    /// ⚠️ 这个提示**不指名任何具体模块**：会写这个节点的程序不止一个，
    ///    写死名字将来必然过时，而且用户可能装的是我们不知道的东西。
    fn conflict_hint(&self) -> &'static str {
        if self.config.warn_on_conflict && self.verify_fail_streak >= 2 {
            "（连续多次写下去就被改回 —— 可能有另一个程序也在写这个节点，请二选一）"
        } else {
            ""
        }
    }

    /// 当前配置
    pub fn config(&self) -> &SocLimitConfig {
        &self.config
    }

    /// 覆盖配置（**不会**自动写节点 —— 下一拍 `tick` 才会生效）
    pub fn set_config(&mut self, config: SocLimitConfig) {
        self.config = config;
    }

    /// 命中的控制节点路径
    pub fn node_ctrl(&self) -> Option<&str> {
        self.node_ctrl.as_deref()
    }

    /// 命中的读取节点路径
    pub fn node_read(&self) -> Option<&str> {
        self.node_read.as_deref()
    }

    /// 最近一次读到的生效上限
    pub fn last_seen(&self) -> Option<u8> {
        self.last_seen
    }

    /// 累计写入次数
    pub fn write_count(&self) -> u64 {
        self.write_count
    }

    /// 只读地把当前生效值刷新一次（供 `limit show` 之类的查询命令用）
    ///
    /// **只读，不写节点。**
    pub fn refresh_active(&mut self, io: &mut dyn AddonIo) {
        if let Some(read) = self.node_read.clone() {
            self.last_seen = io
                .read(&read)
                .ok()
                .and_then(|raw| logic::parse_active(&raw));
        }
    }

    // ============================================================
    //  定期满充（电池校准）
    // ============================================================

    /// 定期满充的运行状态（只读）
    pub fn full_state(&self) -> &FullChargeState {
        &self.state
    }

    /// 满充目标：解除上限、尽量充到 100%
    ///
    /// 若本机检测出的上限区间上界低于 100（极少见），取区间上界 ——
    /// 驱动不支持 100 时硬写只会被拒绝，然后连续失败熔断。
    fn full_target(&self) -> u8 {
        let (_, max) = self.effective_range();
        max.clamp(config::SOC_PHYSICAL_MIN, config::FULL_SOC)
    }

    /// 本拍实际要写入的上限
    ///
    /// | 状态 | 目标 |
    /// |---|---|
    /// | 定期满充进行中 | [`Self::full_target`]（通常 100%） |
    /// | 其余 | `config.soc`（用户设定值） |
    fn effective_target(&self) -> u8 {
        if self.state.pending {
            self.full_target()
        } else {
            self.config.soc
        }
    }

    /// 首次巡检时加载状态（每个进程一次）
    ///
    /// 坏文件 / 缺文件一律退回默认值：最多从头计数，
    /// **绝不能**因为状态坏了而让充电上限失效。
    fn ensure_state(&mut self, io: &mut dyn AddonIo, now_s: i64) {
        if self.state_loaded {
            return;
        }
        self.state_loaded = true;

        if let Some(raw) = io.load_state(config::STATE_ID) {
            match serde_json::from_str::<FullChargeState>(&raw) {
                Ok(s) => self.state = s,
                Err(e) => log::warn!("[soc_limit] 满充状态文件损坏，已重置：{e}"),
            }
        }

        // 周计时的起点：从未显式记录过就取"现在"
        if self.state.enabled_since_s <= 0 {
            self.state.enabled_since_s = now_s;
            self.state_dirty = true;
        }
        // 时钟回拨：记录的上次满充时间在未来 ⇒ 视为从未满充
        if self.state.last_full_epoch_s > now_s.saturating_add(config::DAY_S) {
            log::warn!("[soc_limit] 满充时间戳异常（在未来），已重置");
            self.state.last_full_epoch_s = 0;
            self.state_dirty = true;
        }
        // 上次进程退出时正在满充 ⇒ 继续完成它（不许悄悄吞掉）
        if self.state.pending {
            self.state_dirty = true;
        }
    }

    /// 读当前电量（%）：优先驱动真实值，其次系统口径
    ///
    /// 只读；两个节点都读不到返回 `None`（本拍不累计，不猜）。
    fn read_soc(&mut self, io: &mut dyn AddonIo) -> Option<u8> {
        for p in [self.node_rsoc.clone(), self.node_soc.clone()]
            .into_iter()
            .flatten()
        {
            if let Ok(raw) = io.read(&p) {
                if let Some(v) = logic::parse_active(&raw) {
                    return Some(v);
                }
            }
        }
        None
    }

    /// 是否已经充满（只读）
    ///
    /// 两路判据：SOC 到 100%，或驱动状态变成 `Full`。
    fn is_full(&mut self, io: &mut dyn AddonIo, soc: u8) -> bool {
        if soc >= config::FULL_SOC {
            return true;
        }
        let Some(p) = self.node_status.clone() else {
            return false;
        };
        io.read(&p).map(|s| s.trim() == "Full").unwrap_or(false)
    }

    /// 累计充入电量 / 完成满充 / 触发满充（**每拍一次**）
    ///
    /// # 完整循环怎么算（用户明确要求）
    ///
    /// 用户原话（2026-11-15）：「充电 10 次必须是 10 次完整循环，
    /// 不能插一次电源就计一次」。
    ///
    /// ⇒ 按 **SOC 正增量**累计：只在"充入"时增加，拔电/使用时下降不扣减。
    ///
    /// | 场景 | 计数 |
    /// |---|---|
    /// | 20% → 80%（一个充电会话） | **+60%**（0.6 个循环） |
    /// | 80% 拔电用一天到 30% | 不变 |
    /// | 第二天 30% → 80% | +50% |
    /// | 两次合计 | 110% ⇒ **1 个完整循环 + 余 10%** |
    ///
    /// 只有累计到 10 × 100% 才触发满充，因此"一天插拔四五次"
    /// 绝不会被误计成十次。
    fn observe_charge(&mut self, io: &mut dyn AddonIo, now_s: i64) {
        let Some(soc) = self.read_soc(io) else {
            return;
        };

        // ── 满充进行中：只等"充满"，不再累计循环 ──
        //
        // 满充本身充入的电量不占下一个周期额度：那是校准，不是日常循环。
        if self.state.pending {
            if self.is_full(io, soc) {
                self.finish_full(now_s);
            } else {
                self.state.last_soc = Some(soc);
            }
            return;
        }

        // ── 正常累计：只认正增量 ──
        if let Some(last) = self.state.last_soc {
            if soc > last {
                self.state.charged_pct = self
                    .state
                    .charged_pct
                    .saturating_add(u32::from(soc - last))
                    .min(config::CYCLE_CAP_PCT);
                self.state_dirty = true;
            }
        }
        self.state.last_soc = Some(soc);

        // ── 条件满足 ⇒ 本拍起临时放开上限 ──
        if self.should_start_full(now_s) {
            self.state.pending = true;
            self.state_dirty = true;
            self.state_force_save = true;
            let target = self.full_target();
            log::info!(
                "[soc_limit] 定期满充触发（已充入 {} 个完整循环，上次满充 {}）：上限临时放开到 {}%",
                self.state.cycles_done(),
                self.last_full_desc(),
                target
            );
        }
    }

    /// 「上次满充」的人话描述（仅日志用）
    fn last_full_desc(&self) -> String {
        if self.state.last_full_epoch_s > 0 {
            format!("于 {}", self.state.last_full_epoch_s)
        } else {
            "从未".to_string()
        }
    }

    /// 满充完成：复位计数、记下时间
    ///
    /// ⚠️ 下一拍起 `effective_target()` 自动回到用户设定值，
    ///    从而把上限**恢复**回去（不需要单独一次"释放"）。
    fn finish_full(&mut self, now_s: i64) {
        let cycles = self.state.cycles_done();
        self.state.pending = false;
        self.state.last_full_epoch_s = now_s;
        self.state.enabled_since_s = now_s;
        self.state.charged_pct = 0;
        // 从 100% 往下用掉再充，才算新周期的充入
        self.state.last_soc = Some(config::FULL_SOC);
        self.state_dirty = true;
        self.state_force_save = true;
        log::info!("[soc_limit] 定期满充完成（累计 {cycles} 个完整循环），已恢复充电上限");
    }

    /// 是否应当开始一次满充
    fn should_start_full(&self, now_s: i64) -> bool {
        if !self.config.full_enabled || self.state.pending {
            return false;
        }
        // 用户本来就不限上限 ⇒ 不需要"放开"
        if self.config.soc >= self.full_target() {
            return false;
        }

        let by_cycles = self.config.full_after_cycles > 0
            && self.state.cycles_done() >= self.config.full_after_cycles;

        let by_days = self.config.full_interval_days > 0 && {
            let base = if self.state.last_full_epoch_s > 0 {
                self.state.last_full_epoch_s
            } else {
                self.state.enabled_since_s
            };
            base > 0
                && now_s.saturating_sub(base)
                    >= i64::from(self.config.full_interval_days) * config::DAY_S
        };

        by_cycles || by_days
    }

    /// 周条件下的"下次满充时间"（Unix 秒）；未启用该条件时为 `None`
    fn full_next_epoch_s(&self) -> Option<i64> {
        if !self.config.full_enabled || self.config.full_interval_days == 0 {
            return None;
        }
        let base = if self.state.last_full_epoch_s > 0 {
            self.state.last_full_epoch_s
        } else {
            self.state.enabled_since_s
        };
        if base <= 0 {
            return None;
        }
        Some(base + i64::from(self.config.full_interval_days) * config::DAY_S)
    }

    /// 状态落盘（节流；关键变化立即写）
    ///
    /// ⚠️ 落盘失败**不是**功能错误：最多丢一点进度，绝不能让
    ///    充电上限因此熔断。只记一条 warn（不刷屏）。
    fn persist_state(&mut self, io: &mut dyn AddonIo, now_s: i64) {
        if !self.config.enabled || !self.state_loaded || !self.state_dirty {
            return;
        }
        let due = now_s.saturating_sub(self.state.saved_at_s) >= config::STATE_SAVE_MIN_GAP_S;
        if !due && !self.state_force_save {
            return;
        }
        let json = match serde_json::to_string(&self.state) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("[soc_limit] 满充状态序列化失败：{e}");
                return;
            }
        };
        match io.save_state(config::STATE_ID, &json) {
            Ok(()) => {
                self.state.saved_at_s = now_s;
                self.state_dirty = false;
                self.state_force_save = false;
                self.state_save_warned = false;
            }
            Err(e) => {
                if !self.state_save_warned {
                    log::warn!("[soc_limit] 满充状态落盘失败（不影响充电上限）：{e}");
                    self.state_save_warned = true;
                }
            }
        }
    }
}

impl Addon for SocLimit {
    fn spec(&self) -> AddonSpec {
        AddonSpec {
            id: "soc_limit",
            label: "充电上限",
            summary: "充到指定电量即停止，保护电池寿命",
            // ⚠️ 来源说明**不指名任何第三方模块**（用户要求，2026-10-16）
            //
            // 这里只描述**机制本身是怎么确证的** —— 那才是这条信息
            // 对使用者的价值（"这不是猜的"）。
            // 而"参考过哪个程序"既不影响使用，也会让成品看起来像是别人的附庸，
            // 因此不写进任何面向用户的字段。
            //
            // ⚠️ 按方言说（2026-10-19）：标准 `power_supply` 的机型
            //    既没有 `smart_chg` 也没有 `soc_limit`，照抄小米那套
            //    说法会让用户去核对两个他机器上根本不存在的节点。
            origin: match self.dialect {
                Dialect::ControlWord => {
                    "节点机制由真机实测确证：写 smart_chg 控制字、读 soc_limit 生效值"
                }
                Dialect::Percent => {
                    "节点机制由真机实测确证：写 charge_control_end_threshold 直读百分比"
                }
            },
            default_interval_s: 15,
            // ⚠️ **使用前提**（用户明确要求加上，2026-10-16）
            //
            // # 为什么这个前提是真的
            //
            // 本功能写的是 `smart_charge/smart_chg` —— 它属于系统
            // 「电池保护」（HyperOS 里也叫「电池健康保护」）功能的实现：
            // 系统设置页里那个 80% 默认值，改的就是这个节点，
            // 而我们改的是同一个节点上的自定义值。
            //
            // 也就是说：**我们和系统在写同一个开关**。系统那侧的功能开关
            // 关掉时，驱动是否仍执行 SOC 限制**不可依赖** —— 它本来就是
            // 该功能的一部分。
            //
            // ⇒ **系统开关关掉时，这个节点的行为不可靠**（驱动可能不执行
            //    SOC 限制）。所以必须让用户先打开它。
            //
            // ⚠️ 未插充电器时驱动**只记录不执行**（dmesg:
            //    `adapter is plugout, don't handle soc limit`），
            //    因此"没插电时看不到效果"是正常的，与开关无关。
            //
            // ⚠️ **按方言选文案**（2026-10-19）：标准 `power_supply` 的
            //    `charge_control_end_threshold` 不归「电池保护」开关管，
            //    对它喊"请先打开电池健康保护"是错的。见
            //    [`REQUIRES_STANDARD_PSY`] 的说明。
            requires: Some(match self.dialect {
                Dialect::ControlWord => REQUIRES_BATTERY_PROTECT,
                Dialect::Percent => REQUIRES_STANDARD_PSY,
            }),
        }
    }

    fn probe(&mut self, io: &mut dyn AddonIo) -> ProbeOutcome {
        for cand in SMART_CHG_CANDIDATES {
            if io.writable(cand) {
                // ⚠️ 先定方言，再据此决定"生效值"节点是哪一个
                let d = Dialect::of(cand);
                let read_path = read_node_for(cand, d);

                // ── ⚠️ 探测阶段**不做试写**（2026-10-19 撤销）──
                //
                // 曾经在这里"试写一次自己的原值"，想验证"权限看着可写、
                // 驱动是否真接受"。撤销的原因有两条，都是硬的：
                //
                // ① **写回原值不是零副作用**。
                //    `smart_chg` 是**控制字**节点，而它的**读值**在
                //    16 / 17 / 80 之间漂移，与写入值没有稳定对应关系
                //    （真机实测确证，见 `logic` 的注释）。把读到的
                //    `80` 写回去，驱动按十六进制解析成 `0x80` ——
                //    低 16 位没有 `0x11` 使能位 ⇒ **等于把用户当前的
                //    SOC 限制关掉**。这不是"内容不变"。
                // ② **它破坏了「未启用 = 零写入」**。
                //    `probe` 在模块启动时无条件执行，用户哪怕没启用
                //    充电上限，也会被写一次节点。这条纪律是用户明确
                //    要求的（决策 B），不能因为"想早点知道能不能写"
                //    而破例。
                //
                // ⇒ 可写性验证**推迟到真要写的那一刻**（`tick`）：
                //   那时写的就是目标值本身，写不进去会如实报错
                //   （含 SELinux 的 `Permission denied`）、计入连续失败、
                //   最终熔断。用户看到的是真实原因，而不是一个
                //   探测阶段的"支持/不支持"。
                self.dialect = d;
                self.node_ctrl = Some((*cand).to_string());
                self.node_read = Some(read_path);

                // ── 探测**使能节点**（仅标准百分比方言需要）──
                //
                // ⚠️ 只探测、**不写入**（保持 probe 零写入的纪律）。
                //    真正的 `1` 在 `tick` 要写目标值时才写。
                self.node_enable = if d == Dialect::Percent {
                    SOC_ENABLE_CANDIDATES
                        .iter()
                        .find(|p| io.writable(p))
                        .map(|p| (*p).to_string())
                } else {
                    None
                };
                self.enable_engaged = false;

                // ── 探测 SOC 口径校正所需的两组节点（**只读**）──
                //
                // 见 [`FAKE_SOC_CANDIDATES`]：把系统显示校正成驱动真实值。
                // ⚠️ probe 阶段**不写** fake_soc，只记下原值备用。
                self.node_rsoc = RSOC_CANDIDATES
                    .iter()
                    .find(|p| io.exists(p))
                    .map(|p| (*p).to_string());
                self.node_fake_soc = if self.node_rsoc.is_some() {
                    FAKE_SOC_CANDIDATES
                        .iter()
                        .find(|p| io.writable(p))
                        .map(|p| (*p).to_string())
                } else {
                    None
                };
                self.fake_soc_orig = self
                    .node_fake_soc
                    .as_ref()
                    .and_then(|p| io.read(p).ok())
                    .map(|s| s.trim().to_string());
                self.fake_soc_last = None;
                self.sync_error = false;

                // ── 定期满充要用的两个**只读**节点（探测阶段不写）──
                //
                // 都只是采样：`capacity` 用于累计充入电量，`status` 用于
                // 判断"已充满"。读不到也不影响充电上限本身 —— 满充功能
                // 会退化为"只能靠 SOC 判据"，最坏情况是满充完成后多等
                // 一次巡检才恢复上限。
                self.node_soc = CAPACITY_CANDIDATES
                    .iter()
                    .find(|p| io.exists(p))
                    .map(|p| (*p).to_string());
                self.node_status = STATUS_CANDIDATES
                    .iter()
                    .find(|p| io.exists(p))
                    .map(|p| (*p).to_string());

                let outcome = ProbeOutcome::Supported {
                    detail: match &self.node_enable {
                        Some(e) => format!("命中 {cand}（使能 {e}）"),
                        None => format!("命中 {cand}"),
                    },
                };
                self.support = Some(outcome.clone());
                return outcome;
            }
        }

        self.node_ctrl = None;
        self.node_read = None;
        self.node_enable = None;
        self.enable_engaged = false;
        self.node_soc = None;
        self.node_status = None;
        // ⚠️ 找不到节点时，**先怀疑系统开关、再怀疑机型**
        //
        // # 为什么把"电池保护没开"放在第一位
        //
        // 这两种原因的**表象完全相同**（都表现为节点不存在或不可写），
        // 但处理成本差了一个数量级：
        //
        // | 原因 | 用户要做什么 | 成本 |
        // |---|---|---|
        // | **电池保护开关没开** | 去设置里打开 | 10 秒 |
        // | 机型真的不支持 | 无解，放弃该功能 | —— |
        //
        // 若只写"本机型不支持"，绝大多数用户会直接放弃 ——
        // 而他们很可能只是**少开了一个开关**。
        //
        // 反过来写（先提示查开关）不会误导：真不支持的机型用户
        // 查完开关发现本来就是开的，自然得出"确实不支持"的结论。
        //
        // ⚠️ 2026-10-19：文案**不再点名 `smart_chg`**，也不再假定
        //    "本机一定有电池保护开关"。标准 `power_supply` 的机型
        //    用的是 `charge_control_end_threshold`，有些机型压根没有
        //    「电池健康保护」这个设置项 —— 点名某个节点/开关会让
        //    这类用户以为自己的机器缺了东西。
        //    措辞改成条件式（"若你的机型有…开关"），两类机型都对得上。
        let outcome = ProbeOutcome::Unsupported {
            reason: format!(
                "未找到可写的充电上限节点。若你的机型有「电池健康保护」开关，\
                 请先确认它已打开（设置 → 省电与电池 → 电池保护）；\
                 若本来就开着，则本机型可能不支持充电上限\
                 （已尝试 {} 条候选路径）",
                SMART_CHG_CANDIDATES.len()
            ),
        };
        self.support = Some(outcome.clone());
        outcome
    }

    fn tick(&mut self, io: &mut dyn AddonIo, now_s: i64) -> TickOutcome {
        // 上一拍累计的进度在这里节流落盘（触发/完成当拍会再写一次，见 ②）
        self.persist_state(io, now_s);

        let interval = self.config.interval_s();

        // ① 未启用 → 停止干预
        //
        // ⚠️ 但**必须恢复 fake_soc**：本功能可能改过电量显示，
        //    关掉后不还原，电池图标就会一直显示假电量。
        if !self.config.enabled {
            self.restore_display(io);
            // 🔴 还要把**我们开过的使能位**关回去（2026-11-01 K70 Pro 真机实测）
            //
            // # 原来的 bug
            //
            // `release()` 里有「开过 en 就关回 0」的逻辑，但**这条分支
            // 根本不调 release()** —— 它直接 `idle` 返回。于是：
            //
            // | 操作 | `charge_control_en` |
            // |---|---|
            // | 启用上限（写了 85） | 1（我们开的） |
            // | `ctl limit off` | **仍是 1** ❌ |
            // | 卸载（走 `release_all`） | 0 ✓ |
            //
            // 即「关闭功能」不还原使能位，只有「卸载」才还原。而 `en=1`
            // 时驱动会**继续执行**我们上次写的阈值 —— 用户以为关掉了，
            // 实际充到 85 就停。这违反 R12「关闭 ≠ 卸载」。
            //
            // 判据用**节点真实值**而不是 `enable_engaged`：worker 重启后
            // 内存标记是 false，但节点可能仍是上次的 1（见 ①-1 的说明）。
            let mut wrote = false;
            if let Some(en) = self.node_enable.clone() {
                let engaged = io.read(&en).ok().map(|v| v.trim() != "0").unwrap_or(false);
                if engaged && io.write(&en, "0").is_ok() {
                    wrote = true;
                    log::info!("[soc_limit] 功能已关闭，使能位 {en} 已写回 0");
                }
            }
            self.enable_engaged = false;
            self.last_written = None;
            self.follow_resume = false;
            self.follow_threshold = None;
            self.follow_unreachable = false;
            return if wrote {
                TickOutcome::wrote(interval)
            } else {
                TickOutcome::idle(interval)
            };
        }

        // ①-0 电量显示同步（用户要求，见 `sync_display_to_driver` 的文档）
        //
        // | 配置 `limit.sync_display` | 行为 |
        // |---|---|
        // | `true`（默认） | 每拍把 `fake_soc` 写成当前 `rsoc`，图标与驱动同口径 |
        // | `false` | **恢复原值**并停止改写 |
        //
        // ⚠️ 关闭时必须 `restore_display`，不能只是"不写" ——
        //    否则图标会停在**上一次同步的结果**上（一个过期的假值），
        //    与系统自己的算法脱节。这与"关闭功能"走的是同一条恢复路径。
        // ⚠️ 标准接口默认**不写** fake_soc：驱动按系统电量截止，改写图标
        //    会让显示与截止口径打架（见 `sync_supported` 的文档）。
        //    开启「跟随 SOC」后必须同步，否则显示的又不是截止口径。
        let want_sync = self.sync_supported() && (self.config.sync_display || self.follow_mode());
        if want_sync {
            self.sync_display_to_driver(io);
        } else {
            self.restore_display(io);
        }

        // ①-0b 定期满充：加载跨重启状态（每个进程只加载一次）
        //
        // ⚠️ 必须在下面的"是否插电"判断之前：拔电时也要能读到
        //    `pending`（界面显示"正在满充"），只是不累计。
        self.ensure_state(io, now_s);

        // ①-1 🔴 **从节点回读使能位真实状态**（2026-10-20 真机反馈）
        //
        // # 为什么不能只信内存标记
        //
        // `enable_engaged` 只是本进程的**内存标记**，而 `charge_control_en`
        // 是**驱动持久化**的节点值。两者会脱节：
        //
        // | 场景 | 内存标记 | 节点实际 |
        // |---|---|---|
        // | worker 重启 | `false` | **`1`**（上次写的一直在） |
        // | 拔电期间 | 不写、也不置位 | 保持 `1` |
        //
        // 旧逻辑据此判 `soc_in_effect = false` ⇒ 界面显示**「未生效」**，
        // 而实际限制是生效的 —— 用户看到就认定功能坏了。
        //
        // ⇒ 每拍读一次真实值即可对齐。代价：一次只读 sysfs（15~60 秒一拍）。
        if let Some(en) = self.node_enable.clone() {
            if let Ok(v) = io.read(&en) {
                self.enable_engaged = v.trim() != "0";
            }
        }

        // ①-2 🔋 定期满充的采样 / 触发 / 完成（每拍一次，含拔电时）
        //
        // 只认 SOC **正增量**：拔电后使用导致下降不会扣减已充入的电量，
        // 也不会把"插一次电源"误记成一次充电（用户明确要求）。
        // 拔电时仍要做完成判断 —— 用户常常充满就拔，不能因为拔得快
        // 就永远停在满充模式。
        self.observe_charge(io, now_s);
        // 触发/完成满充是状态机关键点：当拍立即落盘，不等 60 秒节流
        self.persist_state(io, now_s);

        // ② ⚠️ **插着充电器才巡检**        //
        // # 为什么要加这个判据
        //
        // 限制值只可能在**充电时**被改写 —— 没插电时驱动根本不会去充，
        // 也就不存在"限制失效"的风险。而每次巡检都是一次 sysfs 读取，
        // 会打断 CPU 深度睡眠。
        //
        // 宿主（worker）据同一判据把"未充电 + 无客户端"整段进入深度阻塞，
        // 真正做到空闲时零周期性唤醒。详见 docs/13-检测功能与后台策略.md §5.3。
        //
        // ⚠️ 必须**每拍现查**（而不是缓存成字段）：缓存会在插拔瞬间过期，
        //    导致"插了电却仍在跳过"或反之。
        //
        // # 边界：插电瞬间
        //
        // 深度阻塞由 `power_supply` uevent 唤醒，插上充电器会**立即**跑一拍，
        // 此时 `is_plugged()` 为真，本函数正常工作。因此不存在
        // "插了电但限制值没同步"的窗口。
        //
        // # 边界：拔电后限制值被系统改写
        //
        // 拔电后不再巡检，若此时驱动自行改写了限制值，要等到**下次插电**
        // 才会被发现并修回。这是可接受的：
        //   · 没插电时限制值是多少都无所谓（不充电）
        //   · 下次插电的第一件事就是校验并修正（见上面"插电瞬间"）
        //
        // ── ⚠️ 但**必须读一次生效值**（2026-10-20 真机反馈）──
        //
        // 原先这里直接 `idle` 返回，于是 `last_seen` 永远是 `None`：
        //
        // | 字段 | 拔电时 | 用户看到 |
        // |---|---|---|
        // | `soc_active` | `null` | 「—」 |
        // | `soc_in_effect` | `false` | **「未生效」** |
        //
        // 而那时节点里的值其实是**正确**的（`threshold=60`、`en=1`）。
        // 用户看到「未生效」就判定**功能坏了** —— 本轮"充电上限无效"
        // 的反馈正是这么来的。
        //
        // ⇒ 拔电时不写、不校验，但**读一次**用于状态显示。
        //   代价：一次只读 sysfs；收益：界面不再谎报"未生效"。
        if !io.is_plugged() {
            // 拔电时仍读一次生效值，供界面显示（否则会一直报「未生效」）
            if let Some(read) = self.node_read.clone() {
                self.last_seen = io
                    .read(&read)
                    .ok()
                    .and_then(|raw| logic::parse_active(&raw));
            }
            return TickOutcome::idle(interval);
        }

        // ③ 配置自检（fail-closed：坏配置绝不拿去写节点）
        //
        // ⚠️ 用**本机检测出的区间**校验，而非写死的 60~100
        let (range_min, range_max) = self.effective_range();
        if let Err(e) = self.config.validate_within(range_min, range_max) {
            return TickOutcome::failed(AddonError::out_of_range(e), interval);
        }

        // ③-b 目标 100% = 不限（标准接口，2026-11-19 真机反馈）
        //
        // 标准接口的阈值按**系统电量**比较：系统电量到 100 时驱动就停充，
        // 哪怕电池实际还没充满。用户设 100 的本意是「充满 / 不限制」，
        // 因此不能写 100，而是把使能位关回 0，交回系统自己的充电逻辑。
        //
        // ⚠️ 与「关闭功能」一样要释放 en，否则用户设 100 后仍然充不进。
        // ⚠️ 只对标准百分比方言生效：小米控制字写 100|0x11 就是正常满充。
        let target = self.effective_target();
        if self.is_unlimited_target(target) {
            self.last_target = Some(target);
            self.last_written = None;
            let mut wrote = false;
            if let Some(en) = self.node_enable.clone() {
                let en_on = io.read(&en).ok().map(|v| v.trim() != "0").unwrap_or(false);
                if en_on && io.write(&en, "0").is_ok() {
                    wrote = true;
                    log::info!("[soc_limit] 上限 {target}% 视为不限，已释放使能位 {en}");
                }
            }
            self.enable_engaged = false;
            // 读一次生效值供界面诊断（此时它可能是上一次的上限值）
            if let Some(read) = self.node_read.clone() {
                self.last_seen = io
                    .read(&read)
                    .ok()
                    .and_then(|raw| logic::parse_active(&raw));
            }
            return if wrote {
                TickOutcome::wrote(interval)
            } else {
                TickOutcome::idle(interval)
            };
        }

        // ③-c 跟随 SOC（标准接口闭环，2026-11-20）
        //
        // 只在 `limit.follow_soc` 开启且节点齐全时进入；否则走下面的
        // 「按系统电量」路径，行为与 v1.1.1 一致。
        if self.follow_mode() {
            return self.tick_follow(io, target, interval);
        }

        // ④ 节点必须已探测到
        let (Some(ctrl), Some(read)) = (self.node_ctrl.clone(), self.node_read.clone()) else {
            return TickOutcome::failed(
                AddonError::node_missing("smart_chg 节点不可用（本机型可能不支持充电上限）"),
                interval,
            );
        };

        // ⑤ 读"生效值"——**永远读 soc_limit，不读 smart_chg**
        // ⚠️ 读不到「生效值」**不是错误**
        //
        // `read` 由 `read_node_for` 推出：
        //   · 小米方言 → 同目录的 `soc_limit`
        //   · 标准方言 → 控制节点自己
        //
        // 而 `soc_limit` 这个节点**大多数机型根本没有**（实测 2026-10-19：
        // 走 `qcom-battery/smart_chg` 的机型上它不存在）。原实现把它当
        // `failed`，于是每拍都报错、`writes` 永远是 0 —— 限制值一次都
        // 没写进去，而界面上只看到"失败"，看不出原因。
        //
        // 读不到 ⇒ 视为**未知** ⇒ 照常走下面的 `needs_rewrite` 写入。
        // 这正是那个函数的设计意图：`actual: Option<u8>`，`None` 就写。
        let actual = match io.read(&read) {
            Ok(raw) => logic::parse_active(&raw),
            Err(_) => None,
        };

        self.last_seen = actual;

        // ⑤-1 写入值：平时 = **用户设定值，原样写**
        //
        // 标准接口按系统电量截止，写进去的就是用户设定值；小米控制字
        // 也是同一口径 —— 不需要任何换算或闭环。
        //
        // 🔋 定期满充进行中 ⇒ 临时把上限放到 100%（见 `effective_target`）：
        //    充满后 `finish_full` 复位 `pending`，下一拍自动写回用户设定值。
        self.last_target = Some(target);

        // ⑥ 已一致 → 不写（这是常态：绝大多数拍什么都不做）
        //
        // 🔴 **但标准方言下使能位为 0 时不算"已一致"**（2026-11-01 K70 Pro 真机）
        //
        // # 这个坑很隐蔽
        //
        // `charge_control_en = 0` 时驱动**不执行**阈值，但仍会把上次写入的
        // 值留在 `charge_control_end_threshold` 里。于是：
        //
        // ```text
        // 用户设 85 → 写入 → 读回 85  → 判定"已一致" ✓
        // 某次 en 被清成 0（系统重置 / 我们的 off 分支）
        //   ⇒ 阈值节点**仍然读 85** ⇒ needs_rewrite=false ⇒ 直接 idle
        //   ⇒ 使能位永远没人去开 ⇒ 限制实际失效，电量继续涨过 85
        // ```
        //
        // 真机实测（K70 Pro，2026-11-01）：
        //
        // | en | 阈值节点 | 实际是否停充 |
        // |---|---|---|
        // | 0 | 85 | **否**（电量涨到 87） |
        // | 1 | 85 | 是（`current_now = -79 mA`） |
        //
        // ⇒ 判据必须带上使能位：`en=0` ⇒ 强制重写（走 ⑦ 重新开 en）。
        let enable_off = match (self.dialect, self.node_enable.as_deref()) {
            (Dialect::Percent, Some(en)) => {
                io.read(en).ok().map(|v| v.trim() == "0").unwrap_or(false)
            }
            _ => false,
        };

        if !logic::needs_rewrite(target, actual) && !enable_off {
            return TickOutcome::idle(interval);
        }

        // ⑦ 写控制字 `(上限 << 16) | 0x11`
        // 按命中节点的方言生成写入内容
        let text = match self.dialect {
            // 小米：控制字（位域打包）
            Dialect::ControlWord => logic::format_word(logic::encode(target)),
            // 标准：纯百分比
            Dialect::Percent => target.to_string(),
        };

        // ⑦-0 🔴 **先开使能位**（标准百分比方言，2026-10-19 真机查明）
        //
        // # 不开会怎样
        //
        // K70 Pro 上 `charge_control_en = 0` 时，写
        // `charge_control_end_threshold = 95` **返回成功但值被吞**，
        // 读回永远是 0。模块的 ⑧ 回读校验于是判"不符"，连续三次后
        // 误报"有别的程序在抢"并熔断 —— 界面上就成了"充电上限不可用"。
        //
        // ⇒ 写目标值之前必须先把使能位置 1。
        //
        // ⚠️ 只在**需要写目标值**时才开（即走到这里），不在 probe 阶段开
        //    —— 否则又破了"未启用 = 零写入"。
        // ⚠️ `enable_engaged` 只是省掉重复写：掉电/系统重置后它会
        //    变回 0，但那时回读校验失败会让本拍重来一次，不会卡死。
        if let (Dialect::Percent, Some(en)) = (self.dialect, self.node_enable.clone()) {
            if !self.enable_engaged {
                // 先读一次当前值：已经是 1 就不重复写（写 sysfs 有 I2C 开销）
                let already = io.read(&en).ok().map(|s| s.trim() != "0").unwrap_or(false);
                if !already {
                    if let Err(e) = io.write(&en, "1") {
                        return TickOutcome::failed(
                            AddonError::bad_cond(format!(
                                "打开充电控制使能 {en} 失败：{e}\
                                 （该节点必须为 1，否则充电上限写入会被驱动丢弃）"
                            )),
                            interval,
                        );
                    }
                }
                self.enable_engaged = true;
            }
        }

        if let Err(e) = io.write(&ctrl, &text) {
            return TickOutcome::failed(
                AddonError::bad_cond(format!("写入 {ctrl} 失败：{e}")),
                interval,
            );
        }
        self.last_written = Some(target);
        self.write_count = self.write_count.saturating_add(1);

        // ⑧ 回读校验（读 soc_limit）：写得下去 ≠ 生效
        let verify = io
            .read(&read)
            .ok()
            .and_then(|raw| logic::parse_active(&raw));
        self.last_seen = verify;
        if logic::needs_rewrite(target, verify) {
            self.verify_fail_streak = self.verify_fail_streak.saturating_add(1);
            let seen = verify.map_or_else(|| "读不到".to_string(), |v| format!("{v}%"));
            return TickOutcome {
                wrote: true,
                changed: true,
                error: Some(AddonError::bad_cond(format!(
                    "写入后回读不符：期望 {target}%，实得 {seen}{}",
                    self.conflict_hint()
                ))),
                next_interval_s: interval,
            };
        }
        // 生效了 ⇒ 冲突计数清零（单次不符可能只是驱动延迟一拍）
        self.verify_fail_streak = 0;

        TickOutcome::wrote(interval)
    }

    fn release(&mut self, io: &mut dyn AddonIo) -> TickOutcome {
        // 先把系统电量显示还原（我们可能改过它）
        self.restore_display(io);

        // ⚠️ 标准百分比方言下，**开过使能位就要关回去**（2026-10-19）
        //
        // 小米 `smart_chg` 那套是"零写入"就能停止干预（控制字自带使能位，
        // 不写就保持原状）。但标准框架的 `charge_control_en` 是**我们主动
        // 打开的** —— 不关它，用户关掉模块功能后，驱动仍在执行我们上次
        // 写入的阈值，等于"关了但没关"。
        //
        // ⇒ 只有真的开过（`enable_engaged`）才写 0，避免无谓写入。
        let mut wrote = false;
        if self.enable_engaged {
            if let Some(en) = self.node_enable.clone() {
                if io.write(&en, "0").is_ok() {
                    wrote = true;
                }
            }
            self.enable_engaged = false;
        }

        // 其余为决策 B：停止干预 = **零写入**。
        // 小米那套由系统自带的智能断充（默认 80%）照常工作。
        self.last_written = None;
        self.follow_resume = false;
        self.follow_threshold = None;
        self.follow_unreachable = false;

        if wrote {
            // 有写入就必须让下一拍尽快确认状态
            TickOutcome::wrote(IDLE_INTERVAL_S)
        } else {
            TickOutcome::idle(IDLE_INTERVAL_S)
        }
    }

    fn status(&self) -> AddonStatus {
        let spec = self.spec();
        // 「已生效」的判据：**读生效值节点**
        //
        // ⚠️ 不能用控制节点的读值（小米方言下那是 `smart_chg`）——
        //    它在 16/17/80 之间漂移，与写入值无稳定对应关系
        //    （真机实测确证）。详见 [`logic`] 的文档。
        //    标准方言下控制节点自己就是生效值，读它恰好是对的
        //    （见 [`read_node_for`]）。
        //
        // 未插充电器时驱动只记录不执行，此时生效值可能读到一个
        // 与目标不符的值 —— 这**不是失败**，插电后自会生效。
        // 因此这里只如实报告"实际读到多少"，不在后端做"是否生效"的判决。
        let active = self.last_seen;
        // 本拍实际目标：平时 = `config.soc`；定期满充中 = 100%
        let target = self.effective_target();
        let matches_target = active == Some(target);
        // 标准接口的目标 100% 等价于「不限」：没有"生效中的上限"
        let unlimited = self.config.enabled && self.is_unlimited_target(target);
        // 本拍是否走「跟随 SOC」闭环
        let follow = self.follow_mode();

        // 🔴 **"生效中"显示的是用户设定值**（2026-10-20 用户明确要求）
        //
        // # 用户反馈
        //
        // 「设定的充电上限与生效中的怎么不一样」——之前两次都是把
        // **节点内部值**当成了"生效值"暴露给用户：
        //
        // | 时期 | 界面显示 | 原因 |
        // |---|---|---|
        // | 折算方案 | 目标 85 / 生效 65 | 把节点值当生效值 |
        // | 折算+换算 | 目标 85 / 生效 89 | 旧阈值配新偏差 |
        //
        // 用户的选择是「界面显示跟设定一样」，所以这里直接显示
        // `config.soc` —— 节点里写的就是它，两者本来就该相等。
        //
        // ⚠️ 只在**已启用**时显示：关掉功能后没有"生效中的上限"，
        //    显示 `null` 才诚实（界面显示「—」）。
        //
        // 🔋 定期满充进行中显示的是**临时放开后的目标**（通常 100%）：
        //    那一刻驱动里确实是 100，显示用户设定值反而会让界面与
        //    「正在满充」的提示自相矛盾。
        let active_shown = if self.config.enabled && !unlimited {
            Some(target)
        } else {
            None
        };

        // ⚠️ 使能位没开时，即使读数与目标一致也**不算生效**（2026-10-19）
        //
        // # 为什么必须扣掉这一种情况
        //
        // `charge_control_end_threshold` 的读值是**持久化**的：写进去
        // 90 之后，即使把 `charge_control_en` 关回 0，节点仍然读 90。
        // 而 en=0 时驱动**不执行**这个阈值。
        //
        // 于是（真机实测）：模块刚启动、还没写过使能位，读回 90 恰好
        // 等于目标 ⇒ 报"已生效" ⇒ 用户以为限制生效了，其实系统根本没管。
        //
        // ⇒ 本机有使能节点、且我们没开它 ⇒ 一律 `false`。
        let enable_ok = match &self.node_enable {
            // 没有使能节点的机型：不受这条限制
            None => true,
            // 有使能节点：必须是我们开过了
            Some(_) => self.enable_engaged,
        };
        // 关掉功能后不可能是"已生效"
        //
        // 写入值恒等于 `config.soc`，所以"值匹配"就是可靠判据；
        // 目标 100% 在标准接口上等价于「不限」，也没有"生效中的上限"。
        //
        // 跟随 SOC 模式写的是**平移后的阈值**（不是 target），所以改用
        // 「en 已开 + 本拍写过阈值」判定；恢复等待期（en=0 一拍）也算，
        // 避免界面在 15 秒窗口里闪「未生效」。
        let in_effect = if follow {
            self.config.enabled
                && !unlimited
                && (self.enable_engaged || self.follow_resume)
                && self.follow_threshold.is_some()
        } else {
            self.config.enabled && !unlimited && enable_ok && matches_target
        };

        AddonStatus {
            id: spec.id.to_string(),
            label: spec.label.to_string(),
            summary: spec.summary.to_string(),
            origin: spec.origin.to_string(),
            enabled: self.config.enabled,
            supported: self.supported(),
            tripped: false, // 熔断状态由 runner 覆盖
            interval_s: self.config.interval_s(),
            probe: self
                .support
                .as_ref()
                .map_or_else(|| "尚未探测".to_string(), |p| p.message().to_string()),
            // ⚠️ 与 `spec().requires` **同源** —— 界面与 CLI 拿同一份文案
            requires: spec.requires.map(str::to_string),
            detail: json!({
                "soc_target": self.config.soc,
                // 已换算回 `capacity` 口径（与 `soc_target` 同一刻度）
                "soc_active": active_shown,
                // 目标与生效是否一致（界面据此决定要不要提示"尚未生效"）
                "soc_in_effect": in_effect,
                "verify_s": self.config.verify_s,
                "keep_on_restore": self.config.keep_on_restore,
                "warn_on_conflict": self.config.warn_on_conflict,
                // 当前是否已构成"疑似写入方冲突"（界面可据此显示提示）
                "conflict_suspected": self.verify_fail_streak >= 2,
                // ── 电量显示同步（2026-11-18）──
                //
                // ⚠️ 只追加、不改既有字段（R8：契约只增不改）。
                //
                // 界面据此渲染「系统显示 ↔ SOC 电量」那一行与同步开关：
                // `sync_available=false` 时开关置灰并说明原因（本机没有
                // 可写的显示节点，或标准接口按系统电量截止、不能同步），
                // 而不是让用户点了没反应。
                "sync_display": self.config.sync_display,
                "sync_available": self.sync_available(),
                // 本机方言是否支持同步（标准接口为 false；界面据此换文案）
                "sync_supported": self.sync_supported(),
                // 不支持时的人话原因（标准接口 / 缺节点）
                "sync_reason": self.sync_reason(),
                // 目标 100% 在标准接口上等价于「不限」（界面显示「不限」）
                "soc_unlimited": unlimited,
                // ── 跟随 SOC（2026-11-20）──
                //
                // 只追加字段（R8）。`follow_soc` 是用户配置；
                // `follow_soc_supported` 是本机节点是否齐全；
                // `soc_threshold` 是本拍写入的阈值（capacity 口径），
                // `follow_offset = capacity - rsoc` 用于诊断。
                "follow_soc": self.config.follow_soc,
                "follow_soc_supported": self.follow_supported(),
                "follow_offset": self.follow_offset,
                "soc_threshold": self.follow_threshold,
                "soc_unreachable": self.follow_unreachable,
                // 是否**正在**改写系统显示（写过且未恢复）
                "sync_engaged": self.fake_soc_last.is_some(),
                // 同步开着但**写不进去**（SELinux / 只读 / 被占用）
                // ⇒ 界面必须据此提示"开了但没生效"，不能静默
                "sync_error": self.sync_error,
                // 两条节点路径（诊断用：一眼看出本机命中了哪个节点）
                "node_rsoc": self.node_rsoc,
                "node_fake_soc": self.node_fake_soc,
                // ── 定期满充（电池校准）──
                //
                // ⚠️ 只追加、不改既有字段（R8：契约只增不改）。
                //    `soc_effective_target` 是本拍**实际**写入的目标，
                //    与用户设定值 `soc_target` 在满充期间不同。
                "full_enabled": self.config.full_enabled,
                "full_after_cycles": self.config.full_after_cycles,
                "full_interval_days": self.config.full_interval_days,
                "soc_effective_target": target,
                "full_pending": self.state.pending,
                // 累计充入的**完整循环**数（整数）与当前循环进度（%）
                "full_cycles_done": self.state.cycles_done(),
                "full_cycle_progress_pct": self.state.cycle_progress_pct(),
                "full_last_epoch_s": self.state.last_full_epoch_s,
                // 周条件下的下次满充时间（Unix 秒）；未启用该条件时为 null
                "full_next_epoch_s": self.full_next_epoch_s(),
                "node_ctrl": self.node_ctrl,
                "node_read": self.node_read,
                "last_written": self.last_written,
                "write_count": self.write_count,
                // 系统界面不会同步 —— 必须主动说明，否则用户看到系统的
                // 80% 会以为没生效
                "note_ui_not_synced": NOTE_UI_NOT_SYNCED,
            }),
        }
    }

    fn enabled(&self) -> bool {
        self.config.enabled
    }

    fn supported(&self) -> bool {
        self.support
            .as_ref()
            .is_some_and(ProbeOutcome::is_supported)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mtk_sc_tuisoc_是百分比方言() {
        let p = "/sys/class/power_supply/usb/device/sc_tuisoc";
        assert!(SMART_CHG_CANDIDATES.contains(&p), "候选表应包含 {p}");
        assert_eq!(Dialect::of(p), Dialect::Percent);
        assert_eq!(read_node_for(p, Dialect::Percent), p);
    }
}
